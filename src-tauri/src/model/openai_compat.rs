//! OpenAI 兼容端点的流式适配器：URL 规范化、`Message` → 协议映射、请求组装、
//! chunk → `ModelDelta` 翻译四件事都在这个文件里（Task 6 的文件清单如此）。
//!
//! 这里不出现 `tauri`：模型层必须能脱离 Tauri、脱离网络单测（Plan A 约束）。
//!
//! 两条与「用户看到什么」直接相关的规则都在这里落地，改动前先读它们：
//!
//! - **URL 形态**：`normalize_base_url`（R27）负责把用户从各家控制台粘贴的那一行
//!   收敛成 async-openai 能用的 base。控制台的惯例是给完整端点
//!   （dashscope：`https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions`），
//!   而客户端自己还要再拼一次 `/chat/completions`（`chat.rs:90`），所以端点段必须被摘掉。
//! - **错误方向**：`classify_openai_error`（R28）是上游错误进入 `ModelError` 的唯一入口。
//!   日志上必须一眼分得开这两件事：
//!   `模型返回错误: 401 Unauthorized invalid_api_key: Incorrect API key provided`
//!   （已连通，是 key/配额/模型名的问题，去查设置）
//!   vs `无法连接模型端点 https://…: http error: dns error`（没连通，去查网络/代理）。
//!   分错的代价是把「你的 key 不对」送去查网络。
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
        CreateChatCompletionRequestArgs, FunctionCall,
        FunctionObject,
    },
};
use futures::StreamExt;

/// 端点段后缀：它们属于**请求路径**，不属于 base_url。async-openai 自己在
/// `chat.rs:55`（非流式）与 `chat.rs:90`（流式）后面拼 `/chat/completions`，
/// 所以 base 里留着它就会被拼成 `…/chat/completions/v1/chat/completions` → 404。
const CHAT_COMPLETIONS_SUFFIX: &str = "/chat/completions";
const COMPLETIONS_SUFFIX: &str = "/completions";
const VERSION_SUFFIX: &str = "/v1";
/// 上面几条的字面拼接（`concat!` 只接受字面量，所以必须重抄一遍；
/// `endpoint_suffixes_are_spelled_as_expected` 这条测试负责钉住拼写一致）。
const COMPLETIONS_V1_SUFFIX: &str = "/completions/v1";
const V1_V1_SUFFIX: &str = "/v1/v1";

/// 若 `s` 以 ASCII 大小写不敏感的 `suffix` 结尾，返回**去掉它之后**的字节长度。
///
/// 为什么按字节比对而不是 `s.to_lowercase().ends_with(..)`：`str::to_lowercase`
/// 对少数码位会改变字节长度（U+212A KELVIN SIGN → 1 字节的 'k'），那样
/// `truncate(len - suffix.len())` 就不保证落在字符边界上。这里命中即意味着尾段
/// 每个字节都折叠成 ASCII 后缀对应字节（≥0x80 的字节永远折叠不出 ASCII），
/// 所以 cut 点必然是边界——大小写判定能力不变（`/V1`、`/CHAT/Completions` 照样命中）。
fn strip_len(s: &str, suffix: &str) -> Option<usize> {
    let sb = s.as_bytes();
    if sb.len() >= suffix.len()
        && sb[sb.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
    {
        Some(sb.len() - suffix.len())
    } else {
        None
    }
}

/// 幂等地整理成 `<origin><path>/v1`，无尾斜杠，且**不含端点段**。规则按顺序：
/// 1. trim；纯空白输入原样返回空串（`OpenAiCompat::new` 会先用 `NotConfigured` 拦下它）。
/// 2. 截掉第一个 `?` 或 `#` 之后的内容——查询串/fragment 不属于 base_url 的形态，
///    留着会拼出 `…?x=1/v1` 这种非法 URL。
/// 3. 循环（每步都严格变短，必然终止）：
///    a. 去全部尾斜杠；
///    b. 尾部是「端点段 + 版本段」的颠倒粘贴（`…/chat/completions/v1`、`…/completions/v1`）
///       → 只摘尾部 `/v1`，剩下的端点段交给 d 处理；
///    c. 尾部恰为 `/v1/v1`（复制粘贴常见，可多层，循环收敛）→ 摘掉一个 `/v1`；
///    d. 尾部是端点段 → 摘掉它。**`/chat/completions` 必须先于 `/completions` 判定**，
///       否则 `…/v1/chat/completions` 会被削成 `…/v1/chat` 再加 `/v1`（R27 的主形态）。
/// 4. 尾段不是 `/v1` 就补一个。
///
/// R27 的动机：用户粘贴的是各家控制台显示的那一行。dashscope 控制台给的就是
/// `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions`，按旧规则会得到
/// `…/v1/chat/completions/v1`，再被 `client.rs:674`（`post_stream("/chat/completions", …)`，
/// 见 `chat.rs:90`）拼成双端点 404 —— 而 404 在用户那儿长得和「模型报错」甚至「连不上」一样。
///
/// 大小写只用于**判定**（`v1`/`V1`、`Chat/Completions` 都算），不改写用户的路径大小写——
/// URL 路径段区分大小写，改写会让一个本来能用的 base 变成 404。
pub fn normalize_base_url(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return s;
    }
    if let Some(cut) = s.find(['?', '#']) {
        s.truncate(cut);
    }
    loop {
        while s.ends_with('/') {
            s.pop();
        }
        // b. 端点段在前、版本段在后是同一个粘贴事故的两种写法之一
        if strip_len(&s, COMPLETIONS_V1_SUFFIX).is_some() {
            s.truncate(s.len() - VERSION_SUFFIX.len());
            continue;
        }
        // c. `/v1/v1`（折叠一轮后再走 d/b 的判定）
        if strip_len(&s, V1_V1_SUFFIX).is_some() {
            s.truncate(s.len() - VERSION_SUFFIX.len());
            continue;
        }
        // d. 端点段本身，长后缀先判
        if let Some(n) = strip_len(&s, CHAT_COMPLETIONS_SUFFIX) {
            s.truncate(n);
            continue;
        }
        if let Some(n) = strip_len(&s, COMPLETIONS_SUFFIX) {
            s.truncate(n);
            continue;
        }
        break;
    }
    if strip_len(&s, VERSION_SUFFIX).is_none() {
        s.push_str(VERSION_SUFFIX);
    }
    s
}

/// 判据：这个 `base_url` 是不是**只可能**打到本机自己的回环接口。
/// 装配层用它决定「凭据管理器里没有 key」是否可以放行（见 `ipc.rs` 的 `send_message`）。
///
/// 判假的代价是「还得填一次 key」，判真的代价是一个**无凭据的推理端点被当成安全**。
/// 所以整条链每一段都朝「假」收：认不出的形态一律 false，不用 `url` crate（本仓纪律是不加
/// 依赖，T46b 同因），scheme 只认 http/https（`socks5://127.0.0.1` 是把流量转出去的代理，
/// 不是本机），host 用 `std::net` 的 `is_loopback()` 判定（127/8 与 ::1 的语义交给系统，
/// 不自己比字符串 —— `127.0.0.1.evil.test` 就是给 `starts_with("127.")` 那种写法准备的）。
pub fn is_loopback_endpoint(raw: &str) -> bool {
    let s = raw.trim();
    // 1. scheme：只认 http/https，大小写不敏感。没有 `://` 的（裸 `localhost`、`not a url`）在这里就假。
    let (scheme, rest) = match s.split_once("://") {
        Some(pair) => pair,
        None => return false,
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // 2. authority：`/`、`?`、`#` 之前的那一段。路径/查询里再出现别的 URL 不参与判定。
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 3. 凭据：host 是**最后一个** `@` 之后的部分（RFC 3986 允许 userinfo 里有 `@`）。
    //    这一步同时挡掉 `http://localhost@evil.test/` —— 真去的不是本机。
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // 4. 端口：带方括号的是 IPv6 字面量，取 `]` 之前；不带括号却出现第二个 `:` 是非法形态，假。
    let host = if let Some(bracketed) = hostport.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else if let Some((h, port)) = hostport.split_once(':') {
        if port.contains(':') {
            return false;
        }
        h
    } else {
        hostport
    };
    if host.is_empty() {
        return false;
    }
    // 5. `localhost` 这个名字只有回环这一个含义（RFC 6761），可以直接认；其余一律要求它
    //    真的能解析成 IP，并且那个 IP 自己说是回环。
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
    /// 端点级别的采样温度。`None` = 不写进请求体，由端点自己定；`Some(t)` 由
    /// `ChatRequest.temperature` 带上（谁给它赋值见 `agent/session.rs` 的 `request()`）。
    pub temperature: Option<f32>,
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
            .field("temperature", &self.temperature)
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
    ///
    /// **块-角色守卫（folded item 1）**：每个角色能承载的块由协议决定，见下面三处
    /// `require_slots`。不在集合内的块**拒绝**（`ModelError::Decode`），不静默丢：
    /// 静默丢会让一条内容被削空的消息上线，上层拿不到任何信号，而端点回的 400
    /// 与真实原因（我们的消息树不合法）隔了一层——正是 R28 要消灭的那类误导。
    /// 唯一保留的丢弃是 user 轮的 `ToolCall`（见该臂注释）：它与同轮 ToolResult
    /// 所指同一次调用，丢弃不丢任何端点需要的信息。
    fn to_api(messages: &[Message]) -> Result<Vec<ChatCompletionRequestMessage>, ModelError> {
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            match m.role {
                Role::System => {
                    // system 的 content 只能是字符串（`chat_.rs` 的
                    // `ChatCompletionRequestSystemMessageContent`）：任何非 Text 块都没位置。
                    require_slots(m, &["Text"])?;
                    out.push(ChatCompletionRequestMessage::System(
                        ChatCompletionRequestSystemMessageArgs::default()
                            .content(m.text_of())
                            .build()
                            .map_err(request_build_err)?,
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
                                    .map_err(request_build_err)?
                                    .into(),
                            ),
                            ContentBlock::Image { data, mime } => parts.push(
                                ChatCompletionRequestMessageContentPartImageArgs::default()
                                    .image_url(format!("data:{};base64,{}", mime_url(mime), data))
                                    .build()
                                    .map_err(request_build_err)?
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
                            // 这里**不**走 `require_slots`（与 assistant/system 两臂不同）是有意的：
                            // Task 5/7 会把「调用 + 结果」成对塞进一轮，ToolCall 的信息在同轮
                            // ToolResult 里已经在场（同一个 call_id），丢掉它不丢端点需要的东西。
                            ContentBlock::ToolCall { .. } => {}
                        }
                    }
                    if !parts.is_empty() {
                        out.push(ChatCompletionRequestMessage::User(
                            ChatCompletionRequestUserMessageArgs::default()
                                .content(parts)
                                .build()
                                .map_err(request_build_err)?,
                        ));
                    }
                }
                Role::Assistant => {
                    // assistant 的 content 只允许 text/refusal（`chat_.rs:292-295`，**没有** image
                    // 位），ToolCall 走 `tool_calls` 字段；ToolResult 必须属于 user 轮。
                    require_slots(m, &["Text", "ToolCall"])?;
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
        b.build().map_err(request_build_err)
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
    let text = m.text_of();
    let mut tool_calls: Vec<ChatCompletionMessageToolCalls> = Vec::new();
    for (id, name, input) in m.tool_calls() {
        let arguments = serde_json::to_string(&input).map_err(|e| {
            ModelError::Decode(format!(
                "请求构造失败（未发出网络请求）: tool_call 的 arguments 无法序列化为 JSON 字符串: {e}"
            ))
        })?;
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
    b.build().map_err(request_build_err)
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
            "[工具返回了一张 {} 图片：tool 消息不能带图，它会随下一条 user 消息送出]",
            mime_url(mime)
        ),
        // R317：多块结果在这一层按块摊平成文本（tool 角色只吃文本，见本函数文档）。
        // 图片依旧只留一行说明；顺序保持 server 给的顺序。
        ToolOutput::Multi { parts } => parts
            .iter()
            .map(|part| match part {
                ToolOutput::Text { text } => text.clone(),
                ToolOutput::Image { mime, .. } => format!(
                    "[工具返回了一张 {} 图片：tool 消息不能带图，它会随下一条 user 消息送出]",
                    mime_url(mime)
                ),
                ToolOutput::Multi { .. } => "[嵌套的多块结果，未展开]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
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
            .map_err(request_build_err)?,
    ))
}

fn mime_url(m: &ImageMime) -> &'static str {
    match m {
        ImageMime::Png => "image/png",
        ImageMime::Jpeg => "image/jpeg",
    }
}

fn role_label(r: &Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// 块的类型名，只用于错误文本（消息要能自报「哪一类块放不下」）。
fn block_label(b: &ContentBlock) -> &'static str {
    match b {
        ContentBlock::Text { .. } => "Text",
        ContentBlock::Image { .. } => "Image",
        ContentBlock::ToolCall { .. } => "ToolCall",
        ContentBlock::ToolResult { .. } => "ToolResult",
    }
}

/// 块-角色守卫：`allowed` 之外出现任何块就返回 `ModelError::Decode`。
/// 选「拒绝」而不是 `debug_assert!`/静默丢弃的理由：`debug_assert!` 在 release 构建里
/// 不存在，而用户拿到的就是 release 构建；静默丢弃会把「系统提示整条消失」这种
/// 事变成模型的一次无声降级回答，只有真端点跑起来才看得见。
fn require_slots(m: &Message, allowed: &[&str]) -> Result<(), ModelError> {
    for b in &m.blocks {
        let label = block_label(b);
        if !allowed.contains(&label) {
            return Err(ModelError::Decode(format!(
                "{} 轮的 {label} 块在 OpenAI 协议里没有位置（该轮只接受 {}）",
                role_label(&m.role),
                allowed.join("/")
            )));
        }
    }
    Ok(())
}

/// 请求**还没出门**就失败的分支（derive_builder 的 `build()`、arguments 序列化、
/// 客户端侧参数校验）统一走这里。措辞刻意是「请求构造失败」而不是「响应无法解析」：
/// 这些情况下根本还没有响应可谈（folded item 2）。
fn request_build_err(e: OpenAIError) -> ModelError {
    ModelError::Decode(format!("请求构造失败（未发出网络请求）: {e}"))
}

/// 响应正文进错误信息时必须截断：一个 404 的 HTML 页能有几十 KB，而 Task 10 会把
/// 这条错误整行显示给用户。200 字符足够看出「回来的不是 JSON，是网关/登录页」。
const RAW_BODY_KEEP: usize = 200;

/// 切法共用 `crate::text::head_chars`（与 `assistant_acc::args_snippet`、事件流预览、
/// 会话标题同一份实现）；这里只决定标记文案，因为用户要的只是「后头还有，别当没看见」。
fn body_snippet(raw: &str) -> String {
    let (head, dropped) = crate::text::head_chars(raw, RAW_BODY_KEEP);
    if dropped > 0 {
        format!("{head}…（正文已截断）")
    } else {
        head
    }
}

/// **R28：两个调用点共用的唯一分类入口。**
///
/// 上游 `OpenAIError`（`async-openai-0.42.2/src/error.rs:11-41`，`_api` feature 开着，
/// 我们只有 `chat-completion,realtime`，没有 `middleware`，所以 `Boxed`（`error.rs:34-36`）
/// 在我们的编译产物里不存在；`qwen-do` 是 Cargo.lock 里唯一依赖 async-openai 的包，
/// 没有别处能把 feature 打开）在两个调用点上实际能出现的变体：
///
/// | 变体 | 从哪来 | 归入 |
/// |---|---|---|
/// | `ApiError(ApiErrorResponse)` `error.rs:18` | `client.rs:629-631` → `read_error_response`（5xx：`:740-749`；4xx 带 JSON body：`:754-757`） | `Api` |
/// | `Reqwest(reqwest::Error)` `error.rs:14` | `client.rs:628` 发出请求本身（DNS/TLS/拒连）、`client.rs:672-677` 构造请求（base 不是合法 URL）、`client.rs:725/733` 读 body | `Transport` |
/// | `StreamError(Box<StreamError>)` `error.rs:32` | 只可能在中途：`client.rs:848-851`（非 wasm 的 SSE 读环，`EventStream(String)` 在 `error.rs:78`）；`create_stream` 的返回类型里没有它（`client.rs:642-643` 只把 `stream(response).await` 包成流） | `Transport` |
/// | `JSONDeserialize(serde_json::Error, String)` `error.rs:21` | `client.rs:769-770` 的 event_mapper（chunk 不是合法 JSON）、以及 `client.rs:758`（4xx 但 body 不是 `{ "error": … }` 形态，例如 HTML 404 页） | `Decode` |
/// | `InvalidArgument(String)` `error.rs:40` | `chat.rs:82`（stream=false，我们恒 true，不可达）、`client.rs:399`（请求体序列化失败） | `Decode`（请求侧） |
/// | `FileSaveError` / `FileReadError` `error.rs:25/29` | 文件类 API 专用，两个调用点都到不了 | `Decode`（本地侧） |
///
/// 匹配**写满七个变体、没有 `_` 兜底**：将来若某个 feature 加了新变体（如 `Boxed`），
/// 编译器会在这里报错逼我们重新判，而不是让它顺着兜底被误分类。
///
/// 方向性要求（这条 finding 的全部意义）：**已经连通的端点回了错误，绝不能报成
/// 「无法连接」**；反过来连接/SSE 断流也不能报成「模型返回错误」。401 与 DNS 失败
/// 在日志里必须长得不一样，见 `provider.rs` 的 `ModelError` 文档表。
fn classify_openai_error(err: OpenAIError, url: String) -> ModelError {
    match err {
        OpenAIError::ApiError(body) => ModelError::Api(body.to_string()),
        e @ OpenAIError::Reqwest(_) => ModelError::Transport { url, source: e },
        e @ OpenAIError::StreamError(_) => ModelError::Transport { url, source: e },
        OpenAIError::JSONDeserialize(inner, raw) => ModelError::Decode(format!(
            "响应体无法解析: {inner}｜响应正文: {}｜请求地址: {}",
            body_snippet(&raw),
            // 地址夹到 120 字符：这条的总长上限（`an_unparseable_response_is_decode_with_a_bounded_body_snippet`）
            // 是靠「正文 ≤ RAW_BODY_KEEP + 固定前后缀」撑的，而 base_url 是用户粘贴进来的自由文本，
            // 不夹就等于把那个上限交给用户的手。
            crate::text::truncate_display(&url, 120)
        )),
        OpenAIError::InvalidArgument(msg) => {
            ModelError::Decode(format!("请求构造失败（未发出网络请求）: {msg}"))
        }
        OpenAIError::FileSaveError(msg) | OpenAIError::FileReadError(msg) => {
            ModelError::Decode(format!("本地文件读写失败（未发出网络请求）: {msg}"))
        }
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
            match client.chat().create_stream_byot::<_, RawChunk>(request).await {
                // R28：这里**不能**无条件 `Transport`。`create_stream_byot` 对任何非 2xx 都返回
                // `ApiError`（`client.rs:629-631` → `error.rs:18`），把 401/429/模型不存在
                // 说成「无法连接模型端点」就是本条 finding 要消灭的误导。
                Err(e) => yield Err(classify_openai_error(e, url.clone())),
                Ok(mut s) => {
                    // R334：思考增量包成一对 `<think>…</think>` 再交给上层 —— 前端据此折叠，
                    // 历史里也留下与 14B 内联思考同样的形状。`in_reasoning` 保证只开一次。
                    let mut in_reasoning = false;
                    while let Some(item) = s.next().await {
                        match item {
                            // 同一个分类函数、同一个方向：SSE 中途断流 → Transport，
                            // 端点错误 → Api，chunk 解不出来 → Decode。
                            Err(e) => yield Err(classify_openai_error(e, url.clone())),
                            Ok(chunk) => {
                                for d in raw_chunk_to_deltas(&chunk, &mut in_reasoning) {
                                    yield Ok(d);
                                }
                            }
                        }
                    }
                    if in_reasoning {
                        yield Ok(ModelDelta::Text("</think>".into()));
                    }
                }
            }
        })
    }
}

/// 自有的流式分片（R334）：`async-openai` 的 delta 类型没有 `reasoning_content`，
/// 且不 `flatten` 未知字段 ⇒ Ollama/Qwen3 的思考增量在反序列化时被丢掉。
/// `byot` 让我们用自己的宽松类型收流，纯文本/工具调用的形状与原来逐字段一致。
#[derive(serde::Deserialize)]
struct RawChunk {
    #[serde(default)]
    choices: Vec<RawChoice>,
    usage: Option<RawUsage>,
}

#[derive(serde::Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[derive(serde::Deserialize, Default)]
struct RawChoice {
    #[serde(default)]
    delta: RawDelta,
}

#[derive(serde::Deserialize, Default)]
struct RawDelta {
    content: Option<String>,
    /// Ollama 走 OpenAI 兼容层时叫 `reasoning_content`；别的网关可能叫 `reasoning`/`thinking`。
    #[serde(default, alias = "reasoning", alias = "thinking")]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<RawToolCall>>,
}

#[derive(serde::Deserialize, Default)]
struct RawToolCall {
    #[serde(default)]
    index: u32,
    id: Option<String>,
    function: Option<RawFunction>,
}

#[derive(serde::Deserialize, Default)]
struct RawFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// 原始分片 → 增量（R334）：思考增量自动包成一对 `<think>…</think>`；`in_reasoning`
/// 跨分片保存状态，保证只开一次、正文到来时先闭合。纯文本与工具调用的顺序与旧实现一致。
fn raw_chunk_to_deltas(chunk: &RawChunk, in_reasoning: &mut bool) -> Vec<ModelDelta> {
    let mut v = Vec::new();
    if let Some(u) = &chunk.usage {
        v.push(ModelDelta::Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });
    }
    for ch in &chunk.choices {
        if let Some(r) = ch.delta.reasoning_content.as_deref().filter(|s| !s.is_empty()) {
            if !*in_reasoning {
                *in_reasoning = true;
                v.push(ModelDelta::Text("<think>".into()));
            }
            v.push(ModelDelta::Text(r.to_string()));
        }
        if let Some(t) = ch.delta.content.as_deref().filter(|s| !s.is_empty()) {
            if *in_reasoning {
                *in_reasoning = false;
                v.push(ModelDelta::Text("</think>".into()));
            }
            v.push(ModelDelta::Text(t.to_string()));
        }
        for f in ch.delta.tool_calls.iter().flatten() {
            let name = f.function.as_ref().and_then(|x| x.name.clone());
            let args = f.function.as_ref().and_then(|x| x.arguments.clone());
            if name.is_some() || f.id.is_some() {
                v.push(ModelDelta::ToolCallStart {
                    index: f.index,
                    id: f.id.clone(),
                    name,
                });
            }
            if let Some(a) = args {
                v.push(ModelDelta::ToolCallArgs {
                    index: f.index,
                    fragment: a,
                });
            }
        }
    }
    v
}

/// 一个 SSE chunk → 0..n 个 `ModelDelta`。顺序：usage 先于文本与工具（usage 只在
/// 最后一个分片出现，且那一轮 `choices` 通常是空数组）。
#[cfg(test)]
fn chunk_to_deltas(c: &async_openai::types::chat::CreateChatCompletionStreamResponse) -> Vec<ModelDelta> {
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
                // 走的是 `classify_openai_error` → `Decode`，这里的 `f.index` 不可能"缺省成 0"。
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
    // `use super::*` 已经带进 ChatRequest / ModelError / ModelProvider / ToolDef /
    // OpenAIError / async_openai::types::chat::CreateChatCompletionStreamResponse / futures::StreamExt /
    // Message / ContentBlock / Role / ToolOutput / normalize_base_url /
    // classify_openai_error / require_slots；这里只补 `super` 没有引到的名字。
    // （取文本不在此列：`text_of` 自 R40 起是 `agent::types::Message` 的方法，写作 `m.text_of()`。）
    use super::*;
    use async_openai::error::{ApiError, ApiErrorResponse, StreamError};
    // `json!` 是宏，父模块没有 `use serde_json::json`，所以必须自己引。
    use serde_json::json;

    /// R25 的夹具 key：**刻意不带任何真实密钥的前缀形态**（形如密钥的字面量会触发密钥扫描器，
    /// 而这个仓库里没有任何真 key，也就不该有任何看起来像 key 的东西），
    /// 同时保持足够长且含唯一片段（`4f9d2c7be1a8`），让「渲染结果里搜不到它」
    /// 不是一句空话——一个 3 字符的 fixture 会被任何掩码后的字符串偶然包含。
    const FAKE_API_KEY: &str = "not-a-real-key-fixture-4f9d2c7be1a8";
    /// `FAKE_API_KEY` 的一个尾部片段：用来证明掩码不是「只把开头藏起来」。
    const FAKE_API_KEY_TAIL: &str = "4f9d2c7be1a8";

    fn cfg() -> ModelConfig {
        ModelConfig {
            base_url: "https://x.com".into(),
            model: "qwen-test".into(),
            api_key: FAKE_API_KEY.into(),
            max_output_tokens: 512,
            // 端点级温度默认不带：温度进请求体那一条线由 `ChatRequest.temperature`
            // 测（`request_declares_tools_and_temperature`），夹具这里保持中性。
            temperature: None,
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
    /// 断言相反（`/api/openai` 这种任意路径尾部确实会被补 `/v1`）。改名以描述它真正证明的行为。
    ///
    /// 注意本测试**不是**报告 §R23 旧文本里写的「`…/v1/chat/completions` → `…/v1/chat/completions/v1`」：
    /// 那个输入从来就没有对应的测试（幻影条目，已在修复报告里更正），而且自 R27 起该输入的结果
    /// 已经反转为 `…/v1` —— 见 `strips_a_pasted_full_endpoint_from_the_console`。
    #[test]
    fn appends_v1_to_an_arbitrary_path() {
        assert_eq!(
            normalize_base_url("https://x.com/api/openai"),
            "https://x.com/api/openai/v1"
        );
    }

    /// R27 的主形态：各家控制台粘贴出来的是**完整端点**，而 async-openai 会自己再拼一次
    /// `/chat/completions`（`chat.rs:90`），所以尾部端点段必须摘掉，留下版本段。
    /// 不摘的话 `…/v1/chat/completions` 会先被补成 `…/v1/chat/completions/v1`，
    /// 最终请求 URL 是 `…/v1/chat/completions/v1/chat/completions` → 404。
    #[test]
    fn strips_a_pasted_full_endpoint_from_the_console() {
        assert_eq!(
            normalize_base_url(
                "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
            ),
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
        // 控制台/文档里常带尾斜杠，先去斜杠再摘端点段
        assert_eq!(
            normalize_base_url("https://x.test/v1/chat/completions/"),
            "https://x.test/v1"
        );
        // 双斜杠这种粘贴事故也要收敛
        assert_eq!(
            normalize_base_url("https://x.test/v1//chat/completions"),
            "https://x.test/v1"
        );
    }

    /// R27 要求钉死的第二个分支：文本补全（非 tool）端点后缀 `/completions` 同样不是
    /// base 的一部分。本适配器只会说 chat 协议，所以规则是**照摘**：摘完补 `/v1`，
    /// 请求打到 `…/v1/chat/completions`（同一个 base 上唯一我们能用的端点）。
    #[test]
    fn strips_a_pasted_text_completions_suffix() {
        assert_eq!(
            normalize_base_url("https://x.test/v1/completions"),
            "https://x.test/v1"
        );
        assert_eq!(
            normalize_base_url("https://x.test/completions"),
            "https://x.test/v1"
        );
    }

    /// R27 要求钉死的第三个分支：端点段与版本段的顺序颠倒（`…/chat/completions/v1`）。
    /// 规则：与顺序无关，一律收敛成「一个版本段」——先摘尾部的 `/v1` 再摘端点段。
    /// 不这么定的话它会以 `…/chat/completions/v1` 原样上线（尾段是 `/v1`，旧规则不补），
    /// 然后被拼成 `…/chat/completions/v1/chat/completions`，正是 I-1 要消灭的那类 404。
    #[test]
    fn normalizes_an_endpoint_pasted_before_the_version_segment() {
        assert_eq!(
            normalize_base_url("https://x.test/api/chat/completions/v1"),
            "https://x.test/api/v1"
        );
        assert_eq!(
            normalize_base_url("https://x.test/v1/chat/completions/v1"),
            "https://x.test/v1"
        );
        assert_eq!(
            normalize_base_url("https://x.test/v1/completions/v1"),
            "https://x.test/v1"
        );
    }

    /// `/chat/completions` 必须先于它的短后缀 `/completions` 判定：顺序写反会把
    /// `…/v1/chat/completions` 削成 `…/v1/chat`，再加 `/v1` 得到一个谁都不认识的 URL。
    #[test]
    fn strips_chat_completions_before_the_shorter_completions_alias() {
        let out = normalize_base_url("https://x.test/v1/chat/completions");
        assert_eq!(out, "https://x.test/v1");
        assert!(!out.contains("/chat"), "短后缀抢判会得到 {out}");
    }

    /// 端点段判定同样只用大小写做判定、不改写其余路径（与 `/V1` 同一条约定）。
    #[test]
    fn matches_the_endpoint_suffix_case_insensitively() {
        assert_eq!(
            normalize_base_url("https://x.test/v1/CHAT/Completions"),
            "https://x.test/v1"
        );
        assert_eq!(
            normalize_base_url("https://x.test/Compatible-Mode/V1/Chat/Completions"),
            "https://x.test/Compatible-Mode/V1"
        );
    }

    /// 端点段之后还有别的段时**不算**尾部端点段：`…/chat/completions/prefill` 这种
    /// 路径我们不动它（除它需要 `/v1` 之外没有别的规则可套），
    /// 免得「摘后缀」变成任意子串删除。
    #[test]
    fn only_strips_an_endpoint_segment_at_the_very_end() {
        assert_eq!(
            normalize_base_url("https://x.test/chat/completions/prefill"),
            "https://x.test/chat/completions/prefill/v1"
        );
    }

    /// `strip_len` 用的那几个字面拼接常量和它们拆开的形式必须一致，
    /// 否则 `…/completions/v1` 这条规则会静默地不再命中。
    #[test]
    fn endpoint_suffixes_are_spelled_as_expected() {
        assert_eq!(
            format!("{COMPLETIONS_SUFFIX}{VERSION_SUFFIX}"),
            COMPLETIONS_V1_SUFFIX
        );
        assert_eq!(format!("{VERSION_SUFFIX}{VERSION_SUFFIX}"), V1_V1_SUFFIX);
        assert_eq!(
            normalize_base_url(&format!("https://x.test/api{CHAT_COMPLETIONS_SUFFIX}")),
            "https://x.test/api/v1"
        );
    }

    #[test]
    fn keeps_localhost_ollama_without_key_but_still_normalizes() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1"
        );
    }

    // ---- is_loopback_endpoint（本机端点免 key 的唯一判据）----
    //
    // 背景：`OpenAiCompat::new` 从来就允许空 `api_key`（上面那条测试就是为它写的），
    // 真正拦着本机模型的是装配层那句「尚未配置 API key」。所以这里要的不是「放宽 key 要求」，
    // 而是**一条只能窄到回环的判据** —— 判错的后果是把一个无凭据的推理端点暴露给任意网络，
    // 因此下面每一支「假」都比「真」更重要。
    // 为什么不用 `url` crate：本仓的纪律是不加依赖（T46b 同因），而这里只需要 scheme +
    // authority + host，`std::net` 的 `is_loopback()` 已经把 127/8 与 ::1 的判定权给了系统。

    #[test]
    fn accepts_every_loopback_shape_of_a_local_endpoint() {
        assert!(is_loopback_endpoint("http://127.0.0.1:11434/v1"));
        // 整段 127/8 都是回环，不只是 .0.1（systemd-resolved 那类会绑 127.0.0.53）。
        assert!(is_loopback_endpoint("http://127.0.0.53:11434/v1"));
        assert!(is_loopback_endpoint("http://localhost:11434/v1"));
        assert!(is_loopback_endpoint("http://[::1]:11434/v1"));
        // scheme 与 host 的大小写不敏感（用户从配置里粘出来的形态不止一种）。
        assert!(is_loopback_endpoint("HTTP://LOCALHOST:11434/v1"));
        // 没有显式端口、没有 /v1 尾巴，也还是本机。
        assert!(is_loopback_endpoint("http://127.0.0.1"));
        // 带凭据的本机端点（LM Studio 一类可以设密码）：凭据在 `@` 左边，不是 host。
        assert!(is_loopback_endpoint("http://user:pw@localhost:1234/v1"));
    }

    #[test]
    fn rejects_every_non_loopback_host() {
        assert!(!is_loopback_endpoint(
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        ));
        // 前缀像回环，其实是 DNS 名 —— 字符串 `starts_with("127.")` 会放它过。
        assert!(!is_loopback_endpoint("http://127.0.0.1.evil.test/v1"));
        assert!(!is_loopback_endpoint("http://localhost.evil.test/v1"));
        assert!(!is_loopback_endpoint("http://notlocalhost/v1"));
        // `0.0.0.0` 在 Windows 上连出去确实落到本机，但它的语义是「所有网卡」：
        // 认它等于给「把无凭据端点绑到局域网」的配置背书，所以判假。
        assert!(!is_loopback_endpoint("http://0.0.0.0:11434/v1"));
        assert!(!is_loopback_endpoint("http://192.168.1.10:11434/v1"));
        // 回环地址写在别的 host 后面：真去的不是本机。
        assert!(!is_loopback_endpoint("http://evil.test/@127.0.0.1:11434"));
    }

    #[test]
    fn rejects_unparseable_and_non_http_inputs_as_not_loopback() {
        // 判假的代价是「还要填 key」，判真的代价是无凭据端点出网 —— 所以默认值必须是假。
        assert!(!is_loopback_endpoint(""));
        assert!(!is_loopback_endpoint("   "));
        assert!(!is_loopback_endpoint("localhost"));
        assert!(!is_loopback_endpoint("not a url"));
        assert!(!is_loopback_endpoint("http://:11434/v1"));
        assert!(!is_loopback_endpoint("http:///v1"));
        assert!(!is_loopback_endpoint("socks5://127.0.0.1:1080"));
        assert!(!is_loopback_endpoint("file:///C:/Users/x/model.gguf"));
        // 查询串里藏一个回环 URL 不算回环端点。
        assert!(!is_loopback_endpoint(
            "http://evil.test/?next=http://127.0.0.1:11434"
        ));
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
    /// 单次折叠做不到，所以实现里用的是循环）。R27 之后新增的端点段形态必须同样收敛，
    /// 所以它们的**一次结果**也要在这张表里再走一遍。
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
            // ---- R27 新增 ----
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions/",
            "https://x.com/completions",
            "https://x.com/v1/completions",
            "https://x.com/v1/chat/completions/v1",
            "https://x.com/api/chat/completions/v1",
            "https://x.com/v1/CHAT/Completions",
            "https://x.com/chat/completions/prefill",
            "https://x.com/v1//chat/completions",
            "https://x.com/v1/chat/completions?stream=true",
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

    /// 端点段形态必须**收敛到带版本段的标准 base**——这条不是幂等（f(x) != x），
    /// 而是「一次就到位」：`f(f(x)) == f(x)` 由上面那条保证，这条保证的是
    /// 「粘贴完整端点的人和一个规规矩矩填 `…/v1` 的人最终请求同一个 URL」。
    #[test]
    fn a_pasted_endpoint_lands_on_the_same_base_as_a_well_formed_one() {
        assert_eq!(
            normalize_base_url(
                "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
            ),
            normalize_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        );
        assert_eq!(
            normalize_base_url("https://x.test/v1/completions"),
            normalize_base_url("https://x.test/v1"),
        );
    }

    // ---- R25：Debug 不得带出 api_key ----

    #[test]
    fn model_config_debug_masks_the_api_key() {
        let rendered = format!("{:?}", cfg());
        assert!(
            !rendered.contains(FAKE_API_KEY),
            "Debug 里绝不能出现完整 api_key：{rendered}"
        );
        assert!(
            !rendered.contains(FAKE_API_KEY_TAIL),
            "Debug 里不能出现 api_key 的任何连续片段（掩码不是只遮开头）：{rendered}"
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
            .contains(FAKE_API_KEY_TAIL),
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

    // ---- folded item 1：块-角色守卫（没有协议位的块 = 拒绝，不静默丢）----

    /// assistant 轮里的 `ToolResult`：Task 5 的契约是 tool 结果属于 user 轮
    /// （`Message::tool` 用的就是 `Role::User`），assistant 轮的 content 只允许
    /// text/refusal（`chat_.rs:292-295`）。旧代码把它交给 `text_of` 一句话不说地丢掉，
    /// 结果是一条「有 tool_calls、没有对应回应」的消息上线，端点回 400，
    /// 用户看到的错误离真实原因隔了一层。现在在这里就拒绝，且错误文本指名角色与块。
    #[test]
    fn an_assistant_tool_result_is_rejected_where_it_used_to_be_dropped() {
        let m = Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "fs.read".into(),
                    input: json!({}),
                },
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    is_error: false,
                    output: ToolOutput::text("不该出现在 assistant 轮"),
                },
            ],
        };
        let err = OpenAiCompat::to_api(&[m]).expect_err("assistant 轮的 ToolResult 必须被拒绝");
        match &err {
            ModelError::Decode(text) => {
                assert!(text.contains("assistant"), "{text}");
                assert!(text.contains("ToolResult"), "{text}");
            }
            other => panic!("期望 Decode 拒绝，实际 {other:?}"),
        }
        // 拒绝发生在组装阶段：没有任何半截消息被发出去。
        assert!(!format!("{err}").contains("不该出现在 assistant 轮"));
    }

    /// `text_of` 只收 Text，所以一条「只有一个 Image」的 System 消息过去会序列化成
    /// `"content": ""`：系统提示整条消失，模型照常回答，谁都不知道。现在同样拒绝。
    #[test]
    fn a_system_message_with_no_text_slot_is_rejected_not_emptied() {
        let only_image = Message {
            role: Role::System,
            blocks: vec![ContentBlock::Image {
                data: "iVBOR".into(),
                mime: ImageMime::Png,
            }],
        };
        let err = OpenAiCompat::to_api(&[only_image]).expect_err("空 system 不是合法输入");
        assert!(
            matches!(&err, ModelError::Decode(text) if text.contains("system") && text.contains("Image"))
        );
        // 混合形态也一样：文本在场不代表 Image 有了位置。
        let mixed = Message {
            role: Role::System,
            blocks: vec![
                ContentBlock::Text {
                    text: "你是助手".into(),
                },
                ContentBlock::Image {
                    data: "iVBOR".into(),
                    mime: ImageMime::Png,
                },
            ],
        };
        assert!(matches!(
            OpenAiCompat::to_api(&[mixed]),
            Err(ModelError::Decode(_))
        ));
        // 纯文本 system 照常通过，且多块无分隔拼接（协议 content 是单串）。
        let ok = Message {
            role: Role::System,
            blocks: vec![
                ContentBlock::Text {
                    text: "你是".into(),
                },
                ContentBlock::Text {
                    text: "助手".into(),
                },
            ],
        };
        assert_eq!(ok.text_of(), "你是助手");
        let v = serde_json::to_value(OpenAiCompat::to_api(&[ok]).unwrap()).unwrap();
        assert_eq!(v[0]["content"], json!("你是助手"));
    }

    /// 规则的另一半（也是唯一被保留的丢弃）：user 轮的 `ToolCall` 丢掉，
    /// 同轮其它块照常发送。它是同轮 ToolResult 的冗余信息，丢掉不丢端点需要的东西
    /// —— 与上面两条守卫的区别就在这里，改动前先读 `to_api` 的注释。
    #[test]
    fn a_user_turn_tool_call_is_still_dropped_and_the_rest_still_serializes() {
        let m = Message {
            role: Role::User,
            blocks: vec![
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "fs.read".into(),
                    input: json!({}),
                },
                ContentBlock::Text {
                    text: "继续".into(),
                },
            ],
        };
        let v = serde_json::to_value(OpenAiCompat::to_api(&[m]).unwrap()).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
        assert_eq!(v[0]["role"], json!("user"));
        assert_eq!(v[0]["content"][0]["text"], json!("继续"));
    }

    /// 守卫的可达性检查：Task 5 的类型层确实允许 assistant 轮带 ToolResult
    /// （`blocks: Vec<ContentBlock>` 无约束），所以这条不是纸面威胁。
    #[test]
    fn the_type_layer_does_really_allow_an_unmappable_assistant_block() {
        let m = Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolResult {
                call_id: "c1".into(),
                is_error: false,
                output: ToolOutput::text("x"),
            }],
        };
        // 序列化没问题（那是 Task 5 的 wire 形态），是**映射到协议**这一步放不下。
        assert!(serde_json::to_value(&m).is_ok());
        assert!(matches!(
            OpenAiCompat::to_api(&[m]),
            Err(ModelError::Decode(_))
        ));
    }

    // ---- chunk → ModelDelta（R21 的 index 形态在这里被钉住）----

    #[test]
    fn reasoning_is_wrapped_once_and_closed_before_the_answer() {
        let texts = |ds: Vec<ModelDelta>| -> Vec<String> {
            ds.into_iter()
                .filter_map(|d| if let ModelDelta::Text(t) = d { Some(t) } else { None })
                .collect()
        };
        let raw = |j: &str| serde_json::from_str::<RawChunk>(j).unwrap();
        let mut in_r = false;
        assert_eq!(
            texts(raw_chunk_to_deltas(&raw(r#"{"choices":[{"delta":{"reasoning_content":"想"}}]}"#), &mut in_r)),
            vec!["<think>", "想"]
        );
        assert!(in_r);
        assert_eq!(
            texts(raw_chunk_to_deltas(&raw(r#"{"choices":[{"delta":{"reasoning_content":"考"}}]}"#), &mut in_r)),
            vec!["考"],
            "同一段思考只开一次 <think>"
        );
        assert_eq!(
            texts(raw_chunk_to_deltas(&raw(r#"{"choices":[{"delta":{"content":"答案"}}]}"#), &mut in_r)),
            vec!["</think>", "答案"],
            "正文到来前必须闭合思考"
        );
        assert!(!in_r);
        assert_eq!(
            texts(raw_chunk_to_deltas(&raw(r#"{"choices":[{"delta":{"content":"纯文本"}}]}"#), &mut in_r)),
            vec!["纯文本"],
            "没有思考的分片不许凭空加标签"
        );
    }

    fn chunk(json_text: &str) -> async_openai::types::chat::CreateChatCompletionStreamResponse {
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
        let err = serde_json::from_str::<async_openai::types::chat::CreateChatCompletionStreamResponse>(raw)
            .expect_err("缺 index 的分片在 0.42.2 上必须解不出来");
        assert!(
            err.to_string().contains("index"),
            "报错应指向缺失的 index，实际：{err}"
        );
    }

    /// `stream()` 不 panic、也不返回空流：连接层面的失败必须作为一个 `Transport` item
    /// 发出来，并带上已解析的 URL（Task 10 靠这条把错误投给前端）。
    /// 这里用无法解析的 base_url 走这条路 —— reqwest 在发出请求前就拒了，所以不需要网络。
    ///
    /// 这条同时是 **`Reqwest` 变体的真实覆盖**：`reqwest::Error` 在 0.13.5 里没有任何
    /// 公开构造函数、也没有 `impl From<…> for Error`（`reqwest-0.13.5/src/error.rs` 里
    /// 只有 `pub fn is_*`/`status()` 这类访问器），所以它**无法用字面量造出来**。
    /// 让它经过 `create_stream` → `client.rs:672-677` 的构造失败产生一个真正的
    /// `OpenAIError::Reqwest`，是这里唯一诚实的取法。
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
                let line = format!("{url} {source}");
                assert!(
                    !line.contains(FAKE_API_KEY) && !line.contains(FAKE_API_KEY_TAIL),
                    "错误信息不得带出 api_key"
                );
                // R28：这条必须走 Transport 的 Display 文案（连接层面失败）。
                let rendered = format!("{}", items[0].as_ref().unwrap_err());
                assert!(
                    rendered.starts_with("无法连接模型端点 not a url/v1: "),
                    "真实 reqwest 失败必须报成连接问题，实际：{rendered}"
                );
            }
            other => panic!("期望 Transport 错误，实际 {other:?}"),
        }
    }

    // ---- R28：classify_openai_error 的方向性（两个调用点共用这一个入口）----

    /// 造一个 `ApiError`。`status_code` 的类型是 `reqwest::StatusCode`，而我们的
    /// Cargo.toml 里没有、也不允许加 `reqwest` 依赖 —— 这里靠**结构体字段的类型推断**
    /// 走 `TryFrom<u16>`（`http-1.5.0/src/status.rs:291`，`Error = InvalidStatusCode`
    /// 且有 `Debug`，`:533`，所以 `expect` 可用），全程不必点名那个类型。
    fn api_error(status: u16, kind: &str, message: &str) -> OpenAIError {
        OpenAIError::ApiError(ApiErrorResponse {
            status_code: status.try_into().expect("状态码必须落在 100..=999"),
            api_error: ApiError {
                message: message.into(),
                r#type: Some(kind.into()),
                param: None,
                code: None,
                misalignment: None,
            },
        })
    }

    fn classify(err: OpenAIError) -> ModelError {
        classify_openai_error(err, "https://x.test/v1".into())
    }

    /// I-2 的主案发现场：401（key 不对/过期）是首次运行最常见的失败，
    /// 简报片段把它报成「无法连接模型端点」——用户会去查网络，而问题在设置里。
    #[test]
    fn an_http_error_from_the_endpoint_is_api_not_transport() {
        for (status, kind) in [
            (401u16, "invalid_api_key"),
            (429, "rate_limit_exceeded"),
            (404, "model_not_found"),
            (500, "internal_server_error"),
        ] {
            let m = classify(api_error(status, kind, "Incorrect API key provided"));
            match &m {
                ModelError::Api(text) => {
                    assert!(text.contains(&status.to_string()), "{text} 必须带上状态码");
                    assert!(text.contains(kind), "{text} 必须带上 error.type");
                    assert!(
                        text.contains("Incorrect API key provided"),
                        "{text} 必须原样带上 error.message"
                    );
                }
                other => panic!("{status}/{kind} 应归为 Api，实际 {other:?}"),
            }
            let line = format!("{m}");
            assert!(line.starts_with("模型返回错误: "), "{line}");
            assert!(
                !line.contains("无法连接"),
                "{status} 绝不能报成连不上：{line}"
            );
        }
    }

    /// 反向也要成立：SSE 中途断流是连接问题，不能报成「模型返回错误」
    /// （旧代码的 `stream_err` 正是这样反着分的，`StreamError` 落进了 `Api`）。
    /// `StreamError` 只在流中途出现（`client.rs:848-851`）；`create_stream` 的返回里
    /// 不会有它（`client.rs:642-643` 只是把响应包成流）。
    ///
    /// **构造能力如实记录**：`OpenAIError::StreamError(Box<StreamError>)` 里
    /// `StreamError::EventStream(String)`（`error.rs:77-78`）能用字面量造，
    /// 而兄弟变体 `StreamError::UnknownEvent(eventsource_stream::Event)`（`error.rs:74-75`）
    /// **造不出来**——载荷类型来自 `eventsource-stream`，我们没有也不该加这个依赖，
    /// 类型名都点不到。两者走同一条 `Transport` 分支（都由 `client.rs:848` 那一个
    /// `StreamError(...)` 构造点进入本函数），所以这一臂的覆盖对两个内层变体一致。
    #[test]
    fn a_dropped_stream_is_transport_not_api() {
        let m = classify(OpenAIError::StreamError(Box::new(
            StreamError::EventStream("connection closed by peer".into()),
        )));
        match &m {
            ModelError::Transport { url, source } => {
                assert_eq!(url, "https://x.test/v1");
                assert!(
                    format!("{source}").contains("stream failed"),
                    "上游文案要原样保留：{source}"
                );
                assert!(
                    format!("{source}").contains("connection closed by peer"),
                    "断流原因不能被吃掉"
                );
            }
            other => panic!("StreamError 是连接问题，不该归到 {other:?}"),
        }
        let line = format!("{m}");
        assert!(line.starts_with("无法连接模型端点 https://x.test/v1: stream failed: "));
        assert!(!line.contains("模型返回错误"), "断流别报成端点报错：{line}");
    }

    /// chunk 不是合法 JSON（或 4xx 回来一个 HTML 页，`client.rs:758`）→ `Decode`，
    /// 且**必须保留一段正文**（否则「回来的根本不是 JSON」和「端点报了 404」分不开），
    /// 同时**必须截断**（Task 10 会把这行整个显示给用户，一个网关 404 页有几十 KB）。
    #[test]
    fn an_unparseable_response_is_decode_with_a_bounded_body_snippet() {
        let html = format!(
            "<html><body>404 Not Found from the gateway</body>{}</>",
            "x".repeat(5000)
        );
        let inner =
            serde_json::from_str::<serde_json::Value>(&html).expect_err("HTML 必然不是合法 JSON");
        let m = classify(OpenAIError::JSONDeserialize(inner, html));
        match &m {
            ModelError::Decode(text) => {
                assert!(text.contains("响应体无法解析"), "{text}");
                assert!(
                    text.contains("404 Not Found from the gateway"),
                    "正文要能看出这不是 JSON"
                );
                assert!(text.contains("正文已截断"), "长正文必须被截断");
                assert!(
                    text.chars().count() < RAW_BODY_KEEP + 200,
                    "错误文本长度必须受控（实际 {} 字符）",
                    text.chars().count()
                );
            }
            other => panic!("解不出 JSON 属于 Decode，实际 {other:?}"),
        }
        assert!(!format!("{m}").contains("无法连接"));
        assert!(!format!("{m}").contains("模型返回错误"));
    }

    /// 空正文那一支是**实测**出来的，不是设想：2026-10-22 用
    /// `cargo run --example stream_probe` 把 `QWEN_BASE_URL` 依次设成
    /// `…/compatible-mode/v1/`、`…/compatible-mode`、`https://dashscope.aliyuncs.com` 三种形态，
    /// 三次都回 `JSONDeserialize(Error("EOF while parsing a value", line 1, column 0), "")`
    /// ——非 2xx 且正文为空。这时「响应正文」那段是空的，整条错误里**没有任何东西指向设置页**，
    /// 而 base_url 少填一段正是产生它最常见的原因（normalize 只会补 `/v1`，补不出
    /// `/compatible-mode`）。所以这一支必须带上请求地址，否则用户拿到一句无法自助的术语。
    #[test]
    fn an_empty_response_body_still_names_the_url_it_came_from() {
        let inner = serde_json::from_str::<serde_json::Value>("").expect_err("空正文不是合法 JSON");
        let m = classify(OpenAIError::JSONDeserialize(inner, String::new()));
        let line = format!("{m}");
        assert!(matches!(m, ModelError::Decode(_)), "{line}");
        assert!(
            line.contains("https://x.test/v1"),
            "正文为空时，请求地址是唯一的线索：{line}"
        );
        assert!(
            !line.contains("无法连接模型端点"),
            "有响应就不是网络问题：{line}"
        );
        assert!(
            !line.contains("模型返回错误"),
            "端点没报出可解析的错：{line}"
        );
    }

    /// 客户端侧的参数/序列化问题（`chat.rs:82`、`client.rs:399`）：请求根本没出门。
    /// 既不能说「连不上」（网络没问题），也不能说「模型返回错误」（模型没收到东西）。
    #[test]
    fn client_side_failures_never_blame_the_endpoint_or_the_network() {
        let m = classify(OpenAIError::InvalidArgument(
            "When stream is false, use Chat::create".into(),
        ));
        let line = format!("{m}");
        assert!(matches!(m, ModelError::Decode(_)), "{line}");
        assert!(line.contains("请求构造失败（未发出网络请求）"), "{line}");
        assert!(!line.contains("无法连接模型端点"), "{line}");
        assert!(!line.contains("模型返回错误"), "{line}");
    }

    /// 文件类变体在两个调用点都到不了（只有 files API 用得上），但它们在
    /// `_api` 开着 + 非 wasm 时是**真实存在的变体**（`error.rs:22-29`），
    /// 我们的 match 写满七条、不留 `_`，所以这里一并钉住归属。
    #[test]
    fn local_file_variants_are_decode_and_stay_local() {
        for m in [
            classify(OpenAIError::FileSaveError("disk full".into())),
            classify(OpenAIError::FileReadError("no such file".into())),
        ] {
            let line = format!("{m}");
            assert!(matches!(m, ModelError::Decode(_)), "{line}");
            assert!(
                line.contains("本地文件读写失败（未发出网络请求）"),
                "{line}"
            );
            assert!(!line.contains("无法连接模型端点"), "{line}");
        }
    }

    /// 这条 finding 的全部意义落在两行日志的差别上：**401 和 DNS 失败必须长得不一样**。
    #[test]
    fn a_bad_key_and_a_dead_network_read_differently() {
        let bad_key = format!(
            "{}",
            classify(api_error(
                401,
                "invalid_api_key",
                "Incorrect API key provided"
            ))
        );
        let dead_network = format!(
            "{}",
            classify(OpenAIError::StreamError(Box::new(
                StreamError::EventStream("dns failure".into())
            )))
        );
        assert!(bad_key.starts_with("模型返回错误: 401"), "{bad_key}");
        assert!(
            dead_network.starts_with("无法连接模型端点 "),
            "{dead_network}"
        );
        assert_ne!(bad_key, dead_network);
    }

    /// folded item 2：`build()`/arguments 序列化这类**请求侧**失败过去被 `decode_err`
    /// 一律写成「响应无法解析」——那时还没有响应。措辞改由 `request_build_err` 负责。
    #[test]
    fn request_side_build_failures_do_not_claim_a_response_was_parsed() {
        let m = request_build_err(OpenAIError::InvalidArgument("field required".into()));
        let line = format!("{m}");
        assert!(matches!(m, ModelError::Decode(_)), "{line}");
        assert!(line.contains("请求构造失败（未发出网络请求）"), "{line}");
        assert!(!line.contains("响应"), "请求侧失败不该提响应：{line}");
    }

    /// R28 的结构守卫：单测能抓住「分类函数判错」，抓不住「某个调用点绕过它」。
    /// 简报片段里 `Err(e) => yield Err(ModelError::Transport { url, source: e })`
    /// 那行就是病灶，这里禁止它在实现部分复活。
    #[test]
    fn both_stream_error_sites_go_through_the_classifier() {
        let src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/model/openai_compat.rs"
        ));
        let impl_part = src.split("#[cfg(test)]").next().expect("本文件有测试模块");
        assert_eq!(
            impl_part.matches("Err(classify_openai_error").count(),
            2,
            "create_stream 的返回错误与流中途错误必须共用同一个分类入口"
        );
        assert!(
            !impl_part.contains("Err(ModelError::Transport"),
            "不许把 create_stream 的错误无条件归为 Transport（那是 401 报成连不上）"
        );
        assert!(
            !impl_part.contains("fn stream_err"),
            "旧的反向分类函数（StreamError → Api）不许留下"
        );
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
            temperature: None,
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
