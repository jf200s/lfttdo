import { useEffect, useRef } from "react";

/**
 * TextDelta 每秒可到达 50-100 次。Rust 侧已合到 30ms（`ipc.rs` 的 `Coalescer::MAX_WAIT`），
 * 这里再合到一帧一次，使 React 渲染频率 ≤ 帧率，长回答不掉帧。
 *
 * 三条不属于实现细节的约定：
 * 1. 回调**只走 ref**（`cb.current = onFlush`）：`App` 传进来的闭包每帧都是新的，
 *    而这里绝不能因为父组件重渲染就重新调度或丢缓冲。
 * 2. `flushNow()` 在非文本事件（工具卡、`done`）之前必调：否则那一帧的文本会落在
 *    工具卡**后面**渲染，时间线倒序（Rust 侧 `ipc.rs` 的 `other => { flush(); send(other) }`
 *    只管到 IPC 出口，管不到这里的帧边界）。
 * 3. 卸载时清空缓冲而不是冲刷：回调要写的组件已经不在了。
 */
export function useTextBuffer(onFlush: (chunk: string) => void) {
  const buf = useRef("");
  const marker = useRef<string | null>(null);
  const queued = useRef(false);
  const cb = useRef(onFlush);
  cb.current = onFlush;

  useEffect(
    () => () => {
      buf.current = "";
      queued.current = false;
    },
    [],
  );

  const schedule = () => {
    if (queued.current) return;
    queued.current = true;
    requestAnimationFrame(() => {
      queued.current = false;
      const t = buf.current;
      buf.current = "";
      if (t) cb.current(t);
    });
  };

  return {
    push(chunk: string) {
      buf.current += chunk;
      schedule();
    },
    /** 工具卡片、done 等非文本事件前必须先冲刷，否则时间线会倒序 */
    flushNow() {
      const t = buf.current;
      buf.current = "";
      queued.current = false;
      if (t) cb.current(t);
    },
    marker(m: string) {
      marker.current = m;
    },
    pendingMarker: () => marker.current,
  };
}
