import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import App from "./App";
import type { AgentEvent, SessionRow, SettingsView } from "./types";

// 命令层整体换掉：这一份测试要钉的是 **App 的义务**（按 sessionId 过滤、respond 返回 false
// 时不许动卡片、设置驱动 applyTheme、切会话先 interrupt 再 replay），
// 线格式那一半归 `ipc/commands.test.ts`。
const h = vi.hoisted(() => {
  const listeners: ((sessionId: string, e: AgentEvent) => void)[] = [];
  return {
    sendMessage: vi.fn(),
    respond: vi.fn(),
    interrupt: vi.fn(),
    sessions: vi.fn(),
    createSession: vi.fn(),
    deleteSession: vi.fn(),
    renameSession: vi.fn(),
    replaySession: vi.fn(),
    getSettings: vi.fn(),
    setSettings: vi.fn(),
    setApiKey: vi.fn(),
    skillsList: vi.fn(),
    skillsOpenFolder: vi.fn(),
    skillsSetEnabled: vi.fn(),
    // P1：切会话时的收尾抽取 + 面板里的记忆写入口。App 的 switch 路径**会真的调**
    // `flushMemory`（fire-and-forget），mock 工厂里缺这一支的话，下面那两条切换用例
    // 会在 `openSession` 里抛 `flushMemory is not a function`，红的还是「不该动的测试」。
    flushMemory: vi.fn(),
    listMemories: vi.fn(),
    deleteMemory: vi.fn(),
    clearMemories: vi.fn(),
    memoryProfile: vi.fn(),
    updateMemoryBody: vi.fn(),
    addMemory: vi.fn(),
    subscribeAgent: (cb: (sessionId: string, e: AgentEvent) => void) => {
      listeners.push(cb);
      return () => {
        const i = listeners.indexOf(cb);
        if (i >= 0) listeners.splice(i, 1);
      };
    },
    listeners,
    emit: (sessionId: string, e: AgentEvent) => {
      for (const l of [...listeners]) l(sessionId, e);
    },
    subCount: () => listeners.length,
  };
});

vi.mock("./ipc/commands", () => h);

const row = (id: string, updated_at: number): SessionRow => ({
  id,
  title: `会话 ${id}`,
  updated_at,
});

const view = (over: Partial<SettingsView> = {}): SettingsView => ({
  base_url: "https://example.invalid/v1",
  model: "qwen-plus",
  system_prompt: "",
  reserve_output: 4096,
  budget_tokens: 32_000,
  window_messages: 40,
  theme: "noir",
  mode: "dark",
  font_size: 15,
  temperature: 0.7,
  compact_threshold: 0.85,
  keep_turns: 10,
  accent: "",
  // T26 的 `roots`：这两份夹具（`App.test.tsx` / `AppImages.test.tsx`）不断言保存载荷，
  // 所以这里给的是**生产默认值**那个空数组 —— 「面板/ App 会不会把它抹掉」由
  // `AppSettings.test.tsx` 与 `SettingsPanel.test.tsx` 里那两条用非空值断言的测试管，
  // 空数组在这儿只承担一件事：让这份镜像少一键就当场编译不过。
  roots: [],
  // 长期记忆四键（Rust 默认 true/5/600/500）：作用与上面的 `roots` 同一条 —— 镜像多一键、
  // 夹具少一键就当场编译不过。仅补键，本文件的用例一行未动（R216②）。
  memory_enabled: true,
  memory_recall_k: 5,
  memory_note_token_cap: 600,
  memory_profile_char_cap: 500,
  memory_decay_half_life_days: 60,
  memory_prospective_window_hours: 24,
  memory_embedding_source: "endpoint",
  memory_embedding_model: "text-embedding-v3",
  api_key_masked: "…abcd",
  corrupt_backup: null,
  ...over,
});

// 帧边界本身已由 `ipc/batching.test.ts` 用手搭的回调队列钉过；这里要钉的是「事件 → 界面」，
// 所以把 rAF 换成同步执行，免得每个断言都要等一次 jsdom 的帧时钟。
beforeEach(() => {
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    cb(0);
    return 1;
  });
  vi.stubGlobal("cancelAnimationFrame", () => {});
  document.documentElement.removeAttribute("data-theme");
  document.documentElement.removeAttribute("data-mode");
  document.documentElement.style.removeProperty("--font-size");

  h.sendMessage.mockReset().mockResolvedValue(undefined);
  // `interrupt()` 在合同里是 `Promise<void>`（commands.ts），App 那两支调用方一支 `await`、
  // 一支链 `.catch`。mock 没有 resolved value 时返回 `undefined`，`stop()` 会当场抛
  // `Cannot read properties of undefined (reading 'catch')` —— 实测确实抛过一次（vitest 把它
  // 记成 unhandled error，全绿也不许留着），所以这里必须给它一个真的 promise。
  h.interrupt.mockReset().mockResolvedValue(undefined);
  h.deleteSession.mockReset().mockResolvedValue(undefined);
  h.renameSession.mockReset().mockResolvedValue(undefined);
  h.setSettings.mockReset().mockResolvedValue(undefined);
  h.setApiKey.mockReset().mockResolvedValue(undefined);
  h.skillsList.mockReset().mockResolvedValue({ root: "", skills: [] });
  h.skillsOpenFolder.mockReset().mockResolvedValue(undefined);
  h.skillsSetEnabled.mockReset().mockResolvedValue(undefined);
  h.replaySession.mockReset().mockResolvedValue([]);
  h.sessions.mockReset().mockResolvedValue([row("B", 2)]);
  h.createSession.mockReset().mockResolvedValue("N");
  h.getSettings.mockReset().mockResolvedValue(view());
  // R131 的默认答案：pending 里没有这一笔。卡片状态只由事件驱动。
  h.respond.mockReset().mockResolvedValue(false);
  // P1：默认「这一轮没记住新东西」（后端零条时就是 Ok(0)），切换用例关心别的值再覆写。
  h.flushMemory.mockReset().mockResolvedValue(0);
  h.listMemories.mockReset().mockResolvedValue([]);
  h.deleteMemory.mockReset().mockResolvedValue(undefined);
  h.clearMemories.mockReset().mockResolvedValue(undefined);
  h.memoryProfile.mockReset().mockResolvedValue("");
  h.updateMemoryBody.mockReset().mockResolvedValue(undefined);
  h.addMemory.mockReset().mockResolvedValue(1);
  h.listeners.length = 0;
});

const settled = async () => {
  await waitFor(() => expect(h.getSettings).toHaveBeenCalled());
};

describe("App", () => {
  it("loads the session list, picks the newest and replays it", async () => {
    h.sessions.mockResolvedValue([row("old", 1), row("new", 9), row("mid", 5)]);
    h.replaySession.mockResolvedValue([
      { t: "user_message", blocks: [{ type: "text", text: "上次问的" }] },
      { t: "turn_start", turn: 1 },
      { t: "text_delta", text: "上次答的" },
    ] as AgentEvent[]);
    render(<App />);
    await settled();
    // 选中的必须是 updated_at 最大的那条（R128 的排序在这一层才有可见后果）。
    await waitFor(() =>
      expect(h.replaySession).toHaveBeenCalledWith("new"),
    );
    expect(await screen.findByText("上次问的")).toBeInTheDocument();
    expect(screen.getByText("上次答的")).toBeInTheDocument();
    expect(
      within(screen.getByTestId("bubble-assistant")).getByText("上次答的"),
    ).toBeInTheDocument();
  });

  it("opens the skills panel from the sidebar", async () => {
    h.skillsList.mockResolvedValue({
      root: "E:\\skills",
      skills: [
        {
          name: "翻译润色",
          description: "中英互译",
          folder: "translate",
          enabled: true,
        },
      ],
    });
    render(<App />);
    await settled();
    fireEvent.click(screen.getByRole("button", { name: "技能" }));
    const panel = await screen.findByRole("region", { name: "技能" });
    expect(within(panel).getByText("翻译润色")).toBeInTheDocument();
    expect(within(panel).getByText("中英互译")).toBeInTheDocument();
  });

  it("prefills the composer when choosing a skill from the panel", async () => {
    h.skillsList.mockResolvedValue({
      root: "E:\\skills",
      skills: [
        {
          name: "翻译润色",
          description: "中英互译",
          folder: "translate",
          enabled: true,
        },
      ],
    });
    render(<App />);
    await settled();
    fireEvent.click(screen.getByRole("button", { name: "技能" }));
    const panel = await screen.findByRole("region", { name: "技能" });
    fireEvent.click(within(panel).getByRole("button", { name: "用这个技能" }));
    expect(screen.getByRole("textbox", { name: "输入消息" })).toHaveValue(
      "使用「翻译润色」技能：",
    );
    expect(screen.queryByRole("region", { name: "技能" })).toBeNull();
  });

  it("creates a session when the store has none", async () => {
    // 两次 `sessions()` 必须有不同答案：第一次空（触发建会话），第二次带上刚建的那条
    // —— App 只有拿到非空列表才 `openSession`，否则这里永远是 `[]`、`replaySession` 永远不被调。
    h.sessions.mockResolvedValueOnce([]).mockResolvedValue([row("N", 1)]);
    h.createSession.mockResolvedValue("N");
    render(<App />);
    await waitFor(() => expect(h.createSession).toHaveBeenCalled());
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledWith("N"));
  });

  // ---- Step 0 第 12 条 / 硬性要求 10：主题接线 ----
  it("applies theme, mode and font size from settings_get", async () => {
    h.getSettings.mockResolvedValue(
      view({ theme: "paper", mode: "light", font_size: 17 }),
    );
    render(<App />);
    await waitFor(() =>
      expect(document.documentElement.getAttribute("data-theme")).toBe("paper"),
    );
    expect(document.documentElement.getAttribute("data-mode")).toBe("light");
    expect(
      document.documentElement.style.getPropertyValue("--font-size"),
    ).toBe("17px");
    // 合法名字不许喊回落 —— 否则 notice 变成常驻噪音，用户分不清哪次是真的。
    expect(screen.queryByText(/未知主题/)).toBeNull();
  });

  it("shouts when the stored theme name is unknown instead of silently falling back", async () => {
    h.getSettings.mockResolvedValue(view({ theme: "holographic-2099" }));
    render(<App />);
    // 回落本身要成立（画成 noir，不是白屏），
    await waitFor(() =>
      expect(document.documentElement.getAttribute("data-theme")).toBe("noir"),
    );
    // 但「设置没生效」必须让用户看见：静默回落等于界面在撒谎。
    expect(await screen.findByText(/未知主题/)).toBeInTheDocument();
  });

  // ---- 硬性要求 9：跨会话过滤 ----
  it("drops events that belong to another session", async () => {
    render(<App />);
    await settled();
    await screen.findByTestId("bubble-list");

    act(() => {
      h.emit("A", { t: "turn_start", turn: 1 });
      h.emit("A", { t: "text_delta", text: "别家的话" });
    });
    expect(screen.queryByText(/别家的话/)).toBeNull();
    expect(screen.getByTestId("bubble-list")).toBeEmptyDOMElement();

    act(() => {
      h.emit("B", { t: "turn_start", turn: 1 });
      h.emit("B", { t: "text_delta", text: "自家的话" });
    });
    expect(screen.getByText(/自家的话/)).toBeInTheDocument();
  });

  // ---- R131：respond 的 bool 不许被当成卡片状态 ----
  it("leaves the permission card untouched when respond answers false", async () => {
    render(<App />);
    await settled();
    const perm = {
      t: "permission_request",
      id: "p1",
      risk: "write",
      call: { id: "c", name: "write_file", risk: "write", input: {} },
    } as const;
    act(() => h.emit("B", perm));
    const card = await screen.findByTestId("perm-p1");
    expect(card).toHaveAttribute("data-state", "pending");

    fireEvent.click(within(card).getByRole("button", { name: /仅本次/ }));
    await waitFor(() => expect(h.respond).toHaveBeenCalledWith("p1", {
      allow: true,
      scope: "once",
    }));
    // false = pending 里压根没有这一笔。界面不许替用户发明第二份真相。
    expect(screen.getByTestId("perm-p1")).toHaveAttribute(
      "data-state",
      "pending",
    );

    // 连 true 也不算：卡片最终态只由 permission_resolved 驱动。
    h.respond.mockResolvedValue(true);
    fireEvent.click(
      within(screen.getByTestId("perm-p1")).getByRole("button", {
        name: /本会话内都允许/,
      }),
    );
    await waitFor(() => expect(h.respond).toHaveBeenCalledTimes(2));
    expect(screen.getByTestId("perm-p1")).toHaveAttribute(
      "data-state",
      "pending",
    );

    act(() =>
      h.emit("B", {
        t: "permission_resolved",
        id: "p1",
        decision: { allow: true, scope: "session" },
      }),
    );
    await waitFor(() =>
      expect(screen.getByTestId("perm-p1")).toHaveAttribute(
        "data-state",
        "allowed",
      ),
    );
  });

  it("sends the deny reason the user typed and keeps the card pending until the event lands", async () => {
    render(<App />);
    await settled();
    act(() =>
      h.emit("B", {
        t: "permission_request",
        id: "p2",
        risk: "exec",
        call: { id: "c", name: "run", risk: "exec", input: {} },
      }),
    );
    const card = await screen.findByTestId("perm-p2");
    fireEvent.click(within(card).getByRole("button", { name: /拒绝/ }));
    fireEvent.change(within(card).getByRole("textbox"), {
      target: { value: "太危险" },
    });
    fireEvent.click(within(card).getByRole("button", { name: /提交拒绝理由/ }));
    await waitFor(() =>
      expect(h.respond).toHaveBeenCalledWith("p2", {
        allow: false,
        reason: "太危险",
      }),
    );
    expect(screen.getByTestId("perm-p2")).toHaveAttribute(
      "data-state",
      "pending",
    );
  });

  // ---- 硬性要求 7：助手文本不许落进用户气泡 ----
  it("keeps the model reply out of the user bubble it just optimistically inserted", async () => {
    render(<App />);
    await settled();
    const box = await screen.findByRole("textbox", { name: /输入/ });
    fireEvent.change(box, { target: { value: "你好" } });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() =>
      expect(h.sendMessage).toHaveBeenCalledWith("B", [
        { type: "text", text: "你好" },
      ]),
    );
    const user = screen.getByTestId("bubble-user");
    expect(user).toHaveTextContent("你好");

    act(() => {
      h.emit("B", { t: "turn_start", turn: 1 });
      h.emit("B", { t: "text_delta", text: "模型的回答" });
    });
    expect(within(user).queryByText(/模型的回答/)).toBeNull();
    expect(screen.getByTestId("bubble-assistant")).toHaveTextContent(
      "模型的回答",
    );
  });

  // ---- 真窗口实测（R234）：自动改名的标题回不到侧栏 ----
  // 改名发生在 Rust 侧（`ipc.rs` 的 `auto_title`），而侧栏的 `rows` 只在
  // 新建/删除/重命名时重新 `sessions()`。症状：发完第一句话，那一行仍然写着
  // 「新对话」，直到用户再建一条会话才更新 —— 功能在库里成了、在界面上没成。
  it("re-reads the session list after a turn so the auto-renamed title shows up", async () => {
    render(<App />);
    await settled();
    const before = h.sessions.mock.calls.length;
    // 下一轮 `sessions()` 才给出改名后的标题，等价于 Rust 侧刚写完 `sessions.title`。
    h.sessions.mockResolvedValue([{ ...row("B", 2), title: "你好" }]);

    const box = await screen.findByRole("textbox", { name: /输入/ });
    fireEvent.change(box, { target: { value: "你好" } });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(h.sendMessage).toHaveBeenCalled());

    await waitFor(() =>
      expect(h.sessions.mock.calls.length).toBeGreaterThan(before),
    );
    expect(
      await screen.findByRole("button", { name: "你好" }),
    ).toBeInTheDocument();
  });

  // ---- 硬性要求 6：停止按钮不做乐观恢复 ----
  it("switches the composer to a self-disabling stop button until done arrives", async () => {
    render(<App />);
    await settled();
    const box = await screen.findByRole("textbox", { name: /输入/ });
    expect(box).not.toBeDisabled();

    act(() => h.emit("B", { t: "turn_start", turn: 1 }));
    const stop = await screen.findByRole("button", { name: /停止/ });
    expect(box).toBeDisabled();

    fireEvent.click(stop);
    expect(h.interrupt).toHaveBeenCalled();
    // spec:475 的后半句：点击之后除了自禁用，还要把话改成「停止中…」。中断是真异步的，
    // 按钮上这几个字是用户唯一看得见的「已经在路上了」；仍写着「停止」的症状是他以为
    // 没点着、再点一次，而第二次 `interrupt()` 什么也不会改变。
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "停止中…" })).toBeDisabled(),
    );
    // 禁用必须由 `streaming` 这一个真相撑着：`interrupt()` 的 promise 已经完成，
    // 但这一轮还没结束（done 没来），按钮不许自己活过来。
    expect(box).toBeDisabled();

    act(() => h.emit("B", { t: "done", stop_reason: "interrupted" }));
    const send = await screen.findByRole("button", { name: /发送/ });
    expect(send).toBeEnabled();
    expect(box).not.toBeDisabled();

    // 下一轮开始时「停止」按钮必须是**活的**：`stopRequested` 若只由 `streaming` 置假那一支
    // 清掉、而那一支没了，第二轮的停止按钮会一出现就是灰的（用户点不动、只能等 done）。
    act(() => h.emit("B", { t: "turn_start", turn: 2 }));
    const stop2 = await screen.findByRole("button", { name: /停止/ });
    expect(stop2).toBeEnabled();
  });

  // 帧合流与切会话的接缝（`ipc/batching.ts` 的时间线纪律在 App 这一侧的落点）：
  // 攒在缓冲里、还没到帧的半句，必须在重建界面**之前**被冲掉，否则它会在下一帧
  // 落进**新**会话的气泡里 —— 那是跨会话串字，比倒序更糟。
  it("drains the buffered chunk before rebuilding the view for another session", async () => {
    h.sessions.mockResolvedValue([row("A", 1), row("B", 2)]);
    // 这一次要的是**真的**帧时钟：把 rAF 换成手动队列，才能观测「冲刷」与「入帧」的先后。
    const frames: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      frames.push(cb);
      return frames.length;
    });
    vi.stubGlobal("cancelAnimationFrame", () => {});

    render(<App />);
    await settled();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledWith("B"));

    act(() => {
      h.emit("B", { t: "turn_start", turn: 1 });
      h.emit("B", { t: "text_delta", text: "上一句的尾巴" });
    });
    // 帧还没跑，文本应当还压在缓冲里（这条同时钉住「rAF 确实被用了」而不是同步冲刷）。
    expect(screen.queryByText(/上一句的尾巴/)).toBeNull();

    fireEvent.click(
      within(screen.getByTestId("session-row-A")).getByRole("button", {
        name: /会话 A/,
      }),
    );
    await waitFor(() => expect(h.interrupt).toHaveBeenCalled());
    // 把攒着的那一帧放出去：冲刷已经发生 ⇒ 这一帧是空转。
    act(() => {
      while (frames.length > 0) frames.shift()?.(0);
    });
    expect(screen.queryByText(/上一句的尾巴/)).toBeNull();
  });

  it("interrupts the running turn before replaying the session it switches to", async () => {
    h.sessions.mockResolvedValue([row("A", 9), row("B", 2)]);
    h.replaySession.mockImplementation(async (id: string) =>
      id === "A"
        ? ([{ t: "user_message", blocks: [{ type: "text", text: "A 的历史" }] }] as AgentEvent[])
        : [],
    );
    render(<App />);
    await settled();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledWith("A"));

    const other = screen.getByTestId("session-row-B");
    h.replaySession.mockResolvedValueOnce([
      { t: "user_message", blocks: [{ type: "text", text: "B 的历史" }] },
    ] as AgentEvent[]);
    fireEvent.click(within(other).getByRole("button", { name: /会话 B/ }));

    // 顺序是合同的一半：先打断正在跑的那一轮，再重建界面，否则旧会话的尾巴会画进新气泡。
    await waitFor(() => expect(h.interrupt).toHaveBeenCalled());
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledWith("B"));
    expect(await screen.findByText("B 的历史")).toBeInTheDocument();
    expect(screen.queryByText("A 的历史")).toBeNull();
  });

  // A→B→A 快切时，第一次 A 的重放会迟到。只比 sessionId 的实现会把它当成
  // 「当前会话的合法快照」，覆盖掉第二次 A 已经渲染好的内容。
  it("drops a stale replay for the same session after a round trip", async () => {
    h.sessions.mockResolvedValue([row("A", 1), row("B", 2)]);
    const pendingA: Array<(events: AgentEvent[]) => void> = [];
    h.replaySession.mockImplementation((id: string) => {
      if (id === "B") return Promise.resolve([]);
      return new Promise<AgentEvent[]>((resolve) => {
        pendingA.push(resolve);
      });
    });

    render(<App />);
    await settled();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledWith("B"));

    const clickA = () =>
      fireEvent.click(
        within(screen.getByTestId("session-row-A")).getByRole("button", {
          name: /会话 A/,
        }),
      );
    const clickB = () =>
      fireEvent.click(
        within(screen.getByTestId("session-row-B")).getByRole("button", {
          name: /会话 B/,
        }),
      );

    clickA();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledTimes(2));
    clickB();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledTimes(3));
    clickA();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledTimes(4));
    expect(pendingA).toHaveLength(2);

    // 后一次 A 先回来；先回来的那一份就是「迟到」的那一份。
    act(() =>
      pendingA[1]([
        { t: "user_message", blocks: [{ type: "text", text: "新的 A" }] },
      ]),
    );
    expect(await screen.findByText("新的 A")).toBeInTheDocument();
    act(() =>
      pendingA[0]([
        { t: "user_message", blocks: [{ type: "text", text: "过期的 A" }] },
      ]),
    );
    await waitFor(() => expect(screen.queryByText("过期的 A")).toBeNull());
    expect(screen.getByText("新的 A")).toBeInTheDocument();
  });

  it("shows the token usage the last usage event carried", async () => {
    render(<App />);
    await settled();
    act(() =>
      h.emit("B", { t: "usage", input_tokens: 120, output_tokens: 34 }),
    );
    // 钉整句而不是「两个数字都在」：只查 in/toMatch 的话输入/输出对调照样绿（实测过一次）。
    expect(await screen.findByTestId("usage")).toHaveTextContent(
      "本轮 token：输入 120 · 输出 34",
    );
  });

  it("unsubscribes on unmount so a hidden App cannot keep feeding state", async () => {
    const { unmount } = render(<App />);
    await settled();
    expect(h.subCount()).toBe(1);
    unmount();
    await waitFor(() => expect(h.subCount()).toBe(0));
  });

  // ==== P1：记忆写入的轻提示与切换前的收尾抽取 ====

  it("announces how many memories were saved, politely and without stealing focus", async () => {
    // `role="status"` + `aria-live="polite"` 是这条提示的无障碍合同：它不该抢焦点
    // （用户可能正在打字），只该排到当前朗读之后播一次。
    render(<App />);
    await settled();
    act(() => h.emit("B", { t: "memory_saved", count: 3 }));
    const status = screen.getByRole("status");
    expect(status).toHaveAttribute("aria-live", "polite");
    expect(await screen.findByText("已记住 3 条")).toBeInTheDocument();
    // 与 text_delta 同一条跨会话闸门：A 的事件不许在 B 的界面上冒出来。
    act(() => h.emit("A", { t: "memory_saved", count: 9 }));
    expect(screen.queryByText("已记住 9 条")).toBeNull();
  });

  it("keeps only the newest toast and gives it a fresh eight seconds", async () => {
    // 两条纪律在同一条用例里：连续事件只留最后一条（旧文案消失），
    // 以及计时是**重置**而不是共享 —— 否则第二条只剩第一条剩下的零头时间。
    render(<App />);
    await settled();
    vi.useFakeTimers();
    try {
      act(() => h.emit("B", { t: "memory_saved", count: 2 }));
      expect(screen.getByText("已记住 2 条")).toBeInTheDocument();
      act(() => h.emit("B", { t: "memory_saved", count: 5 }));
      expect(screen.queryByText("已记住 2 条")).toBeNull();
      expect(screen.getByText("已记住 5 条")).toBeInTheDocument();

      act(() => {
        vi.advanceTimersByTime(7_500);
      });
      expect(screen.getByText("已记住 5 条")).toBeInTheDocument();
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(screen.queryByText(/已记住/)).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("opens the settings panel at the memory group when the toast is clicked", async () => {
    // 轻提示的价值一半在「点得进去」：只弹一句「已记住 3 条」而没有任何入口，
    // 用户想知道记住了什么时还得自己想起来去侧栏点设置。
    const scroll = vi.fn();
    // jsdom 没有实现 scrollIntoView；面板里用的是可选调用，这里补上才能断言「真的滚了」。
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
      configurable: true,
      value: scroll,
    });
    try {
      render(<App />);
      await settled();
      act(() => h.emit("B", { t: "memory_saved", count: 3 }));
      fireEvent.click(await screen.findByRole("button", { name: /已记住 3 条/ }));
      const dialog = await screen.findByRole("dialog");
      expect(within(dialog).getByText("长期记忆")).toBeInTheDocument();
      // 滚动发生在面板挂载之后：App 只递「去记忆区」这个意图，DOM 位置的知识留在面板里。
      await waitFor(() => expect(scroll).toHaveBeenCalled());
    } finally {
      delete (HTMLElement.prototype as { scrollIntoView?: unknown }).scrollIntoView;
    }
  });

  it("flushes the session it is leaving before rebuilding the next one", async () => {
    h.sessions.mockResolvedValue([row("A", 9), row("B", 2)]);
    h.replaySession.mockImplementation(async (id: string) =>
      id === "A"
        ? ([{ t: "user_message", blocks: [{ type: "text", text: "A 的历史" }] }] as AgentEvent[])
        : ([{ t: "user_message", blocks: [{ type: "text", text: "B 的历史" }] }] as AgentEvent[]),
    );
    render(<App />);
    await settled();
    await screen.findByText("A 的历史");
    fireEvent.click(
      within(screen.getByTestId("session-row-B")).getByRole("button", {
        name: /会话 B/,
      }),
    );
    await waitFor(() => expect(h.flushMemory).toHaveBeenCalledWith("A"));
    expect(await screen.findByText("B 的历史")).toBeInTheDocument();
    // 「切走前」不是修辞：收尾抽取必须发生在重建新会话之前，否则游标已经错过了
    // 用户最后那次手势（而空闲 worker 要等到下一个 tick）。
    const replayB = h.replaySession.mock.calls.findIndex((c) => c[0] === "B");
    expect(
      h.flushMemory.mock.invocationCallOrder[0],
    ).toBeLessThan(h.replaySession.mock.invocationCallOrder[replayB]);
  });

  it("does not block or complain when the flush before a switch fails", async () => {
    // 切换会话是主路径。一次后台抽取失败不该让用户卡在原地，也不该给他一条
    // 他无法处理的错误 —— 后端本来就把它收成 Ok(0)，这里兜的是 IPC 层。
    h.sessions.mockResolvedValue([row("A", 9), row("B", 2)]);
    h.replaySession.mockImplementation(async (id: string) =>
      id === "A"
        ? ([{ t: "user_message", blocks: [{ type: "text", text: "A 的历史" }] }] as AgentEvent[])
        : ([{ t: "user_message", blocks: [{ type: "text", text: "B 的历史" }] }] as AgentEvent[]),
    );
    h.flushMemory.mockRejectedValue("抽取挂了：端点 500");
    render(<App />);
    await settled();
    await screen.findByText("A 的历史");
    fireEvent.click(
      within(screen.getByTestId("session-row-B")).getByRole("button", {
        name: /会话 B/,
      }),
    );
    await waitFor(() => expect(h.flushMemory).toHaveBeenCalledWith("A"));
    expect(await screen.findByText("B 的历史")).toBeInTheDocument();
    expect(screen.queryByText(/抽取挂了/)).toBeNull();
  });

  it("shows the same toast for what the pre-switch flush saved", async () => {
    // 收尾抽取的 `memory_saved` 事件永远到不了（那时已经在 B 的界面里），
    // 所以这条路径必须自己把返回值说出来 —— 复用同一个提示槽，未免两套文案。
    h.sessions.mockResolvedValue([row("A", 9), row("B", 2)]);
    h.flushMemory.mockResolvedValue(4);
    render(<App />);
    await settled();
    await waitFor(() => expect(h.replaySession).toHaveBeenCalledWith("A"));
    fireEvent.click(
      within(screen.getByTestId("session-row-B")).getByRole("button", {
        name: /会话 B/,
      }),
    );
    expect(await screen.findByText("已记住 4 条")).toBeInTheDocument();
  });
});
