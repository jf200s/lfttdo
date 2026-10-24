// mcp 模块：MCP server 的**配置读取层**、**工具命名层**与 **stdio 桥**三个子模块。
//
// `config.rs` 把 `mcp.json` 读成一份 server 列表：不建连接、不 spawn 子进程、不接任何调用点。
// `naming.rs` 把 (server, tool) 拼成兼容端点能接受的工具名：纯字符串算术，同样不接任何调用点——
// 它不读配置、不碰 registry，第一个消费者是 T32 的注册路径（桥只把 server 原值存着，不改名）。
// `bridge.rs`（T31）是本目录里**第一个会真起进程**的一层：spawn 选型 + 并行连接 + 状态台账。
// 它的边界止于「握手 + 数工具 + 状态」——**不**注册工具（T32）、**不**接 `ipc.rs`（T35 的
// `mcp_connect` 才是第一个消费者）、**不**做进程树回收（T34 的 JobObject）。
// HTTP 传输（T36）与管理界面（唯一的写入者，T35）都还没进来。

pub mod bridge;
pub mod config;
pub mod naming;
