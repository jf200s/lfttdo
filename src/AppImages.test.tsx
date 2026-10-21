// T19 在 **App 接线** 那一层的三条。为什么单开一个文件而不是往 `App.test.tsx` 里加：
// R216② 裁定那十三条「一条都不许改」，把它们所在的文件保持**字节不动**是最强的证明；
// 而 T19 的连带风险恰好都在 App 这一侧（载荷原样转发、失败要不要把 rejection 传回去、
// 没有会话时待发图留不留），`Composer.test` 那一边只能证明 Composer 自己的合同。
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import App from "./App";
import type { ContentBlock, SessionRow, SettingsView } from "./types";

const h = vi.hoisted(() => {
  const listeners: ((sessionId: string, e: never) => void)[] = [];
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
    // App 订阅的是 `subscribeAgent`（不是 `listen`）：mock 工厂里缺这一支，渲染期就会抛
    // 「No "subscribeAgent" export is defined on the mock」，十三条一起红 —— 红的是接线不是行为。
    subscribeAgent: (cb: (sessionId: string, e: never) => void) => {
      listeners.push(cb);
      return () => {
        const i = listeners.indexOf(cb);
        if (i >= 0) listeners.splice(i, 1);
      };
    },
    listeners,
  };
});

vi.mock("./ipc/commands", () => h);

const row = (id: string, updated_at: number): SessionRow => ({
  id,
  title: `会话 ${id}`,
  updated_at,
});

const view = (): SettingsView => ({
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
});

const imageBlock: ContentBlock = { type: "image", data: "AAAA", mime: "png" };

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
  h.normalize.mockReset().mockResolvedValue(imageBlock);
  h.listeners.length = 0;
});

/** 等 App 走完「拉列表 + 打开最新那条」那一段，输入区才有人接得住事件。 */
const settled = async () => {
  await waitFor(() => expect(h.getSettings).toHaveBeenCalled());
  await waitFor(() => expect(h.replaySession).toHaveBeenCalled());
};

const dropPng = (byte: number) =>
  fireEvent.drop(screen.getByTestId("drop-zone"), {
    dataTransfer: {
      files: [new File([new Uint8Array([byte])], "截图.png", { type: "image/png" })],
    } as unknown as DataTransfer,
  });

describe("App 图片接线", () => {
  it("forwards the blocks the composer built, images first and text last", async () => {
    render(<App />);
    await settled();
    dropPng(1);
    await screen.findByTestId("thumb-0");
    fireEvent.change(screen.getByRole("textbox", { name: "输入消息" }), {
      target: { value: "图里是什么" },
    });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() =>
      expect(h.sendMessage).toHaveBeenCalledWith("B", [
        imageBlock,
        { type: "text", text: "图里是什么" },
      ]),
    );
    // 乐观插入那支气泡里也必须有图：`appendUserTurn` 收到的是同一份 blocks。
    expect(await screen.findByTestId("bubble-user")).toHaveTextContent("图里是什么");
    // 发成功之后待发图清空（Step 2 第 8 条的上半场，在 App 这一层同样成立）。
    await waitFor(() => expect(screen.queryByTestId("thumb-0")).toBeNull());
  });

  // 变异判据（交付判据 c）：删掉 `App.send` 的 `throw e` ⇒ 这条红。
  // 少了那句，App 把失败吞成 resolved，Composer 以为发出去了、清空四张待发图。
  it("keeps the pending images when send_message rejects", async () => {
    h.sendMessage.mockRejectedValue("尚未配置 API key");
    render(<App />);
    await settled();
    dropPng(2);
    await screen.findByTestId("thumb-0");
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(h.sendMessage).toHaveBeenCalled());
    // 图和那句「还没配 key」都得上屏可见，且图还在。
    expect(await screen.findByTestId("thumb-0")).toHaveAccessibleName("截图.png");
    expect(await screen.findByTestId("app-notice")).toHaveTextContent(
      "发送失败：尚未配置 API key",
    );
  });

  // 没有可发的会话时同理：静默 return 会吃掉待发图，症状是「点了发送，图没了，什么也没发」。
  // 变异判据：把 `throw` 换回 `return` ⇒ 这条红。
  it("keeps the pending images when there is no session to send to", async () => {
    h.sessions.mockRejectedValue("存储不可用");
    render(<App />);
    await waitFor(() => expect(h.getSettings).toHaveBeenCalled());
    dropPng(3);
    await screen.findByTestId("thumb-0");
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    // 这一条场景里界面上有**两条** notice（「读取会话失败」+「还没有可用的会话」），
    // 所以只能按全体去找，不能用 getByTestId（多条命中会先抛「Found multiple」）。
    await waitFor(() =>
      expect(
        screen.getAllByTestId("app-notice").map((n) => n.textContent),
      ).toContain("还没有可用的会话，先新建一条"),
    );
    expect(screen.getByTestId("thumb-0")).toHaveAccessibleName("截图.png");
    expect(h.sendMessage).not.toHaveBeenCalled();
  });
});
