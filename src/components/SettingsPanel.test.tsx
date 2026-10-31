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
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SettingsPanel } from "./SettingsPanel";
import type { MemoryView, SettingsView } from "../types";

const downloadPersonaText = vi.hoisted(() => vi.fn());
vi.mock("../persona", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../persona")>();
  return { ...actual, downloadPersonaText };
});

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
  storage_dir: "",
  // 长期记忆四键取 **Rust 默认值**（true/5/600/500）：它们与 accent/roots 不同，面板**渲染**它们，
  // 「漏带」会在渲染层先露馅（复选框与数值框读到 undefined），所以默认值在这里够用；
  // 「带成别的值」由下面那条 carry-through 用非默认覆盖值另钉。
  memory_enabled: true,
  memory_recall_k: 5,
  memory_note_token_cap: 600,
  memory_profile_char_cap: 500,
  // T12：两个节奏旋钮同样取 Rust 默认值（60 天 / 24 小时）。面板**渲染**它们，
  // 所以「漏带」会在渲染层先露馅（输入框读到 undefined），默认值在这里够用。
  memory_decay_half_life_days: 60,
  memory_prospective_window_hours: 24,
  memory_embedding_source: "endpoint",
  memory_embedding_model: "text-embedding-v3",
  api_key_masked: null,
  corrupt_backup: null,
  ...p,
});

/** 记忆列表的一行。默认全空、全有效：测试只覆写它关心的那几键（同 `view()` 的用法）。
 *  T12 给视图加了三个展示字段，默认值取「不显示任何标价」的那一档（0 / null / false）——
 *  于是每条不关心它们的用例都不必写，而关心它们的用例覆写成看得见差别的值。 */
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
  // P1：默认「模型抽出来的」，所以行上不会多出「· 已编辑」。与 reinforcements 同一口径：
  // 默认值取「不显示任何标价」的那一档，关心它的用例再覆写成 true。
  user_edited: false,
  ...p,
});

type PanelProps = Parameters<typeof SettingsPanel>[0];

// 四个 mock 先命名、再各自展开进 JSX，而不是塞进一个字面量再 `{ ...props }`：
// 那样合并会让每个属性的推断类型变成「`Mock` 与那条签名的**联合**」，`.mock.calls`
// 当场不存在（tsc rc=2 报的就是这个），而联合是对象展开的必然结果，看不出任何异常。
const setup = async (over: Partial<SettingsView> = {}, props: Partial<PanelProps> = {}) => {
  const onSave = vi.fn<PanelProps["onSave"]>(async () => {});
  const onSaveKey = vi.fn<PanelProps["onSaveKey"]>(async () => {});
  const onClearKey = vi.fn<PanelProps["onClearKey"]>(async () => {});
  const onClose = vi.fn<PanelProps["onClose"]>();
  // 记忆子区的三个回调**必须**给默认实现：面板一挂载就要拉列表，默认给空表，
  // 既有用例才不必每条都关心记忆区 —— 想测记忆的用例再用 `props` 覆盖。
  const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => []);
  const onDeleteMemory = vi.fn<PanelProps["onDeleteMemory"]>(async () => {});
  const onClearMemories = vi.fn<PanelProps["onClearMemories"]>(async () => {});
  // T12：画像与列表同一个挂载期，默认给空串（= 后端从未写过画像 ⇒ 不渲染那一行）。
  // 必须给默认实现：面板一挂载就拉画像，缺这支的话每条用例都会走进 error 通道。
  const onLoadProfile = vi.fn<PanelProps["onLoadProfile"]>(async () => "");
  // P1：用户权威的两个写入口（行内编辑、手动新增）。与上面同一条口径 —— 不 catch、
  // 失败进面板的 error 通道；默认成功，只有关心失败路径的用例才覆写。
  const onUpdateMemory = vi.fn<PanelProps["onUpdateMemory"]>(async () => {});
  const onAddMemory = vi.fn<PanelProps["onAddMemory"]>(async () => 1);
  const onPickFolder = vi.fn<PanelProps["onPickFolder"]>(async () => null);
  render(
    <SettingsPanel
      initial={view(over)}
      onSave={onSave}
      onSaveKey={onSaveKey}
      onClearKey={onClearKey}
      onClose={onClose}
      onLoadMemories={onLoadMemories}
      onDeleteMemory={onDeleteMemory}
      onClearMemories={onClearMemories}
      onLoadProfile={onLoadProfile}
      onUpdateMemory={onUpdateMemory}
      onAddMemory={onAddMemory}
      onPickFolder={onPickFolder}
      {...props}
    />,
  );
  // 面板一挂载就要拉记忆列表，而那次 setState 落在 promise 的 then 里 —— 微任务只在
  // JS 栈清空后才 drain，若不在这里（act 内）清掉，setup 同步返回后它会在**测试体之外**
  // 落地，每条不 await 的用例都收到一条 act 警告，把 stderr 淹掉（文件头那条 waitFor
  // 纪律的同一逻辑：await 之后的 setState 必须有 act 兜着）。mock 是立即 resolve 的，
  // 一个空转的 act 就够把队列清干净。
  await act(async () => {});
  return {
    onSave,
    onSaveKey,
    onClearKey,
    onClose,
    onLoadMemories,
    onDeleteMemory,
    onClearMemories,
    onLoadProfile,
    onUpdateMemory,
    onAddMemory,
    onPickFolder,
  };
};

describe("SettingsPanel", () => {
  it("renders the persona textarea and saves the edited prompt", async () => {
    const p = await setup({ system_prompt: "旧的人设" });
    const box = screen.getByLabelText(/人设（系统提示词）/) as HTMLTextAreaElement;
    expect(box.value).toBe("旧的人设");
    fireEvent.change(box, { target: { value: "新的人设" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].system_prompt).toBe("新的人设");
  });

  it("imports a persona file with BOM and CRLF normalized", async () => {
    const p = await setup({ system_prompt: "旧的人设" });
    const file = new File(["\uFEFF第一行\r\n第二行\r第三行"], "人设.txt", {
      type: "text/plain",
    });
    fireEvent.change(screen.getByLabelText("导入人设文件"), {
      target: { files: [file] },
    });
    const box = screen.getByLabelText(/人设（系统提示词）/) as HTMLTextAreaElement;
    await waitFor(() => expect(box.value).toBe("第一行\n第二行\n第三行"));
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].system_prompt).toBe("第一行\n第二行\n第三行");
  });

  it("rejects a persona file over 64,000 characters without replacing the draft", async () => {
    await setup({ system_prompt: "保留我" });
    const file = new File(["x".repeat(64_001)], "big.txt", { type: "text/plain" });
    fireEvent.change(screen.getByLabelText("导入人设文件"), {
      target: { files: [file] },
    });
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent(/64,000/),
    );
    const box = screen.getByLabelText(/人设（系统提示词）/) as HTMLTextAreaElement;
    expect(box.value).toBe("保留我");
  });

  it("disables export until the persona has content, then exports the draft", async () => {
    await setup({ system_prompt: "" });
    const exportButton = screen.getByRole("button", { name: "导出 .txt" });
    expect(exportButton).toBeDisabled();
    fireEvent.change(screen.getByLabelText(/人设（系统提示词）/), {
      target: { value: "新的人设" },
    });
    expect(exportButton).toBeEnabled();
    fireEvent.click(exportButton);
    expect(downloadPersonaText).toHaveBeenCalledWith("新的人设");
  });

  it("clears the persona back to the built-in prompt", async () => {
    const p = await setup({ system_prompt: "旧的人设" });
    fireEvent.click(screen.getByRole("button", { name: "恢复内置" }));
    const box = screen.getByLabelText(/人设（系统提示词）/) as HTMLTextAreaElement;
    expect(box.value).toBe("");
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].system_prompt).toBe("");
  });

  it("saves the whole settings table with only the edited field changed", async () => {
    const p = await setup({ model: "qwen-plus", budget_tokens: 32_000, reserve_output: 4096 });
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
      storage_dir: "",
      memory_enabled: true,
      memory_recall_k: 5,
      memory_note_token_cap: 600,
      memory_profile_char_cap: 500,
      memory_decay_half_life_days: 60,
      memory_prospective_window_hours: 24,
      memory_embedding_source: "endpoint",
      memory_embedding_model: "text-embedding-v3",
    });
  });

  it("renders the configured roots as one path per line", async () => {
    await setup();
    const box = screen.getByLabelText(/可访问目录/) as HTMLTextAreaElement;
    expect(box.tagName).toBe("TEXTAREA");
    expect(box.value).toBe("C:\\notes\nD:\\code");
  });

  it("saves the roots draft with blank lines and surrounding spaces removed", async () => {
    const p = await setup({ roots: [] });
    fireEvent.change(screen.getByLabelText(/可访问目录/), {
      target: { value: "  C:\\notes  \n\nD:\\code\n" },
    });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].roots).toEqual(["C:\\notes", "D:\\code"]);
  });

  it("fills the default storage location from the folder picker", async () => {
    const p = await setup();
    p.onPickFolder.mockResolvedValue("D:\\LFTTDO-output");
    fireEvent.click(screen.getByRole("button", { name: "选择文件夹" }));
    await waitFor(() =>
      expect(
        (screen.getByLabelText(/默认储存位置/) as HTMLInputElement).value,
      ).toBe("D:\\LFTTDO-output"),
    );
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].storage_dir).toBe("D:\\LFTTDO-output");
  });

  it("sends an empty roots list when the box is cleared", async () => {
    const p = await setup();
    fireEvent.change(screen.getByLabelText(/可访问目录/), {
      target: { value: "\n  \n" },
    });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].roots).toEqual([]);
  });

  it("carries the fields it does not render through a save", async () => {
    // T39 给 Rust 的 `Settings` 加了 `temperature`/`compact_threshold`/`keep_turns`，
    // 于是逐字段列名单的 `settingsOf` 会少带它们。少带不报错：`settings.rs:260` 的结构体级
    // `#[serde(default)]` 会把它们抹回 Rust 默认值 ⇒ **一次普通的点保存，就把用户手改进
    const p = await setup();
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    const sent = p.onSave.mock.calls[0][0];
    expect(sent.temperature).toBe(0.3);
    expect(sent.compact_threshold).toBe(0.5);
    expect(sent.keep_turns).toBe(3);
    // T43：`accent` 是第四只「面板不渲染但必须带走」的键。夹具里它不等于 Rust 默认值 `""`，
    // 所以「漏带」和「带成默认」在这里是两种都看得出来的错。
    expect(sent.accent).toBe("#aa00ff");
    // 而落盘的那份已经不是用户列出的那张表了。
    expect(sent.roots).toEqual(["C:\\notes", "D:\\code"]);
    // 同一次展开不许把 `SettingsView` 那两只只读键一起带进 body：
    // `settings_set` 收的是 `Settings`，把掩码当设置存盘是另一件事（上面那条 toEqual 也守着）。
    expect(sent).not.toHaveProperty("api_key_masked");
    expect(sent).not.toHaveProperty("corrupt_backup");
  });

  // ==== 长期记忆切片（设计文档 §7）：复选框 + 三个数值框 + 可审计的记忆管理子区 ====
  // 四键与 accent/roots 的差别：面板**渲染**它们，所以 carry-through 钉的是「用户没碰的键
  // 按载入值原样回去」，覆盖成非默认值才测得出来（T39 那条注释的同一逻辑）。

  it("carries the six memory fields through a save as-is", async () => {
    const p = await setup({
      memory_enabled: false,
      memory_recall_k: 7,
      memory_note_token_cap: 800,
      memory_profile_char_cap: 900,
      // T12 两项取**非默认值**：60/24 恰好是 Rust 默认值，用它测「带成默认」看不出来。
      memory_decay_half_life_days: 7,
      memory_prospective_window_hours: 72,
      memory_embedding_source: "endpoint",
      memory_embedding_model: "text-embedding-v3",
    });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    const sent = p.onSave.mock.calls[0][0];
    expect(sent.memory_enabled).toBe(false);
    expect(sent.memory_recall_k).toBe(7);
    expect(sent.memory_note_token_cap).toBe(800);
    expect(sent.memory_profile_char_cap).toBe(900);
    expect(sent.memory_decay_half_life_days).toBe(7);
    expect(sent.memory_prospective_window_hours).toBe(72);
  });

  it("edits the two pacing knobs through their labelled numeric fields", async () => {
    // 两个新键不只是「能带过」，用户得真能改。标签就是设置页口径；输入后载荷必须变，
    // 否则它们只是两行装饰文字（这是 T39 那三个键至今没有控件的对照面）。
    const p = await setup();
    fireEvent.change(screen.getByLabelText(/记忆衰减半衰期/), {
      target: { value: "30" },
    });
    fireEvent.change(screen.getByLabelText(/待办提醒窗口/), {
      target: { value: "12" },
    });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    const sent = p.onSave.mock.calls[0][0];
    expect(sent.memory_decay_half_life_days).toBe(30);
    expect(sent.memory_prospective_window_hours).toBe(12);
  });

  // ---- T12：记忆列表每行的强度 / 待办 / 「已到期」标价 ----

  it("shows the reinforcement count only when the fact was confirmed again", async () => {
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, { body: "用户住在北京", reinforcements: 3 }),
      memory(2, { body: "用户喜欢咖啡" }),
    ]);
    await setup({}, { onLoadMemories });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(2));
    // 有确认次数的行显示「确认 N 次」，0 次的那行什么都不显示 —— 从未确认过不带标价。
    expect(screen.getByText(/确认 3 次/)).toBeInTheDocument();
    expect(screen.queryByText(/确认 0 次/)).toBeNull();
    expect(screen.queryAllByText(/确认 \d+ 次/)).toHaveLength(1);
  });

  it("shows a prospective memory's due date as a local YYYY-MM-DD string", async () => {
    // 固定时间戳用**本地**构造（`new Date(2026, 10, 1)` = 本地 2026-11-01），
    // 面板也用手工拼的本地日期 —— 两端同一口径，于是这条断言在哪个时区跑都成立。
    // 这正是不用 `toLocaleDateString` 的理由：它会随 ICU/语言环境漂移。
    const localNov1 = new Date(2026, 10, 1).getTime();
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, {
        kind: "prospective",
        body: "用户 11 月 1 日交季度报告",
        due_at: localNov1,
        is_due: false,
      }),
    ]);
    await setup({}, { onLoadMemories });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(1));
    expect(screen.getByText(/待办 2026-11-01/)).toBeInTheDocument();
    // 未到期就不许出现「已到期」—— 两个标记一起出现等于告诉用户「明天到期且已经过期」。
    expect(screen.queryByText(/已到期/)).toBeNull();
  });

  it("marks an overdue prospective memory as due without a second time comparison in the UI", async () => {
    // `is_due` 是 Rust 现算的布尔，前端只渲染。构造一个 `is_due: true` 的行，
    // 逾期判据不归组件管（`due_at` 与 `is_due` 故意矛盾，测的是「组件听谁的」）。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, {
        kind: "prospective",
        body: "用户要交季度报告",
        due_at: new Date(2020, 0, 2).getTime(),
        is_due: true,
      }),
    ]);
    await setup({}, { onLoadMemories });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(1));
    expect(screen.getByText(/待办 2020-01-02/)).toBeInTheDocument();
    expect(screen.getByText(/已到期/)).toBeInTheDocument();
  });

  it("does not dress a plain fact up as a todo", async () => {
    // `due_at` 只有 `prospective` 才有值，但视图是原样镜像；组件用 kind 把关，
    // 否则一条解析残留的 due_at 会让普通事实看起来像待办。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, { kind: "fact", body: "用户住在北京", due_at: new Date(2026, 10, 1).getTime() }),
    ]);
    await setup({}, { onLoadMemories });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(1));
    expect(screen.queryByText(/待办 2026-11-01/)).toBeNull();
  });

  // ---- T12：常驻画像展示（有内容才渲染） ----

  it("renders the resident profile above the list when there is one", async () => {
    const onLoadProfile = vi.fn<PanelProps["onLoadProfile"]>(
      async () => "用户是 Rust 开发者，偏好简洁回答",
    );
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [memory(1)]);
    await setup({}, { onLoadProfile, onLoadMemories });
    const profile = await screen.findByTestId("memory-profile");
    expect(profile).toHaveTextContent("用户是 Rust 开发者，偏好简洁回答");
    // 「列表上方」是这一屏的布局承诺：画像行必须排在第一行记忆之前。
    const firstItem = screen.getAllByTestId("memory-item")[0];
    expect(
      profile.compareDocumentPosition(firstItem) & Node.DOCUMENT_POSITION_FOLLOWING,
      "画像必须是列表上方的一行，不是列表下方的小结",
    ).toBeTruthy();
    // 拉一次就够：面板挂载时一次，App 侧用 useCallback 固定身份。
    expect(onLoadProfile).toHaveBeenCalledTimes(1);
  });

  it("stays silent when the profile is empty", async () => {
    // 空串 = 后端从未写过画像。渲染一行空的「用户画像」会让用户以为系统记了一片空白，
    // 那和「没记过」是两件事 —— 所以空串时连 `memory-profile` 这个节点都不出现。
    await setup({}, { onLoadProfile: vi.fn(async () => "") });
    await waitFor(() => expect(screen.getByTestId("memory-empty")).toBeInTheDocument());
    expect(screen.queryByTestId("memory-profile")).toBeNull();
  });

  it("routes a failed profile load into the existing error channel", async () => {
    const onLoadProfile = vi.fn<PanelProps["onLoadProfile"]>(async () => {
      throw new Error("memory_profile 失败：库打不开");
    });
    await setup({}, { onLoadProfile });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "memory_profile 失败：库打不开",
    );
  });

  it("toggles memory_enabled through the checkbox", async () => {
    // 夹具载入 true（= Rust 默认）：点一下必须把载荷翻成 false —— 否则「启用长期记忆」
    // 只是装饰文字，用户永远关不掉注入。
    const p = await setup();
    fireEvent.click(screen.getByLabelText(/启用长期记忆/));
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].memory_enabled).toBe(false);
  });

  it("renders the memories onLoadMemories returned and marks the invalid ones", async () => {
    // 120 个字，超过 60：列表是「审计」不是「阅读」，正文要截断 —— 完整内容反正会以
    // 注入的形态进上下文，设置页不许变成第二个阅读器。
    const long = "一二三四五六七八九十".repeat(12);
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, { body: long }),
      memory(2, { body: "用户偏好深色主题", kind: "preference", valid: false, invalid_at: 1 }),
    ]);
    await setup({}, { onLoadMemories });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(2));
    // 截断到 60 字 + 省略号，且整段原文不许出现在界面上。
    expect(screen.getByText(new RegExp(long.slice(0, 60)))).toBeInTheDocument();
    expect(screen.queryByText(new RegExp(long))).toBeNull();
    // kind 原样展示（名单只有 Rust 说了算，前端不翻译）；失效的那行必须有「已失效」标记。
    expect(screen.getByText(/preference/)).toBeInTheDocument();
    expect(screen.getByText("已失效")).toBeInTheDocument();
  });

  it("deletes one memory through onDeleteMemory and reloads the list", async () => {
    // 第一次给两行，删除后再拉一次只剩一行：列表的真值永远来自重拉，前端不许自己 splice。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>()
      .mockResolvedValueOnce([memory(1), memory(2)])
      .mockResolvedValueOnce([memory(2)]);
    const onDeleteMemory = vi.fn<PanelProps["onDeleteMemory"]>(async () => {});
    await setup({}, { onLoadMemories, onDeleteMemory });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(2));
    const rows = screen.getAllByTestId("memory-item");
    fireEvent.click(within(rows[0]).getByRole("button", { name: "删除" }));
    await waitFor(() => expect(onDeleteMemory).toHaveBeenCalledWith(1));
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(1));
    expect(onLoadMemories).toHaveBeenCalledTimes(2);
  });

  it("clears every memory only through an explicit, confirmed action", async () => {
    // 与「清除已存的 key」同一条红线：零弹窗，第一次点只是把按钮变成确认态，
    // 确认之后才打命令、重拉列表 —— 空了才许出 memory-empty。
    const onClearMemories = vi.fn<PanelProps["onClearMemories"]>(async () => {});
    // 两次拉取分开钉：打开时一行、清空后重拉空表 —— 重拉的返回才是列表的真值，
    // 不能用一个「永远返回一行」的 mock（那样界面永远不出空态，测的就是 mock 而不是面板）。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>()
      .mockResolvedValueOnce([memory(1)])
      .mockResolvedValueOnce([]);
    await setup({}, { onLoadMemories, onClearMemories });
    await screen.findByTestId("memory-item");
    fireEvent.click(screen.getByRole("button", { name: "清空全部记忆" }));
    expect(onClearMemories).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "确认清空" }));
    await waitFor(() => expect(onClearMemories).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.getByTestId("memory-empty")).toBeInTheDocument());
    expect(screen.queryByTestId("memory-item")).toBeNull();
  });

  it("says so when the memory store is empty", async () => {
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => []);
    await setup({}, { onLoadMemories });
    expect(await screen.findByTestId("memory-empty")).toBeInTheDocument();
    expect(screen.queryByTestId("memory-item")).toBeNull();
  });

  it("routes a failed memory load into the existing error channel", async () => {
    // 拉列表失败不炸面板、不弹窗：进既有 error 通道（role=alert），原文照贴（R195 口径）。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => {
      throw new Error("memories_list 失败：库打不开");
    });
    await setup({}, { onLoadMemories });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "memories_list 失败：库打不开",
    );
  });

  it("never puts the key into the settings payload", async () => {
    // 夹具里的 key 必须是明显假的（R164）。这条同时钉三件事：不进 `setSettings` 的 body、
    // 走专用命令 `settings_set_api_key`、提交后本地输入框清空。
    const FAKE = "sk-THIS-IS-NOT-A-REAL-KEY";
    const p = await setup();
    fireEvent.change(screen.getByLabelText(/API key/i), { target: { value: FAKE } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSaveKey).toHaveBeenCalledWith(FAKE));
    expect(JSON.stringify(p.onSave.mock.calls)).not.toMatch(/NOT-A-REAL-KEY/);
    expect((screen.getByLabelText(/API key/i) as HTMLInputElement).value).toBe("");
  });

  it("shows the mask, and an untouched key field does not clear the credential", async () => {
    const p = await setup({ api_key_masked: "sk-a…9z24" });
    expect(screen.getByText(/sk-a…9z24/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    // 输入框留空 = 不动凭据。把空串当「没改」转发给 `setApiKey` 会让一次普通保存
    // 悄悄删掉 key —— 命令侧的空串语义是清除（settings.rs R88）。
    expect(p.onSaveKey).not.toHaveBeenCalled();
    expect(p.onClearKey).not.toHaveBeenCalled();
  });

  it("clears the credential only through an explicit, confirmed action", async () => {
    const p = await setup({ api_key_masked: "sk-a…9z24" });
    fireEvent.click(screen.getByRole("button", { name: /清除/ }));
    // 零弹窗红线：不许 `window.confirm`，第一次点只是把按钮本身变成确认态。
    expect(p.onClearKey).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: /确认清除/ }));
    await waitFor(() => expect(p.onClearKey).toHaveBeenCalledTimes(1));
  });

  it("keeps the panel open with the raw failure text when saving rejects", async () => {
    const p = await setup(
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
    const p = await setup();
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

  it("closes on Escape but not on a backdrop click", async () => {
    // 两条一起写，因为它们是同一个取舍的两半：键盘用户要能一按就走（`aria-modal` 的承诺），
    // 而**点空白处不许**把没保存的草稿丢掉 —— 这里没有弹窗可以「点外面关掉」，
    // 误点代价是重打一遍模型名。肉眼检查（一次性预览页）确认过这两条的实际行为。
    const p = await setup();
    const dialog = screen.getByRole("dialog");
    fireEvent.change(screen.getByLabelText(/模型名/), { target: { value: "qwen-turbo" } });
    fireEvent.click(dialog.parentElement as HTMLElement);
    expect(p.onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(p.onClose).toHaveBeenCalledTimes(1);
  });

  it("names the corrupted settings backup only when there is one", async () => {
    await setup({ corrupt_backup: "C:\\Users\\x\\settings.json.corrupt-1700000000000" });
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
    const p = await setup({ accent: "" });
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
    const p = await setup();
    const follow = screen.getByLabelText(/跟随预设/) as HTMLInputElement;
    expect(follow.checked, "载入自选色时复选框不该是勾上的").toBe(false);
    fireEvent.click(follow);
    expect(screen.queryByLabelText(/强调色/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0].accent).toBe("");
  });

  it("loads a stored custom accent into an unchecked box and a picker showing that colour", async () => {
    // 「面板每次打开都是全新挂载」（App.tsx 按 settingsView !== null 条件渲染），所以
    // defaultValue 只在挂载时读一次就够 —— 这条钉的就是那一次：已存的色必须看得见。
    await setup();
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
    const p = await setup({ accent: "#aa00ff" });
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
    const p = await setup({ accent: "" });
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
    // 这里钉的从来不是「存不存得进」，而是「不许半拉子地先加一个没有校验、没有说明的输入框」——
    for (const label of [/温度/, /temperature/i, /压缩|阈值|keep_turns/i, /MCP/i]) {
      expect(screen.queryByLabelText(label)).toBeNull();
    }
  });

  // ==== P1：用户权威（行内编辑 / 手动新增 / 「已编辑」标记）====
  // 这一组的立组理由（spec §11.4 的取向）：记忆是模型抽的，但**用户的话更权威**。
  // 没有编辑权，用户看到一条记错的正文只能删掉再重新等模型抽；没有新增入口，
  // 想让它记住一件模型没抽到的事就无处可说。

  it("marks a memory the user edited by hand, and only that one", async () => {
    // `user_edited` 是后端开关合并管线的那个布尔（模型此后的 UPDATE/INVALIDATE 会降级成
    // 「并存」）。行上的标记不是装饰：它让用户认得出哪一条是自己的说法。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, { body: "用户住在上海", user_edited: true }),
      memory(2, { body: "用户喜欢咖啡" }),
    ]);
    await setup({}, { onLoadMemories });
    await waitFor(() => expect(screen.getAllByTestId("memory-item")).toHaveLength(2));
    expect(screen.getByText(/已编辑/)).toBeInTheDocument();
    // 只有一行带标记：两条都渲染「已编辑」的话这个标记就没有信息量了。
    expect(screen.queryAllByText(/已编辑/)).toHaveLength(1);
  });

  it("edits the full body inline and reloads the list after the save", async () => {
    // 列表展示的是截断到 60 字的 `brief`，编辑框必须是**完整正文** ——
    // 从截断串编辑等于让用户亲手把自己记忆的后半截删掉。
    const long = "一二三四五六七八九十".repeat(12); // 120 字
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>()
      .mockResolvedValueOnce([memory(1, { body: long })])
      .mockResolvedValueOnce([memory(1, { body: "用户住在上海", user_edited: true })]);
    const onUpdateMemory = vi.fn<PanelProps["onUpdateMemory"]>(async () => {});
    await setup({}, { onLoadMemories, onUpdateMemory });
    await screen.findByTestId("memory-item");
    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    const box = screen.getByLabelText(/编辑记忆/) as HTMLTextAreaElement;
    expect(box.value, "编辑框拿到的是被截断的 brief 的话，保存一次就丢掉半条记忆").toBe(long);
    fireEvent.change(box, { target: { value: "用户住在上海" } });
    fireEvent.click(screen.getByRole("button", { name: "保存修改" }));
    await waitFor(() => expect(onUpdateMemory).toHaveBeenCalledWith(1, "用户住在上海"));
    // 写入后的真值来自重拉，不在前端 splice（与删除同一条口径）。
    await waitFor(() => expect(screen.getByText(/用户住在上海/)).toBeInTheDocument());
    expect(onLoadMemories).toHaveBeenCalledTimes(2);
    expect(screen.queryByLabelText(/编辑记忆/)).toBeNull();
  });

  it("cancels an inline edit without writing anything", async () => {
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [
      memory(1, { body: "用户住在北京" }),
    ]);
    const onUpdateMemory = vi.fn<PanelProps["onUpdateMemory"]>(async () => {});
    await setup({}, { onLoadMemories, onUpdateMemory });
    await screen.findByTestId("memory-item");
    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText(/编辑记忆/), { target: { value: "改了但不想要" } });
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    expect(onUpdateMemory).not.toHaveBeenCalled();
    expect(screen.getByText(/用户住在北京/)).toBeInTheDocument();
    expect(screen.queryByLabelText(/编辑记忆/)).toBeNull();
  });

  it("refuses to save an inline edit that would be an empty body", async () => {
    // 前端只做这一层轻校验（真校验在 Rust 的 `reject_empty_body`）：按钮禁用比
    // 「点了才报错」少一次往返，也不给用户一个「看起来能点」的承诺。
    // 纯空白同样算空 —— 后端就是这么判的（`body.trim().is_empty()`）。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => [memory(1)]);
    await setup({}, { onLoadMemories });
    await screen.findByTestId("memory-item");
    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.change(screen.getByLabelText(/编辑记忆/), { target: { value: "   " } });
    expect(screen.getByRole("button", { name: "保存修改" })).toBeDisabled();
  });

  it("adds a hand-written memory with the kind the user picked and reloads", async () => {
    // 新增的记忆从出生起就是用户权威（`add_user_memory` 直接打 `user_edited = 1`），
    // 所以重拉回来的这一行会带「已编辑」标记 —— 面板不自己造这条状态，只渲染后端给的。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>()
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([
        memory(9, { kind: "preference", body: "用户喜欢深色主题", user_edited: true }),
      ]);
    const onAddMemory = vi.fn<PanelProps["onAddMemory"]>(async () => 9);
    await setup({}, { onLoadMemories, onAddMemory });
    await screen.findByTestId("memory-empty");
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "preference" } });
    fireEvent.change(screen.getByLabelText(/记忆正文/), {
      target: { value: "用户喜欢深色主题" },
    });
    fireEvent.click(screen.getByRole("button", { name: "添加" }));
    await waitFor(() =>
      expect(onAddMemory).toHaveBeenCalledWith(
        "preference",
        "用户喜欢深色主题",
        // 非待办一律传 null(提醒时间输入框整体不渲染,草稿里也不许有残留值)。
        null,
      ),
    );
    await waitFor(() => expect(screen.getByTestId("memory-item")).toHaveTextContent("用户喜欢深色主题"));
    expect(onLoadMemories).toHaveBeenCalledTimes(2);
    // 成功之后输入框清空，但类型选择留给下一次（用户往往连续记同一类事）。
    expect(screen.getByLabelText(/记忆正文/)).toHaveValue("");
    expect(screen.getByLabelText(/记忆类型/)).toHaveValue("preference");
  });

  it("renders the reminder time box only for a prospective and hides it again", async () => {
    // 输入框的出现/消失直接跟 `addKind` 走:非待办没有「提醒时间」这个概念,
    // 摆一个灰掉的框只会让人以为「填了也会有用」。
    await setup({}, { onLoadMemories: vi.fn<PanelProps["onLoadMemories"]>(async () => []) });
    await screen.findByTestId("memory-empty");
    expect(screen.queryByLabelText(/提醒时间/)).toBeNull();
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "prospective" } });
    expect(screen.getByLabelText(/提醒时间/)).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "fact" } });
    expect(screen.queryByLabelText(/提醒时间/)).toBeNull();
  });

  it("sends the datetime-local literal as UTC milliseconds for a prospective", async () => {
    // 时区口径是 T9/T10 的既有裁定:字面值**按 UTC 解释**(Rust 侧
    // `parse_iso8601_ms("2026-11-01T09:00:00") == 1_793_523_600_000`)。
    // 所以这里钉死的 `1_793_523_600_000` 必须等于 `Date.UTC(2026, 10, 1, 9, 0)`;
    // 若前端误用 `new Date("2026-11-01T09:00")`(按**本地**时区解析),
    // 东八区会算出 1_793_494_800_000,差 28_800_000 ms —— 这条断言就是那道闸。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => []);
    const onAddMemory = vi.fn<PanelProps["onAddMemory"]>(async () => 1);
    await setup({}, { onLoadMemories, onAddMemory });
    await screen.findByTestId("memory-empty");
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "prospective" } });
    fireEvent.change(screen.getByLabelText(/记忆正文/), {
      target: { value: "2026 年 11 月 1 日上午交周报" },
    });
    fireEvent.change(screen.getByLabelText(/提醒时间/), {
      target: { value: "2026-11-01T09:00" },
    });
    fireEvent.click(screen.getByRole("button", { name: "添加" }));
    await waitFor(() =>
      expect(onAddMemory).toHaveBeenCalledWith(
        "prospective",
        "2026 年 11 月 1 日上午交周报",
        1_793_523_600_000,
      ),
    );
    // 成功后时间框也要清空:留着上一个待办的时刻,下一次「添加」会静默继承它,
    // 用户以为自己只是在记一件事,系统却已经在同一时刻设了第二个闹钟。
    expect(screen.getByLabelText(/提醒时间/)).toHaveValue("");
  });

  it("omits the reminder (null) when the prospective has no time filled in", async () => {
    // 「待办但不提醒」是合法状态(用户可能只是先记下来)。空串必须变成 null,
    // 而不是 0 或 NaN —— 0 是 1970 年,会立刻触发一次提醒。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => []);
    const onAddMemory = vi.fn<PanelProps["onAddMemory"]>(async () => 1);
    await setup({}, { onLoadMemories, onAddMemory });
    await screen.findByTestId("memory-empty");
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "prospective" } });
    fireEvent.change(screen.getByLabelText(/记忆正文/), { target: { value: "改天再说的事" } });
    fireEvent.click(screen.getByRole("button", { name: "添加" }));
    await waitFor(() =>
      expect(onAddMemory).toHaveBeenCalledWith("prospective", "改天再说的事", null),
    );
  });

  it("never forwards a stale reminder draft after switching back to a non-prospective", async () => {
    // 填了时间又改回「事实」时,载荷必须是 null(输入框已经不存在,草稿也不许跟着走)。
    // 这是「两个字段的可见性与载荷同源」的那条规矩在新增区的应用。
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => []);
    const onAddMemory = vi.fn<PanelProps["onAddMemory"]>(async () => 1);
    await setup({}, { onLoadMemories, onAddMemory });
    await screen.findByTestId("memory-empty");
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "prospective" } });
    fireEvent.change(screen.getByLabelText(/提醒时间/), {
      target: { value: "2026-11-01T09:00" },
    });
    fireEvent.change(screen.getByLabelText(/记忆类型/), { target: { value: "fact" } });
    fireEvent.change(screen.getByLabelText(/记忆正文/), { target: { value: "用户养了一只猫" } });
    fireEvent.click(screen.getByRole("button", { name: "添加" }));
    await waitFor(() =>
      expect(onAddMemory).toHaveBeenCalledWith("fact", "用户养了一只猫", null),
    );
  });

  it("keeps the add button disabled until the new memory has a body", async () => {
    const onLoadMemories = vi.fn<PanelProps["onLoadMemories"]>(async () => []);
    await setup({}, { onLoadMemories });
    await screen.findByTestId("memory-empty");
    const add = screen.getByRole("button", { name: "添加" });
    expect(add).toBeDisabled();
    fireEvent.change(screen.getByLabelText(/记忆正文/), { target: { value: "   " } });
    expect(add).toBeDisabled();
    fireEvent.change(screen.getByLabelText(/记忆正文/), { target: { value: "记一件事" } });
    expect(add).toBeEnabled();
  });
});
