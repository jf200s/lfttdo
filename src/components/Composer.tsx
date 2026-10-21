import { useEffect, useState } from "react";

/**
 * 要求 6：发送按钮在 `state.streaming` 时切换成「停止」，点击调 `onStop()`（→ `interrupt()`）
 * 并**禁用自身，直到 `done` 才恢复**。
 *
 * 「不做乐观恢复」落在下面那道 `useEffect` 上：`stopRequested` 只跟着 `streaming` 落地，
 * 而 `streaming` 唯一的置假来源是 `done` 事件（R112）。`interrupt()` 那个 promise 完成时
 * 界面什么都不改 —— 令牌一次性（R63），取消成功和这一轮结束是两件事。
 */
export function Composer({
  streaming,
  onSend,
  onStop,
}: {
  streaming: boolean;
  onSend: (text: string) => void;
  onStop: () => void;
}) {
  const [text, setText] = useState("");
  const [stopRequested, setStopRequested] = useState(false);

  useEffect(() => {
    if (!streaming) setStopRequested(false);
  }, [streaming]);

  const stop = () => {
    setStopRequested(true);
    onStop();
  };

  return (
    <form
      className="composer"
      onSubmit={(e) => {
        e.preventDefault();
        const t = text.trim();
        if (t === "" || streaming) return;
        setText("");
        onSend(t);
      }}
    >
      <textarea
        aria-label="输入消息"
        className="composer-input"
        value={text}
        disabled={streaming}
        rows={3}
        placeholder="说点什么…"
        onChange={(e) => setText(e.target.value)}
      />
      {streaming ? (
        <button type="button" className="stop" disabled={stopRequested} onClick={stop}>
          停止
        </button>
      ) : (
        <button type="submit" className="send">
          发送
        </button>
      )}
    </form>
  );
}
