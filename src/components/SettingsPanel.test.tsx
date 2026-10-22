// T45 的组件层判据。为什么单独一个文件、以及为什么这份夹具**必须写满 10 个键**：
// `SettingsView` 是 `#[serde(flatten)]` 拍平一层的形状（types.ts:128-131），面板少读一个键
// 的话，写成 `as SettingsView` 的断言转换会照样绿 —— 这里用完整字面量，缺项由 TS 拦。
// `waitFor` 用 `@testing-library/react` 那一个，不用 `vi.waitFor`：只有它包在 `act` 里，
// 否则保存这类 await 之后的 setState 会留下一堆 act 警告，把真正的失败淹在 stderr 里。
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsPanel } from "./SettingsPanel";
import type { SettingsView } from "../types";

const view = (p: Partial<SettingsView> = {}): SettingsView => ({
  base_url: "https://example.invalid/v1",
  model: "qwen-plus",
  reserve_output: 4096,
  budget_tokens: 32_000,
  window_messages: 40,
  theme: "noir",
  mode: "dark",
  font_size: 15,
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
  it("saves the whole settings table with only the edited field changed", async () => {
    const p = setup({ model: "qwen-plus", budget_tokens: 32_000, reserve_output: 4096 });
    fireEvent.change(screen.getByLabelText(/模型名/), { target: { value: "qwen-turbo" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(p.onSave).toHaveBeenCalled());
    expect(p.onSave.mock.calls[0][0]).toEqual({
      base_url: "https://example.invalid/v1",
      model: "qwen-turbo",
      reserve_output: 4096,
      budget_tokens: 32_000,
      window_messages: 40,
      theme: "noir",
      mode: "dark",
      font_size: 15,
    });
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

  it("renders no field the store cannot save today", () => {
    // Step 0 的 grep 结果：`Settings` 恰好 8 个键（settings.rs:245-252 / types.ts:109-118）。
    // 渲染一个存不进去的输入框会造出「改了但没生效」的用户可见 bug，比缺一个组更糟。
    // T39/T43/Plan B 落了 `temperature`/`accent`/`roots` 之后这条要跟着改，不许悄悄删。
    for (const label of [/温度/, /temperature/i, /强调色/, /accent/i, /目录|roots/i, /MCP/i]) {
      expect(screen.queryByLabelText(label)).toBeNull();
    }
  });
});
