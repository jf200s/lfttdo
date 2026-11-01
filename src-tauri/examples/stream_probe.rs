//! 风险验证探针（非生产代码）。回答四个问题（对应 findings 文档 Step 5 的必答四问）：
//! 1) async-openai 的 create_stream 在自定义 base_url 上是否真的逐块到达（chunks 是否 > 1）
//! 2) tool_calls 增量是否分片、是否带 index、arguments 是否被切断
//! 3) 首块到达延迟（判断是否有代理缓冲吃掉流式）
//! 4) base_url 哪种形态可用 —— 探针只在启动时读一次 QWEN_BASE_URL，本问题要靠**四次独立运行**
//!    （每种形态一次）；故意的简单设计，不在探针里内置 URL 循环。详见文档「补跑命令」。
//!
//! key 的来源：优先 `QWEN_API_KEY`（留给负向测试），否则读 Windows 凭据管理器里 app 自己在用的
//! 那条 `api_key.qwen-do`。**不要把 key 写进命令行** —— 进程列表和终端缓冲都是泄漏面，见 `api_key()`。
// 偏差 1（async-openai 0.42.2）：chat 相关类型不再平铺在 `types::` 下，
// 而是命名空间化为 `types::chat::`（`types/mod.rs` 里 `pub mod chat`）。
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestUserMessage, ChatCompletionTool, ChatCompletionTools,
        CreateChatCompletionRequestArgs, FunctionObject,
    },
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // R156/R164：真 key 走凭据管理器，**不走命令行**。`QWEN_API_KEY=<真 key> cargo run …`
    // 那种写法会让秘密出现在进程列表（别的用户态进程读得到）和终端滚动缓冲里，本仓的
    // findings 又要原样贴 stdout/stderr —— 两条加起来就是泄漏面。环境变量仍留着，因为它
    // 是唯一能给假 key 做负向测试的入口；库里那条从头到尾只进本进程内存，只打印长度。
    let (key, key_src) = api_key()?;
    let base = std::env::var("QWEN_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
    let model = std::env::var("QWEN_MODEL").unwrap_or_else(|_| "qwen-plus".into());
    // 只打印来源与长度：这行会进 findings 的原始输出，值一打印就等于把 key 写进仓库。
    // base/model 一起打，是为了让贴进表格的那段原文**自己说明**是哪一种形态跑出来的。
    // `key_shape` 是**一个比特**的信息（是不是百炼那串 `sk-` + 32 位小写 hex），不是值的任何
    // 片段：401 有两种成因 ——「库里那条根本不是百炼的 key」（魔搭、OpenAI 的 key 也长 `sk-` 开头）
    // 和「是，但已被吊销/未开通」。这两种要用户做的事相反，而只看错误报文分不开。
    println!(
        "key_source={key_src} key_len={} key_shape={} base={base} model={model}",
        key.chars().count(),
        if is_dashscope_key_shape(&key) {
            "bailian_hex"
        } else {
            "other"
        }
    );

    let cfg = OpenAIConfig::new().with_api_base(&base).with_api_key(key);
    let client = Client::with_config(cfg);

    // 审查修复 1：brief 的探针只在 prompt 里命令模型「调用 get_time 工具」，请求里却从未声明
    // 任何 tools —— 没声明的工具不可能被调用，问题 2/3（分片与 index）在结构上无法被实测。
    // 现在把 get_time 声明成真正的 function 工具（空参数 JSON Schema），并补一条 user 轮。
    let get_time = ChatCompletionTool {
        function: FunctionObject {
            name: "get_time".into(),
            description: Some("获取当前本地时间，ISO 8601 格式；不需要任何参数。".into()),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })),
            strict: None,
        },
    };

    let req = CreateChatCompletionRequestArgs::default()
        .model(&model)
        .stream(true)
        // 偏差 2：`ChatCompletionRequestSystemMessage::new(..)` 在 0.42.2 里不存在，
        // 只有 derive_builder 的 `ChatCompletionRequestSystemMessageArgs` 和
        // `impl From<&str> for ChatCompletionRequestSystemMessage`（见 types/chat/impls.rs:162，
        // user 消息同样有 `impl From<&str>`，impls.rs:150）。
        .messages(vec![
            ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage::from(
                "调用 get_time 工具，不要直接回答。",
            )),
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage::from("现在几点")),
        ])
        // 0.42.2 的 `tools` 字段是 `Option<Vec<ChatCompletionTools>>`（chat_.rs:1083），
        // 函数工具走 `ChatCompletionTools::Function(ChatCompletionTool { function })` 变体。
        .tools(vec![ChatCompletionTools::Function(get_time)])
        .build()?;

    let mut stream = client.chat().create_stream(req).await?;
    let t0 = std::time::Instant::now();
    let mut first = None;
    let mut n = 0usize;

    while let Some(chunk) = stream.next().await {
        // 审查修复 2：这里原本是 `let chunk = chunk?;`。偏差 3（见下）意味着 index-less 分片会以
        // Err 的形态出现在流中间，`?` 则直接从 main 返回，末尾的 `chunks=N first_chunk_in=…`
        // 汇总行永远不打印 —— 恰好是判据说明要求补跑者盯着的情形会丢掉两项测量。
        // 现在改为：打印 STREAM_ERROR + 当时已有的统计，然后 break，任何运行都以汇总行收尾。
        //
        // 偏差 3：0.42.2 的 `ChatCompletionMessageToolCallChunk.index` 是 `u32`（不是
        // `Option<u32>`，字段上也没有 `#[serde(default)]`）。因此本探针**不可能打印出
        // `idx=None`**：分片缺 index 时 serde 反序列化直接失败，`stream.next()` 返回 Err，
        // 在本探针里表现为一行 `STREAM_ERROR … missing field \`index\` …`。
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                eprintln!("STREAM_ERROR after chunks={n} first_chunk_in={first:?}: {e:?}");
                break;
            }
        };
        if first.is_none() {
            first = Some(t0.elapsed());
        }
        n += 1;
        for ch in &chunk.choices {
            if let Some(tc) = &ch.delta.tool_calls {
                for f in tc {
                    println!(
                        "TOOLCALL idx={:?} id={:?} name={:?} args={:?}",
                        f.index,
                        f.id,
                        f.function.as_ref().and_then(|x| x.name.clone()),
                        f.function.as_ref().and_then(|x| x.arguments.clone()),
                    );
                }
            }
            if let Some(t) = &ch.delta.content
                && !t.is_empty() {
                    println!("TEXT {t:?}");
                }
        }
    }
    // 审查修复（附带项）：不再 `first.unwrap_or_default()` —— 零块流会打印成
    // `first_chunk_in=0ns`，像是「首块瞬时」，实际含义是「没有任何数据」。直接打印 Option。
    println!("chunks={n} first_chunk_in={first:?}");
    Ok(())
}

/// 取 key，并返回它是从哪儿来的（`env` / `keyring`）。调用方只打印来源和长度。
///
/// Windows 上走 `settings` 模块的**真实**凭据管理器接缝 —— 和 app 运行时读的是同一条
/// `api_key.qwen-do`。非 Windows 上没有这条接缝（`KeyringSecretStore` 是 `#[cfg(windows)]`），
/// 于是只剩环境变量。
fn api_key() -> Result<(String, &'static str), Box<dyn std::error::Error>> {
    if let Ok(k) = std::env::var("QWEN_API_KEY") {
        return Ok((k, "env"));
    }
    #[cfg(windows)]
    {
        use lfttdo_lib::settings::{KeyringSecretStore, SecretStore};
        let store = KeyringSecretStore::new()?;
        match store.get()? {
            Some(k) => Ok((k, "keyring")),
            None => Err("QWEN_API_KEY 未设置，凭据管理器里也没有 api_key.qwen-do 条目".into()),
        }
    }
    #[cfg(not(windows))]
    {
        Err("非 Windows 平台只能从 QWEN_API_KEY 取 key（凭据管理器接缝是 Windows-only）".into())
    }
}

/// 只回答「像不像百炼的 key」这一个问题，返回一个比特，不回传值的任何片段。
///
/// 百炼（DashScope compatible-mode）的 key 常见形态是 `sk-` + 32 位**小写** hex；魔搭
/// ModelScope 的 key 同样以 `sk-` 开头但字符集是 base62，OpenAI 的更长。把这两件事分开的
/// 代价不该是「打印 key」，所以这里只看长度和字符类。
///
/// **这是启发式，不是厂商承诺**（未查文档、未实测多种真 key）：`other` 只能说明「不像我见过
/// 的那种百炼 key」，不能证明它属于哪家；`bailian_hex` 也不保证可用 —— 判定权在 HTTP 状态码。
fn is_dashscope_key_shape(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("sk-") else {
        return false;
    };
    rest.len() == 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
