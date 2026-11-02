import { describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useTextBuffer } from "./batching";

describe("useTextBuffer", () => {
  it("does not flush more than once per animation frame", () => {
    const onFlush = vi.fn();
    let rafCbs: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      rafCbs.push(cb); return rafCbs.length;
    });
    const { result } = renderHook(() => useTextBuffer(onFlush));

    act(() => { result.current.push("a"); result.current.push("b"); result.current.push("c"); });
    expect(onFlush).not.toHaveBeenCalled();

    act(() => { rafCbs.shift()!(0); });
    expect(onFlush).toHaveBeenCalledTimes(1);
    expect(onFlush).toHaveBeenCalledWith("abc");
    vi.unstubAllGlobals();
  });

  it("flushes immediately for non-text content so timeline stays ordered", () => {
    const onFlush = vi.fn();
    let rafCbs: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => { rafCbs.push(cb); return 1; });
    const { result } = renderHook(() => useTextBuffer(onFlush));

    act(() => { result.current.push("x"); });
    act(() => { result.current.flushNow(); });
    expect(onFlush).toHaveBeenNthCalledWith(1, "x");
    vi.unstubAllGlobals();
  });

  it("drops buffered text on unmount without throwing", () => {
    const onFlush = vi.fn();
    vi.stubGlobal("requestAnimationFrame", () => 1);
    const { result, unmount } = renderHook(() => useTextBuffer(onFlush));
    act(() => result.current.push("z"));
    expect(() => unmount()).not.toThrow();
    vi.unstubAllGlobals();
  });

  // 上面那条只挡住「卸载时抛错」。这里补的是**内容**那一半：卸载之后就算迟到的那一帧真被
  // 跑到了，也不许把半句话交给回调 —— 回调指向的是已经消失的组件（评审对 `not.toThrow()`
  // 这类零断言用例的要求：能红才算守卫）。
  it("never hands the callback text that was still buffered when the hook unmounted", () => {
    const onFlush = vi.fn();
    let rafCbs: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      rafCbs.push(cb); return rafCbs.length;
    });
    const { result, unmount } = renderHook(() => useTextBuffer(onFlush));

    act(() => { result.current.push("ghost"); });
    unmount();
    act(() => { while (rafCbs.length > 0) rafCbs.shift()!(0); });
    expect(onFlush).not.toHaveBeenCalled();
    vi.unstubAllGlobals();
  });

  // 一帧一次这件事必须是**持续性质**而不是只在那一批里成立：帧与帧之间继续喂 delta，
  // 每帧仍然只许出一段，且拼起来的字必须逐字等于原样（掉帧/重排是流式界面最难查的 bug）。
  it("keeps one flush per frame across frames and preserves the exact concatenation", () => {
    const out: string[] = [];
    let rafCbs: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      rafCbs.push(cb); return rafCbs.length;
    });
    const { result } = renderHook(() =>
      useTextBuffer((chunk) => out.push(chunk)),
    );

    const words = ["hel", "lo ", "wor", "ld", "!"];
    for (const w of words) {
      act(() => { result.current.push(w); });
      act(() => { const cb = rafCbs.shift(); if (cb) cb(0); });
    }
    expect(out.join("")).toBe("hello world!");
    expect(out.length).toBe(5);
    vi.unstubAllGlobals();
  });
});
