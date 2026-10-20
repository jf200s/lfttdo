// agent 子系统的类型层：消息模型（types）与跨 IPC 边界的事件（events）。
// 本模块树下任何文件都不得 `use tauri` —— Plan A 约束：只有 `src-tauri/src/ipc.rs`
// 允许依赖 Tauri，其余核心逻辑保持可脱离 Tauri 单测。
pub mod events;
pub mod types;
