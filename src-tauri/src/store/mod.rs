// 持久化子系统。本模块树下任何文件都不得 `use tauri`（Plan A 约束，见 `agent/mod.rs`）：
// SQLite 与「messages → 事件序列」的重放映射都是纯 Rust，能脱离 Tauri 单测。
// 数据库路径由调用方给，本模块不碰 Tauri 的 path API 去找 app data 目录。
pub mod db;
pub mod memory;
