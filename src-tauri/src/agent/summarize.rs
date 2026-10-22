// Task 41：两次发送之间的摘要调用（spec 5.3「滚动摘要压缩」的模型那一半）。
//
// 本文件不得 `use tauri`（Plan A 约束，见 `agent/mod.rs`）：摘要调用只依赖
// `ModelProvider` + `CancellationToken`，所以假模型就能端到端测它。
//
// **它的合同只有一条主线**：摘要文本是给**模型**读的，不是给用户读的。
// 它既不进 `TextDelta`、也不进 `UserMessage`，落库后是一条 `Role::System` 备注
// （R69 让 System 行整行跳过事件投影）。发起调用与落库的事务在
// `ipc.rs` 的 `compact_before_turn` 与 `store::db::compact_session` 里，
// 这里只负责「把一批消息换成一句备注文本，或者诚实地报错」。

use crate::agent::types::Message;
use crate::model::provider::{ChatRequest, ModelDelta, ModelProvider};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

/// 摘要调用用的提示词。**独立于 SYSTEM_PROMPT**（`context.rs:11`）：那条教模型「别编造、
/// 可用工具」，而这里要的是相反的行为——只压缩已给内容，不补充、不调工具。
pub const SUMMARY_INSTRUCTION: &str = "把下面的对话历史压缩成不超过 400 字的要点备忘。只保留事实（人名、数字、结论、未完成的请求），\
不要评价、不要补充对话里没有的内容。用中文。";

/// 摘要备注的前缀（spec 5.3 的 `早期对话摘要：…`）。
/// **只加一次**：备注落库时由 [`summary_note`] 拼，`ContextBuilder::build()` 把 System 行
/// 当 `notes` **原样**带上（`context.rs` 的 head 那段），那里再拼一次就是双重前缀。
pub const SUMMARY_PREFIX: &str = "早期对话摘要：";

/// 摘要调用失败时的占位（spec :365「附一条 `[早期对话已省略]` 占位，使用户知道发生了
/// 信息丢失而非模型无故失忆」）。降级只换正文，**不换事务**：该删的行照删，
/// 走的是与成功路径同一条 `compact_session`（第二条「丢弃」路径就是第二份真相）。
pub const SUMMARY_OMITTED: &str = "[早期对话已省略]";

/// 把正文包成落库用的备注文本。成功与降级两条路径都过这里，所以「前缀只加一次」
/// 这件事只有一处实现。
pub fn summary_note(body: &str) -> String {
    format!("{SUMMARY_PREFIX}{body}")
}

/// 组装摘要请求。§2b：**不带 `temperature` 入参**，一律 `None`。
/// 理由是 `temperature` 是用户给聊天调的风格旋钮（0.0–2.0，默认 0.7），拿它去摘要
/// 会把「编造」调浓，而 spec 对摘要的要求恰恰是**不许补充对话里没有的内容** ——
/// 端点默认值 > 用户的创意值。
pub fn summary_request(src: &[Message]) -> ChatRequest {
    let mut msgs = vec![Message::system_note(SUMMARY_INSTRUCTION)];
    msgs.extend(src.iter().cloned());
    ChatRequest {
        messages: msgs,
        // 空工具声明 = 模型不会去调工具（R43 的教训反过来用：未声明的工具它不可能调用）。
        tools: vec![],
        temperature: None,
    }
}

/// 收流。只认 `ModelDelta::Text`；`ToolCall*` 与 `Usage` 一律忽略（摘要不需要工具，
/// 也不该拿它的 usage 去校准主对话的系数——那不是同一批 token）。
///
/// 循环体是 `session.rs:144-170` 已经跑通的那个形状：`while` + `tokio::select!{ biased; .. }`
/// + `deltas.next()`。**不复用 `AssistantAcc`**：那个累加器的职责是 tool_call 分片，
/// 用它只会让摘要文本的来源多一条路径。取消语义也照它：`biased` 让取消优先，
/// 且取消时**不**把半截文本当成功返回。
///
/// 合同（由测试钉住，见 `mod tests`）：
/// 1) `Ok(空串/纯空白)` 是不可能状态 —— 空摘要必须转成 `Err`，否则会落一条零信息备注
///    并把原文删掉（净损失，比不压更糟）。
/// 2) `Err` 的字符串是给用户看的（进 notice），要能判断「该重试还是该改设置」，
///    所以它带的是端点的原始报错，且**不含**已经收到的半截摘要文本。
pub async fn collect_summary(
    model: &dyn ModelProvider,
    req: ChatRequest,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let mut deltas = model.stream(req);
    let mut text = String::new();
    let mut stream_done = false;
    let mut interrupted = false;
    let mut model_error: Option<String> = None;

    while !stream_done && !interrupted && model_error.is_none() {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => interrupted = true,
            item = deltas.next() => match item {
                None => stream_done = true,
                Some(Ok(ModelDelta::Text(t))) => text.push_str(&t),
                // 工具分片与 usage 都不采纳（见函数文档第 2 行）。
                Some(Ok(_)) => {}
                Some(Err(e)) => model_error = Some(e.to_string()),
            }
        }
    }
    // 借 `session.rs:185-187` 同一招：流还挂着的时候先把它关掉，
    // 不把一条没人读完的响应体拖过这次压缩。
    drop(deltas);

    // 顺序是刻意的：**取消优先于报错**。用户在 429 之前按了停止，那件事就是「我取消了」，
    // 把端点错误报给他会让他去查设置。
    if interrupted {
        return Err("摘要已取消，未压缩本轮".to_string());
    }
    if let Some(message) = model_error {
        return Err(format!("摘要调用失败：{message}"));
    }
    if text.trim().is_empty() {
        return Err("摘要调用返回了空文本".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, Message, Role};
    use crate::model::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
    use async_stream::stream;
    use async_trait::async_trait;
    use futures::Stream;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 每次 `stream()` 记下收到的请求（R57：「模型实际看到了什么」只能从模型这一侧拿）。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<ChatRequest>>>);

    impl Recorder {
        fn push(&self, r: &ChatRequest) {
            // `ChatRequest` 不是 `Clone`？它是（provider.rs:32/:39）。这里刻意存克隆，
            // 这样断言读到的永远是「被调用那一刻」的快照，而不是随后被改动的活对象。
            self.0.lock().unwrap().push(r.clone());
        }
        fn calls(&self) -> Vec<ChatRequest> {
            self.0.lock().unwrap().clone()
        }
    }

    /// 吐固定文本分片的摘要模型。
    struct TextModel {
        parts: Vec<String>,
        /// 混在文本之间的其他 delta（工具分片、usage），用来证明它们一律不被采纳。
        extra: Vec<ModelDelta>,
        recorder: Recorder,
    }

    impl TextModel {
        fn new(parts: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                parts: parts.iter().map(|s| s.to_string()).collect(),
                extra: vec![],
                recorder: Recorder::default(),
            })
        }
        fn with_extra(parts: &[&str], extra: Vec<ModelDelta>) -> Arc<Self> {
            Arc::new(Self {
                parts: parts.iter().map(|s| s.to_string()).collect(),
                extra,
                recorder: Recorder::default(),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for TextModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(&req);
            let texts = self
                .parts
                .iter()
                .cloned()
                .map(ModelDelta::Text)
                .collect::<Vec<_>>();
            let all: Vec<ModelDelta> = texts.into_iter().chain(self.extra.clone()).collect();
            Box::pin(futures::stream::iter(all.into_iter().map(Ok)))
        }
    }

    /// 报错的模型：`ModelError::Api` = 已连通的 429（provider.rs 的分类表）。
    /// `head` 是报错之前先吐出来的分片，留空就是「第一个 delta 之前就断了」。
    struct BrokenModel {
        head: Vec<ModelDelta>,
        recorder: Recorder,
    }

    impl BrokenModel {
        fn new(head: Vec<ModelDelta>) -> Arc<Self> {
            Arc::new(Self {
                head,
                recorder: Recorder::default(),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for BrokenModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(&req);
            let head = self.head.clone();
            Box::pin(futures::stream::iter(head.into_iter().map(Ok).chain(
                std::iter::once(Err(ModelError::Api("429 rate limited".to_string()))),
            )))
        }
    }

    /// 吐出 `head` 之后**永久挂住**的模型：`ready` 置位时最后一个分片已经抵达消费者，
    /// 所以测试能在「摘要还在流式返回」的那一刻按停止，不靠 sleep 猜时间。
    /// 形状与 `session.rs` 的 `HangingModel` 同源（那是私有测试模块的，跨不来，也不许改成 pub）。
    struct StallingModel {
        head: Vec<ModelDelta>,
        ready: Arc<AtomicBool>,
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for StallingModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(&req);
            let head = self.head.clone();
            let ready = self.ready.clone();
            Box::pin(stream! {
                for d in head {
                    yield Ok(d);
                }
                ready.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            })
        }
    }

    fn never_cancelled() -> CancellationToken {
        CancellationToken::new()
    }

    fn src_messages() -> Vec<Message> {
        vec![
            Message::user(vec![ContentBlock::Text {
                text: "我叫林，卡号尾 4021".into(),
            }]),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "记下了".into(),
                }],
            },
        ]
    }

    #[test]
    fn the_summary_request_is_the_instruction_plus_the_dropped_source() {
        let req = summary_request(&src_messages());
        // 头一条必须是摘要指令，其后**原样**是待摘要的那批消息（不多不少、不改顺序）。
        assert_eq!(req.messages.len(), 1 + src_messages().len());
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[0].text_of(), SUMMARY_INSTRUCTION);
        assert_eq!(&req.messages[1..], src_messages().as_slice());
        // 空工具声明 = 模型不会去调工具（R43 的教训反过来用：未声明的工具它不可能调用）。
        assert!(req.tools.is_empty(), "摘要调用不许带任何工具声明");
    }

    /// §2b：摘要调用**不继承**用户给聊天调的温度旋钮。
    #[test]
    fn the_summary_request_carries_no_temperature_not_the_users_knob() {
        let req = summary_request(&src_messages());
        assert_eq!(
            req.temperature, None,
            "temperature 是聊天风格旋钮（0.0–2.0，默认 0.7），拿它去摘要会把「编造」调浓；\
             spec 对摘要的要求恰恰是不许补充对话里没有的内容 ⇒ 端点默认值 > 用户的创意值"
        );
    }

    #[tokio::test]
    async fn collect_summary_keeps_only_the_text_deltas() {
        // 工具分片与 usage 混在流里：摘要不需要工具，更**不该**拿它的 usage 去校准主对话的
        // 系数（那批 token 不是同一批，见 `session.rs` 的 `set_calibration` 分母口径）。
        let model = TextModel::with_extra(
            &["早期要点：", "用户叫林，", "卡号尾 4021"],
            vec![
                ModelDelta::ToolCallStart {
                    index: 0,
                    id: Some("c1".into()),
                    name: Some("fs.read".into()),
                },
                ModelDelta::ToolCallArgs {
                    index: 0,
                    fragment: "{\"path\"".into(),
                },
                ModelDelta::Usage {
                    input_tokens: 9_000,
                    output_tokens: 12,
                },
            ],
        );
        let got = collect_summary(
            &*model,
            summary_request(&src_messages()),
            &never_cancelled(),
        )
        .await
        .expect("这条夹具必须成功");
        assert_eq!(got, "早期要点：用户叫林，卡号尾 4021");
        assert_eq!(model.recorder.calls().len(), 1, "一次摘要一次调用");
    }

    #[tokio::test]
    async fn an_empty_summary_is_an_error_not_a_note_that_eats_the_original() {
        // 空摘要落库 = 删掉原文、换回一条零信息备注，那是**净损失**。
        // 所以这条路径必须是 Err（调用方据此走 `[早期对话已省略]` 的降级），
        // 绝不允许把 `Ok("")` 当成成功返回。
        for parts in [vec![], vec!["   "], vec!["\n", " "]] {
            let model = TextModel::new(&parts);
            let got = collect_summary(
                &*model,
                summary_request(&src_messages()),
                &never_cancelled(),
            )
            .await;
            let err = match got {
                Ok(text) => panic!("空摘要（{parts:?}）被当成成功返回：{text:?}"),
                Err(e) => e,
            };
            assert!(
                !err.is_empty(),
                "Err 的字符串是给用户看的（进 notice），空串等于什么都没解释：{parts:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_summary_call_reports_what_the_user_can_act_on() {
        // 报错之前先到了半句也不当成功：语义与 `session.rs` 的取消一样，
        // 「半条摘要」不是摘要。
        let model = BrokenModel::new(vec![ModelDelta::Text("早期要点：用户叫".into())]);
        let err = collect_summary(
            &*model,
            summary_request(&src_messages()),
            &never_cancelled(),
        )
        .await
        .expect_err("429 必须是 Err");
        assert!(
            err.contains("429"),
            "错误文本要让人判断「该重试还是该改设置」，实际：{err}"
        );
        assert!(
            !err.contains("早期要点"),
            "失败时不许把半截摘要文本混进错误里：{err}"
        );
    }

    #[tokio::test]
    async fn cancelling_a_summary_returns_an_error_and_keeps_nothing_back() {
        // 取消发生在「模型还在写摘要」时：`collect_summary` 必须报取消，
        // 而不是把已经收到的那半句当成功返回 —— 那会让调用方删掉原文、
        // 换上一条只写了一半的备注（比不压更糟）。
        let ready = Arc::new(AtomicBool::new(false));
        let model = Arc::new(StallingModel {
            head: vec![ModelDelta::Text("早期要点：用户叫林，".into())],
            ready: ready.clone(),
            recorder: Recorder::default(),
        });
        let cancel = CancellationToken::new();
        let (got, ()) = tokio::join!(
            collect_summary(&*model, summary_request(&src_messages()), &cancel),
            async {
                while !ready.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                cancel.cancel();
            }
        );
        assert!(got.is_err(), "取消之后不许返回 Ok，实际：{got:?}");
        assert!(
            model.recorder.calls().len() == 1,
            "夹具前提：模型确实被叫过一次，否则这条测的是「压根没调用」"
        );
    }

    /// Step 7.1：前缀只加一次，两条路径（成功摘要 / 降级占位）都带。
    #[test]
    fn the_note_prefix_is_added_exactly_once_on_both_paths() {
        assert_eq!(SUMMARY_PREFIX, "早期对话摘要：", "前缀本身要有独立锚点");
        assert_eq!(SUMMARY_OMITTED, "[早期对话已省略]");
        for body in ["用户叫林，卡号尾 4021", SUMMARY_OMITTED] {
            let note = summary_note(body);
            assert_eq!(
                note,
                format!("{SUMMARY_PREFIX}{body}"),
                "备注 = 前缀 + 正文，正文原样在后"
            );
            assert!(
                !note[SUMMARY_PREFIX.len()..].starts_with(SUMMARY_PREFIX),
                "双重前缀：`build()` 会把 System 行当 notes 原样带上，那里再拼一次就是这个样子"
            );
        }
        // 降级路径也必须带前缀：「有一条被压过的痕迹」是 spec :365 的意图，
        // 而且带前缀才能让第二次压缩把它一起并进新摘要。
        assert!(summary_note(SUMMARY_OMITTED).starts_with(SUMMARY_PREFIX));
    }
}
