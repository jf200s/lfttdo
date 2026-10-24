// mcp 模块（T29）：`mcp.json` 的配置读取层。
//
// 本轮（T29）只有 `config.rs` 一个子模块：不建连接、不 spawn 子进程、不接任何调用点。
// 真正的 stdio/HTTP 桥是 T31/T36 的事；管理界面（唯一的写入者）是 T35 的事。

pub mod config;
pub mod naming;
