// 一轮对话的编排循环（spec 4.6）：`context` 组装 → `model` 流式 → 权限门 → 工具执行
// → 结果回填，直到模型自己收尾、用户打断、模型报错，或轮数触顶。
//
// 本文件不得 `use tauri`（Plan A 约束，见 `agent/mod.rs`）：整条循环靠脚本化模型 +
// 记录型 sink 就能端到端跑完，文件末尾的测试就是它唯一的运行方式验证。
// Plan A 的生产工具表是**空的**（plan:1854），所以工具路径的正确性完全由这些测试撑着
// ——不要以「生产里没有工具」为理由简化循环本身。
use super::context::ContextBuilder;
use super::events::{AgentEvent, Decision, ErrorWhere, EventSink, StopReason};
use super::types::*;
use crate::model::assistant_acc::AssistantAcc;
use crate::model::provider::{ChatRequest, ModelDelta, ModelProvider};
use crate::permission::gate::PermissionGate;
use crate::tools::registry::{ToolCtx, ToolRegistry};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// 打断 / 报错时补进 history 的那条工具结果的内容（R56）。
/// 两个出口共用一个作用（「这个调用没执行」），但归因写清楚，模型才不至于把
/// 「被打断」当成「工具坏了」再试一遍。
const INTERRUPTED_NOTE: &str = "本轮被打断，工具未执行";
const MODEL_FAILURE_NOTE: &str = "本轮在模型流中断处失败，工具未执行";

/// 事件流里工具输出的预览上限（字符）。
const PREVIEW_CHARS: usize = 200;

/// 循环的护栏。`max_turns` 是防模型自我循环的保险丝（spec 4.6）：日常对话 1 轮收尾，
/// 带工具的多为 2–5 轮，25 是上限而不是目标；触顶时发 `Done{MaxTurns}` 让 UI 明确提示，
/// 而不是静默停止。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub max_turns: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { max_turns: 25 }
    }
}

/// 会话的全部外部依赖，由装配点（Task 13）注入。每一项都是 `Arc`（`context` 除外），
/// 所以「每轮新建一个会话」是廉价的 —— `cancel_handle` 的合同正建立在这上面。
pub struct SessionDeps {
    pub model: Arc<dyn ModelProvider>,
    pub registry: Arc<ToolRegistry>,
    pub gate: Arc<PermissionGate>,
    pub sink: Arc<dyn EventSink>,
    pub context: ContextBuilder,
}

/// 一轮对话的编排者。一次 `run` 只跑一轮（`&mut self`），历史留在结构体里，
/// 由 Task 11 落库、Task 13 经 `restore` 装回来。
pub struct AgentSession {
    deps: SessionDeps,
    history: Vec<Message>,
    cfg: SessionConfig,
    cancel: CancellationToken,
    http: reqwest::Client,
    roots: Vec<PathBuf>,
}

impl AgentSession {
    pub fn new(deps: SessionDeps) -> Self {
        Self {
            deps,
            history: Vec::new(),
            cfg: SessionConfig::default(),
            cancel: CancellationToken::new(),
            http: reqwest::Client::builder()
                .user_agent("qwen-do/0.1")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            roots: Vec::new(),
        }
    }

    /// 文件类工具的作用域根（Plan A 没有生产工具，所以默认是空表；
    /// `ToolCtx::resolve_within_roots` 拿到空表时是**全拒**，不是全放行）。
    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        self
    }

    /// 这一轮之后模型应该看到的完整历史。事件流给 UI，这个给 Task 11 落库。
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// 装回历史（Task 13 每轮新建会话时靠它续上多轮对话）。
    pub fn restore(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// 停止用的令牌。**一次性**（R63）：`cancel()` 之后这个会话只会再产出一个
    /// `Interrupted` 收尾，`run()` 内部**不会**复位它 —— 复位会让前端手里那个
    /// handle 静默失效，比原竞态更糟。
    /// 合同：取消过的会话算用完，想继续聊就带着 `history()` 新建一个会话
    /// （`SessionDeps` 全是 `Arc`，Task 13 正是每轮新建）。
    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 跑完这一轮。返回值同时是 `Done{..}` 事件里的 `stop_reason`，两边不可能分叉
    /// （只有 `finish` 一处发 `Done`）。
    pub async fn run(&mut self, blocks: Vec<ContentBlock>) -> StopReason {
        // 取消发生在开始之前：这一轮压根没发生，连 user 消息都不该留下（R63 的第二条 run 同理）。
        if self.cancel.is_cancelled() {
            return self.finish(StopReason::Interrupted);
        }
        self.history.push(Message::user(blocks));
        // 工具上下文建一次就够：三个字段整轮不变，而 `CancellationToken` 的克隆共享
        // 同一个状态，所以中途按下停止照样被 `ToolRegistry::execute` 的守卫看见。
        let ctx = ToolCtx {
            http: self.http.clone(),
            cancel: self.cancel.clone(),
            roots: self.roots.clone(),
        };

        for turn in 1..=self.cfg.max_turns {
            // 上一轮的工具跑完之后又被点了停止：成对状态已经落好，直接收尾。
            if self.cancel.is_cancelled() {
                return self.finish(StopReason::Interrupted);
            }
            self.deps.sink.emit(AgentEvent::TurnStart { turn });

            let mut acc = AssistantAcc::default();
            let mut deltas = self.deps.model.stream(self.request());
            let mut stream_done = false;
            let mut interrupted = false;
            let mut model_error: Option<String> = None;

            // R55：这里用的是 `tokio::select!`。futures 那个 `select!` 的分支要求
            // `&mut` 位置表达式（`deltas.next()` 不是），而且没有 `biased;`。
            // 同一个形状（`biased` + `cancel.cancelled()` 臂）在 `PermissionGate::check`
            // 里已经由 `cancel_stops_a_pending_request_instead_of_hanging` 跑通了。
            //
            // 两个分支都只置标志、`break` 之后再动 `self`：分支体里不能调 `&mut self`
            // 的方法（`self.cancel.cancelled()` 还借用着 `self.cancel`，E0502）。
            //
            // `biased` 的代价是一条已知且良性的竞态：**流干净收尾的那一次 poll 里**
            // 若取消也同时就绪（用户正好在回复结束的同一刻按停止），取消臂先赢，本轮按
            // `Interrupted` 收尾而不是 `EndTurn`。内容不丢——`commit_partial` 把累加器
            // 里的整条回复照原样提交，只是 `stop_reason` 记成用户做的那件事。
            while !stream_done && !interrupted && model_error.is_none() {
                tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => interrupted = true,
                    item = deltas.next() => match item {
                        None => stream_done = true,
                        Some(Ok(ModelDelta::Usage { input_tokens, output_tokens })) => {
                            // R60：`AssistantAcc::apply` 会**静默丢掉** Usage 里的 token
                            // （assistant_acc.rs:37-39），必须在交给它之前自己取走。
                            self.deps.sink.emit(AgentEvent::Usage { input_tokens, output_tokens });
                        }
                        Some(Ok(d)) => {
                            if let Some(text) = acc.apply(d) {
                                self.deps.sink.emit(AgentEvent::TextDelta { text });
                            }
                        }
                        Some(Err(e)) => model_error = Some(e.to_string()),
                    }
                }
            }
            // `deltas` 借用着 `self.deps.model`，而下面要 `&mut self`；放掉它，
            // 顺带把一个没读完的响应体关在这里，而不是拖到整轮结束。
            drop(deltas);

            let assistant = acc.finish();
            if let Some(message) = model_error {
                // R61：Plan A 里模型错误就结束本轮，所以 fatal 是 true。
                self.deps.sink.emit(AgentEvent::Error {
                    at: ErrorWhere::Model,
                    message,
                    fatal: true,
                });
                self.commit_partial(assistant, MODEL_FAILURE_NOTE);
                return self.finish(StopReason::Error);
            }
            if interrupted {
                self.commit_partial(assistant, INTERRUPTED_NOTE);
                return self.finish(StopReason::Interrupted);
            }
            if assistant.blocks.is_empty() {
                // R58：零块的 assistant 消息发不出去（`assistant_acc.rs:75-79` 明确要求
                // 调用方自己判），所以它既不进 history，也不许被下一轮当成输入。
                // 报一条非致命的 loop 错误收尾：静默的「模型没说话」和静默的统计归零
                // 一样，只有 UI 上那一行能被发现。
                self.deps.sink.emit(AgentEvent::Error {
                    at: ErrorWhere::Loop,
                    message: "模型返回空响应".into(),
                    fatal: false,
                });
                return self.finish(StopReason::EndTurn);
            }

            let calls = assistant.tool_calls();
            self.history.push(assistant);
            if calls.is_empty() {
                return self.finish(StopReason::EndTurn);
            }

            // 工具**顺序**执行，不并行（spec 4.6 的刻意取舍：并行难追踪「哪个写操作改了
            // 哪个文件」，也和分级确认的交互顺序冲突）。
            for (id, name, input) in calls {
                // risk 只从注册表查（`ToolCallView` 的注释就是这条）：模型或前端递来的
                // 任何「我是 Read」都不作数 —— `ToolDef` 里压根没有 risk 字段。
                let risk = self.deps.registry.risk_of(&name);
                let view = ToolCallView {
                    id: id.clone(),
                    name: name.clone(),
                    risk,
                    input: input.clone(),
                };
                // 构造函数负责钉住「顶层 risk == call.risk」这个不变量（events.rs:69）。
                self.deps.sink.emit(AgentEvent::tool_call(view.clone()));

                // 取消臂住在 gate 内部（R43）：弹窗挂着的时候按停止，`select!` 打不断
                // 一个已经在 await 的 `check`，只能由它自己收场。
                let decision = self.deps.gate.check(&view, &ctx.cancel).await;
                let started = Instant::now();
                let (output, is_error) = match decision {
                    Decision::Allow { .. } => {
                        match self.deps.registry.execute(&name, input, &ctx).await {
                            // R64：工具失败只由 `ToolResult{is_error:true}` 承载，
                            // 不再补一条 `Error{at:Tool}` —— 同一个事实报两遍，
                            // 前端会长出两行（那条分支是 Plan B/C 的 MCP/循环错误用的）。
                            Ok(output) => (output, false),
                            Err(e) => (ToolOutput::text(e.to_string()), true),
                        }
                    }
                    // R59：内容就是拒绝理由本身。`ToolError::Failed` 的 Display 前缀是
                    // 「工具执行失败: 」，套上去模型读到的是双重包装（spec:263 说的是理由）。
                    Decision::Deny { reason } => {
                        (ToolOutput::text(format!("用户拒绝执行：{reason}")), true)
                    }
                };
                let preview = preview_of(&output);
                self.deps.sink.emit(AgentEvent::ToolResult {
                    call_id: id.clone(),
                    is_error,
                    ms: started.elapsed().as_millis() as u64,
                    preview,
                });
                self.history.push(Message::tool(&id, output, is_error));
            }
        }
        self.finish(StopReason::MaxTurns)
    }

    /// 每个出口都只经这里：`Done` 恰好一条，且带的就是返回值本身。
    fn finish(&self, r: StopReason) -> StopReason {
        self.deps.sink.emit(AgentEvent::Done { stop_reason: r });
        r
    }

    /// R56：把**已经流给 UI 的部分**提交进 history。
    ///
    /// 事件流是 UI 的真相源（Task 14 合帧，Task 11 重放），`history` 是下一轮模型与
    /// 落库的真相源，两边必须说同一件事：半句话在 UI 上出现过，就必须也在库里 ——
    /// 否则「停止 → 关闭会话 → 重开」那段文字就凭空消失。
    ///
    /// 另一半同样必要：给这条消息里每个 `ToolCall` 补一条未执行的结果。只提交
    /// `assistant(ToolCall)` 会让 history 结束在调用上，下一轮模型看到的请求里那条
    /// `role:"tool"` 就没有对应的调用 —— 正是 Task 8 在窗口边界修掉的 mid-session 400
    /// 从这个入口复发。（spec:242 的草图没有这一步，R56 以裁定身份补上。）
    fn commit_partial(&mut self, assistant: Message, note: &str) {
        if assistant.blocks.is_empty() {
            return; // R58：零块消息不进 history
        }
        let calls = assistant.tool_calls();
        self.history.push(assistant);
        for (id, _, _) in calls {
            self.history
                .push(Message::tool(&id, ToolOutput::text(note), true));
        }
    }

    /// 交给模型的请求：滑窗组装出来的消息 + 注册表的工具声明。
    fn request(&self) -> ChatRequest {
        let built = self.deps.context.build(&self.history);
        // `built.dropped` 与 `est_input_tokens` 在 Plan A 没有消费者：`ContextCompacted`
        // 属切片 5，`context.rs:44` 明确要求 Task 10 不为它造事件。
        ChatRequest {
            messages: built.messages,
            // R66：`defs()` 已经是 `Vec<model::provider::ToolDef>`（registry.rs:143），
            // 这里没有跨层转换要做，逐字段搬运只会让人以为 risk 能在这里被加工。
            tools: self.deps.registry.defs(),
            temperature: None,
        }
    }
}

/// 只把前 `PREVIEW_CHARS` 个字符放进事件流：完整工具输出留在 history（那才是下一轮
/// 模型的输入），否则一次大页面抓取会同时压垮 IPC 与前端渲染。
/// 按**字符**切而不是按字节下标：这里的输入可能是中日韩文本，一个字符 3 字节，
/// `&text[..200]` 会在非边界上 panic。
fn preview_of(o: &ToolOutput) -> String {
    match o {
        ToolOutput::Text { text } => {
            let mut s: String = text.chars().take(PREVIEW_CHARS).collect();
            if text.chars().count() > PREVIEW_CHARS {
                s.push('…');
            }
            s
        }
        // 图片不进事件流：前端要的是一张已经落盘的图的引用，而那是切片 2 的事，
        // 这里先给一行可归因的占位描述，不把 base64 搬过 IPC 边界。
        ToolOutput::Image { mime, .. } => format!(
            "[图片 {}]",
            match mime {
                ImageMime::Png => "png",
                ImageMime::Jpeg => "jpeg",
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    //! Task 10 的六条路径（plan:2010 起）+ 控制器裁定 R56–R63 补上的
    //! 中断 / 配对 / 用量 / 空响应路径。Plan A 的生产工具表是**空的**
    //! （plan:1854：`echo` 不注册进生产），所以工具路径的真相只能在这里建立：
    //! 每条测试都用真注册表、真权限门、真滑窗，只有模型是脚本化的。
    use super::*;
    use crate::agent::context::ContextConfig;
    use crate::agent::events::{Decision, ErrorWhere, Scope, StopReason};
    use crate::agent::types::{ContentBlock, Message, Risk, Role, ToolOutput};
    use crate::model::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
    use crate::permission::gate::PermissionGate;
    use crate::tools::echo::{EchoTool, RiskOverrideTool};
    use crate::tools::registry::{Tool, ToolError, ToolMeta, ToolRegistry};
    use async_stream::stream;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // ---- 测试替身 ----

    /// 记录型事件出口：整条时间线只能从这里取证（生产用的是 `ipc.rs` 里的 TauriEmitter）。
    #[derive(Default)]
    struct VecSink(Mutex<Vec<AgentEvent>>);

    impl EventSink for VecSink {
        fn emit(&self, e: AgentEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    impl VecSink {
        fn events(&self) -> Vec<AgentEvent> {
            self.0.lock().unwrap().clone()
        }

        /// 事件的线上 `t` 标签序列。走 serde 而不是 match：前端读的就是这个标签。
        /// 多数测试对它做**精确**断言——`contains` 那种断言挡不住「多发一个事件」，
        /// 而 R60 的占位 usage、R64 的重复报错正好都是多发型 bug。
        fn kinds(&self) -> Vec<String> {
            self.events()
                .iter()
                .map(|e| {
                    serde_json::to_value(e).unwrap()["t"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        }

        fn done(&self) -> Option<StopReason> {
            self.events().into_iter().find_map(|e| match e {
                AgentEvent::Done { stop_reason } => Some(stop_reason),
                _ => None,
            })
        }

        /// `(where, message, fatal)`，按事件顺序。
        fn errors(&self) -> Vec<(ErrorWhere, String, bool)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::Error { at, message, fatal } => Some((at, message, fatal)),
                    _ => None,
                })
                .collect()
        }

        fn usage(&self) -> Vec<(u32, u32)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => Some((input_tokens, output_tokens)),
                    _ => None,
                })
                .collect()
        }

        fn permission_ids(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::PermissionRequest { id, .. } => Some(id),
                    _ => None,
                })
                .collect()
        }

        /// `(call_id, is_error, preview)`，按事件顺序：工具路径的成败与**顺序**都看这里。
        fn tool_results(&self) -> Vec<(String, bool, String)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::ToolResult {
                        call_id,
                        is_error,
                        preview,
                        ..
                    } => Some((call_id, is_error, preview)),
                    _ => None,
                })
                .collect()
        }

        fn texts(&self) -> String {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::TextDelta { text } => Some(text),
                    _ => None,
                })
                .collect()
        }
    }

    /// 模型侧取证点：每个 `ChatRequest` 在被调用那一刻记下。
    /// R57——「模型实际看到了什么」只能从模型这一侧拿，从 history 反推等于自证。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<ChatRequest>>>);

    impl Recorder {
        fn push(&self, r: ChatRequest) {
            self.0.lock().unwrap().push(r);
        }
        fn recorded(&self) -> Vec<ChatRequest> {
            self.0.lock().unwrap().clone()
        }
    }

    /// 每轮对话吐一组预录 delta；Plan B/C 的测试同样用它。
    struct ScriptedModel {
        rounds: Mutex<Vec<Vec<ModelDelta>>>,
        recorder: Recorder,
    }

    impl ScriptedModel {
        fn new(rounds: Vec<Vec<ModelDelta>>) -> Arc<Self> {
            Arc::new(Self {
                rounds: Mutex::new(rounds),
                recorder: Recorder::default(),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            // 夹具前提：轮数必须够循环用完。`remove(0)` 空了就 panic，
            // 而不是让循环安静地少跑一轮——那会把「触顶」测成「提前收尾」。
            let next = self.rounds.lock().unwrap().remove(0);
            self.recorder.push(req);
            Box::pin(futures::stream::iter(next.into_iter().map(Ok)))
        }
    }

    /// 一问就失败的模型。`ModelError::Api` = 已连通的 429（provider.rs 的分类表）。
    struct FailingModel {
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for FailingModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(req);
            Box::pin(futures::stream::iter(vec![Err(ModelError::Api(
                "429 rate limited".into(),
            ))]))
        }
    }

    /// 吐出 `head` 之后**永久挂住**的模型：模拟一个还连着的流式响应。
    /// 关键是 `ready` 的时机：`stream!` 的每次 `yield` 都把执行权交给消费者，
    /// 所以 `ready` 置位时最后一个 head 分片**已经抵达累加器**——
    /// 测试于是能在「半条回复 + 一个未执行的 tool_call 已经在 acc 里」的那一刻才按停止，
    /// 不靠 sleep 猜时间，也不会打断在流结束之后（那就不叫打断流式了）。
    struct HangingModel {
        head: Vec<ModelDelta>,
        ready: Arc<AtomicBool>,
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for HangingModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(req);
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

    /// 记账工具：Write 级，所以每次调用都得先过权限门——
    /// 「门没放行就绝不执行工具」这件事才有观察点（只看返回内容是证不出的：
    /// echo 的替身被拒后也会回显同样的文本）。
    #[derive(Clone, Default)]
    struct CountingTool(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl Tool for CountingTool {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "counted".into(),
                description: "记下自己被执行过几次".into(),
                input_schema: json!({"type": "object"}),
                risk: Risk::Write,
            }
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push(input.to_string());
            Ok(ToolOutput::text("执行了"))
        }
    }

    impl CountingTool {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    // ---- 装配 ----

    /// 一套完整装配：真注册表、真权限门、真滑窗，模型是脚本化的。
    /// 每个 rig 都注册 `echo`(Read) 与 `guarded`(Write)：guarded 不调用就不弹窗，
    /// 所以「Read 零弹窗」这类断言不受它影响，而需要 Ask 路径的测试可以直接调它。
    struct Rig {
        session: AgentSession,
        sink: Arc<VecSink>,
        gate: Arc<PermissionGate>,
        recorder: Recorder,
    }

    impl Rig {
        fn new(rounds: Vec<Vec<ModelDelta>>) -> Rig {
            Rig::with_context(ContextConfig::default(), rounds)
        }

        fn with_context(context: ContextConfig, rounds: Vec<Vec<ModelDelta>>) -> Rig {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            Rig::assembled(model, context, recorder, vec![])
        }

        /// 装配 + 一个记账工具（名字 `counted`，Write 级）。
        fn with_counted(rounds: Vec<Vec<ModelDelta>>) -> (Rig, Arc<CountingTool>) {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            let tool = Arc::new(CountingTool::default());
            let rig = Rig::assembled(
                model,
                ContextConfig::default(),
                recorder,
                vec![tool.clone()],
            );
            (rig, tool)
        }

        fn assembled(
            model: Arc<dyn ModelProvider>,
            context: ContextConfig,
            recorder: Recorder,
            extra_tools: Vec<Arc<dyn Tool>>,
        ) -> Rig {
            let (session, sink, gate) = parts(model, context, extra_tools);
            Rig {
                session,
                sink,
                gate,
                recorder,
            }
        }

        /// 挂住的模型：`ready` 置位 = head 已全部抵达累加器。
        fn hanging(head: Vec<ModelDelta>) -> (Rig, Arc<AtomicBool>) {
            let ready = Arc::new(AtomicBool::new(false));
            let recorder = Recorder::default();
            let model = Arc::new(HangingModel {
                head,
                ready: ready.clone(),
                recorder: recorder.clone(),
            });
            (
                Rig::assembled(model, ContextConfig::default(), recorder, vec![]),
                ready,
            )
        }

        fn failing() -> Rig {
            let recorder = Recorder::default();
            let model = Arc::new(FailingModel {
                recorder: recorder.clone(),
            });
            Rig::assembled(model, ContextConfig::default(), recorder, vec![])
        }

        async fn run(&mut self, text: &str) -> StopReason {
            self.session.run(user(text)).await
        }

        fn reqs(&self) -> Vec<ChatRequest> {
            self.recorder.recorded()
        }
    }

    fn parts(
        model: Arc<dyn ModelProvider>,
        context: ContextConfig,
        extra_tools: Vec<Arc<dyn Tool>>,
    ) -> (AgentSession, Arc<VecSink>, Arc<PermissionGate>) {
        let sink = Arc::new(VecSink::default());
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(RiskOverrideTool(Risk::Write)));
        for t in extra_tools {
            registry.register(t);
        }
        let gate = Arc::new(PermissionGate::new().with_sink(sink.clone()));
        let session = AgentSession::new(SessionDeps {
            model,
            registry,
            gate: gate.clone(),
            sink: sink.clone(),
            context: ContextBuilder::new(context),
        });
        (session, sink, gate)
    }

    // ---- 夹具 ----

    fn user(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: text.into() }]
    }

    /// 纯文本回复：按片段分 delta（逐字符只会把同一条路径重复几十遍）。
    fn deltas(parts: &[&str]) -> Vec<ModelDelta> {
        parts
            .iter()
            .map(|p| ModelDelta::Text((*p).to_string()))
            .collect()
    }

    /// 一轮工具调用，`calls` 是 (工具名, arguments 原文)，index 升序。
    /// **id 每轮唯一**（R57）：全都复用 `"c1"` 的话，「找得到对应调用」的断言会
    /// latch 到早先那次调用上，配对真的错了也照样绿。
    fn tool_round(round: usize, calls: &[(&str, &str)]) -> Vec<ModelDelta> {
        let mut out = Vec::new();
        for (index, (name, args)) in calls.iter().enumerate() {
            let id = format!("r{round}c{index}");
            out.push(ModelDelta::ToolCallStart {
                index: index as u32,
                id: Some(id),
                name: Some((*name).to_string()),
            });
            out.push(ModelDelta::ToolCallArgs {
                index: index as u32,
                fragment: (*args).to_string(),
            });
        }
        out
    }

    /// 轮询等第 n 条 `PermissionRequest` 落地。用 sleep 而不是 `yield_now`：
    /// 被等的那一轮正跑在**同一个** current-thread 运行时上，纯忙等要么饿死它、
    /// 要么在 `await` 点之前读到空表（gate.rs 的同类循环是跨任务 spawn，那里够用）。
    /// 5 秒上限，超时即 panic——挂死不是通过。
    async fn wait_requests(sink: &VecSink, n: usize) -> Vec<String> {
        for _ in 0..500 {
            let ids = sink.permission_ids();
            if ids.len() >= n {
                return ids;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "等不到第 {n} 条 PermissionRequest，已有事件 {:?}",
            sink.kinds()
        );
    }

    /// 后台应答第一条弹窗。`run()` 会在权限门上阻塞，所以应答任务必须**先 spawn**。
    fn answer_first(
        gate: &Arc<PermissionGate>,
        sink: &Arc<VecSink>,
        d: Decision,
    ) -> tokio::task::JoinHandle<bool> {
        let gate = gate.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let id = wait_requests(&sink, 1).await.remove(0);
            gate.resolve(&id, d)
        })
    }

    /// 请求里所有工具结果消息的 `(call_id, is_error, 文本)`。
    fn tool_results_of(msgs: &[Message]) -> Vec<(String, bool, String)> {
        let mut out = Vec::new();
        for m in msgs {
            for b in &m.blocks {
                if let ContentBlock::ToolResult {
                    call_id,
                    is_error,
                    output,
                } = b
                {
                    let text = match output {
                        ToolOutput::Text { text } => text.clone(),
                        ToolOutput::Image { .. } => "<image>".into(),
                    };
                    out.push((call_id.clone(), *is_error, text));
                }
            }
        }
        out
    }

    // ---- R57 的不变量：模型看到的每个请求都得配对 ----

    /// 可执行形式的 R56/R57 不变量，作用在**模型实际收到的请求**上：
    /// 1. 每个 `ToolResult` 的 `call_id` 必须匹配一条**在前的** assistant `ToolCall`；
    /// 2. 带 N 个调用的 assistant 消息必须紧跟 N 条结果，id 集合完全相同
    ///    （结果没回填 = 下一轮模型看不见结果，这条直接红）；
    /// 3. 调用 id 在单个请求内唯一——这是前两条不 latch 的**夹具前提**，
    ///    写进断言而不是只写在注释里，夹具漂移时是这条先红。
    fn assert_paired(label: &str, req: &ChatRequest) {
        let msgs = &req.messages;
        // 当前 assistant 轮还欠着的结果 id；空 = 没有待回填的调用。
        let mut owed: Vec<String> = Vec::new();
        let mut owed_at = 0usize;
        let mut seen: Vec<String> = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            let calls: Vec<String> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            let results: Vec<String> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                })
                .collect();

            for id in &calls {
                assert!(
                    !seen.contains(id),
                    "{label} 第 {i} 条：调用 id {id} 重复出现——「找得到对应调用」的断言会 latch 到早先那次，什么都测不出（夹具前提）"
                );
                seen.push(id.clone());
            }
            for id in &results {
                assert!(
                    seen.contains(id),
                    "{label} 第 {i} 条：工具结果 {id} 前面没有任何匹配的调用，这个请求上线就是 400"
                );
            }

            let has_calls = !calls.is_empty();
            let has_results = !results.is_empty();

            if has_calls {
                assert!(
                    owed.is_empty(),
                    "{label} 第 {i} 条：上一条 assistant（第 {owed_at} 条）的调用 {owed:?} 还没等到结果，又来了一条带调用的消息"
                );
                owed = calls;
                owed_at = i;
            }
            if has_results {
                assert!(
                    !owed.is_empty(),
                    "{label} 第 {i} 条：结果 {results:?} 不属于任何待回填的调用（重复回填或切口切错了）"
                );
                for id in &results {
                    assert!(
                        owed.iter().any(|c| c == id),
                        "{label}：第 {i} 条结果 {id} 不属于第 {owed_at} 条 assistant 的调用 {owed:?}"
                    );
                    owed.retain(|c| c != id);
                }
            }
            // 严格相邻：调用与它的结果之间不许插别的消息（system 备注恒排在最前，不会走到这里）。
            if !has_calls && !has_results && !owed.is_empty() && m.role != Role::System {
                panic!(
                    "{label} 第 {i} 条：第 {owed_at} 条 assistant 的调用 {owed:?} 还没等到结果，中间插进了这条消息"
                );
            }
        }
        assert!(
            owed.is_empty(),
            "{label}：第 {owed_at} 条 assistant 的调用 {owed:?} 到请求结尾都没有配对结果——history 结束在 tool_call 上，下一轮就是 mid-session 400（R56）"
        );
    }

    /// 整段对话跑完后，把不变量 over 每一个模型收到过的请求。
    fn assert_every_request_paired(rig: &Rig) {
        for (n, req) in rig.reqs().iter().enumerate() {
            assert_paired(&format!("请求 #{n}"), req);
        }
    }

    // ---- 路径 1：纯文本 ----

    #[tokio::test]
    async fn plain_text_turn_emits_turn_start_delta_done_end_turn() {
        let mut rig = Rig::new(vec![deltas(&["你好", "，我是 Qwen Do"])]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);

        // 精确序列：多一个事件就红。特别是 `usage`——plan Step 2 里那句
        // `Usage{0,0}` 占位 emit 会把前端的 token 计数每轮归零（R60 要求删掉）。
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "text_delta", "text_delta", "done"]
        );
        assert_eq!(rig.sink.texts(), "你好，我是 Qwen Do");
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_eq!(rig.sink.usage(), Vec::<(u32, u32)>::new());

        // 纯文本轮只有 user + assistant 两条（Step 3 修正了 plan 里写成 3 的断言）。
        let hist = rig.session.history();
        assert_eq!(hist.len(), 2, "{hist:?}");
        assert_eq!(hist[1].text_of(), "你好，我是 Qwen Do");

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].messages.len(), 2, "system + user");
        assert_eq!(reqs[0].messages[0].role, Role::System);
        // R66：注册表的声明原样进请求，不在这里做字段搬运。
        let names: Vec<String> = reqs[0].tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, ["echo", "guarded"], "工具声明必须来自注册表");
        assert_paired("纯文本请求", &reqs[0]);
    }

    // ---- 路径 2：工具调用 + 回填 + 第二轮 ----

    #[tokio::test]
    async fn tool_call_is_executed_and_backfilled_then_second_round() {
        let mut rig = Rig::new(vec![
            tool_round(0, &[("echo", r#"{"text":"hi"}"#)]),
            deltas(&["拿到结果了"]),
        ]);
        assert_eq!(rig.run("用工具").await, StopReason::EndTurn);

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ],
            "Read 级工具必须零弹窗（spec 5.1），且回填后要再发一次 turn_start 问第二轮"
        );
        assert_eq!(
            rig.sink.tool_results(),
            vec![("r0c0".into(), false, "hi".into())]
        );
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_eq!(rig.sink.texts(), "拿到结果了");

        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            4,
            "user, assistant(ToolCall), tool, assistant：{hist:?}"
        );
        assert_eq!(
            tool_results_of(hist),
            vec![("r0c0".into(), false, "hi".into())]
        );

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2, "工具结果回填后必须再问一轮");
        assert_eq!(reqs[0].messages.len(), 2, "第一次调用时模型还看不到工具");
        assert_eq!(tool_results_of(&reqs[0].messages), vec![]);
        assert_eq!(
            reqs[1].messages.len(),
            4,
            "system + user + assistant + tool"
        );
        assert_eq!(
            tool_results_of(&reqs[1].messages),
            vec![("r0c0".into(), false, "hi".into())],
            "模型必须真的收到那条工具结果，否则第二轮是凭空回答"
        );
        assert_every_request_paired(&rig);
    }

    #[tokio::test]
    async fn two_parallel_calls_each_get_exactly_one_backfilled_result() {
        // 只测一个调用的话，「跑完第一个就 break」这种退化是绿的——
        // 而它留下的正是一条没有结果的 tool_call（R57 的「匹配数量」那一半）。
        let mut rig = Rig::new(vec![
            tool_round(
                0,
                &[
                    ("echo", r#"{"text":"第一"}"#),
                    ("echo", r#"{"text":"第二"}"#),
                ],
            ),
            deltas(&["两个都拿到了"]),
        ]);
        assert_eq!(rig.run("并行调两个").await, StopReason::EndTurn);

        // 顺序执行（spec 4.6）：第 0 个调用的结果必须先于第 1 个调用出现。
        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ]
        );
        assert_eq!(
            rig.sink.tool_results(),
            vec![
                ("r0c0".into(), false, "第一".into()),
                ("r0c1".into(), false, "第二".into())
            ]
        );
        assert_eq!(
            tool_results_of(rig.session.history()),
            vec![
                ("r0c0".into(), false, "第一".into()),
                ("r0c1".into(), false, "第二".into())
            ]
        );
        assert_every_request_paired(&rig);
    }

    // ---- 路径 3：用户拒绝 ----

    #[tokio::test]
    async fn denied_tool_fills_error_result_and_loop_continues() {
        let mut rig = Rig::new(vec![
            tool_round(0, &[("guarded", r#"{"text":"x"}"#)]),
            deltas(&["被拒了，我换法子"]),
        ]);
        let answered = answer_first(&rig.gate, &rig.sink, Decision::deny("我不想让你写"));

        assert_eq!(
            rig.run("写个文件").await,
            StopReason::EndTurn,
            "拒绝不是错误：循环必须继续到模型自己收尾（spec 4.6）"
        );
        assert!(
            answered.await.unwrap(),
            "弹窗没进 pending：拒绝路径压根没经过权限门"
        );

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "permission_request",
                "permission_resolved",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ]
        );
        // R64：工具这一侧的事实只由 tool_result 承载，不再补一条 Error{at:Tool}。
        assert_eq!(rig.sink.errors(), Vec::<(ErrorWhere, String, bool)>::new());

        // R59：内容就是拒绝理由本身，外面不许再套 `ToolError::Failed`
        // （它的 Display 前缀是「工具执行失败: 」，套上去模型会读到双重包装）。
        let refused = vec![(
            "r0c0".to_string(),
            true,
            "用户拒绝执行：我不想让你写".to_string(),
        )];
        assert_eq!(tool_results_of(rig.session.history()), refused);
        assert_eq!(rig.sink.tool_results(), refused);
        assert_eq!(rig.sink.texts(), "被拒了，我换法子");

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            tool_results_of(&reqs[1].messages),
            refused,
            "模型必须看得见这条拒绝，才能自己换路"
        );
        assert_every_request_paired(&rig);
    }

    #[tokio::test]
    async fn an_allowed_call_executes_once_and_a_denied_call_not_at_all() {
        // 「门没放行就不执行」只看返回内容证不出来（guarded 被拒后也会回显同样的字），
        // 所以这里用一次真的执行计数。
        let (mut rig, tool) = Rig::with_counted(vec![
            tool_round(0, &[("counted", r#"{"n":1}"#)]),
            deltas(&["好"]),
        ]);
        let answered = answer_first(&rig.gate, &rig.sink, Decision::allow(Scope::Once));
        assert_eq!(rig.run("放行").await, StopReason::EndTurn);
        assert!(answered.await.unwrap(), "Write 级调用必须弹窗等人");
        assert_eq!(
            tool.calls(),
            vec![r#"{"n":1}"#.to_string()],
            "放行后恰好执行一次，参数原样抵达工具"
        );
        assert_eq!(
            tool_results_of(rig.session.history()),
            vec![("r0c0".into(), false, "执行了".into())]
        );

        let (mut rig2, tool2) = Rig::with_counted(vec![
            tool_round(0, &[("counted", r#"{"n":2}"#)]),
            deltas(&["被拒了"]),
        ]);
        let answered = answer_first(&rig2.gate, &rig2.sink, Decision::deny("不许"));
        assert_eq!(rig2.run("拒绝").await, StopReason::EndTurn);
        assert!(answered.await.unwrap());
        assert!(
            tool2.calls().is_empty(),
            "被拒的工具仍然执行了：权限门只是摆设，实际 {:?}",
            tool2.calls()
        );
        assert_eq!(
            tool_results_of(rig2.session.history()),
            vec![("r0c0".into(), true, "用户拒绝执行：不许".into())]
        );
        assert_every_request_paired(&rig2);
    }

    // ---- 路径 4：注册表报错 ----

    #[tokio::test]
    async fn an_unknown_tool_reports_only_a_tool_result_event() {
        let mut rig = Rig::new(vec![
            tool_round(0, &[("nope", "{}")]),
            deltas(&["没这个工具，我自己来"]),
        ]);
        // 未知工具按 Exec（最严一档，registry.rs:137），所以这里必须有人应答。
        let answered = answer_first(&rig.gate, &rig.sink, Decision::allow(Scope::Once));
        assert_eq!(rig.run("调个不存在的").await, StopReason::EndTurn);
        assert!(answered.await.unwrap());

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "permission_request",
                "permission_resolved",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ]
        );
        let got = rig.sink.tool_results();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1, "工具失败必须由 tool_result 的 is_error 承载");
        assert!(
            got[0].2.contains("工具 nope 未注册"),
            "报错文本必须可归因，实际 {:?}",
            got[0].2
        );
        // R64：同一个事实不许再由 Error{at:Tool} 报第二遍（前端会长出两行）。
        assert_eq!(rig.sink.errors(), Vec::<(ErrorWhere, String, bool)>::new());
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_every_request_paired(&rig);
    }

    // ---- 路径 5：用量与空响应 ----

    #[tokio::test]
    async fn a_usage_delta_reaches_the_ui_as_a_usage_event() {
        // R60：`AssistantAcc::apply` 会**静默丢弃** Usage 里的 token
        // （assistant_acc.rs:37-39 的文档就是这句），循环是唯一能把它们抢救回来的地方。
        // 这条测试是「统计静默为 0」的唯一防线。
        let mut rig = Rig::new(vec![vec![
            ModelDelta::Text("答".into()),
            ModelDelta::Usage {
                input_tokens: 1234,
                output_tokens: 56,
            },
        ]]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        assert_eq!(rig.sink.usage(), vec![(1234, 56)]);
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "text_delta", "usage", "done"],
            "usage 事件只能来自流里的那个 Usage delta"
        );
        assert_paired("用量请求", &rig.reqs()[0]);
    }

    #[tokio::test]
    async fn an_empty_model_response_pushes_nothing_and_reports_the_loop() {
        // R58：`AssistantAcc::finish()` 对空流交回一条**零块**消息，
        // 而零块的 assistant 消息发不出去（assistant_acc.rs:75-79）：
        // 既不进 history，也不许被下一轮当成输入。
        let mut rig = Rig::new(vec![vec![]]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        assert_eq!(
            rig.session.history().len(),
            1,
            "只留那条 user 消息：空 assistant 轮不进 history，也不补工具结果：{:?}",
            rig.session.history()
        );
        assert_eq!(
            rig.sink.errors(),
            vec![(ErrorWhere::Loop, "模型返回空响应".into(), false)]
        );
        assert_eq!(rig.sink.kinds(), ["turn_start", "error", "done"]);
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_eq!(rig.reqs().len(), 1, "空响应不该再问第二遍");
    }

    // ---- 路径 6：模型错误 ----

    #[tokio::test]
    async fn a_model_error_ends_the_turn_with_a_fatal_model_error() {
        // R61：Plan A 里模型错误就收尾，所以 fatal 是 true。
        // plan 的测试名叫 non_fatal 而 body 发的是 fatal:true —— 按 body 走，名字跟着改。
        let mut rig = Rig::failing();
        assert_eq!(rig.run("hi").await, StopReason::Error);
        assert_eq!(
            rig.sink.errors(),
            vec![(
                ErrorWhere::Model,
                "模型返回错误: 429 rate limited".into(),
                true
            )]
        );
        assert_eq!(rig.sink.kinds(), ["turn_start", "error", "done"]);
        assert_eq!(rig.sink.done(), Some(StopReason::Error));
        assert_eq!(
            rig.session.history().len(),
            1,
            "错误在第一个 delta 之前就到了：没有任何内容已经流给 UI，所以不提交空轮"
        );
        assert_eq!(rig.reqs().len(), 1);
    }

    // ---- 触顶 ----

    #[tokio::test]
    async fn max_turns_cap_is_reached_not_silently_stopped() {
        let rounds: Vec<Vec<ModelDelta>> = (0..30usize)
            .map(|n| tool_round(n, &[("echo", r#"{"text":"loop"}"#)]))
            .collect();
        let mut rig = Rig::new(rounds);
        let cap = SessionConfig::default().max_turns;
        assert_eq!(
            rig.run("无限循环").await,
            StopReason::MaxTurns,
            "默认 max_turns={cap}，给了 30 轮必须触顶"
        );
        assert_eq!(
            rig.sink.done(),
            Some(StopReason::MaxTurns),
            "触顶必须显式告知，不可静默"
        );
        assert_eq!(
            rig.reqs().len(),
            cap as usize,
            "每轮恰好问模型一次，触顶即停"
        );
        assert_eq!(rig.sink.kinds().first().unwrap(), "turn_start");
        assert_eq!(rig.sink.kinds().last().unwrap(), "done");
        assert_eq!(
            rig.sink
                .kinds()
                .iter()
                .filter(|k| *k == "turn_start")
                .count(),
            cap as usize
        );

        // 触顶时 history 也必须停在成对状态：最后一轮的调用已经有结果。
        assert_every_request_paired(&rig);
        let hist = rig.session.history();
        assert_eq!(
            hist.last().map(|m| &m.role),
            Some(&Role::User),
            "最后一条必须是工具结果，不能是带 tool_call 的 assistant"
        );
        assert_eq!(
            tool_results_of(hist).len(),
            cap as usize,
            "每轮一个调用一个结果"
        );
    }

    // ---- 取消 ----

    #[tokio::test]
    async fn cancel_before_run_returns_interrupted_and_emits_only_done() {
        // R62：plan 那条测试在 run() **之前**取消，所以它测的其实只是这条路径；
        // 真正的「打断流式」由下面那条负责。这里断言精确到「只有一个 done」。
        let mut rig = Rig::new(vec![deltas(&["不会用到"])]);
        rig.session.cancel_handle().cancel();
        assert_eq!(rig.run("hi").await, StopReason::Interrupted);
        assert_eq!(rig.sink.kinds(), ["done"]);
        assert_eq!(rig.sink.done(), Some(StopReason::Interrupted));
        assert!(
            rig.session.history().is_empty(),
            "取消发生在开始之前：这一轮压根没发生，连 user 消息都不该留下：{:?}",
            rig.session.history()
        );
        assert!(rig.reqs().is_empty(), "取消在开始之前：一次都不该问模型");
    }

    #[tokio::test]
    async fn a_cancelled_session_refuses_a_second_turn() {
        // R63：CancellationToken 是一次性的，`run()` 里**不许**复位
        // （那会让前端手里那个 handle 静默失效，比原来的竞态更糟）。
        // 这里钉住合同：取消后的会话对象只多一个 done，别的都不动，
        // 想继续聊就带着 history() 换一个新会话（Task 13 每轮新建，SessionDeps 全是 Arc）。
        let mut rig = Rig::new(vec![deltas(&["第一轮"])]);
        assert_eq!(rig.run("一").await, StopReason::EndTurn);
        let before = rig.sink.kinds();
        let hist_before = rig.session.history().len();

        rig.session.cancel_handle().cancel();
        assert_eq!(rig.run("二").await, StopReason::Interrupted);
        assert_eq!(&rig.sink.kinds()[before.len()..], ["done"]);
        assert_eq!(
            rig.session.history().len(),
            hist_before,
            "第二次 run 不许留下任何消息"
        );
        assert_eq!(rig.reqs().len(), 1, "取消后不再问模型");
    }

    #[tokio::test]
    async fn interrupting_mid_stream_keeps_the_partial_reply_and_backfills_the_open_tool_call() {
        // R56 + R62：打断流式时，已经流给 UI 的半句话必须进 history
        // （否则 Task 11 重放后文字凭空消失：UI 有、库里没有＝两套真相）；
        // 而那条尚未执行的 tool_call 必须补一条结果——光提交 assistant(ToolCall)
        // 会让下一轮撞上 mid-session 400，正是 Task 8 在窗口边界修掉的那个 bug 从这里复发。
        let (mut rig, ready) = Rig::hanging(vec![
            ModelDelta::Text("前半句".into()),
            ModelDelta::Text("还没说完".into()),
            ModelDelta::ToolCallStart {
                index: 0,
                id: Some("hangc0".into()),
                name: Some("echo".into()),
            },
            ModelDelta::ToolCallArgs {
                index: 0,
                fragment: r#"{"text":"永远不会执行"}"#.into(),
            },
        ]);
        let cancel = rig.session.cancel_handle();
        let canceller = tokio::spawn(async move {
            for _ in 0..500 {
                if ready.load(Ordering::SeqCst) {
                    cancel.cancel();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("模型的头几个 delta 一直没抵达累加器，打断点没能建立");
        });

        assert_eq!(rig.run("说一半").await, StopReason::Interrupted);
        canceller.await.unwrap();

        // UI 确实看到了半句话——这才叫打断流式，而不是「压根没开始」。
        assert_eq!(rig.sink.texts(), "前半句还没说完");
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "text_delta", "text_delta", "done"],
            "工具没执行，所以不该有 tool_call / tool_result / permission_request 事件"
        );
        assert_eq!(rig.sink.done(), Some(StopReason::Interrupted));

        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            3,
            "user + 半句 assistant + 补出来的结果：{hist:?}"
        );
        assert_eq!(
            hist[1].text_of(),
            "前半句还没说完",
            "半句话不能只活在 UI 里"
        );
        assert_eq!(
            tool_results_of(hist),
            vec![("hangc0".into(), true, "本轮被打断，工具未执行".into())]
        );
        assert_eq!(
            hist.last().map(|m| &m.role),
            Some(&Role::User),
            "history 结尾必须是工具结果，不能是带 tool_call 的 assistant"
        );
        assert_eq!(rig.reqs().len(), 1);

        // R56 的动机要真的验一遍：接手这段 history 的下一个会话，
        // 发给模型的第一个请求依然配对（Task 13 每轮新建会话 + restore）。
        let carried = rig.session.history().to_vec();
        let mut cont = Rig::new(vec![deltas(&["接着说"])]);
        cont.session.restore(carried);
        assert_eq!(cont.run("继续").await, StopReason::EndTurn);
        assert_eq!(cont.sink.kinds(), ["turn_start", "text_delta", "done"]);
        assert_eq!(cont.reqs().len(), 1);
        assert_paired("打断后接手的请求", &cont.reqs()[0]);
    }

    // ---- R57 的后一半：配对不变量必须扛过滑窗切口 ----

    /// 60 条成对历史 + 极小的 `window_messages`：切口一定落在成对的 tool 轮里
    /// （Task 8 靠 advance 跳过孤儿结果）。窗口边界和循环边界是同一个 400 的两个入口，
    /// 所以这条不变量得在**模型实际看到的请求**上再验一遍。
    #[tokio::test]
    async fn a_windowed_long_history_still_pairs_every_tool_result_with_its_call() {
        let mut carried: Vec<Message> = Vec::new();
        for n in 0..30usize {
            carried.push(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: format!("r{n}c0"),
                    name: "echo".into(),
                    input: json!({"text": "ok"}),
                }],
            });
            carried.push(Message::tool(
                &format!("r{n}c0"),
                ToolOutput::text("ok"),
                false,
            ));
        }
        assert_eq!(carried.len(), 60);

        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 100_000,
            window_messages: 5,
        };
        let mut rig = Rig::with_context(
            cfg,
            vec![
                tool_round(30, &[("echo", r#"{"text":"新结果"}"#)]),
                deltas(&["看到了"]),
            ],
        );
        rig.session.restore(carried);
        assert_eq!(rig.run("接着用工具").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0].messages.len() <= 6,
            "窗口 5 条 + system，切口必须真的裁掉了 60 条历史，实际 {}",
            reqs[0].messages.len()
        );
        assert!(
            reqs[1].messages.len() <= 6,
            "第二轮同样受窗口约束，实际 {}",
            reqs[1].messages.len()
        );
        // 新那一轮的调用与结果必须都在请求里（窗口不许把刚跑完的这一对裁掉）。
        assert!(
            tool_results_of(&reqs[1].messages)
                .iter()
                .any(|(id, _, text)| id == "r30c0" && text == "新结果"),
            "本轮的工具结果没进第二个请求：模型第二轮是凭空回答，{:?}",
            tool_results_of(&reqs[1].messages)
        );
        for (n, r) in reqs.iter().enumerate() {
            assert_paired(&format!("窗口后的请求 #{n}"), r);
        }
        assert_eq!(
            rig.session.history().len(),
            64,
            "窗口只裁请求，不裁 history：60 条 + user + assistant(调用) + tool(结果) + assistant(收尾)"
        );
    }

    /// `SessionDeps.context` 必须是活的：循环每轮都拿它组装请求，而不是把整个
    /// history 原样发给模型（那会让 Task 8 的窗口与预算在循环里等于没接线）。
    #[tokio::test]
    async fn the_context_window_governs_what_the_model_is_shown() {
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 100_000,
            window_messages: 3,
        };
        let mut rig = Rig::with_context(cfg, vec![deltas(&["只看了最近三条"])]);
        let carried: Vec<Message> = (0..20)
            .map(|i| Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: format!("第 {i} 条"),
                }],
            })
            .collect();
        rig.session.restore(carried);
        assert_eq!(rig.run("最新一条").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].messages.len(), 4, "system + 窗口里的 3 条");
        assert_eq!(reqs[0].messages[1].text_of(), "第 18 条");
        assert_eq!(reqs[0].messages.last().unwrap().text_of(), "最新一条");
        assert_eq!(
            rig.session.history().len(),
            22,
            "20 条恢复的 + 这一轮的 user + assistant：history 不被窗口裁"
        );
    }

    // ---- 预览：一次大输出不许压垮 IPC 与前端 ----

    #[tokio::test]
    async fn a_long_tool_output_reaches_the_ui_as_a_bounded_preview() {
        let long = "测".repeat(500);
        let args = format!(r#"{{"text":"{long}"}}"#);
        let mut rig = Rig::new(vec![
            tool_round(0, &[("echo", args.as_str())]),
            deltas(&["读完了"]),
        ]);
        assert_eq!(rig.run("读一大段").await, StopReason::EndTurn);

        let (id, is_error, preview) = rig.sink.tool_results().into_iter().next().unwrap();
        assert_eq!((id.as_str(), is_error), ("r0c0", false));
        assert_eq!(preview.chars().count(), 201, "200 个字符 + 一个省略号");
        assert!(preview.ends_with('…'), "截断必须留标记：{preview}");
        assert!(
            !preview.contains(&long),
            "整块输出被搬进事件流了（完整内容只该留在 history）"
        );
        assert_eq!(
            tool_results_of(rig.session.history())[0].2,
            long,
            "history 里必须是完整输出——那才是下一轮模型的输入"
        );
        assert_every_request_paired(&rig);
    }

    // ---- Task 13 的形状：session 与它的 run future 都得是 Send ----

    #[tokio::test]
    async fn a_turn_runs_inside_a_spawned_task() {
        // `tokio::spawn` 要求 future 是 `Send + 'static`：这是个编译期事实，
        // 一旦有人往 SessionDeps 里塞 `Rc`，红的就是这里，而不是到装配那天才报
        // （gate.rs / registry.rs 各自也有一条同类的 Send 测试）。
        let rig = Rig::new(vec![deltas(&["在别的任务里回答"])]);
        let Rig {
            mut session,
            sink,
            recorder,
            ..
        } = rig;
        let out = tokio::spawn(async move {
            let r = session.run(user("spawn")).await;
            (r, session.history().to_vec())
        })
        .await
        .unwrap();
        assert_eq!(out.0, StopReason::EndTurn);
        assert_eq!(out.1.len(), 2);
        assert_eq!(sink.texts(), "在别的任务里回答");
        assert_eq!(recorder.recorded().len(), 1);
    }
}
