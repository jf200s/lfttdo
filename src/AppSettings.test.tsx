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
import type { AgentEvent, MemoryView, SessionRow, SettingsView } from "./types";

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
    pickFolder: vi.fn(async () => null),
    sessionOutputView: vi.fn(async () => ({
      workspace: null,
      output_dir: "C:\\out\\session",
    })),
    setSessionWorkspace: vi.fn(),
    // 长期记忆子区的三条命令：面板一挂载就要拉列表，mock 里没有这支的话，
    // 每一条开面板的用例都会走进面板的 error 通道（No "listMemories" export is defined
    // on the mock），把真正的失败淹没 —— 在这里给空表，与面板默认夹具同一口径。
    listMemories: vi.fn(),
    deleteMemory: vi.fn(),
    clearMemories: vi.fn(),
    // T12：面板挂载时还要拉常驻画像。缺这一支的话每条开面板的用例都会走进
    // 面板的 error 通道（No "memoryProfile" export is defined on the mock），
    // 真正的失败会被那条噪声淹掉。
    memoryProfile: vi.fn(),
    // P1：面板里用户权威的两个写入口。App 在这一层只递命令（不持数据、不 catch），
    // 所以判据是「面板里点的那一下，最终打到了哪个命令、带的是哪个正文」。
    updateMemoryBody: vi.fn(),
    addMemory: vi.fn(),
    // Shell 子区（SettingsPanel 的生产默认回调直连这两个命令）：mock 工厂里缺这两支的话，
    // 面板一挂载就会在默认参数求值时抛「No "shellPresets" export is defined on the mock」，
    // 把真正的失败淹掉。预设给空表 —— 本文件的用例不关心 Shell 区。
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

// 夹具写满 19 个键（`SettingsView` 是 flatten 后拍平一层的形状，17 个可写 + 2 只读）：
// App 若把 view 降级成只剩面板渲染的那几键再传进面板，`api_key_masked` / `corrupt_backup`
// 那两块界面会**静默消失**，少写一项 TS 当场报错 —— 这是这份夹具唯一有用的报错方式。
// T39 那三键 + T43 的 `accent` + T26 的 `roots`（五只面板都不渲染）必须一路带到保存，
// 所以它们在这里也得不漏 —— 漏了就是 `SettingsPanel.test.tsx` 那条 carry-through 测试的现场。
// 长期记忆四键（memory_*）面板**渲染**，照旧不漏：载荷断言证明的是「App 原样转交整张表」。
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
  // T26：故意给**非默认**的两条目录（Rust 默认是空表）。空表在这里是看不出来的错值：
  // 面板把它抹回默认 = 抹成空 = 下面那条 `toHaveBeenCalledWith` 照样绿，而用户手改进
  // settings.json 的目录表已经没了，症状是「read_file 什么都读不到」。
  roots: ["C:\\notes", "D:\\code"],
  storage_dir: "",
  // 长期记忆四键给 Rust 默认值（true/5/600/500）：它们与 roots 不同，面板**渲染**它们，
  // 载荷断言只证明「App 没有重新拼表」，默认值在这个判据下够用。
  memory_enabled: true,
  memory_recall_k: 5,
  memory_note_token_cap: 600,
  memory_profile_char_cap: 500,
  memory_decay_half_life_days: 60,
  memory_prospective_window_hours: 24,
  memory_embedding_source: "endpoint",
  memory_embedding_model: "text-embedding-v3",
  // Shell 两键给**非默认**值（Rust 默认 shells: [] / default_shell: "bash"），与上面 roots
  // 的非默认取向同一条：下面 `settings_set` 那条 `toHaveBeenCalledWith` 载荷断言，要能
  // 区分「App 原样转交整张表」与「App 重拼了一份默认表」—— 默认值在这里是看不出来的错值。
  // label 留空顺带钉住「自定义 profile 的显示名可空」这条线上语义（types.ts / spec §5.2）。
  shells: [{ id: "mine", label: "", program: "/opt/mine", args: ["-c"] }],
  default_shell: "pwsh",
  api_key_masked: null,
  corrupt_backup: null,
  ...p,
});

const FAKE = "sk-THIS-IS-NOT-A-REAL-KEY";

/** 记忆列表一行。默认全空、全有效、非用户编辑：用例只覆写它关心的键。
 *  P1 的 `user_edited` 与 T12 那三个标价字段同一口径 —— 默认取「不显示任何标记」那一档。 */
const memory = (id: number, p: Partial<MemoryView> = {}): MemoryView => ({
  id,
  session_id: null,
  kind: "fact",
  subject: null,
  predicate: null,
  object: null,
  body: `记忆 ${id}`,
  confidence: 1,
  valid_from: 0,
  invalid_at: null,
  updated_at: 0,
  valid: true,
  reinforcements: 0,
  due_at: null,
  is_due: false,
  user_edited: false,
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
  // T44：`applyTheme` 现在会写 `<html>` 的 inline 三件套，不清会把上一用例的颜色串给下一条。
  for (const t of ["--accent", "--accent-hover", "--accent-text"]) {
    document.documentElement.style.removeProperty(t);
  }
  // App 成功应用后会写主题缓存（jsdom 的 localStorage 同文件内共享），每条用例从冷启动开始。
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
  h.normalize.mockReset().mockResolvedValue({ type: "image", data: "", mime: "png" });
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

  it("loads the resident memory profile through memory_profile when the panel opens", async () => {
    // T12：面板里的画像那一行靠 App 递进去的 `onLoadProfile` 取数。这条钉的是**接线**
    // 本身（面板组件那一侧只证明「给了回调就会调」，证不了 App 递的是不是
    // `commands.memoryProfile`）。空串的静默由 SettingsPanel.test.tsx 管，这里只钉有内容。
    h.memoryProfile.mockResolvedValue("用户是 Rust 开发者");
    render(<App />);
    await settled();
    const dialog = await openPanel();
    await waitFor(() =>
      expect(within(dialog).getByTestId("memory-profile")).toHaveTextContent(
        "用户是 Rust 开发者",
      ),
    );
    expect(h.memoryProfile).toHaveBeenCalledTimes(1);
  });

  // ==== P1：面板里的用户权威接线（行内编辑 / 手动新增）====
  // 组件层那几条只证明「给了回调就会调」；这里钉的是 App 递进去的确实是
  // `memory_update_body` / `memory_add`，且写入后列表真值来自重拉（不是前端 splice）。

  it("forwards an inline memory edit to memory_update_body and reloads the list", async () => {
    h.listMemories
      .mockResolvedValueOnce([memory(7, { body: "用户住在北京" })])
      .mockResolvedValueOnce([memory(7, { body: "用户住在上海", user_edited: true })]);
    render(<App />);
    await settled();
    const dialog = await openPanel();
    await within(dialog).findByTestId("memory-item");
    fireEvent.click(within(dialog).getByRole("button", { name: "编辑" }));
    fireEvent.change(within(dialog).getByLabelText(/编辑记忆/), {
      target: { value: "用户住在上海" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "保存修改" }));
    await waitFor(() =>
      expect(h.updateMemoryBody).toHaveBeenCalledWith(7, "用户住在上海"),
    );
    // 重拉之后带上「已编辑」标记 —— 这条标记不是前端自己造的，是后端视图给的。
    await waitFor(() =>
      expect(within(dialog).getByText(/用户住在上海/)).toBeInTheDocument(),
    );
    expect(within(dialog).getByText(/已编辑/)).toBeInTheDocument();
  });

  it("forwards a hand-written memory to memory_add with the chosen kind", async () => {
    h.listMemories
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([
        memory(9, {
          kind: "prospective",
          body: "下周三交季度报告",
          user_edited: true,
        }),
      ]);
    h.addMemory.mockResolvedValue(9);
    render(<App />);
    await settled();
    const dialog = await openPanel();
    await within(dialog).findByTestId("memory-empty");
    fireEvent.change(within(dialog).getByLabelText(/记忆类型/), {
      target: { value: "prospective" },
    });
    fireEvent.change(within(dialog).getByLabelText(/记忆正文/), {
      target: { value: "下周三交季度报告" },
    });
    // Shell 子区也有一个「添加」按钮：这里点的是记忆新增区那个，按容器收窄。
    fireEvent.click(
      within(within(dialog).getByTestId("memory-add")).getByRole("button", {
        name: "添加",
      }),
    );
    await waitFor(() =>
      expect(h.addMemory).toHaveBeenCalledWith(
        "prospective",
        "下周三交季度报告",
        // 这条用例没填「提醒时间」→ 待办但不提醒,第三参必须是 null(不是 0)。
        null,
      ),
    );
    await waitFor(() =>
      expect(within(dialog).getByText(/下周三交季度报告/)).toBeInTheDocument(),
    );
    expect(h.listMemories).toHaveBeenCalledTimes(2);
  });

  it("writes the whole table through settings_set and applies the theme once", async () => {    render(<App />);
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
    await waitFor(() =>
      expect(h.setSettings).toHaveBeenCalledWith({
        base_url: "https://example.invalid/v1",
        model: "qwen-turbo",
        system_prompt: "",
        reserve_output: 4096,
        budget_tokens: 32_000,
        window_messages: 40,
        theme: "paper",
        mode: "dark",
        font_size: 17,
        temperature: 0.7,
        compact_threshold: 0.85,
        keep_turns: 10,
        accent: "",
        roots: ["C:\\notes", "D:\\code"],
        storage_dir: "",
        memory_enabled: true,
        memory_recall_k: 5,
        memory_note_token_cap: 600,
        memory_profile_char_cap: 500,
        memory_decay_half_life_days: 60,
        memory_prospective_window_hours: 24,
        memory_embedding_source: "endpoint",
        memory_embedding_model: "text-embedding-v3",
        // Shell 两键原样转交：App 不重拼默认表（夹具给的是非默认值，重拼了这里当场红）。
        shells: [{ id: "mine", label: "", program: "/opt/mine", args: ["-c"] }],
        default_shell: "pwsh",
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

  // T44：accent 进 `applySettingsTheme`（必填第 5 参）+ 成功应用后写缓存，判据在 App 接线层。
  // 面板**没有**取色器控件（任务 #64 另片），所以走「load 带 accent → 原样 carry-through →
  // 保存成功」这条路 —— `accent` 正是面板不渲染、必须带过的键之一（上面整表断言的同一支）。
  it("paints the accent triplet inline and caches it after a successful save", async () => {
    h.getSettings.mockResolvedValue(view({ accent: "#aa00ff" }));
    render(<App />);
    await settled();
    const dialog = await openPanel();
    fireEvent.click(within(dialog).getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    // inline 三件套真的上了 `documentElement`（判据只能打在 `el.style`，jsdom 不合成层叠）。
    const style = document.documentElement.style;
    expect(style.getPropertyValue("--accent")).toBe("#aa00ff");
    expect(style.getPropertyValue("--accent-hover"), "hover 与静止同色 = R168 的病").not.toBe(
      "#aa00ff",
    );
    expect(style.getPropertyValue("--accent-text")).toBe("#ffffff");
    // 成功应用后才写缓存，且只写白名单四键：key/base_url/模型名绝不许出现在里面。
    const cached = localStorage.getItem("qwen-do.theme");
    expect(cached, "applyTheme 成功后必须 writeThemeCache（冷启动播种的唯一数据源）").not.toBeNull();
    expect(JSON.parse(cached as string)).toEqual({
      theme: "noir",
      mode: "dark",
      fontSize: 15,
      accent: "#aa00ff",
    });
    expect(cached).not.toMatch(/sk-|api_key|base_url|example\.invalid|qwen-plus/);
  });

  // 失败路径的对应物（「保存成功才应用」的另一半延伸到 accent + 缓存）：写盘失败时
  // 三件套不许上屏、缓存里也不许出现这次的颜色 —— 否则「没生效但看着生效了」换了个入口复发。
  it("does not paint the accent or touch the cache when the write failed", async () => {
    h.setSettings.mockRejectedValue("写入设置失败：磁盘只读");
    h.getSettings.mockResolvedValue(view({ accent: "#aa00ff" }));
    render(<App />);
    await settled();
    // 启动那次 applySettingsTheme 是成功的（settings_get 回来了），所以此刻 inline 有颜色、
    // 缓存有四键 —— 这正是要钉住的起点：之后写盘失败，界面与缓存都不许跟着假保存走。
    expect(document.documentElement.style.getPropertyValue("--accent")).toBe("#aa00ff");
    const dialog = await openPanel();
    fireEvent.change(within(dialog).getByLabelText(/字号/), { target: { value: "18" } });
    fireEvent.click(within(dialog).getByRole("button", { name: /保存/ }));
    await within(dialog).findByRole("alert");
    expect(
      document.documentElement.style.getPropertyValue("--font-size"),
      "写盘失败不许把 18px 假应用上去",
    ).toBe("15px");
    expect(
      JSON.parse(localStorage.getItem("qwen-do.theme") as string).fontSize,
      "缓存也不许被失败的保存改写",
    ).toBe(15);
  });
});
