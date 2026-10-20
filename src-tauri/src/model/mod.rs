// 模型层与 Tauri 无关：本模块树下不得 `use tauri`（Plan A 约束，同 `agent/mod.rs`）。
// `provider` 是抽象（trait/delta/错误），`openai_compat` 是 Plan A 唯一的实现。
pub mod openai_compat;
pub mod provider;
