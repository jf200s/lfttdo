// R134：本仓库只装了 `@testing-library/react`，`toBeInTheDocument` / `toHaveTextContent` /
// `toBeDisabled` 这些匹配器不是自带的。不接这条线，每条 UI 用例都会报「invalid matcher」，
// 症状像测试写错了，实际是接线缺失 —— 所以这一步必须在 `vite.config.ts` 的 `setupFiles` 里。
import "@testing-library/jest-dom/vitest";
