// 模型层与 Tauri 无关：本模块树下不得 `use tauri`（Plan A 约束，同 `agent/mod.rs`）。
// `provider` 是抽象（trait/delta/错误），`openai_compat` 是 Plan A 唯一的实现，
// `assistant_acc` 把 provider 的 delta 流还原成一条 assistant `Message`。
// `embed` 是长期记忆的嵌入抽象(T6):trait + OpenAI 兼容 `/embeddings` 端点实现。
pub mod assistant_acc;
pub mod embed;
pub mod openai_compat;
pub mod provider;
