// 装配根：只做模块声明与一行转发。
//
// Plan A 的边界要求核心层（`agent/`、`model/`、`permission/`、`settings`、`store/`、
// `tools/`）一概不依赖 Tauri，而 T13 的硬约束是「`src/` 下只有 `ipc.rs` 允许 `use tauri`」。
// 所以 `tauri::Builder`、`State`、`Manager`、`generate_handler!` 全部住在 `ipc::run()` 里，
// 本文件一个 tauri 导入都没有 —— `grep -n "use tauri" src/` 只应命中 `ipc.rs`。
// 下面那行 `cfg_attr(mobile, …)` 是模板给的入口属性，桌面构建里 `cfg(mobile)` 为假、
// 整条被剥掉，不会解析 `tauri` 路径。

pub mod agent;
pub mod image_prep;
pub mod ipc;
pub mod lock;
pub mod model;
pub mod permission;
pub mod settings;
pub mod store;
pub mod text;
pub mod tools;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    ipc::run()
}
