// R134：本仓库只装了 `@testing-library/react`，`toBeInTheDocument` / `toHaveTextContent` /
// `toBeDisabled` 这些匹配器不是自带的。不接这条线，每条 UI 用例都会报「invalid matcher」，
// 症状像测试写错了，实际是接线缺失 —— 所以这一步必须在 `vite.config.ts` 的 `setupFiles` 里。
import "@testing-library/jest-dom/vitest";

// 第 8 条红线（零弹窗）的**机器判据**，也是 T17/T19/T20 都别再碰 `prompt` 的那道绊线。
// 为什么不是「扫源码里的 `prompt(`」：本仓的注释就在讨论这几个函数名，扫源码会一边假阳
// 一边假阴（注释里的 `//` 会吃掉同一行后面的真调用）。换成行为判据：三个函数在测试环境里
// 一调用就抛，任何组件在任何用例里弹窗都会当场红 —— 而 jsdom 默认那三个只是 no-op，
// 什么都不喊，正好是「测试全绿但真机上 WebView2 返回 null、权限那一笔永远 pending」的形状。
for (const name of ["alert", "confirm", "prompt"] as const) {
  Object.defineProperty(window, name, {
    configurable: true,
    writable: true,
    value: () => {
      throw new Error(
        `验收判据「零弹窗」：界面里不许调用 window.${name}()，请改行内区域`,
      );
    },
  });
}
