// T45 在 **App 接线** 那一层的判据。为什么组件层绿了还要单开一份：`SettingsPanel.test.tsx`
// 只能证明「面板给它的那四个回调会被照用」，而真正会出事的全在 App 这一侧 ——
// 打开时重读还是复用启动那份、失败要不要把 rejection 传回面板（同 `App.send` 的 `throw`）、
// key 走哪条命令、面板开起来会不会顺手把订阅和 `ui` 一起拆掉。
// 另有一条硬约束：`App.test.tsx` 那十三条一条都不许改（R216②），所以新接线只能住在这个文件里。
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import App from "./App";
import type { AgentEvent, SessionRow, SettingsView } from "./types";

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
    normalize: vi.fn(),
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

// 夹具写满 13 个键（`SettingsView` 是 flatten 后拍平一层的形状，11 个可写 + 2 只读）：
// App 若把 view 降级成只剩面板渲染的那几键再传进面板，`api_key_masked` / `corrupt_backup`
// 那两块界面会**静默消失**，少写一项 TS 当场报错 —— 这是这份夹具唯一有用的报错方式。
// T39 那三键（`temperature`/`compact_threshold`/`keep_turns`）面板不渲染但必须一路带到保存，
// 所以它们在这里也得不漏 —— 漏了就是 `SettingsPanel.test.tsx` 那条 carry-through 测试的现场。
const view = (p: Partial<SettingsView> = {}): SettingsView => ({
  base_url: "https://example.invalid/v1",
  model: "qwen-plus",
  reserve_output: 4096,
  budget_tokens: 32_000,
  window_messages: 40,
  theme: "noir",
  mode: "dark",
  font_size: 15,
  temperature: 0.7,
  compact_threshold: 0.85,
  keep_turns: 10,
  api_key_masked: null,
  corrupt_backup: null,
  ...p,
});

const FAKE = "sk-THIS-IS-NOT-A-REAL-KEY";

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
  h.normalize.mockReset().mockResolvedValue({ type: "image", data: "", mime: "png" });
  h.listeners.length = 0;
});

const settled = async () => {
  await waitFor(() => expect(h.getSettings).toHaveBeenCalled());
  await waitFor(() => expect(h.replaySession).toHaveBeenCalled());
};

/** 打开面板。`getByRole` 找不到就是「这个入口不存在」，那正是 T45 之前的状态。 */
const openPanel = async () => {
  fireEvent.click(screen.getByRole("button", { name: /设置/ }));
  return screen.findByRole("dialog");
};

describe("App 设置接线", () => {
  it("re-reads settings when the panel opens, and does not drop the subscription", async () => {
    render(<App />);
    await settled();
    // 启动那一次读之后，磁盘上的值变了（面板必须拿新的；复用旧的那份等于让用户对着
    // 一个已经过期的模型名编辑）。
    h.getSettings.mockResolvedValue(view({ model: "qwen-turbo" }));
    const dialog = await openPanel();
    expect(within(dialog).getByLabelText(/模型名/)).toHaveValue("qwen-turbo");
    expect(h.subCount()).toBe(1);
  });

  it("writes the whole table through settings_set and applies the theme once", async () => {
    render(<App />);
    await settled();
    expect(document.documentElement.getAttribute("data-theme")).toBe("noir");
    const dialog = await openPanel();
    fireEvent.change(within(dialog).getByLabelText(/模型名/), {
      target: { value: "qwen-turbo" },
    });
    fireEvent.change(within(dialog).getByLabelText(/主题/), {
      target: { value: "paper" },
    });
    fireEvent.change(within(dialog).getByLabelText(/字号/), {
      target: { value: "17" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: /保存/ }));
    // 载荷是**整表 11 键**：App 这一层不许重新拼一份「只发送编辑过的字段」，
    // 也不许在把 view 交给面板的路上把 `temperature`/`compact_threshold`/`keep_turns`
    // 这三只面板不渲染的键丢掉（丢了不报错，`serde(default)` 会替用户把它们抹回默认值）。
    await waitFor(() =>
      expect(h.setSettings).toHaveBeenCalledWith({
        base_url: "https://example.invalid/v1",
        model: "qwen-turbo",
        reserve_output: 4096,
        budget_tokens: 32_000,
        window_messages: 40,
        theme: "paper",
        mode: "dark",
        font_size: 17,
        temperature: 0.7,
        compact_threshold: 0.85,
        keep_turns: 10,
      }),
    );
    // 保存成功才改界面：这三项是 `applyTheme` 唯一的三个可见后果。
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).toBeNull(),
    );
    expect(document.documentElement.getAttribute("data-theme")).toBe("paper");
    expect(document.documentElement.getAttribute("data-mode")).toBe("dark");
    expect(document.documentElement.style.getPropertyValue("--font-size")).toBe(
      "17px",
    );
  });

  it("sends the plaintext key only through settings_set_api_key", async () => {
    render(<App />);
    await settled();
    const dialog = await openPanel();
    fireEvent.change(within(dialog).getByLabelText(/API key/i), {
      target: { value: FAKE },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(h.setApiKey).toHaveBeenCalledWith(FAKE));
    // 明文可以在面板里存在一秒，但不许进 `settings_set` 的 body（spec:473 / R83 前端一侧）。
    expect(JSON.stringify(h.setSettings.mock.calls)).not.toMatch(/NOT-A-REAL-KEY/);
    expect(JSON.stringify(h.getSettings.mock.calls)).not.toMatch(/NOT-A-REAL-KEY/);
  });

  it("clears the credential through the empty-string arm of the same command", async () => {
    h.getSettings.mockResolvedValue(view({ api_key_masked: "sk-a…9z24" }));
    render(<App />);
    await settled();
    const dialog = await openPanel();
    fireEvent.click(within(dialog).getByRole("button", { name: /清除/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: /确认清除/ }));
    // 空串是命令侧「清除」的唯一编码（settings.rs R88）：App 自己不许再造一条删除路径，
    // 也不许在这里顺手补一次 `setSettings`（key 根本不在那张表里）。
    await waitFor(() => expect(h.setApiKey).toHaveBeenCalledWith(""));
  });

  // 变异判据（交付判据 c）在 App 这一层的对应物：App 把 rejection 吞成 resolved ⇒ 这条红。
  // 吞掉的后果是面板当成保存成功直接关掉，用户重开看到的是旧值，且错误一个字都没露过。
  it("keeps the panel open with the raw failure text when settings_set rejects", async () => {
    h.setSettings.mockRejectedValue("写入设置失败：磁盘只读");
    render(<App />);
    await settled();
    const dialog = await openPanel();
    fireEvent.change(within(dialog).getByLabelText(/模型名/), {
      target: { value: "qwen-turbo" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: /保存/ }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "写入设置失败：磁盘只读",
    );
    expect(within(dialog).getByLabelText(/模型名/)).toHaveValue("qwen-turbo");
    expect(h.setApiKey).not.toHaveBeenCalled();
  });

  it("does not repaint the live theme when the write failed", async () => {
    // 「保存成功才 applyTheme 一次」这句的另一半：写盘失败时界面**不许**先换装。
    // 只有把主题也改了才能测这一条 —— 顺序写反（先 applyTheme 再 await setSettings）
    // 在只改模型名的场景里看不出差别。
    h.setSettings.mockRejectedValue("写入设置失败：磁盘只读");
    render(<App />);
    await settled();
    const dialog = await openPanel();
    fireEvent.change(within(dialog).getByLabelText(/主题/), {
      target: { value: "paper" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: /保存/ }));
    await within(dialog).findByRole("alert");
    expect(document.documentElement.getAttribute("data-theme")).toBe("noir");
  });

  it("keeps rendering the live turn while the panel is open", async () => {
    render(<App />);
    await settled();
    act(() => {
      h.emit("B", { t: "turn_start", turn: 1 });
      h.emit("B", { t: "text_delta", text: "前半句" });
    });
    expect(await screen.findByText("前半句")).toBeInTheDocument();
    const dialog = await openPanel();
    // 面板只是一个视图：它开起来不许动 `ui`（前半句还在），也不许让后面的文本进不来。
    expect(screen.getByText("前半句")).toBeInTheDocument();
    act(() => h.emit("B", { t: "text_delta", text: "后半句" }));
    expect(await screen.findByText("前半句后半句")).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBe(dialog);
  });

  it("shows the corrupted-settings backup path that settings_get reported", async () => {
    // `take_corrupt_backup`（R102）造出来之后一直没有前端消费者；消费者只能是面板，
    // 因为「回落后的默认值」这件事只有设置界面看得见、说得清。
    h.getSettings.mockResolvedValue(
      view({ corrupt_backup: "C:\\Users\\x\\settings.json.corrupt-1700000000000" }),
    );
    render(<App />);
    await settled();
    const dialog = await openPanel();
    expect(within(dialog).getByText(/设置文件曾损坏/)).toBeInTheDocument();
    expect(
      within(dialog).getByText(/settings\.json\.corrupt-1700000000000/),
    ).toBeInTheDocument();
  });
});
