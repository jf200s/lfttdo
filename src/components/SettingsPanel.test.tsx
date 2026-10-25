// T45 的组件层判据。为什么单独一个文件、以及为什么这份夹具**必须写满 13 个键**：
// `SettingsView` 是 `#[serde(flatten)]` 拍平一层的形状（types.ts:128-131），面板少读一个键
// 的话，写成 `as SettingsView` 的断言转换会照样绿 —— 这里用完整字面量，缺项由 TS 拦。
// `waitFor` 用 `@testing-library/react` 那一个，不用 `vi.waitFor`：只有它包在 `act` 里，
// 否则保存这类 await 之后的 setState 会留下一堆 act 警告，把真正的失败淹在 stderr 里。
// 不用 Vite 的 `?raw`（Vitest 默认不处理 CSS，`import x from "./styles.css?raw"` 在测试里拿到空串，
// 断言会假绿），也不把 `new URL(...)` 直接交给 fs：jsdom 环境里全局 `URL` 是 whatwg 实现，
// Node 的 fs 认不出它，会抛 `The URL must be of scheme file`。口径照抄 tokens.test.ts:22-23。
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsPanel } from "./SettingsPanel";
import type { SettingsView } from "../types";

const pathOf = (relative: string): string =>
  fileURLToPath(new URL(relative, import.meta.url).href);

/** 剥块注释后再做正则匹配：注释里的色值不参与渲染，拿它充当「这条声明存在」的证据就是假绿。 */
const stripComments = (css: string): string => css.replace(/\/\*[\s\S]*?\*\//g, "");

const view = (p: Partial<SettingsView> = {}): SettingsView => ({
  base_url: "https://example.invalid/v1",
  model: "qwen-plus",
  system_prompt: "",
  reserve_output: 4096,
  budget_tokens: 32_000,
  window_messages: 40,
  theme: "noir",
  mode: "dark",
  font_size: 15,
  // T39 那三键故意取**与 Rust 默认值不同**的数（默认 0.7 / 0.85 / 10）：
  // 面板不渲染它们，一旦 `settingsOf` 漏带，落盘的就是默认值，而默认值恰好是
  // 「看不出来」的那种错——用 0.3/0.5/3 才测得出来。
  temperature: 0.3,
  compact_threshold: 0.5,
  keep_turns: 3,
  // T43 的 `accent` 同一条理由（Rust 默认是 `""` = 跟随预设）：面板不渲染它，
  // 所以这里也得是一支**看得见差别**的颜色，而不是那个「漏带和带错长得一样」的空串。
  accent: "#aa00ff",
  // T26 的 `roots` 是同一条规矩的第五次应用，而且这里最要紧：Rust 默认是**空表**，
  // 空表 = `read_file` 一律拒绝，「漏带」与「带对」在断言里会长得一模一样。
  // 两条目录而不是一个字符串数组字面量：也顺手钉住「它是数组、不是被 stringify 过的整串」。
  roots: ["C:\\notes", "D:\\code"],
  api_key_masked: null,
  corrupt_backup: null,
  ...p,
});

type PanelProps = Parameters<typeof SettingsPanel>[0];

// 四个 mock 先命名、再各自展开进 JSX，而不是塞进一个字面量再 `{ ...props }`：
// 那样合并会让每个属性的推断类型变成「`Mock` 与那条签名的**联合**」，`.mock.calls`
// 当场不存在（tsc rc=2 报的就是这个），而联合是对象展开的必然结果，看不出任何异常。
const setup = (over: Partial<SettingsView> = {}, props: Partial<PanelProps> = {}) => {
  const onSave = vi.fn<PanelProps["onSave"]>(async () => {});
  const onSaveKey = vi.fn<PanelProps["onSaveKey"]>(async () => {});
  const onClearKey = vi.fn<PanelProps["onClearKey"]>(async () => {});
  const onClose = vi.fn<PanelProps["onClose"]>();
  render(
    <SettingsPanel
      initial={view(over)}
      onSave={onSave}
      onSaveKey={onSaveKey}
      onClearKey={onClearKey}
      onClose={onClose}
      {...props}
    />,
  );
  return { onSave, onSaveKey, onClearKey, onClose };
};

describe("SettingsPanel", () => {
  it("carries the custom system prompt into the save payload", async () => {
    const p = setup({});
    fireEvent.change(screen.getByLabelText(/系统提示词/), {
      target: { value: "你是小宋祺。" },
    });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].system_prompt).toBe("你是小宋祺。");
  });

  it("saves the whole settings table with only the edited field changed", async () => {
    const p = setup({ model: "qwen-plus", budget_tokens: 32_000, reserve_output: 4096 });
    fireEvent.change(screen.getByLabelText(/模型名/), { target: { value: "qwen-turbo" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0]).toEqual({
      base_url: "https://example.invalid/v1",
      model: "qwen-turbo",
      system_prompt: "",
      reserve_output: 4096,
      budget_tokens: 32_000,
      window_messages: 40,
      theme: "noir",
      mode: "dark",
      font_size: 15,
      temperature: 0.3,
      compact_threshold: 0.5,
      keep_turns: 3,
      accent: "#aa00ff",
      roots: ["C:\\notes", "D:\\code"],
    });
  });

  it("carries the fields it does not render through a save", async () => {
    // T39 给 Rust 的 `Settings` 加了 `temperature`/`compact_threshold`/`keep_turns`，
    // T43 又加了 `accent`，T26 加了 `roots`：面板**不渲染**这五键（渲染一个存不进去的输入框才是 bug，见下面那条），
    // 于是逐字段列名单的 `settingsOf` 会少带它们。少带不报错：`settings.rs:260` 的结构体级
    // `#[serde(default)]` 会把它们抹回 Rust 默认值 ⇒ **一次普通的点保存，就把用户手改进
    // settings.json 的温度、压缩阈值、K、强调色和目录表全数清零**。这条测试存在的理由就是这个静默路径。
    const p = setup();
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    const sent = p.onSave.mock.calls[0][0];
    expect(sent.temperature).toBe(0.3);
    expect(sent.compact_threshold).toBe(0.5);
    expect(sent.keep_turns).toBe(3);
    // T43：`accent` 是第四只「面板不渲染但必须带走」的键。夹具里它不等于 Rust 默认值 `""`，
    // 所以「漏带」和「带成默认」在这里是两种都看得出来的错。
    expect(sent.accent).toBe("#aa00ff");
    // T26：第五只。这里断的是**整串数组**而不是 `length`：`settingsOf` 若哪天把 roots
    // 拼成一个字符串（或者反过来把 `[]` 当成「没有」而删键），长度断言都还是绿的，
    // 而落盘的那份已经不是用户列出的那张表了。
    expect(sent.roots).toEqual(["C:\\notes", "D:\\code"]);
    // 同一次展开不许把 `SettingsView` 那两只只读键一起带进 body：
    // `settings_set` 收的是 `Settings`，把掩码当设置存盘是另一件事（上面那条 toEqual 也守着）。
    expect(sent).not.toHaveProperty("api_key_masked");
    expect(sent).not.toHaveProperty("corrupt_backup");
  });

  it("never puts the key into the settings payload", async () => {
    // 夹具里的 key 必须是明显假的（R164）。这条同时钉三件事：不进 `setSettings` 的 body、
    // 走专用命令 `settings_set_api_key`、提交后本地输入框清空。
    const FAKE = "sk-THIS-IS-NOT-A-REAL-KEY";
    const p = setup();
    fireEvent.change(screen.getByLabelText(/API key/i), { target: { value: FAKE } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSaveKey).toHaveBeenCalledWith(FAKE));
    expect(JSON.stringify(p.onSave.mock.calls)).not.toMatch(/NOT-A-REAL-KEY/);
    expect((screen.getByLabelText(/API key/i) as HTMLInputElement).value).toBe("");
  });

  it("shows the mask, and an untouched key field does not clear the credential", async () => {
    const p = setup({ api_key_masked: "sk-a…9z24" });
    expect(screen.getByText(/sk-a…9z24/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    // 输入框留空 = 不动凭据。把空串当「没改」转发给 `setApiKey` 会让一次普通保存
    // 悄悄删掉 key —— 命令侧的空串语义是清除（settings.rs R88）。
    expect(p.onSaveKey).not.toHaveBeenCalled();
    expect(p.onClearKey).not.toHaveBeenCalled();
  });

  it("clears the credential only through an explicit, confirmed action", async () => {
    const p = setup({ api_key_masked: "sk-a…9z24" });
    fireEvent.click(screen.getByRole("button", { name: /清除/ }));
    // 零弹窗红线：不许 `window.confirm`，第一次点只是把按钮本身变成确认态。
    expect(p.onClearKey).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: /确认清除/ }));
    await waitFor(() => expect(p.onClearKey).toHaveBeenCalledTimes(1));
  });

  it("keeps the panel open with the raw failure text when saving rejects", async () => {
    const p = setup(
      {},
      { onSave: vi.fn(async () => Promise.reject(new Error("写入设置失败：disk full"))) },
    );
    fireEvent.change(screen.getByLabelText(/模型名/), { target: { value: "qwen-turbo" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    // 前端不解释错误文案（R195 口径）：原文必须整条出现在界面上。
    expect(await screen.findByText(/写入设置失败：disk full/)).toBeInTheDocument();
    expect(p.onClose).not.toHaveBeenCalled();
    // 已输入的值不许丢 —— 关掉再开就回到旧值，等于让用户重打一遍。
    expect(screen.getByLabelText(/模型名/)).toHaveValue("qwen-turbo");
  });

  it("refuses to submit a field the store would reject and says which one", async () => {
    const p = setup();
    const budget = screen.getByLabelText(/上下文预算/) as HTMLInputElement;
    // 清空 number 输入 → `valueAsNumber` 是 NaN。NaN 进 `Settings` 会被 serde 拒收，
    // 症状是「点了保存什么也没发生」，所以这里必须在前头拦下并指出是哪一项。
    fireEvent.change(budget, { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    // 认到 `role=alert` 那一支而不是全文搜索：这条界面的每个标签都写在自己的 `<span>` 里，
    // 「哪里有 上下文预算 四个字」回答不了「用户看得见那条错误吗」。
    expect(await screen.findByRole("alert")).toHaveTextContent(/上下文预算/);
    expect(p.onSave).not.toHaveBeenCalled();
  });

  it("closes on Escape but not on a backdrop click", () => {
    // 两条一起写，因为它们是同一个取舍的两半：键盘用户要能一按就走（`aria-modal` 的承诺），
    // 而**点空白处不许**把没保存的草稿丢掉 —— 这里没有弹窗可以「点外面关掉」，
    // 误点代价是重打一遍模型名。肉眼检查（一次性预览页）确认过这两条的实际行为。
    const p = setup();
    const dialog = screen.getByRole("dialog");
    fireEvent.change(screen.getByLabelText(/模型名/), { target: { value: "qwen-turbo" } });
    fireEvent.click(dialog.parentElement as HTMLElement);
    expect(p.onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(p.onClose).toHaveBeenCalledTimes(1);
  });

  it("names the corrupted settings backup only when there is one", () => {
    setup({ corrupt_backup: "C:\\Users\\x\\settings.json.corrupt-1700000000000" });
    expect(screen.getByText(/设置文件曾损坏/)).toBeInTheDocument();
    expect(screen.getByText(/settings\.json\.corrupt-1700000000000/)).toBeInTheDocument();
  });

  // ==== 任务 #64：强调色那一行（计划 :942 的「`<input type="color">` + 跟随预设复选框」） ====
  // 四条用例的分工是**三态**，不重复：`null`（勾选跟随预设）/ `""`（取消勾选但没选过色）/
  // 合法 hex（真选过）。判据只有载荷那一条 —— 控件本身的形状随设计走，`accent` 的字符串形状
  // 由 Rust 认（`normalize_accent`），TS 侧不许再校验第二遍（R123：两处边界会漂移）。

  it("keeps following the preset when the picker was shown but never used", async () => {
    // 本片的 headline。`<input type="color">` 的 DOM 值**永远**是合法 6 位 hex：空白态在 DOM 里
    // 被规范化成黑（jsdom 与 Chromium 同一算法，下面第二条断言钉的就是这件事）。
    // 于是「打开面板 → 取消勾选 → 什么也没选 → 保存」这条最自然的探索路径，只要载荷取自 DOM
    // 就会存进一支用户从没选过的黑色，症状是界面突然变黑底，而用户完全看不出为什么。
    const p = setup({ accent: "" });
    fireEvent.click(screen.getByLabelText(/跟随预设/));
    const picker = screen.getByLabelText(/强调色/) as HTMLInputElement;
    expect(picker.value, "DOM 的空白态就是黑 —— 所以载荷绝不许读 input.value").toBe("#000000");
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].accent).toBe("");
  });

  it("submits the empty accent while following the preset, even over a loaded colour", async () => {
    // 复选框真的能**压掉**已有值：夹具里躺着一支 `#aa00ff`，勾选之后载荷必须是 `""`，
    // 否则「跟随预设」只是个装饰文字，回到预设档要点两次保存。
    const p = setup();
    const follow = screen.getByLabelText(/跟随预设/) as HTMLInputElement;
    expect(follow.checked, "载入自选色时复选框不该是勾上的").toBe(false);
    fireEvent.click(follow);
    expect(screen.queryByLabelText(/强调色/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].accent).toBe("");
  });

  it("loads a stored custom accent into an unchecked box and a picker showing that colour", () => {
    // 「面板每次打开都是全新挂载」（App.tsx 按 settingsView !== null 条件渲染），所以
    // defaultValue 只在挂载时读一次就够 —— 这条钉的就是那一次：已存的色必须看得见。
    setup();
    expect(screen.getByLabelText(/跟随预设/)).not.toBeChecked();
    const picker = screen.getByLabelText(/强调色/) as HTMLInputElement;
    expect(picker.type).toBe("color");
    expect(picker.value).toBe("#aa00ff");
    expect(screen.getByTestId("accent-swatch")).toBeInTheDocument();
  });

  it("restores the loaded colour when custom mode is turned back on", async () => {
    // 审查抓到的形状不一致（简报 §3 裁定 2 的第四条路）：`defaultValue` 取的是载入值 `form.accent`，
    // 而载荷判据在 `customHex === ""` 时发空串。于是「存着 `#aa00ff` → 勾选跟随预设 → 再取消勾选」
    // 这一条路上，取色器**显示** `#aa00ff` 而保存的是 `""` —— 与 headline 那条（显示黑、存进去黑）
    // 同一个病的反面：显示与载荷分叉。修法是让取消勾选把用户自己的存色恢复成待定值，
    // 种子来自设置本身而不是硬编码 hex，于是两条路都同源。
    const p = setup({ accent: "#aa00ff" });
    const follow = screen.getByLabelText(/跟随预设/) as HTMLInputElement;
    fireEvent.click(follow);
    expect(follow.checked, "第一次点击 = 跟随预设").toBe(true);
    fireEvent.click(follow);
    expect(follow.checked, "第二次点击 = 回到自选，取色器重新出现").toBe(false);
    expect(
      (screen.getByLabelText(/强调色/) as HTMLInputElement).value,
      "取色器重新出现时显示的是用户存着的那支色",
    ).toBe("#aa00ff");
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(
      p.onSave.mock.calls[0][0].accent,
      "显示 #aa00ff 却保存成空串：用户在取色器里看见的颜色与他存进去的颜色不是一份",
    ).toBe("#aa00ff");
  });

  it("saves the colour the user actually picked", async () => {
    const p = setup({ accent: "" });
    fireEvent.click(screen.getByLabelText(/跟随预设/));
    fireEvent.change(screen.getByLabelText(/强调色/), { target: { value: "#336699" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].accent).toBe("#336699");
  });

  it("paints the swatch chip from the accent token, not from a value in script", () => {
    // 裁定 2 的另一半 + R201 的「有名单没消费者」同族：chip 显示的必须是界面**真的**在用的
    // 那支色（预设档来自预设块、自选档来自 `applyTheme` 的 inline 三件套），所以色值只能写在
    // CSS 里取 `var(--accent)`，不许 TS 递一支进 `style`。jsdom 读不到层叠，判据就落在源码上
    // （R235 立的口径，与 tokens.test.ts 那几条 CSS 闸同一族）。
    const css = stripComments(readFileSync(pathOf("../styles.css"), "utf8"));
    const swatch = /\.panel-swatch\s*\{([^}]*)\}/.exec(css);
    expect(swatch, ".panel-swatch 没有声明 —— chip 是个死元素").not.toBeNull();
    // `?? ""` 不参与判定：上一条 expect 在 null 时当场抛，走不到这里。写它只为让 TS 认识
    // 到这条已经 narrowed（`not.toBeNull()` 不窄化类型，`swatch[1]` 报 TS18047）。
    expect(
      swatch?.[1] ?? "",
      "chip 不许自带色值：它必须显示界面真的在用的那支 accent",
    ).toMatch(/background(-color)?:\s*var\(--accent\)/);
  });

  it("lets the checkbox take the theme accent instead of the system one", () => {
    // 真窗口实测（R245）：勾上「跟随预设」时那个勾是 **Windows 蓝** —— 勾是操作系统画的，
    // CSS 的 `color`/`background` 都管不到它，只有 `accent-color` 是命令它的标准属性。
    // 一个跟主题无关的蓝色方块是这一屏唯一看得见的破绽，所以它进闸而不进「以后再说」。
    const css = stripComments(readFileSync(pathOf("../styles.css"), "utf8"));
    const box = /\.panel-row input\[type="checkbox"\]\s*\{([^}]*)\}/.exec(css);
    expect(box, ".panel-row input[type=\"checkbox\"] 没有声明了").not.toBeNull();
    expect(
      box?.[1] ?? "",
      "复选框没有 accent-color —— 勾会退回系统蓝，不跟主题",
    ).toMatch(/accent-color:\s*var\(--accent\)/);
  });

  it("sizes the colour picker as a well, not as a text field", () => {
    // 同一次实测：取色器渲染成 50×27 的胶囊，里面一条 26×14 的黑色矩形。那条小矩形不是原生
    // 控件的错，是我们自己给它 `padding: 6px 12px`（跟着文本框那条规则继承来的）把色块挤没了。
    // 不用 `::-webkit-color-swatch` 剥它：`selector-no-vendor-prefix` 会红，口径同 styles.css
    // 末尾「滚动条走标准属性」那条。判据落在源码上：这一条必须存在、且横向不许留内边距。
    const css = stripComments(readFileSync(pathOf("../styles.css"), "utf8"));
    const well = /input\[type="color"\]\s*\{([^}]*)\}/.exec(css);
    expect(well, "取色器没有自己的尺寸规则 —— 它会继续继承文本框的 6px 12px 内边距").not.toBeNull();
    const body = well?.[1] ?? "";
    // 单独把 padding 的值取出来判，而不是拿正则在整条规则体里搜：后者会把「padding 后面还有
    // 别的声明」误读成「padding 有第二个值」，判据就会随声明顺序漂。
    const padding = /padding:\s*([^;]+);/.exec(body);
    expect(padding, "取色器没有 padding —— 色块会被文本框那套内边距挤成一条").not.toBeNull();
    expect(
      padding?.[1].trim() ?? "",
      "取色器的 padding 带了第二个值（横向内边距）—— 色块左右会缩进去",
    ).not.toMatch(/\s/);
  });

  it("renders no control that this slice has no design for", async () => {
    // 禁名单的历史（一路变了三次，别把这条当成还在测「存不存得进去」）：
    // T39 之后 `temperature`/`compact_threshold`/`keep_turns` 是存得进去的（spec 5.3 的三个内部
    // 参数，默认值就够用）；T43 之后 `accent` 也存得进去、但**还没有控件**，于是当时给它加了
    // `/强调色/` `/accent/i` 两项预告；任务 #64（计划 :942）把控件补上了 ⇒ **这两项今天删掉**，
    // 面板从此渲染强调色那一行（上面那四条取色器用例就是它的判据）。
    // 剩下四项继续禁：它们的后端字段存在、面板**有意**不渲染，那是「有意的不渲染」不是遗漏。
    // `/目录|roots/i` 这一项从 T26 起也在名单里：brief 没给 roots 控件，任务裁定也明说
    // 「本任务不加 UI 控件」（R282）—— 目录表要能改，得先有一个「选目录 + 列表 + 删除」的
    // 形状，那是另一件事，不在这一片里半拉子地开一个输入框。
    // 这里钉的从来不是「存不存得进」，而是「不许半拉子地先加一个没有校验、没有说明的输入框」——
    // 上面那条 carry-through 测试保证「不渲染」不等于「不带上」：那五键照样得原样回到 `settings_set`。
    for (const label of [/温度/, /temperature/i, /压缩|阈值|keep_turns/i, /目录|roots/i, /MCP/i]) {
      expect(screen.queryByLabelText(label)).toBeNull();
    }
  });
});
