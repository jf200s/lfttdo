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

  // 链接出口的命令名与参数键必须一起钉住：Rust 侧按 `url` 取值，写成别的键
  // 会变成一次无声的失败（协议白名单根本没机会跑）。
  it("opens external links through the allowlisted command", async () => {
    await api.openUrl("https://example.com/a?b=1");
    expect(invoke).toHaveBeenCalledWith("open_url", {
      url: "https://example.com/a?b=1",
    });
  });

  // R131：第二参名必须是 `requestId`（`ipc.rs:423-428`）。写成 `id` 的话 Rust 收不到参数，
  // 而前端拿到的是一个 rejected promise，权限门那一笔永远 pending。
  it("passes the session and path to reveal_path", async () => {
    await api.revealPath("s1", "C:\\notes\\周报.md");
    expect(invoke).toHaveBeenCalledWith("reveal_path", {
      sessionId: "s1",
      path: "C:\\notes\\周报.md",
    });
  });

  it("wires the workspace storage commands", async () => {
    await api.pickFolder();
    await api.sessionOutputView("s1");
    await api.setSessionWorkspace("s1", "C:\\work");
    expect(invoke.mock.calls).toEqual([
      ["pick_folder"],
      ["session_output_view", { id: "s1" }],
      ["session_workspace_set", { id: "s1", path: "C:\\work" }],
    ]);
  });

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
    // 不掺进 `api_key_masked` 那两只只读键。写满 18 键是因为 `Settings` 现在就是 18 键
    // （新增 `system_prompt`；T26 加了 `roots`：Rust 侧 `Vec<PathBuf>` 的线上形状是字符串数组，
    // 由 `ipc.rs::tests::the_settings_view_carries_roots_at_the_top_level_as_strings` 钉着；
    // 长期记忆切片再加 `memory_*` 四键）。
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
      storage_dir: "",
      memory_enabled: true,
      memory_recall_k: 5,
      memory_note_token_cap: 600,
      memory_profile_char_cap: 500,
      memory_decay_half_life_days: 60,
      memory_prospective_window_hours: 24,
      memory_embedding_source: "endpoint",
      memory_embedding_model: "text-embedding-v3",
      // Shell 两键（Shell 切片）：Settings 的必填键随类型镜像加进来，取生产默认值。
      shells: [],
      default_shell: "bash",
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
        storage_dir: "",
        memory_enabled: true,
        memory_recall_k: 5,
        memory_note_token_cap: 600,
        memory_profile_char_cap: 500,
        memory_decay_half_life_days: 60,
        memory_prospective_window_hours: 24,
        memory_embedding_source: "endpoint",
        memory_embedding_model: "text-embedding-v3",
        shells: [],
        default_shell: "bash",
      },
    });
  });

  it("hands the api key to the only command that may carry it", async () => {
    await api.setApiKey("sk-1");
    expect(invoke).toHaveBeenCalledWith("settings_set_api_key", { apiKey: "sk-1" });
  });

  // ==== 长期记忆的管理命令（设计文档 §7：记忆对用户必须可审计）====
  // 与 sessions/deleteSession 同一判据：命令名 + 参数对象原样钉住。命令名写错时 Rust 侧
  // 收不到（Tauri 按名字路由），界面只会看见一次无声的失败 —— UI 测试看不出，只有这层能钉。
  it("lists memories without arguments", async () => {
    invoke.mockResolvedValueOnce([]);
    const rows = await api.listMemories();
    expect(rows).toEqual([]);
    expect(invoke).toHaveBeenCalledWith("memories_list");
  });

  it("passes the memory id the delete command expects", async () => {
    await api.deleteMemory(7);
    expect(invoke).toHaveBeenCalledWith("memories_delete", { id: 7 });
  });

  it("clears every memory without arguments", async () => {
    await api.clearMemories();
    expect(invoke).toHaveBeenCalledWith("memories_clear");
  });

  it("asks for the resident profile as one string", async () => {
    invoke.mockResolvedValueOnce("");
    const body = await api.memoryProfile();
    expect(body).toBe("");
    expect(invoke).toHaveBeenCalledWith("memory_profile");
  });

  // ==== P1：会话切换的收尾抽取 + 用户编辑/新增 ====
  // 三个命令都是 `session_id` / `id` / `kind` 这类 Rust 形参。Tauri v2 按 camelCase 收
  // JS 侧的键（本仓 `send_message` 对 `session_id` 就是这么写的，见 commands.ts 顶部），
  // 所以这里必须抄同一条惯例：写成 `session_id` 反而收不到值。
  it("flushes the session being left, keyed the same way send_message keys session_id", async () => {
    invoke.mockResolvedValueOnce(2);
    const saved = await api.flushMemory("s1");
    // 返回的是「这次新记住了几条」：调用方用它决定要不要弹轻提示，所以必须原样交回。
    expect(saved).toBe(2);
    expect(invoke).toHaveBeenCalledWith("memory_flush_session", { sessionId: "s1" });
  });

  it("updates one memory's body through memory_update_body", async () => {
    await api.updateMemoryBody(7, "用户住在上海");
    expect(invoke).toHaveBeenCalledWith("memory_update_body", {
      id: 7,
      body: "用户住在上海",
    });
  });

  it("adds a memory with its kind and body and returns the new row id", async () => {
    invoke.mockResolvedValueOnce(11);
    const id = await api.addMemory("preference", "用户喜欢深色主题", null);
    expect(id).toBe(11);
    // 参数名是 `kind` / `body` / `dueAt`（ipc.rs 的 command 形参 `kind`/`body`/`due_at`，
    // 第三键按 Tauri v2 的 camelCase 映射），不是 `memoryKind` 之类的别名。
    // 非待办也必须显式带 `dueAt: null`：省略与 null 在线上虽然都解成 None，
    // 但命令层的形状只有一处可断言，这里写全才看得见「谁负责给这一格赋值」。
    expect(invoke).toHaveBeenCalledWith("memory_add", {
      kind: "preference",
      body: "用户喜欢深色主题",
      dueAt: null,
    });
  });

  it("forwards a prospective memory's due date as UTC milliseconds", async () => {
    // 值本身由面板按 UTC 算好（`Date.UTC(2026, 10, 1, 9, 0)`，与 Rust
    // `parse_iso8601_ms` 的 1_793_523_600_000 同口径）；命令层只负责原样递过去。
    invoke.mockResolvedValueOnce(12);
    await api.addMemory("prospective", "2026 年 11 月 1 日交周报", 1_793_523_600_000);
    expect(invoke).toHaveBeenCalledWith("memory_add", {
      kind: "prospective",
      body: "2026 年 11 月 1 日交周报",
      dueAt: 1_793_523_600_000,
    });
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
