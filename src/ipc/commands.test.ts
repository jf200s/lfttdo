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

  it("asks for the flat settings view and writes back only the eight keys", async () => {
    await api.getSettings();
    await api.setSettings({
      base_url: "u",
      model: "m",
      reserve_output: 1,
      budget_tokens: 2,
      window_messages: 3,
      theme: "noir",
      mode: "dark",
      font_size: 15,
    });
    expect(invoke).toHaveBeenNthCalledWith(1, "settings_get");
    expect(invoke).toHaveBeenNthCalledWith(2, "settings_set", {
      settings: {
        base_url: "u",
        model: "m",
        reserve_output: 1,
        budget_tokens: 2,
        window_messages: 3,
        theme: "noir",
        mode: "dark",
        font_size: 15,
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
});
