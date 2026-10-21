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
  reserve_output: 4096,
  budget_tokens: 32_000,
  window_messages: 40,
  theme: "noir",
  mode: "dark",
  font_size: 15,
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
  h.replaySession.mockReset().mockResolvedValue([]);
  h.sessions.mockReset().mockResolvedValue([row("B", 2)]);
  h.createSession.mockReset().mockResolvedValue("N");
  h.getSettings.mockReset().mockResolvedValue(view());
  // R131 的默认答案：pending 里没有这一笔。卡片状态只由事件驱动。
  h.respond.mockReset().mockResolvedValue(false);
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
    // 禁用必须由 `streaming` 这一个真相撑着：`interrupt()` 的 promise 已经完成，
    // 但这一轮还没结束（done 没来），按钮不许自己活过来。
    await waitFor(() => expect(h.interrupt).toHaveBeenCalled());
    expect(screen.getByRole("button", { name: /停止/ })).toBeDisabled();
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
});
