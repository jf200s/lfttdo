//! OpenAI 兼容端点的流式适配器：URL 规范化、`Message` → 协议映射、请求组装、
//! chunk → `ModelDelta` 翻译四件事都在这个文件里（Task 6 的文件清单如此）。
//!
//! 这里不出现 `tauri`：模型层必须能脱离 Tauri、脱离网络单测（Plan A 约束）。
use super::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider, ToolDef};
use crate::agent::types::{ContentBlock, ImageMime, Message, Role, ToolOutput};
use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartImageArgs,
        ChatCompletionRequestMessageContentPartTextArgs, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionRequestUserMessageContentPart, ChatCompletionStreamOptions,
        ChatCompletionTool, ChatCompletionTools, CreateChatCompletionRequest,
        CreateChatCompletionRequestArgs, CreateChatCompletionStreamResponse, FunctionCall,
        FunctionObject,
    },
};
use futures::StreamExt;

/// 幂等地整理成 `<origin><path>/v1`，无尾斜杠。规则按顺序：
/// 1. trim；纯空白输入原样返回空串（`OpenAiCompat::new` 会先用 `NotConfigured` 拦下它）。
/// 2. 截掉第一个 `?` 或 `#` 之后的内容——查询串/fragment 不属于 base_url 的形态，
///    留着会拼出 `…?x=1/v1` 这种非法 URL。
/// 3. 去掉全部尾斜杠。
/// 4. 反复折叠尾部的 `/v1/v1`（复制粘贴常见），直到尾段是且仅是 `v1`；否则补一个 `/v1`。
/// 大小写只用于**判定**（`v1`/`V1` 都算已经带版本段），不改写用户的路径大小写——
/// URL 路径段区分大小写，改写会让一个本来能用的 base 变成 404。
pub fn normalize_base_url(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return s;
    }
    if let Some(cut) = s.find(['?', '#']) {
        s.truncate(cut);
    }
    while s.ends_with('/') {
        s.pop();
    }
    loop {
        let lower = s.to_lowercase();
        if lower.ends_with("/v1/v1") {
            // 能进这个分支说明尾部三个字节就是 ASCII 的 "/v1"，truncate 必落在字符边界上。
            s.truncate(s.len() - 3);
            continue;
        }
        if !lower.ends_with("/v1") {
            s.push_str("/v1");
        }
        break;
    }
    s
}

/// 一次模型调用的配置。
/// **没有 `#[derive(Debug)]`**：derive 出来的 Debug 会把明文 `api_key` 打进任何 `{:?}`、
/// `dbg!` 或 panic 回溯，违反 Plan A 约束「api_key 不进导出、不写日志」。
/// 下面手写的 `Debug` 只报「有没有配」，不报内容。
#[derive(Clone)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_output_tokens: u32,
}

impl std::fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field(
                "api_key",
                if self.api_key.is_empty() {
                    &"<未配置>"
                } else {
                    &"<已配置·masked>"
                },
            )
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

pub struct OpenAiCompat {
    client: Client<OpenAIConfig>,
    cfg: ModelConfig,
}

impl OpenAiCompat {
    /// 只校验 `base_url`：`api_key` 允许为空，本机 ollama 之类端点就没有 key
    /// （见测试 `keeps_localhost_ollama_without_key_but_still_normalizes`）。
    pub fn new(cfg: ModelConfig) -> Result<Self, ModelError> {
        if cfg.base_url.trim().is_empty() {
            return Err(ModelError::NotConfigured);
        }
        let base = normalize_base_url(&cfg.base_url);
        let oc = OpenAIConfig::new()
            .with_api_base(base)
            .with_api_key(cfg.api_key.clone());
        Ok(Self {
            client: Client::with_config(oc),
            cfg,
        })
    }

    /// 规范化是幂等的，所以配置里已经存了规范化结果的（Task 12 的 `to_model_config`）
    /// 再走一次也不会变形。
    pub fn base_url_resolved(&self) -> String {
        normalize_base_url(&self.cfg.base_url)
    }

    /// `Message` → 协议消息。一个 `Message` 可能展开成**多条**协议消息：
    /// Task 5 的 `Message::tool(..)` 用 `Role::User` 承载 `ToolResult`，而协议的
    /// `role: "tool"` 必须独立成条，所以这里把 ToolResult 块拆出去先出 tool 消息，
    /// 同一 Message 里的 text/image 再合成一条 user 消息。
    fn to_api(messages: &[Message]) -> Result<Vec<ChatCompletionRequestMessage>, ModelError> {
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            match m.role {
                Role::System => {
                    out.push(ChatCompletionRequestMessage::System(
                        ChatCompletionRequestSystemMessageArgs::default()
                            .content(text_of(m))
                            .build()
                            .map_err(decode_err)?,
                    ));
                }
                Role::User => {
                    let mut parts: Vec<ChatCompletionRequestUserMessageContentPart> = Vec::new();
                    for b in &m.blocks {
                        match b {
                            ContentBlock::Text { text } => parts.push(
                                ChatCompletionRequestMessageContentPartTextArgs::default()
                                    .text(text)
                                    .build()
                                    .map_err(decode_err)?
                                    .into(),
                            ),
                            ContentBlock::Image { data, mime } => parts.push(
                                ChatCompletionRequestMessageContentPartImageArgs::default()
                                    .image_url(format!("data:{};base64,{}", mime_url(mime), data))
                                    .build()
                                    .map_err(decode_err)?
                                    .into(),
                            ),
                            ContentBlock::ToolResult {
                                call_id,
                                output,
                                is_error,
                            } => {
                                out.push(tool_msg(call_id, output, *is_error)?);
                            }
                            // 用户轮携带 ToolCall 不是协议形态（那是 assistant 的），
                            // 类型层允许、协议层没有对应位置：显式丢弃，不静默塞进 content。
                            ContentBlock::ToolCall { .. } => {}
                        }
                    }
                    if !parts.is_empty() {
                        out.push(ChatCompletionRequestMessage::User(
                            ChatCompletionRequestUserMessageArgs::default()
                                .content(parts)
                                .build()
                                .map_err(decode_err)?,
                        ));
                    }
                }
                Role::Assistant => {
                    out.push(ChatCompletionRequestMessage::Assistant(assistant_msg(m)?))
                }
            }
        }
        Ok(out)
    }

    /// 单独成函数（不埋在 `stream` 里）是为了让它能被**离线**断言：
    /// `stream_options.include_usage` 这类「不发给真端点就永远发现不了的零值」
    /// 必须能在没有 key 的 CI 里被钉住（R24）。
    fn build_request(&self, req: &ChatRequest) -> Result<CreateChatCompletionRequest, ModelError> {
        let mut b = CreateChatCompletionRequestArgs::default();
        b.model(self.cfg.model.clone())
            .messages(Self::to_api(&req.messages)?)
            .stream(true)
            // OpenAI 兼容流式里，usage 分片只在请求要求时才发；不设置的话
            // `chunk.usage` 恒为 None → `ModelDelta::Usage` 永不触发 →
            // Task 8 的 token 预算与 Task 16 的用量显示会静默读到 0。
            .stream_options(ChatCompletionStreamOptions {
                include_usage: Some(true),
                // 不主动开混淆填充（它是 Azure 侧的字段，严格端点可能对未知字段更挑剔）；
                // 字段本身 `skip_serializing_if = "Option::is_none"`，留 None 即不上线。
                include_obfuscation: None,
            })
            .max_tokens(self.cfg.max_output_tokens);
        if let Some(t) = req.temperature {
            b.temperature(t);
        }
        // 未声明的工具模型不可能调用（findings「探针设计缺口」），所以 tools 必须真的进请求体。
        // 空列表时整个字段不上线（`Option<Vec<_>>` + skip_serializing_if），免得严格端点
        // 对 `tools: []` 有额外意见。
        if !req.tools.is_empty() {
            b.tools(req.tools.iter().map(to_api_tool).collect::<Vec<_>>());
        }
        b.build().map_err(decode_err)
    }
}

/// `ToolDef` → 协议的 function 工具声明。
fn to_api_tool(t: &ToolDef) -> ChatCompletionTools {
    ChatCompletionTools::Function(ChatCompletionTool {
        function: FunctionObject {
            name: t.name.clone(),
            description: (!t.description.is_empty()).then(|| t.description.clone()),
            parameters: Some(t.input_schema.clone()),
            strict: None,
        },
    })
}

/// assistant 轮映射：`ContentBlock::ToolCall` → 协议 `tool_calls` 数组
/// （`{"type":"function","id":..,"function":{"name":..,"arguments":"<JSON 字符串>"}}`），
/// `ContentBlock::Text` → content。
/// 协议要 `arguments` 是**字符串**，所以这里对 `Value` 做 `serde_json::to_string`。
/// 只有 tool_calls、没有文本时不设 content（协议规定 content 在 tool_calls 存在时可省）。
fn assistant_msg(m: &Message) -> Result<ChatCompletionRequestAssistantMessage, ModelError> {
    let text = text_of(m);
    let mut tool_calls: Vec<ChatCompletionMessageToolCalls> = Vec::new();
    for (id, name, input) in m.tool_calls() {
        let arguments =
            serde_json::to_string(&input).map_err(|e| ModelError::Decode(e.to_string()))?;
        tool_calls.push(
            ChatCompletionMessageToolCall {
                id,
                function: FunctionCall { name, arguments },
            }
            .into(),
        );
    }
    let mut b = ChatCompletionRequestAssistantMessageArgs::default();
    if !text.is_empty() {
        b.content(text);
    }
    if !tool_calls.is_empty() {
        b.tool_calls(tool_calls);
    }
    b.build().map_err(decode_err)
}

/// `ToolResult` → 协议 `role: "tool"` 消息。
/// 两点协议约束决定了这里的取舍：
/// - tool 消息的内容只能是 text（`ChatCompletionRequestToolMessageContentPart`
///   只有 `Text` 一个变体），所以图片型工具输出**不能**塞 base64；这里只留一行说明，
///   真要送模型看的图必须由上层作为 user 轮的 `ContentBlock::Image` 送出（Task 19/20）。
/// - 协议没有 error 位，失败语义只能写进内容前缀（`ToolResult.is_error`）。
fn tool_msg(
    call_id: &str,
    output: &ToolOutput,
    is_error: bool,
) -> Result<ChatCompletionRequestMessage, ModelError> {
    let body = match output {
        ToolOutput::Text { text } => text.clone(),
        ToolOutput::Image { mime, .. } => format!(
            "[工具返回了一张 {} 图片，未随 tool 消息发送：该角色只支持文本]",
            mime_url(mime)
        ),
    };
    let content = if is_error {
        format!("[tool error] {body}")
    } else {
        body
    };
    Ok(ChatCompletionRequestMessage::Tool(
        ChatCompletionRequestToolMessageArgs::default()
            .content(content)
            .tool_call_id(call_id)
            .build()
            .map_err(decode_err)?,
    ))
}

fn mime_url(m: &ImageMime) -> &'static str {
    match m {
        ImageMime::Png => "image/png",
        ImageMime::Jpeg => "image/jpeg",
    }
}

fn text_of(m: &Message) -> String {
    m.blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("")
}

fn decode_err(e: OpenAIError) -> ModelError {
    ModelError::Decode(e.to_string())
}

/// 流里回来的错误分两类：端点自己报错，和响应解不出来。
/// 后者必须进 `Decode`，否则 Task 10 会把「分片缺 index」这类解析问题显示成
/// 「模型返回错误」，排查方向被带偏（findings 风险 1 判据 2）。
fn stream_err(e: &OpenAIError) -> ModelError {
    match e {
        OpenAIError::JSONDeserialize(inner, _) => ModelError::Decode(inner.to_string()),
        other => ModelError::Api(other.to_string()),
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompat {
    fn stream<'a>(
        &'a self,
        req: ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
    {
        let request = match self.build_request(&req) {
            Ok(r) => r,
            Err(e) => return Box::pin(futures::stream::once(async move { Err(e) })),
        };
        let url = self.base_url_resolved();
        let client = self.client.clone();
        Box::pin(async_stream::stream! {
            match client.chat().create_stream(request).await {
                Err(e) => yield Err(ModelError::Transport { url, source: e }),
                Ok(mut s) => while let Some(item) = s.next().await {
                    match item {
                        Err(e) => yield Err(stream_err(&e)),
                        Ok(chunk) => for d in chunk_to_deltas(&chunk) { yield Ok(d); }
                    }
                },
            }
        })
    }
}

/// 一个 SSE chunk → 0..n 个 `ModelDelta`。顺序：usage 先于文本与工具（usage 只在
/// 最后一个分片出现，且那一轮 `choices` 通常是空数组）。
fn chunk_to_deltas(c: &CreateChatCompletionStreamResponse) -> Vec<ModelDelta> {
    let mut v = Vec::new();
    if let Some(u) = &c.usage {
        v.push(ModelDelta::Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });
    }
    for ch in &c.choices {
        if let Some(t) = &ch.delta.content {
            if !t.is_empty() {
                v.push(ModelDelta::Text(t.clone()));
            }
        }
        if let Some(tcs) = &ch.delta.tool_calls {
            for f in tcs {
                // R21：0.42.2 的 `index` 是非可选 `u32`，且没有 `#[serde(default)]`
                // （chat_.rs:1301-1311）。分片不带 index 时**整个 chunk** 反序列化失败，
                // 走的是 `stream_err` → `Decode`，这里的 `f.index` 不可能"缺省成 0"。
                let index = f.index;
                let name = f.function.as_ref().and_then(|x| x.name.clone());
                let args = f.function.as_ref().and_then(|x| x.arguments.clone());
                if name.is_some() || f.id.is_some() {
                    v.push(ModelDelta::ToolCallStart {
                        index,
                        id: f.id.clone(),
                        name,
                    });
                }
                if let Some(a) = args {
                    v.push(ModelDelta::ToolCallArgs { index, fragment: a });
                }
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::provider::{ChatRequest, ModelError, ModelProvider, ToolDef};
    use async_openai::types::chat::CreateChatCompletionStreamResponse;
    use futures::StreamExt;
    use serde_json::json;

    fn cfg() -> ModelConfig {
        ModelConfig {
            base_url: "https://x.com".into(),
            model: "qwen-test".into(),
            api_key: "sk-secret-123".into(),
            max_output_tokens: 512,
        }
    }

    fn provider() -> OpenAiCompat {
        OpenAiCompat::new(cfg()).unwrap()
    }

    fn one_text() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::system_note("只说一句")],
            tools: vec![],
            temperature: None,
        }
    }

    // ---- normalize_base_url ----

    #[test]
    fn strips_trailing_slash() {
        assert_eq!(normalize_base_url("https://x.com/v1/"), "https://x.com/v1");
    }

    #[test]
    fn adds_v1_when_missing() {
        assert_eq!(normalize_base_url("https://x.com"), "https://x.com/v1");
    }

    #[test]
    fn does_not_duplicate_v1() {
        assert_eq!(normalize_base_url("https://x.com/v1"), "https://x.com/v1");
    }

    #[test]
    fn tolerates_double_v1_from_user_copy_paste() {
        assert_eq!(
            normalize_base_url("https://x.com/v1/v1"),
            "https://x.com/v1"
        );
    }

    /// R23：原测试名 `does_not_add_v1_when_path_already_ends_with_endpoint` 与它自己的
    /// 断言相反（这里 `/v1` 确实会被补上）。改名以描述它真正证明的行为。
    #[test]
    fn appends_v1_to_an_arbitrary_path() {
        assert_eq!(
            normalize_base_url("https://x.com/api/openai"),
            "https://x.com/api/openai/v1"
        );
    }

    #[test]
    fn keeps_localhost_ollama_without_key_but_still_normalizes() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1"
        );
    }

    /// brief 未定义的输入：按契约「没有 origin 就谈不上 `<origin><path>/v1`」定为原样空串；
    /// `OpenAiCompat::new` 在此之前已经用 `NotConfigured` 拦下空 base_url。
    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(normalize_base_url(""), "");
        assert_eq!(normalize_base_url("   "), "");
    }

    /// 查询串与 fragment 不属于 base_url 的形态（留着会拼出 `…?x=1/v1`），先截断再规范化。
    #[test]
    fn drops_query_and_fragment_before_appending_v1() {
        assert_eq!(
            normalize_base_url("https://x.com/v1?api-version=2"),
            "https://x.com/v1"
        );
        assert_eq!(
            normalize_base_url("https://x.com/compatible-mode#frag"),
            "https://x.com/compatible-mode/v1"
        );
    }

    /// 判定只看「尾段是不是 v1」，不做前缀匹配：`/v1beta` 是另一个路径段，照规则补 `/v1`。
    #[test]
    fn treats_v1beta_as_an_ordinary_path_segment() {
        assert_eq!(
            normalize_base_url("https://x.com/v1beta"),
            "https://x.com/v1beta/v1"
        );
    }

    /// 大小写只用于判定，不改写用户原样路径（URL 路径段区分大小写）。
    #[test]
    fn matches_v1_case_insensitively_and_keeps_original_case() {
        assert_eq!(normalize_base_url("https://x.com/V1"), "https://x.com/V1");
        assert_eq!(
            normalize_base_url("https://x.com/API/OpenAI"),
            "https://x.com/API/OpenAI/v1"
        );
    }

    /// 契约里的「幂等」= f(f(x)) == f(x)，对全部样本钉住（含 `/v1/v1/v1` 这种极端形态：
    /// 单次折叠做不到，所以实现里用的是循环）。
    #[test]
    fn is_idempotent_for_every_fixture_input() {
        let fixtures = [
            "",
            "https://x.com",
            "https://x.com/",
            "https://x.com///",
            "https://x.com/v1",
            "https://x.com/v1/",
            "https://x.com/v1/v1",
            "https://x.com/v1/v1/v1",
            "https://x.com/api/openai",
            "https://x.com/v1beta",
            "https://x.com/V1",
            "https://x.com/v1?api-version=2",
            "http://localhost:11434/v1",
        ];
        for raw in fixtures {
            let once = normalize_base_url(raw);
            assert_eq!(
                normalize_base_url(&once),
                once,
                "normalize_base_url 对 {raw:?} 不幂等（第一次得到 {once:?}）"
            );
        }
    }

    // ---- R25：Debug 不得带出 api_key ----

    #[test]
    fn model_config_debug_masks_the_api_key() {
        let rendered = format!("{:?}", cfg());
        assert!(
            !rendered.contains("sk-secret-123"),
            "Debug 里绝不能出现完整 api_key：{rendered}"
        );
        assert!(
            !rendered.contains("secret-123"),
            "Debug 里不能出现 api_key 的任何连续片段：{rendered}"
        );
        assert!(
            rendered.contains("<已配置·masked>"),
            "Debug 必须标注该字段已被掩码：{rendered}"
        );
        // 非敏感字段照常可见，否则排查时这个 Debug 没用。
        assert!(rendered.contains("qwen-test") && rendered.contains("https://x.com"));
        assert!(
            !format!(
                "{:?}",
                ModelConfig {
                    api_key: String::new(),
                    ..cfg()
                }
            )
            .contains("sk-secret"),
        );
        assert!(
            format!(
                "{:?}",
                ModelConfig {
                    api_key: String::new(),
                    ..cfg()
                }
            )
            .contains("<未配置>")
        );
    }

    #[test]
    fn new_rejects_blank_base_url_but_not_blank_api_key() {
        assert!(matches!(
            OpenAiCompat::new(ModelConfig {
                base_url: "   ".into(),
                ..cfg()
            }),
            Err(ModelError::NotConfigured)
        ));
        // 本机 ollama 之类端点没有 key：空 api_key 必须还能建出 provider。
        let p = OpenAiCompat::new(ModelConfig {
            base_url: "http://localhost:11434".into(),
            api_key: String::new(),
            ..cfg()
        })
        .unwrap();
        assert_eq!(p.base_url_resolved(), "http://localhost:11434/v1");
    }

    // ---- R24：请求体必须带上 stream_options.include_usage ----

    #[test]
    fn request_asks_for_usage_and_streams() {
        let v = serde_json::to_value(provider().build_request(&one_text()).unwrap()).unwrap();
        assert_eq!(v["stream"], json!(true), "不发 stream 就没有流式");
        assert_eq!(
            v["stream_options"]["include_usage"],
            json!(true),
            "不带 include_usage 时 usage 分片永不出现，ModelDelta::Usage 静默为零"
        );
        // include_obfuscation 是 Azure 侧字段，必须留在线上缺席（严格端点对未知字段更挑剔）。
        assert!(v["stream_options"].get("include_obfuscation").is_none());
        assert_eq!(v["model"], json!("qwen-test"));
        assert_eq!(v["max_tokens"], json!(512));
    }

    #[test]
    fn request_declares_tools_and_temperature() {
        let req = ChatRequest {
            messages: vec![Message::system_note("用工具")],
            tools: vec![ToolDef {
                name: "fs.read".into(),
                description: "读文件".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            }],
            temperature: Some(0.5),
        };
        let v = serde_json::to_value(provider().build_request(&req).unwrap()).unwrap();
        // 摘掉 `.tools(..)` 这条断言就红：未声明的工具模型不可能调用。
        assert_eq!(v["tools"][0]["type"], json!("function"));
        assert_eq!(v["tools"][0]["function"]["name"], json!("fs.read"));
        assert_eq!(v["tools"][0]["function"]["description"], json!("读文件"));
        assert_eq!(
            v["tools"][0]["function"]["parameters"]["type"],
            json!("object")
        );
        assert_eq!(v["temperature"], json!(0.5));
        assert!(v["messages"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn empty_tools_list_is_omitted() {
        let v = serde_json::to_value(provider().build_request(&one_text()).unwrap()).unwrap();
        assert!(
            v.get("tools").is_none(),
            "没有工具时不该发 tools 字段（部分兼容端点对空 tools 更挑剔）"
        );
    }

    // ---- Message → 协议映射 ----

    #[test]
    fn assistant_tool_calls_map_to_the_protocol_tool_calls_array() {
        let m = Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "先看一眼".into(),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "fs.read".into(),
                    input: json!({"path": "a.txt"}),
                },
            ],
        };
        let api = OpenAiCompat::to_api(&[m]).unwrap();
        assert_eq!(
            serde_json::to_value(&api).unwrap(),
            json!([{
                "role": "assistant",
                "content": "先看一眼",
                "tool_calls": [{
                    "type": "function",
                    "id": "c1",
                    "function": {
                        "name": "fs.read",
                        // 协议要的是 JSON 字符串，不是对象
                        "arguments": "{\"path\":\"a.txt\"}"
                    }
                }]
            }])
        );
    }

    #[test]
    fn assistant_with_only_tool_calls_omits_content() {
        let m = Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "fs.read".into(),
                input: json!({}),
            }],
        };
        let v = serde_json::to_value(OpenAiCompat::to_api(&[m]).unwrap()).unwrap();
        assert!(v[0].get("content").is_none());
        assert_eq!(v[0]["tool_calls"][0]["function"]["arguments"], json!("{}"));
    }

    #[test]
    fn tool_result_is_split_out_of_the_user_message_into_role_tool() {
        // Task 5 的 Message::tool 用 Role::User 承载 ToolResult，这里必须拆成 role=tool。
        let m = Message::tool("c1", ToolOutput::text("内容正文"), false);
        let api = OpenAiCompat::to_api(&[m]).unwrap();
        assert_eq!(
            serde_json::to_value(&api).unwrap(),
            json!([{ "role": "tool", "tool_call_id": "c1", "content": "内容正文" }])
        );
    }

    #[test]
    fn is_error_is_carried_in_the_content_prefix() {
        // 协议里没有 error 位，`ToolResult.is_error` 只能这样表达。
        let m = Message::tool("c1", ToolOutput::text("文件不存在"), true);
        let v = serde_json::to_value(OpenAiCompat::to_api(&[m]).unwrap()).unwrap();
        assert_eq!(v[0]["content"], json!("[tool error] 文件不存在"));
    }

    #[test]
    fn mixed_user_message_emits_tool_messages_before_the_user_message() {
        let m = Message {
            role: Role::User,
            blocks: vec![
                ContentBlock::Text {
                    text: "请看这张图".into(),
                },
                ContentBlock::Image {
                    data: "iVBOR".into(),
                    mime: ImageMime::Png,
                },
                ContentBlock::ToolResult {
                    call_id: "c9".into(),
                    is_error: false,
                    output: ToolOutput::text("ok"),
                },
            ],
        };
        let v = serde_json::to_value(OpenAiCompat::to_api(&[m]).unwrap()).unwrap();
        assert_eq!(v[0]["role"], json!("tool"));
        assert_eq!(v[0]["tool_call_id"], json!("c9"));
        assert_eq!(v[1]["role"], json!("user"));
        assert_eq!(v[1]["content"][0]["type"], json!("text"));
        assert_eq!(v[1]["content"][1]["type"], json!("image_url"));
        assert_eq!(
            v[1]["content"][1]["image_url"]["url"],
            json!("data:image/png;base64,iVBOR")
        );
    }

    #[test]
    fn image_tool_output_never_puts_base64_into_a_tool_message() {
        // tool 角色只能是 text：图片必须留一行说明，不能静默丢、也不能塞 base64。
        let m = Message::tool(
            "c1",
            ToolOutput::Image {
                data: "iVBORw0KGgo=".into(),
                mime: ImageMime::Jpeg,
            },
            false,
        );
        let v = serde_json::to_value(OpenAiCompat::to_api(&[m]).unwrap()).unwrap();
        assert_eq!(v[0]["role"], json!("tool"));
        let text = v[0]["content"].as_str().unwrap();
        assert!(text.contains("image/jpeg"), "{text}");
        assert!(
            !text.contains("iVBORw0KGgo"),
            "tool 消息里不该出现图片字节：{text}"
        );
    }

    // ---- chunk → ModelDelta（R21 的 index 形态在这里被钉住）----

    fn chunk(json_text: &str) -> CreateChatCompletionStreamResponse {
        serde_json::from_str(json_text).unwrap()
    }

    #[test]
    fn text_and_usage_chunks_become_deltas() {
        let d = chunk_to_deltas(&chunk(
            r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m",
               "choices":[{"index":0,"delta":{"content":"你好"}}]}"#,
        ));
        assert_eq!(d, vec![ModelDelta::Text("你好".into())]);

        // include_usage 的收尾分片：choices 是空数组，只有 usage。
        let d = chunk_to_deltas(&chunk(
            r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m",
               "choices":[],"usage":{"prompt_tokens":11,"completion_tokens":5,"total_tokens":16}}"#,
        ));
        assert_eq!(
            d,
            vec![ModelDelta::Usage {
                input_tokens: 11,
                output_tokens: 5
            }]
        );
    }

    #[test]
    fn tool_call_fragments_carry_their_index_through() {
        // 首分片带 id+name，后续分片只有 arguments 片段：两种都要翻译成对应 delta。
        let first = chunk_to_deltas(&chunk(
            r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m",
               "choices":[{"index":0,"delta":{"tool_calls":[
                 {"index":0,"id":"call_1","type":"function",
                  "function":{"name":"fs.read","arguments":"{\"pa"}}]}}]}"#,
        ));
        assert_eq!(
            first,
            vec![
                ModelDelta::ToolCallStart {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("fs.read".into())
                },
                ModelDelta::ToolCallArgs {
                    index: 0,
                    fragment: "{\"pa".into()
                },
            ]
        );

        let later = chunk_to_deltas(&chunk(
            r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m",
               "choices":[{"index":0,"delta":{"tool_calls":[
                 {"index":2,"function":{"arguments":"th\":1}"}}]}}]}"#,
        ));
        assert_eq!(
            later,
            vec![ModelDelta::ToolCallArgs {
                index: 2,
                fragment: "th\":1}".into()
            }]
        );
    }

    #[test]
    fn empty_content_fragments_are_dropped() {
        // 心跳/role-only 分片不应该变成空 Text 干扰下游累加器。
        let d = chunk_to_deltas(&chunk(
            r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m",
               "choices":[{"index":0,"delta":{"content":"","role":"assistant"}}]}"#,
        ));
        assert!(d.is_empty(), "{d:?}");
    }

    /// R21 的解码路径风险：`index` 上没有 `#[serde(default)]`，所以**一个**不带 index 的
    /// tool_call 分片会让**整个 chunk** 反序列化失败（不是给个默认 0）。
    /// 真端点是否会这样，属 findings 未实测的第 3 问；这里只钉住类型层的既定事实。
    #[test]
    fn tool_call_fragment_without_index_fails_to_decode_the_whole_chunk() {
        let raw = r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"m",
                      "choices":[{"index":0,"delta":{"tool_calls":[
                        {"id":"call_1","type":"function","function":{"name":"fs.read"}}]}}]}"#;
        let err = serde_json::from_str::<CreateChatCompletionStreamResponse>(raw)
            .expect_err("缺 index 的分片在 0.42.2 上必须解不出来");
        assert!(
            err.to_string().contains("index"),
            "报错应指向缺失的 index，实际：{err}"
        );
    }

    /// `stream()` 不 panic、也不返回空流：连接层面的失败必须作为一个 `Transport` item
    /// 发出来，并带上已解析的 URL（Task 10 靠这条把错误投给前端）。
    /// 这里用无法解析的 base_url 走这条路 —— reqwest 在发出请求前就拒了，所以不需要网络。
    #[tokio::test]
    async fn stream_yields_transport_error_with_the_resolved_url() {
        let p = OpenAiCompat::new(ModelConfig {
            base_url: "not a url".into(),
            ..cfg()
        })
        .unwrap();
        let items: Vec<_> = p.stream(one_text()).collect().await;
        assert_eq!(items.len(), 1, "{items:?}");
        match &items[0] {
            Err(ModelError::Transport { url, source }) => {
                assert_eq!(url, "not a url/v1", "错误里要能看到我们真正请求的 URL");
                assert!(
                    !format!("{url} {source}").contains("sk-secret-123"),
                    "错误信息不得带出 api_key"
                );
            }
            other => panic!("期望 Transport 错误，实际 {other:?}"),
        }
    }

    // ---- Step 5：真端点回归（需要 key，不进通过数，ledger R5）----

    /// 一次运行同时给 findings 风险 1 的四个未实测问题提供原始行，外加 R24 的
    /// 「端点是否真的兑现 include_usage」。
    /// 补跑（`--nocapture` 是必须的，否则上面那些测量行会被测试捕获器吞掉）：
    /// `cd src-tauri && QWEN_API_KEY=<真 key> cargo test -- --ignored --nocapture stream_real_endpoint`
    #[tokio::test]
    #[ignore = "需要真实 QWEN_API_KEY；结论见 docs/findings/plan-a-risk-verification.md 风险 1"]
    async fn stream_real_endpoint() {
        use std::collections::BTreeMap;
        use std::time::Instant;

        let key = std::env::var("QWEN_API_KEY").expect("set QWEN_API_KEY");
        let base = std::env::var("QWEN_BASE_URL")
            .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
        let model = std::env::var("QWEN_MODEL").unwrap_or_else(|_| "qwen-plus".into());
        let provider = OpenAiCompat::new(ModelConfig {
            base_url: base,
            model,
            api_key: key,
            max_output_tokens: 512,
        })
        .unwrap();
        println!("RESOLVED_BASE={}", provider.base_url_resolved());

        let req = ChatRequest {
            messages: vec![
                Message::system_note("调用 get_time 工具，不要直接回答。"),
                Message::user(vec![ContentBlock::Text {
                    text: "现在几点".into(),
                }]),
            ],
            tools: vec![ToolDef {
                name: "get_time".into(),
                description: "获取当前本地时间，ISO 8601 格式；不需要任何参数。".into(),
                input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            }],
            temperature: None,
        };

        let mut s = provider.stream(req);
        // 口径注意：本实现的 HTTP 请求要到第一次 poll 才发出，所以 first_delta_in 含
        // DNS/TLS/首字节，**不可**与探针的 `first_chunk_in`（起点在 create_stream 返回后）直接比。
        let t0 = Instant::now();
        let mut deltas = 0usize;
        let mut text_deltas = 0usize;
        let mut first_delta_in: Option<std::time::Duration> = None;
        let mut starts: Vec<(u32, Option<String>, Option<String>)> = Vec::new();
        let mut args_by_index: BTreeMap<u32, String> = BTreeMap::new();
        let mut usage: Option<(u32, u32)> = None;
        let mut errors: Vec<String> = Vec::new();

        while let Some(item) = s.next().await {
            if first_delta_in.is_none() {
                first_delta_in = Some(t0.elapsed());
            }
            match item {
                Ok(ModelDelta::Text(t)) => {
                    deltas += 1;
                    text_deltas += 1;
                    println!("TEXT {t:?}");
                }
                Ok(ModelDelta::ToolCallStart { index, id, name }) => {
                    deltas += 1;
                    starts.push((index, id.clone(), name.clone()));
                    println!("TOOLSTART index={index} id={id:?} name={name:?}");
                }
                Ok(ModelDelta::ToolCallArgs { index, fragment }) => {
                    deltas += 1;
                    println!("TOOLARGS index={index} fragment={fragment:?}");
                    args_by_index
                        .entry(index)
                        .and_modify(|acc| acc.push_str(&fragment))
                        .or_insert_with(|| fragment.clone());
                }
                Ok(ModelDelta::Usage {
                    input_tokens,
                    output_tokens,
                }) => {
                    deltas += 1;
                    usage = Some((input_tokens, output_tokens));
                }
                Err(e) => {
                    // 缺 index 的分片会以 `Decode` 的形态出现在这里（R21 的解码路径风险）。
                    errors.push(format!("{e}"));
                }
            }
        }

        for (index, joined) in &args_by_index {
            match serde_json::from_str::<serde_json::Value>(joined) {
                Ok(v) => println!("ARGUMENTS_REASSEMBLED index={index} ok={v}"),
                Err(e) => println!("ARGUMENTS_REASSEMBLED index={index} BROKEN={e} raw={joined:?}"),
            }
        }
        println!(
            "deltas={deltas} text_deltas={text_deltas} first_delta_in={first_delta_in:?} \
             tool_starts={} distinct_indexes={:?} usage={usage:?}",
            starts.len(),
            args_by_index.keys().copied().collect::<Vec<u32>>(),
        );
        println!(
            "usage_observed={} （include_usage 是否被端点兑现；None=没收到 usage 分片）",
            usage.is_some()
        );
        println!(
            "注意：chunk 数无法从 ModelDelta 观测（一个 chunk 可能出多个 delta），\
             要 chunk 数请用 examples/stream_probe.rs。"
        );
        for e in &errors {
            println!("STREAM_ERROR {e}");
        }

        assert!(
            deltas > 1,
            "流式只回来 1 个 delta 说明响应被缓冲（spec 风险 #4），deltas={deltas} errors={errors:?}"
        );
    }
}
