// 真窗口 CDP 探针（WebView2，零依赖：Node 24 内置 fetch + WebSocket）。
// 用法：
//   node cdp-probe.mjs eval   '<js 表达式>'     在页面里求值并打印 JSON
//   node cdp-probe.mjs arm                       注册 document-start 探针 → 重新加载 → 打印时间线
//   node cdp-probe.mjs shot  <out.png>           截图
// 前提：目标进程是用 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222 起的。
// 9222 是本机**无鉴权**端口，随进程生灭；探针跑完就把窗口留着，不驻留、不写配置。
const PORT = process.env.CDP_PORT ?? "9222";
const cmd = process.argv[2];

const targets = async () => {
  for (let i = 0; i < 40; i += 1) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
      // 只认应用页面，认不出 dev/产物的 origin 差别 —— 按 type=page 且非 devtools 取第一条。
      const page = list.find((t) => t.type === "page" && !/devtools/i.test(t.url));
      if (page?.webSocketDebuggerUrl) return page;
    } catch {
      /* 窗口还没起来 */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`端口 ${PORT} 上没有 page target（窗口没起？端口没开？）`);
};

const connectTo = async (page) => {
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => {
    ws.onopen = res;
    ws.onerror = () => rej(new Error("WebSocket 连不上"));
  });
  let id = 0;
  const waiting = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && waiting.has(msg.id)) {
      const { resolve, reject } = waiting.get(msg.id);
      waiting.delete(msg.id);
      msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    }
  };
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const n = (id += 1);
      waiting.set(n, { resolve, reject });
      ws.send(JSON.stringify({ id: n, method, params }));
    });
  return { send, close: () => ws.close(), url: page.url };
};

const connect = async () => connectTo(await targets());

// 紧轮询发现 target：`targets()` 的 250ms 间隔是给"窗口已经开着"用的，
// 而 trace 要抢在启动那次 `settings_get` 回来之前读到行内值，慢一步就读到覆盖后的了。
const targetsFast = async (budgetMs) => {
  const t0 = Date.now();
  for (;;) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
      const page = list.find((t) => t.type === "page" && !/devtools/i.test(t.url));
      if (page?.webSocketDebuggerUrl) return { page, waitedMs: Date.now() - t0 };
    } catch {
      /* 端口还没起来 */
    }
    if (Date.now() - t0 > budgetMs) throw new Error(`${budgetMs}ms 内没有 page target`);
    await new Promise((r) => setTimeout(r, 5));
  }
};

const evaluate = async (send, expression) => {
  const r = await send("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (r.exceptionDetails) {
    throw new Error(`页面里抛错：${r.exceptionDetails.text} ${JSON.stringify(r.exceptionDetails.exception ?? "")}`);
  }
  return r.result.value;
};

// document-start 探针：记下 `documentElement` 上每一次属性/行内样式写入的时刻。
// 之所以用 addScriptToEvaluateOnNewDocument 而不是 eval —— eval 只能在 target 出现之后跑，
// 而那一时刻 main.tsx 的播种**已经发生完了**，测不到「首帧之前有没有值」。
// 观察的是 `document` 而非 `documentElement`：注入发生在文档刚建立时，此刻根元素可能还没被
// 解析出来，`observe(null)` 会当场抛错、探针整个不存在；`subtree + attributes` 一样抓得到
// 根元素上的写入。回调里现取 `documentElement`，不缓存安装时的引用。
const PROBE = `
(() => {
  const log = [];
  const t = () => Math.round(performance.now() * 1000) / 1000;
  const attr = (n) => { const el = document.documentElement; return el === null ? null : el.getAttribute(n); };
  const inline = (n) => { const el = document.documentElement; return el === null ? null : el.style.getPropertyValue(n); };
  const obs = new MutationObserver((records) => {
    for (const r of records) {
      log.push({ at: t(), attr: r.attributeName, theme: attr("data-theme"), mode: attr("data-mode"),
        fs: inline("--font-size"), accent: inline("--accent") });
    }
  });
  window.__probe = {
    installAt: t(),
    cacheAtInstall: (() => { try { return localStorage.getItem("qwen-do.theme"); } catch (e) { return "throw:" + e; } })(),
    themeAtInstall: attr("data-theme"),
    accentAtInstall: inline("--accent"),
    rootAtInstall: document.documentElement === null ? "null" : "present",
    log: [],
    stop() { obs.disconnect(); this.log = log; return { installAt: this.installAt, cacheAtInstall: this.cacheAtInstall, themeAtInstall: this.themeAtInstall, accentAtInstall: this.accentAtInstall, rootAtInstall: this.rootAtInstall, log, paint: performance.getEntriesByType("paint").map((p) => [p.name, Math.round(p.startTime)]) }; },
  };
  obs.observe(document, { subtree: true, attributes: true, attributeFilter: ["data-theme", "data-mode", "style"] });
})();`;

const main = async () => {
  const { send, close, url } = await connect();
  await send("Runtime.enable");
  await send("Page.enable");
  if (cmd === "eval") {
    console.log(JSON.stringify(await evaluate(send, process.argv[3]), null, 1));
// 重新加载会换掉 target（WebView2 在 Tauri 里 reload 后旧的 webSocketDebuggerUrl 不一定还在），
// 所以 arm 分两段：前一段只负责注册 + 触发重载并留下 `__marker`；后一段重新发现 target 再取数据。
// `__marker` 的存活与否区分两种失败：标记没了 = 确实换了文档（探针没注入）；
// 标记还在 = 压根没重载（那这份时间线是假的，必须报告为测不到）。
  } else if (cmd === "arm") {
    await evaluate(send, "(() => { window.__marker = 1; return 'marked'; })()");
    // 先试绕过 CSP：Tauri 产物在 index.html 里带 CSP，注入被静默丢弃的最省事解释就是它。
    const bypass = await send("Page.setBypassCSP", { enabled: true }).then(() => "ok").catch((e) => `ERR ${e.message}`);
    const injected = await send("Page.addScriptToEvaluateOnNewDocument", { source: PROBE }).then((r) => JSON.stringify(r)).catch((e) => `ERR ${e.message}`);
    console.log(`setBypassCSP=${bypass} addScript→${injected}`);
    await send("Page.reload", { ignoreCache: false });
    close();
    await new Promise((r) => setTimeout(r, 2500));
    const again = await connect();
    await again.send("Runtime.enable");
    const diag = await evaluate(again.send,
      'JSON.stringify({marker: typeof window.__marker, probe: typeof window.__probe, url: location.href, ready: document.readyState})');
    console.log(`重连后：${diag}`);
    if (/probe":"object/.test(diag)) {
      console.log(JSON.stringify(await evaluate(again.send, "JSON.stringify(window.__probe.stop())"), null, 1));
    }
    again.close();
  } else if (cmd === "trace") {
    // 三个时刻各读一次行内 `--accent`：抢在 IPC 之前读到假色，就证明写它的是缓存而不是 settings_get。
    const { page, waitedMs } = await targetsFast(15000);
    const c = await connectTo(page);
    await c.send("Runtime.enable");
    const SAMPLE = `JSON.stringify({perfNow: Math.round(performance.now()), ready: document.readyState,
      inline: document.documentElement.style.getPropertyValue("--accent"),
      hover: document.documentElement.style.getPropertyValue("--accent-hover"),
      theme: document.documentElement.getAttribute("data-theme"),
      paints: performance.getEntriesByType("paint").map((p) => [p.name, Math.round(p.startTime)]),
      dcl: Math.round(performance.getEntriesByType("navigation")[0].domContentLoadedEventStart)})`;
    console.log(JSON.stringify({ targetFoundAfterMs: waitedMs, first: await evaluate(c.send, SAMPLE) }));
    for (const wait of [600, 1500, 3000]) {
      await new Promise((r) => setTimeout(r, wait));
      console.log(JSON.stringify({ at: `+${wait}ms`, sample: await evaluate(c.send, SAMPLE) }));
    }
    c.close();
  } else if (cmd === "shot") {
    const r = await send("Page.captureScreenshot", { format: "png" });
    const fs = await import("node:fs");
    fs.writeFileSync(process.argv[3], Buffer.from(r.data, "base64"));
    console.log(`截图写入 ${process.argv[3]}（页面 ${url}）`);
  } else {
    throw new Error("用法：eval <js> | arm | shot <out.png>");
  }
  close();
};

await main();
