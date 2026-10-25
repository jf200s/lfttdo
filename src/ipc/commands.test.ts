// 命令层的线格式断言。UI 测试证明不了 `respond` 的第二参到底叫 `requestId` 还是 `id`，
// 而那个键名写错时 Rust 侧压根收不到参数（Tauri 按名字取值），界面只会看见一次无声的失败。
// 所以这一层的每个 invoke 都把「命令名 + 参数对象」原样钉住。
import { describe, expect, it, vi, beforeEach } from "vitest";

const invoke = vi.fn();
const listen = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: (...a: unknown[]) => listen(...a) }));

const api = await import("./commands");
const AGENT_CHANNEL = (await import("../types")).AGENT_CHANNEL;

beforeEach(() => {
  invoke.mockReset().mockResolvedValue(undefined);
  listen.mockReset();
});

describe("ipc commands", () => {
  it("sends only the blocks the wire is allowed to carry", async () => {
    await api.sendMessage("s1", [{ type: "text", text: "hi" }]);
    expect(invoke).toHaveBeenCalledWith("send_message", {
      sessionId: "s1",
      blocks: [{ type: "text", text: "hi" }],
    });
  });

  // R131：第二参名必须是 `requestId`（`ipc.rs:423-428`）。写成 `id` 的话 Rust 收不到参数，
  // 而前端拿到的是一个 rejected promise，权限门那一笔永远 pending。
  it("names the permission decision by requestId, not id", async () => {
    invoke.mockResolvedValueOnce(true);
    const ok = await api.respond("p1", { allow: true, scope: "once" });
    expect(invoke).toHaveBeenCalledWith("respond", {
      requestId: "p1",
      decision: { allow: true, scope: "once" },
    });
    // 那个 bool 是要交回调用方的（false = 没有这一笔被闭合，卡片状态因此不许动）。
    expect(ok).toBe(true);
  });

  it("passes the snake_case id keys the store expects", async () => {
    await api.deleteSession("a");
    await api.renameSession("a", "t");
    await api.replaySession("a");
    expect(invoke.mock.calls.map((c) => c)).toEqual([
      ["session_delete", { id: "a" }],
      ["session_rename", { id: "a", title: "t" }],
      ["session_messages", { id: "a" }],
    ]);
  });

  it("asks for the flat settings view and writes the whole table back untouched", async () => {
    // 键数不是这条的判据（`SettingsPanel.test.tsx` 里那条才管「少带一键」），
    // 这里钉的是**信封形状**：`settings_set` 收 `{ settings: … }`，且 body 原样透传、
    // 不掺进 `api_key_masked` 那两只只读键。写满 14 键是因为 `Settings` 现在就是 14 键（新增 `system_prompt`）
    // （T26 加了 `roots`：Rust 侧 `Vec<PathBuf>` 的线上形状是字符串数组，由
    // `ipc.rs::tests::the_settings_view_carries_roots_at_the_top_level_as_strings` 钉着）。
    await api.getSettings();
    await api.setSettings({
      base_url: "u",
      model: "m",
      system_prompt: "",
      reserve_output: 1,
      budget_tokens: 2,
      window_messages: 3,
      theme: "noir",
      mode: "dark",
      font_size: 15,
      temperature: 0.7,
      compact_threshold: 0.85,
      keep_turns: 10,
      accent: "",
      roots: ["C:\\notes"],
    });
    expect(invoke).toHaveBeenNthCalledWith(1, "settings_get");
    expect(invoke).toHaveBeenNthCalledWith(2, "settings_set", {
      settings: {
        base_url: "u",
        model: "m",
        system_prompt: "",
        reserve_output: 1,
        budget_tokens: 2,
        window_messages: 3,
        theme: "noir",
        mode: "dark",
        font_size: 15,
        temperature: 0.7,
        compact_threshold: 0.85,
        keep_turns: 10,
        accent: "",
        roots: ["C:\\notes"],
      },
    });
  });

  it("hands the api key to the only command that may carry it", async () => {
    await api.setApiKey("sk-1");
    expect(invoke).toHaveBeenCalledWith("settings_set_api_key", { apiKey: "sk-1" });
  });

  it("interrupts without arguments", async () => {
    await api.interrupt();
    expect(invoke).toHaveBeenCalledWith("interrupt");
  });

  // R147：`normalize_image` **只吃 raw body**（`ipc.rs:564` 对 json body 直接 Err）。
  // 写成 `invoke("normalize_image", { bytes })` 会进 `InvokeBody::Json`，症状是「每次贴图都
  // 报错」而原因藏在 IPC 形状里，UI 测试根本看不出是形状错了 —— 所以这一条只能在命令层钉。
  it("hands the image bytes to normalize_image as a raw body, not a wrapped field", async () => {
    const bytes = new ArrayBuffer(8);
    await api.normalize(bytes);
    const [cmd, arg] = invoke.mock.calls[0];
    expect(cmd).toBe("normalize_image");
    // 同一个对象原样交出去：`{ bytes }` 包一层的话这两条都红（`toBe` 比身份、`instanceof`
    // 比形状），而 JS 侧写不出第三种「看起来对」的传法。
    expect(arg).toBe(bytes);
    expect(arg).toBeInstanceOf(ArrayBuffer);
  });

  // 第 7 条：通道名必须来自导入的常量。抄字面量 `"agent"` 的代价是将来改名时
  // TS 一侧一声不响，而 `types.ts` 那份镜像是唯一的名字来源。
  it("subscribes on the mirrored channel constant and forwards both halves", () => {
    // Tauri v2 的 `listen()` resolve 出来**就是**那个 unlisten 函数本身，不是包装对象。
    const unlisten = vi.fn();
    listen.mockResolvedValue(unlisten);
    const seen: [string, unknown][] = [];
    const off = api.subscribeAgent((sessionId, e) => seen.push([sessionId, e]));

    const [channel, handler] = listen.mock.calls[0] as unknown as [
      string,
      (p: { payload: { sessionId: string; event: unknown } }) => void,
    ];
    expect(channel).toBe(AGENT_CHANNEL);
    handler({ payload: { sessionId: "s9", event: { t: "done", stop_reason: "end_turn" } } });
    expect(seen).toEqual([["s9", { t: "done", stop_reason: "end_turn" }]]);

    off();
    return Promise.resolve().then(() => expect(unlisten).toHaveBeenCalled());
  });

  // 评审 #29（前端与配置）：清理函数原来只挂 `onfulfilled`，`listen()` 一旦 reject
  // （窗口已销毁是真实的一种）就留下一条没人处理的 promise。断言的不是「`off()` 不抛」——
  // 它从来不抛 —— 而是 Node 层面确实没有 `unhandledRejection` 冒出来，那才是这条修复的
  // 可观测后果。摘掉第二支参数，这条就红。
  it("swallows a rejected listen instead of leaking an unhandled rejection", async () => {
    const seen: unknown[] = [];
    const onRejection = (r: unknown) => seen.push(r);
    process.on("unhandledRejection", onRejection);
    try {
      listen.mockRejectedValue(new Error("窗口已销毁"));
      const off = api.subscribeAgent(() => {});
      off();
      // `unhandledRejection` 在微任务队列清空**之后**的宏任务里才发，所以这里必须真的
      // 让出一个 timer：只 `await Promise.resolve()` 会稳定地看不见它。
      await new Promise((r) => setTimeout(r, 0));
      expect(seen).toEqual([]);
    } finally {
      process.off("unhandledRejection", onRejection);
    }
  });
});
