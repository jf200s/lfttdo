// 模型访问层的抽象：trait + 请求/增量/错误类型。
// 本模块树（`src-tauri/src/model/`）不得 `use tauri` —— Plan A 约束：只有
// `src-tauri/src/ipc.rs` 允许依赖 Tauri，核心逻辑必须能脱离 Tauri、脱离网络单测。
use crate::agent::types::Message;
use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

/// 模型流式响应被归一化后的最小增量集合。
/// 下游（Task 7 的 `AssistantAcc`、Task 10 的会话循环）只认这四种，不认协议细节。
/// `PartialEq` 比 brief 多一个：测试要能整体比对 delta 序列，而不必逐字段 match。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelDelta {
    Text(String),
    /// 首个分片：id 与 name 可能为 None（部分兼容端点只在中间分片给 name）
    ToolCallStart {
        index: u32,
        id: Option<String>,
        name: Option<String>,
    },
    /// arguments 的字符串片段，必须按 index 归组后整体解析
    ToolCallArgs {
        index: u32,
        fragment: String,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    /// 必须真的进请求体：未声明的工具模型不可能调用（见 findings「探针设计缺口」）。
    pub tools: Vec<ToolDef>,
    pub temperature: Option<f32>,
}

/// 模型层的错误分类。**分类规则见 `openai_compat::classify_openai_error`（R28）**，
/// 四个变体的分工如下，面向的是「用户看到这一行能不能判断该去查什么」：
///
/// | 变体 | 何时用 | 日志示例（同一件事的两种失败） |
/// |---|---|---|
/// | `Transport` | 请求没送到端点：DNS 解析失败、TLS、连接被掐断、SSE 流中途断开 | `无法连接模型端点 https://x.test/v1: http error: dns error` |
/// | `Api` | **已连通**，端点自己回了非 2xx：401 key 不对、429 限流、模型名不存在、5xx | `模型返回错误: 401 Unauthorized invalid_api_key: Incorrect API key provided` |
/// | `Decode` | HTTP 层之外的编解码问题：响应体不是合法 JSON、请求侧构造失败 | `协议数据转换失败: 响应体无法解析: …` |
/// | `NotConfigured` | `base_url` 空，压根没建出客户端 | `base_url 或 api_key 未配置` |
///
/// 「401 报成连不上」是这一层最贵的错（用户会去查网络），所以 `Transport` 与 `Api`
/// 的边界必须由上游真实变体决定，不能靠约定俗成的 `else`。
#[derive(Error, Debug)]
pub enum ModelError {
    /// 连接层面失败。`source` 保留上游错误（含 `reqwest::Error` 的 downcast 能力），
    /// `url` 是**规范化后我们真正请求的 base**，方便一眼看出是不是 base_url 形态问题。
    /// 这里绝不携带 `api_key`：`OpenAIError` 的任何 Display 分支都不含请求头。
    #[error("无法连接模型端点 {url}: {source}")]
    Transport {
        url: String,
        source: async_openai::error::OpenAIError,
    },
    /// 端点已经响应了。携带的是上游 `ApiErrorResponse` 的 Display，即
    /// `<状态码> <状态名> <type>: <error.message> …`（`async-openai` 的
    /// `error.rs:17-18` + `:132`），状态码与 `error.message` 原样透传，
    /// 这样「你的 key 不对」和「网不通」在两行日志里长得就不一样。
    #[error("模型返回错误: {0}")]
    Api(String),
    #[error("base_url 或 api_key 未配置")]
    NotConfigured,
    /// 前缀刻意不写「响应」：这条也承载**请求侧**构造失败（请求体没组装好、
    /// 本地文件读不出来），那种情况下没有任何响应可谈。方向写在携带文本里。
    #[error("协议数据转换失败: {0}")]
    Decode(String),
}

/// `stream` 返回 `Pin<Box<dyn Stream + Send>>` 而非 `async fn`：调用方只需
/// `stream.next().await`，且测试替身（Task 10 的 `ScriptedModel`）能直接返回
/// `stream::iter(..)`。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn stream<'a>(
        &'a self,
        req: ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>;
}
