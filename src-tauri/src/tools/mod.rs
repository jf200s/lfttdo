// 工具层与 Tauri 无关：本模块树下不得 `use tauri`（Plan A 约束，同 `agent/mod.rs`）。
pub mod echo;
pub mod registry;
