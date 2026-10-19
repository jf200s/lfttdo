//! 风险验证探针（非生产代码）。回答三个问题：
//! 1) async-openai 的 create_stream 在自定义 base_url 上是否真的逐块到达
//! 2) tool_calls 增量是否分片、是否带 index、arguments 是否被切断
//! 3) 首块到达延迟（判断是否有代理缓冲吃掉流式）
// 偏差 1（async-openai 0.42.2）：chat 相关类型不再平铺在 `types::` 下，
// 而是命名空间化为 `types::chat::`（`types/mod.rs` 里 `pub mod chat`）。
use async_openai::{
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        CreateChatCompletionRequestArgs,
    },
    Client,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("QWEN_API_KEY").expect("set QWEN_API_KEY");
    let base = std::env::var("QWEN_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
    let model = std::env::var("QWEN_MODEL").unwrap_or_else(|_| "qwen-plus".into());

    let cfg = OpenAIConfig::new().with_api_base(&base).with_api_key(key);
    let client = Client::with_config(cfg);

    let req = CreateChatCompletionRequestArgs::default()
        .model(&model)
        .stream(true)
        // 偏差 2：`ChatCompletionRequestSystemMessage::new(..)` 在 0.42.2 里不存在，
        // 只有 derive_builder 的 `ChatCompletionRequestSystemMessageArgs` 和
        // `impl From<&str> for ChatCompletionRequestSystemMessage`（见 types/chat/impls.rs）。
        .messages(vec![ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessage::from("调用 get_time 工具，不要直接回答。"),
        )])
        .build()?;

    let mut stream = client.chat().create_stream(req).await?;
    let t0 = std::time::Instant::now();
    let mut first = None;
    let mut n = 0usize;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if first.is_none() {
            first = Some(t0.elapsed());
        }
        n += 1;
        for ch in &chunk.choices {
            if let Some(tc) = &ch.delta.tool_calls {
                for f in tc {
                    // 偏差 3：0.42.2 的 `ChatCompletionMessageToolCallChunk.index` 是
                    // `u32`（不是 `Option<u32>`，字段上也没有 `#[serde(default)]`）。
                    // 因此本探针**不可能打印出 `idx=None`**：分片缺 index 时 serde
                    // 反序列化直接失败，`stream.next()` 返回 Err 而不是 None。
                    println!(
                        "TOOLCALL idx={:?} id={:?} name={:?} args={:?}",
                        f.index,
                        f.id,
                        f.function.as_ref().and_then(|x| x.name.clone()),
                        f.function.as_ref().and_then(|x| x.arguments.clone()),
                    );
                }
            }
            if let Some(t) = &ch.delta.content {
                if !t.is_empty() {
                    println!("TEXT {t:?}");
                }
            }
        }
    }
    println!("chunks={n} first_chunk_in={:?}", first.unwrap_or_default());
    Ok(())
}
