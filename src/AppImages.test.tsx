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
  temperature: 0.7,
  compact_threshold: 0.85,
  keep_turns: 10,
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
    // 修复轮 F3 —— 原先这里只断文字，于是「把 text 那一支交给 appendUserTurn、图只发给
    // sendMessage」这种错实现照样绿，症状是用户重开这条会话才发现自己的图没进历史。
    const bubble = await screen.findByTestId("bubble-user");
    expect(bubble).toHaveTextContent("图里是什么");
    const thumb = bubble.querySelector("img.thumb");
    expect(thumb).not.toBeNull();
    expect(thumb).toHaveAttribute("src", "data:image/png;base64,AAAA");
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

  // 评审 #29 规格轴：`库里那一行 → 重放气泡里的图` 是整条视觉链上唯一没人断言的一环。
  // 它不是洁癖 —— 1aafbe2 修的那个 bug（`data:png;base64,…` 不是合法 data URL，图直接不显示）
  // 就住在 `MessageBubble` 的 image 支，而直播与重放共用这一个组件：当时靠乐观插入看得见，
  // 重开这条会话就看不见。直播那半边已由第一条用例钉住，这条钉另外半边，
  // 并且把两侧的 src **写成同一个字符串**，这样「两条路各修一半」当场可红。
  // 变异判据：把 `MessageBubble.tsx` 的 `data:image/${mime}` 改回 `data:${mime}` ⇒ 这两条一起红。
  it("renders a replayed image with the very same data URL the live path uses", async () => {
    h.replaySession.mockResolvedValue([
      { t: "user_message", blocks: [imageBlock] },
    ]);
    render(<App />);
    await settled();
    const bubble = await screen.findByTestId("bubble-user");
    const thumb = bubble.querySelector("img.thumb");
    expect(thumb).not.toBeNull();
    expect(thumb).toHaveAttribute("src", "data:image/png;base64,AAAA");
  });

  // 评审 #29（前端与配置）：归一化是异步的，而 `submit` 在那次 `await` 之后清一次待发区。
  // 于是「贴图的 await 还挂着、用户已经按了回车」这段时间里回来的那张图会被一起清掉 ——
  // 症状与 0650036 修掉的「已忽略 N 张永不消失」同一族：界面先承诺，然后无声吞掉。
  // 该留下的判据是**归属**：这张图没进那次 `send_message` 的载荷，它就属于下一条消息，
  // 必须原样留在待发区让用户再按一次发送，而不是被上一次发送的清理带走。
  it("keeps an image whose normalize lands while the send is in flight", async () => {
    let resolveNormalize: (b: ContentBlock) => void = () => {};
    let resolveSend: () => void = () => {};
    h.normalize.mockImplementation(
      () =>
        new Promise<ContentBlock>((r) => {
          resolveNormalize = r;
        }),
    );
    h.sendMessage.mockImplementation(
      () =>
        new Promise<void>((r) => {
          resolveSend = r;
        }),
    );

    render(<App />);
    await settled();
    dropPng(7); // 归一化故意悬着：此刻待发区还是空的
    fireEvent.change(screen.getByRole("textbox", { name: "输入消息" }), {
      target: { value: "先看这个" },
    });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(h.sendMessage).toHaveBeenCalled());
    // 载荷里只有文本：那张图当时还不存在，不许替用户把它塞进这一条消息。
    expect(h.sendMessage).toHaveBeenCalledWith("B", [
      { type: "text", text: "先看这个" },
    ]);

    resolveNormalize(imageBlock);
    await screen.findByTestId("thumb-0");
    resolveSend();
    // 输入框变空 = 成功路径的清理确实跑过了；在这之后图还在，才是「只撤发出去的那几张」。
    await waitFor(() =>
      expect(screen.getByRole("textbox", { name: "输入消息" })).toHaveValue(""),
    );
    expect(screen.getByTestId("thumb-0")).toHaveAccessibleName("截图.png");
  });
});
