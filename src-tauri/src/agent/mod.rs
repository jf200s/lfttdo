// agent 子系统的核心层：消息模型（types）、跨 IPC 边界的事件（events）、
// 上下文组装与滑窗（context）、一轮对话的编排循环（session）、
// 两次发送之间的摘要调用（summarize）、长期记忆的模型侧管线（memory,
// 存储在 store::memory,这里只做「模型说了算」的那几段）。
// 本模块树下任何文件都不得 `use tauri` —— Plan A 约束：只有 `src-tauri/src/ipc.rs`
// 允许依赖 Tauri，其余核心逻辑保持可脱离 Tauri 单测。
pub mod context;
pub mod events;
pub mod memory;
pub mod session;
pub mod summarize;
pub mod types;
