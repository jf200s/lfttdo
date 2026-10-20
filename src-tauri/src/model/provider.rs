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

#[derive(Error, Debug)]
pub enum ModelError {
    #[error("无法连接模型端点 {url}: {source}")]
    Transport {
        url: String,
        source: async_openai::error::OpenAIError,
    },
    #[error("模型返回错误: {0}")]
    Api(String),
    #[error("base_url 或 api_key 未配置")]
    NotConfigured,
    #[error("响应无法解析: {0}")]
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
