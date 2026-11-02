// 工具层与 Tauri 无关：本模块树下不得 `use tauri`（Plan A 约束，同 `agent/mod.rs`）。
pub mod edit_file;
pub mod echo;
pub mod fetch;
pub mod fetch_guard;
pub(crate) mod fs_write;
pub mod memory_search;
pub mod read_file;
pub mod registry;
// run_command（agent 的 shell 执行工具）：2026-11-01 先交付接口契约桩，
// spec 见 docs/superpowers/specs/2026-11-01-agent-shell-selection-design.md。
pub mod shell;
pub mod use_skill;
pub mod write_file;
