// 令牌与预设必须在任何组件样式之前引入：Vite 的层叠顺序由 import 顺序决定，
// `App.css`（经 `App.tsx:4`）随后覆盖同名声明。不新建 `styles.css`（R120）。
import "./theme/tokens.css";
import "./theme/presets.css";
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { applyTheme } from "./theme/theme";

// 启动时先把属性写进 DOM，第一帧就不会带着「没有 data-theme」的默认色闪一下。
// 设置驱动的那次调用归 T16（它才有 `Settings.theme/mode/font_size` 这份数据）。
applyTheme(document.documentElement, "noir", "dark", 15);

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
