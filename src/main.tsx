// 层叠纪律（R133 / 评审 F9）：令牌与预设必须在**任何**组件样式之前引入 —— Vite 的产物顺序
// 由 import 顺序决定，而 `styles.css` 里的 `:root` 只消费 `--*`、不声明，所以两组声明集合
// 不相交。这个先后关系由 `tokens.test.ts` 那条读 `main.tsx` 的用例守着，不是口头约定。
import "./theme/tokens.css";
import "./theme/presets.css";
import "./styles.css";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

// R169：这里**不再有**启动期的 `applyTheme` 调用。以前那一行
// `applyTheme(document.documentElement, "noir", "dark", 15)` 是 `Settings::default()`
// （`settings.rs:265-267`）的第三份拷贝，`tokens.css` 的 `--font-size: 15px` 是第四份。
// 四处今天逐字相等 —— 这正是危险：Rust 侧把默认字号改成 14，第一帧照旧闪成 15px，
// 而 cargo test / vitest / tsc 全绿。主题现在只有**一个**来源：`App` 里那次 `settings_get`。
//
// 第一帧（HTML 已上屏、设置还没读回来）的底色由 `tokens.css` 的 `:root` 兜着 —— 那就是
// noir/dark 那一套值。没有把它「修掉」：往 `index.html` 写 `data-theme=noir` 不会改变
// 任何一个计算值（`:root` 与 noir/dark 那块逐字相同），只会多出第五份硬编码默认值。
// 残余风险：设置里存的是别的预设时，首帧到设置返回之间可能有一次配色跳变（肉眼可见与否
// 属未实测项 —— 见 T16 报告的 未实测 清单，`tauri dev` 不归本任务）。

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
