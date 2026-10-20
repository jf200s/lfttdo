// 权限层与 Tauri 无关：本模块树下不得 `use tauri`（Plan A 约束，同 `agent/mod.rs`）。
// 咨询人的唯一入口是 `gate::PermissionGate`，工具真实风险的唯一来源是
// `tools::registry::ToolRegistry::risk_of`（R46：`check` 信任调用方传进来的 `risk`，
// 而那个字段只允许由装配点从注册表查表填入）。
pub mod gate;
