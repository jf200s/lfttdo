// mcp 模块：MCP server 的**配置读取层**与**工具命名层**，两个子模块都是不接调用点的叶子。
//
// `config.rs` 把 `mcp.json` 读成一份 server 列表：不建连接、不 spawn 子进程、不接任何调用点。
// `naming.rs` 把 (server, tool) 拼成兼容端点能接受的工具名：纯字符串算术，同样不接任何调用点——
// 它不读配置、不碰 registry，第一个消费者是桥。
// 真正的 stdio/HTTP 桥是 T31/T36 的事；管理界面（唯一的写入者）是 T35 的事。

pub mod config;
pub mod naming;
