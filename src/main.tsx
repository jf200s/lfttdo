// 层叠纪律（R133 / 评审 F9）：令牌与预设必须在**任何**组件样式之前引入 —— Vite 的产物顺序
// 由 import 顺序决定，而 `styles.css` 里的 `:root` 只消费 `--*`、不声明，所以两组声明集合
// 不相交。这个先后关系由 `tokens.test.ts` 那条读 `main.tsx` 的用例守着，不是口头约定。
import "./theme/tokens.css";
import "./theme/presets.css";
import "./styles.css";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { seedFromCache } from "./theme/cache";

// T44（裁定 2 重写此段；旧版断言「主题只有一个来源」从这一刻起不再成立）：
// 首帧前同步读一次主题**缓存**。R169 当年删掉的是
// `applyTheme(document.documentElement, "noir", "dark", 15)` —— `Settings::default()`
// 的第 N 份硬编码拷贝；这里装的不是默认值，是**用户上一次会话的真值**。两者的区别就是
// 这条线存在的理由：拷贝会随 Rust 侧漂移而全绿，缓存不会说谎。
// 失败模式（裁定 2/缓存注释）：缓存缺失、坏 JSON、名单外的主题名、不可用的 mode ——
// `seedFromCache` 全部什么都不做，第一帧回到今天的老行为：`tokens.css` 的 `:root` 兜底
// （noir/dark 那一套值）。播种只走 `resolveTheme` + `applyTheme`（一份实现、两个入口），
// 所以这里不出现 `applyTheme(` 本身 —— `tokens.test.ts` 那条翻面后的 R169 守卫钉着这两点。
// 权威仍是 `App` 里那次 `settings_get`：它回来后**无条件覆盖**缓存与界面，
// 磁盘上的设置永远赢过首帧播种的那一份。
seedFromCache(document.documentElement);

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
