// G20 在 **App 接线** 那一层的判据。组件层（QuestionCard.test.tsx）只能证明
// 「给了回调就会带着对的选择调它」，而真正会出事的全在 App 这一侧：
// `question_respond` 返回 false 要不要收面板、true 之后本地记录走哪条通路、
// 命令失败时面板留不留（那一笔还挂着，用户可以重试）。
// 硬约束与 AppSettings.test.tsx 同一条：`App.test.tsx` 那些用例一条都不许改（R216②），
// 所以新的接线测试住在自己的文件里。
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import App from "./App";
import type { AgentEvent, SessionRow, SettingsView } from "./types";

const h = vi.hoisted(() => {
  const listeners: ((sessionId: string, e: AgentEvent) => void)[] = [];
  return {
    sendMessage: vi.fn(),
    respond: vi.fn(),
    // G20：本文件的主角。mock 工厂里缺这支的话，第一次应答就抛
    // 「No "questionRespond" export is defined on the mock」，把真正的失败淹掉。
    questionRespond: vi.fn(),
    interrupt: vi.fn(),
    sessions: vi.fn(),
    createSession: vi.fn(),
    deleteSession: vi.fn(),
    renameSession: vi.fn(),
    replaySession: vi.fn(),
    getSettings: vi.fn(),
    setSettings: vi.fn(),
    setApiKey: vi.fn(),
    normalize: vi.fn(),
    pickFolder: vi.fn(async () => null),
    sessionOutputView: vi.fn(async () => ({
      workspace: null,
      output_dir: "C:\\out\\session",
    })),
    setSessionWorkspace: vi.fn(),
    flushMemory: vi.fn(),
    listMemories: vi.fn(),
    deleteMemory: vi.fn(),
    clearMemories: vi.fn(),
    memoryProfile: vi.fn(),
    updateMemoryBody: vi.fn(),
    addMemory: vi.fn(),
    shellPresets: vi.fn(async () => []),
    shellProfileValidate: vi.fn(async () => {}),
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
  temperature: 0.7,
  compact_threshold: 0.85,
  keep_turns: 10,
  accent: "",
  roots: [],
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
  api_key_masked: null,
  corrupt_backup: null,
  ...p,
});

beforeEach(() => {
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    cb(0);
    return 1;
  });
  vi.stubGlobal("cancelAnimationFrame", () => {});
  document.documentElement.removeAttribute("data-theme");
  document.documentElement.removeAttribute("data-mode");
  document.documentElement.style.removeProperty("--font-size");
  for (const t of ["--accent", "--accent-hover", "--accent-text"]) {
    document.documentElement.style.removeProperty(t);
  }
  localStorage.clear();

  h.sendMessage.mockReset().mockResolvedValue(undefined);
  h.interrupt.mockReset().mockResolvedValue(undefined);
  h.deleteSession.mockReset().mockResolvedValue(undefined);
  h.renameSession.mockReset().mockResolvedValue(undefined);
  h.setSettings.mockReset().mockResolvedValue(undefined);
  h.setApiKey.mockReset().mockResolvedValue(undefined);
  h.replaySession.mockReset().mockResolvedValue([]);
  h.sessions.mockReset().mockResolvedValue([row("B", 2)]);
  h.createSession.mockReset().mockResolvedValue("N");
  h.getSettings.mockReset().mockResolvedValue(view());
  h.respond.mockReset().mockResolvedValue(false);
  // 默认「确实闭合了这道题」：false 是本文件要专门钉的分支，用例里覆写。
  h.questionRespond.mockReset().mockResolvedValue(true);
  h.normalize.mockReset().mockResolvedValue({ type: "image", data: "", mime: "png" });
  h.flushMemory.mockReset().mockResolvedValue(0);
  h.listMemories.mockReset().mockResolvedValue([]);
  h.deleteMemory.mockReset().mockResolvedValue(undefined);
  h.clearMemories.mockReset().mockResolvedValue(undefined);
  h.memoryProfile.mockReset().mockResolvedValue("");
  h.updateMemoryBody.mockReset().mockResolvedValue(undefined);
  h.addMemory.mockReset().mockResolvedValue(1);
  h.shellPresets.mockReset().mockResolvedValue([]);
  h.shellProfileValidate.mockReset().mockResolvedValue(undefined);
  h.listeners.length = 0;
});

const settled = async () => {
  await waitFor(() => expect(h.getSettings).toHaveBeenCalled());
  await waitFor(() => expect(h.replaySession).toHaveBeenCalled());
  await act(async () => {});
};

const ask = (): AgentEvent => ({
  t: "question",
  id: "q1",
  question: "选哪个方案？",
  options: ["方案一", "方案二"],
});

describe("App 提问接线（G20）", () => {
  it("renders the pending question as a card when the event arrives", async () => {
    render(<App />);
    await settled();
    act(() => h.emit("B", ask()));
    expect(screen.getByTestId("question-card")).toHaveTextContent("选哪个方案？");
    expect(screen.getByRole("button", { name: "方案一" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "方案二" })).toBeInTheDocument();
  });

  it("calls question_respond with the clicked option, clears the panel and records the answer locally", async () => {
    render(<App />);
    await settled();
    act(() => h.emit("B", ask()));
    fireEvent.click(screen.getByRole("button", { name: "方案二" }));
    await waitFor(() => expect(h.questionRespond).toHaveBeenCalledWith("q1", "方案二"));
    // 成功后：面板收起 + 消息流里一条用户回答的本地记录（走 appendUserTurn 的既有渲染）。
    await waitFor(() => expect(screen.queryByTestId("question-card")).not.toBeInTheDocument());
    expect(screen.getByTestId("bubble-user")).toHaveTextContent("方案二");
    // 旧通知行为不回归：通知条里仍有那条透传记录。
    expect(screen.getByTestId("notice-strip")).toHaveTextContent("模型提问");
  });

  it("submits a custom answer verbatim through the same command", async () => {
    render(<App />);
    await settled();
    act(() => h.emit("B", ask()));
    fireEvent.change(screen.getByLabelText("自定义回答"), {
      target: { value: "  都行，按你的来  " },
    });
    fireEvent.click(screen.getByRole("button", { name: "提交" }));
    await waitFor(() => expect(h.questionRespond).toHaveBeenCalledWith("q1", "都行，按你的来"));
    await waitFor(() => expect(screen.queryByTestId("question-card")).not.toBeInTheDocument());
    expect(screen.getByTestId("bubble-user")).toHaveTextContent("都行，按你的来");
  });

  it("notices and clears when question_respond returns false for a stale id", async () => {
    h.questionRespond.mockResolvedValue(false);
    render(<App />);
    await settled();
    act(() => h.emit("B", ask()));
    fireEvent.click(screen.getByRole("button", { name: "方案一" }));
    await waitFor(() => expect(h.questionRespond).toHaveBeenCalledWith("q1", "方案一"));
    // false = 无此 id（已应答、已取消或那一轮已收场）→ 提示并收起面板，
    // 而且**不**补本地记录 —— 那一声应答后端没有收下。提示走的是 App 侧的
    // `app-notices` 通道（它只属于这次会话，不是 reducer 的可重放事实）。
    await waitFor(() =>
      expect(screen.getByTestId("app-notices")).toHaveTextContent("已失效"),
    );
    expect(screen.queryByTestId("question-card")).not.toBeInTheDocument();
    expect(screen.queryByTestId("bubble-user")).not.toBeInTheDocument();
  });

  it("keeps the panel when the command itself fails, so the answer can be retried", async () => {
    h.questionRespond.mockRejectedValue("IPC 断了");
    render(<App />);
    await settled();
    act(() => h.emit("B", ask()));
    fireEvent.click(screen.getByRole("button", { name: "方案一" }));
    await waitFor(() =>
      expect(screen.getByTestId("app-notices")).toHaveTextContent("提交回答失败"),
    );
    // 命令失败与 false 不同：那一笔还挂着，面板留着让用户重试。
    expect(screen.getByTestId("question-card")).toBeInTheDocument();
  });
});
