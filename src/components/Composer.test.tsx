// 语音输入（Phase 2/3 前端）的组件层判据。为什么单开一份文件而不是塞进
// `ImageStrip.test.tsx`：那一份的立组理由是「图片三入口」，语音是另一条链路
// （开始/停止/取消 + 选区插入 + 自动发送），混进去会让两组用法都难找。
//
// 判据口径与那份文件一致：接线（谁被调了几次、载荷是什么）全部断言在 mock 上，
// 样式另由 stylelint 与 SettingsPanel.test.tsx 的源码闸管。
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import type { ContentBlock, SendBlock } from "../types";

const imageBlock: ContentBlock = { type: "image", data: "AAAA", mime: "png" };

type ComposerProps = React.ComponentProps<typeof Composer>;

const setup = (over: Partial<ComposerProps> = {}) => {
  const onSend = vi.fn<ComposerProps["onSend"]>(async () => {});
  const onNormalize = vi.fn<ComposerProps["onNormalize"]>(async () => imageBlock);
  const onVoiceStart = vi.fn<NonNullable<ComposerProps["onVoiceStart"]>>(
    async () => {},
  );
  const onVoiceStop = vi.fn<NonNullable<ComposerProps["onVoiceStop"]>>(
    async () => "你好",
  );
  const onVoiceCancel = vi.fn<NonNullable<ComposerProps["onVoiceCancel"]>>(
    async () => {},
  );
  render(
    <Composer
      streaming={false}
      onSend={onSend}
      onStop={vi.fn()}
      onNormalize={onNormalize}
      onVoiceStart={onVoiceStart}
      onVoiceStop={onVoiceStop}
      onVoiceCancel={onVoiceCancel}
      {...over}
    />,
  );
  return { onSend, onNormalize, onVoiceStart, onVoiceStop, onVoiceCancel };
};

const textarea = () =>
  screen.getByRole("textbox", { name: "输入消息" }) as HTMLTextAreaElement;

/** 点「语音」并等到按钮真的翻成「停止 m:ss」（`voice_start` 的 promise 已落地）。 */
const startVoice = async () => {
  fireEvent.click(screen.getByRole("button", { name: "语音" }));
  return screen.findByRole("button", { name: /^停止/ });
};

describe("Composer 语音输入", () => {
  it("starts recording on the mic click and stops through voice_stop", async () => {
    const p = setup();
    const stopButton = await startVoice();
    expect(p.onVoiceStart).toHaveBeenCalledTimes(1);
    // 三态里的中间态：录音中按钮就是「停止 + 计时」，且带 data-recording 供样式挂钩。
    expect(stopButton).toHaveAttribute("data-recording", "true");

    p.onVoiceStop.mockResolvedValue("你好世界");
    fireEvent.click(stopButton);
    await waitFor(() => expect(p.onSend).toHaveBeenCalledTimes(1));
    expect(p.onVoiceStop).toHaveBeenCalledTimes(1);
    // 空草稿 + 识别文本 ⇒ 恰好一条 text 块，且只发一次（语音停止不许触发第二次发送）。
    expect(p.onSend.mock.calls[0][0]).toEqual([
      { type: "text", text: "你好世界" },
    ]);
  });

  it("inserts the transcript into the current selection and sends the merged text", async () => {
    // 发送失败（reject）用来把插入后的草稿留在输入框里 —— 成功路径清空文本，
    // 「值正确」就只能从载荷反推。这里两条都能断言。
    const onSend = vi.fn<ComposerProps["onSend"]>(() => Promise.reject("不要真的发"));
    const p = setup({ onSend });
    fireEvent.change(textarea(), { target: { value: "一二三四" } });
    // 选区 (1,3) = 选中「二三」，识别结果替换它 ⇒ 一 + 语音 + 四。
    textarea().setSelectionRange(1, 3);

    p.onVoiceStop.mockResolvedValue("语音");
    fireEvent.click(await startVoice());
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));

    expect(onSend.mock.calls[0][0]).toEqual([{ type: "text", text: "一语音四" }]);
    expect(textarea()).toHaveValue("一语音四");
  });

  it("shows a voice error and does not send when nothing was recognized", async () => {
    const p = setup();
    p.onVoiceStop.mockResolvedValue("   ");
    fireEvent.click(await startVoice());

    expect(await screen.findByTestId("voice-error")).toHaveTextContent(
      "没有识别到文字",
    );
    expect(p.onSend).not.toHaveBeenCalled();
    // 空结果不许改动草稿，也不许留下一个「识别中…」的死按钮。
    expect(textarea()).toHaveValue("");
    expect(screen.getByRole("button", { name: "语音" })).toBeInTheDocument();
  });

  it("echoes a rejected voice_stop verbatim and leaves the draft alone", async () => {
    const p = setup();
    fireEvent.change(textarea(), { target: { value: "别动我" } });
    p.onVoiceStop.mockRejectedValue("麦克风被占用，无法开始识别");
    fireEvent.click(await startVoice());

    expect(await screen.findByTestId("voice-error")).toHaveTextContent(
      "麦克风被占用，无法开始识别",
    );
    expect(p.onSend).not.toHaveBeenCalled();
    expect(textarea()).toHaveValue("别动我");
  });

  it("cancels with Escape while recording without sending", async () => {
    const p = setup();
    await startVoice();
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(p.onVoiceCancel).toHaveBeenCalledTimes(1));
    expect(p.onVoiceStop).not.toHaveBeenCalled();
    expect(p.onSend).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "语音" })).toBeInTheDocument();
  });

  it("auto-sends the transcript with pending images ahead of the text", async () => {
    const p = setup();
    fireEvent.drop(screen.getByTestId("drop-zone"), {
      dataTransfer: {
        files: [new File([new Uint8Array([1])], "截图.png", { type: "image/png" })],
      } as unknown as DataTransfer,
    });
    await screen.findByTestId("thumb-0");
    fireEvent.change(textarea(), { target: { value: "看这个" } });
    textarea().setSelectionRange(3, 3);

    p.onVoiceStop.mockResolvedValue("顺便听听");
    fireEvent.click(await startVoice());
    await waitFor(() => expect(p.onSend).toHaveBeenCalledTimes(1));

    // 图在前、文本在后（与手动发送同一条拼装路径），没有第二份发送实现。
    expect(p.onSend.mock.calls[0][0]).toEqual([
      imageBlock,
      { type: "text", text: "看这个顺便听听" },
    ]);
    // 成功之后待发图随草稿一起清掉（走的是 `submit` 的既有清理）。
    await waitFor(() => expect(screen.queryByTestId("thumb-0")).toBeNull());
  });

  it("renders no mic button when the host does not provide voice callbacks", () => {
    render(
      <Composer
        streaming={false}
        onSend={vi.fn<(b: SendBlock[]) => Promise<void>>(async () => {})}
        onStop={vi.fn()}
        onNormalize={vi.fn<ComposerProps["onNormalize"]>(async () => imageBlock)}
      />,
    );
    expect(screen.queryByTestId("voice-button")).toBeNull();
  });

  it("disables the mic while streaming", () => {
    setup({ streaming: true });
    // streaming 时按钮不在初始文案「语音」上？它在（三态仍是 idle），但必须禁用。
    expect(screen.getByTestId("voice-button")).toBeDisabled();
  });

  it("blocks manual send while recording so the transcript is sent exactly once", async () => {
    const p = setup();
    await startVoice();
    // 录音中发送按钮必须是灰的：否则用户先手动发一次、停止后又会自动发一次。
    expect(screen.getByRole("button", { name: "发送" })).toBeDisabled();
    fireEvent.submit(screen.getByTestId("composer"));
    expect(p.onSend).not.toHaveBeenCalled();
  });

  it("does not enter recording when voice_start resolves after streaming began", async () => {
    // 回归（竞态）：voice_start 在途时发送先被 startingRef 挡住；即便宿主在 start
    // resolve 前已把 streaming 置真（turn_start），beginVoice 也必须复查并丢弃会话
    // （cancel）、留在 idle，不在生成中录音 —— 否则 120 秒自动停止后识别文本只插
    // 草稿、自动发送被静默跳过且无提示。原仓库外 repro 断言的是缺陷行为，这里反转。
    let resolveStart!: () => void;
    const onVoiceStart = vi.fn<NonNullable<ComposerProps["onVoiceStart"]>>(
      () =>
        new Promise<void>((res) => {
          resolveStart = res;
        }),
    );
    const onVoiceStop = vi.fn<NonNullable<ComposerProps["onVoiceStop"]>>(
      async () => "你好",
    );
    const onVoiceCancel = vi.fn<NonNullable<ComposerProps["onVoiceCancel"]>>(
      async () => {},
    );
    const onSend = vi.fn<ComposerProps["onSend"]>(async () => {});
    const props = {
      onSend,
      onStop: vi.fn(),
      onNormalize: vi.fn<ComposerProps["onNormalize"]>(async () => imageBlock),
      onVoiceStart,
      onVoiceStop,
      onVoiceCancel,
    };
    const { rerender } = render(<Composer streaming={false} {...props} />);

    // 1) 点麦克风：voice_start 挂起（真机上引擎初始化可达数秒）。
    fireEvent.click(screen.getByTestId("voice-button"));
    expect(onVoiceStart).toHaveBeenCalledTimes(1);

    // 2) 在途窗口里打字 + 回车：被 startingRef 挡住，发送不发生。
    fireEvent.change(textarea(), { target: { value: "先发这句" } });
    fireEvent.keyDown(textarea(), { key: "Enter" });
    await act(async () => {});
    expect(onSend).not.toHaveBeenCalled();

    // 3) 宿主把 streaming 置真（turn_start 落地）。
    rerender(<Composer streaming={true} {...props} />);

    // 4) voice_start 现在才 resolve：复查发现生成已开始 ⇒ cancel、留在 idle。
    await act(async () => {
      resolveStart();
    });
    expect(onVoiceCancel).toHaveBeenCalledTimes(1);
    expect(onVoiceStop).not.toHaveBeenCalled();
    const btn = screen.getByTestId("voice-button");
    expect(btn).not.toHaveAttribute("data-recording", "true");
    expect(btn).toHaveTextContent("语音");
    // 草稿原文留在输入区，没有被清空也没有被发送。
    expect(textarea()).toHaveValue("先发这句");
    expect(onSend).not.toHaveBeenCalled();
  });
});

describe("Composer 语音计时", () => {
  it("counts recording seconds on the stop button", async () => {
    vi.useFakeTimers();
    try {
      setup();
      // fake timers 下 `findByRole` 的轮询也会被冻住，所以这里手动推进并让 promise 落地。
      fireEvent.click(screen.getByRole("button", { name: "语音" }));
      await act(async () => {});
      expect(
        screen.getByRole("button", { name: "停止 0:00" }),
      ).toBeInTheDocument();
      act(() => {
        vi.advanceTimersByTime(65_000);
      });
      expect(
        screen.getByRole("button", { name: "停止 1:05" }),
      ).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("stops automatically at 120 seconds", async () => {
    vi.useFakeTimers();
    try {
      const p = setup();
      p.onVoiceStop.mockResolvedValue("到点了");
      fireEvent.click(screen.getByRole("button", { name: "语音" }));
      await act(async () => {});
      // 120 秒那一刻 effect 调 finishVoice；promise 落地后再断言一次。
      await act(async () => {
        vi.advanceTimersByTime(120_000);
      });
      await act(async () => {});
      expect(p.onVoiceStop).toHaveBeenCalledTimes(1);
      expect(p.onSend).toHaveBeenCalledTimes(1);
      expect(p.onSend.mock.calls[0][0]).toEqual([
        { type: "text", text: "到点了" },
      ]);
    } finally {
      vi.useRealTimers();
    }
  });
});
