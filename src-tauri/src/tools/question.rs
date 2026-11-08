//! question（G20）：模型在运行中向用户提多选题，答案回到对话（对齐 OpenCode 的
//! 同名工具）。
//!
//! # 数据通路
//!
//! execute 生成 uuid → 把 oneshot sender 登记进挂起表 → 经 `ctx.events` 发出
//! [`AgentEvent::Question`]（**不经权限门——提问不是执行**，工具是 Read 档）→
//! 前端渲染候选 → 用户的选择经 ipc 的 `question_respond(id, choice)` 命令回到
//! [`QuestionTool::resolve`] → oneshot 把答案送回挂起的 execute → 输出
//! `用户选择了：{answer}` 进对话。
//!
//! # 为什么是进程级共享实例（`shared()`）
//!
//! 产品语义：进程内只有一个提问面板。`build_production_registry` 每轮会话都可能
//! 被重建（测试里每个用例一份），而挂起表登记的是「正在等哪一道题」—— 若每个
//! 注册表各持一个 `QuestionTool`，A 轮挂起的题在 B 轮的实例上根本找不到，用户的
//! 选择就永远送不回去。所以挂起表必须跨装配共享：`shared()` 内部 `OnceLock`，
//! 首次构造者成为全局实例，此后所有注册表拿到的都是同一个。测试需要隔离时直接
//! 构造私有实例（`new` 不出模块），互不干扰。
use crate::agent::events::AgentEvent;
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::oneshot;

pub const NAME: &str = "question";

/// 选项数量下限。单选题给一个选项没有意义（答案恒定，不该问）。
const MIN_OPTIONS: usize = 2;
/// 选项数量上限。超过 6 个候选人在界面上已经滚屏，多选题会退化成菜单。
const MAX_OPTIONS: usize = 6;

pub struct QuestionTool {
    /// 挂起表：题目 id → 通往对应 execute 的应答通道。
    /// std Mutex 够用：临界区只有 insert/remove，不跨 await 持锁。
    pending: Mutex<HashMap<String, oneshot::Sender<String>>>,
}

impl QuestionTool {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 进程级共享实例（理由见文件头「为什么是进程级共享实例」）。
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<QuestionTool>> = OnceLock::new();
        SHARED.get_or_init(|| Arc::new(QuestionTool::new())).clone()
    }

    /// 把用户的选择送回挂起的 execute（ipc `question_respond` 命令的唯一后端）。
    ///
    /// 返回值是「答案确实送到了」：未知 id（题目已取消/从来没问过/拼写错）与
    /// 接收端已消失（execute 那一侧已被取消收尾）都是 `false` —— 静默失败，
    /// 不 panic：应答与提问是两个并发的用户动作，迟到一步是正常竞态不是故障。
    pub fn resolve(&self, id: &str, answer: String) -> bool {
        // 先摘再发：id 只许应答一次，第二次 resolve 同一道题自然落 Unknown。
        let tx = crate::lock::guard(&self.pending).remove(id);
        match tx {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

#[async_trait]
impl Tool for QuestionTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: "在需要用户拿主意时向用户提出多选题并等待回答：先在对话里说清你在\
                          问什么、为什么问，再用本工具列出候选。返回「用户选择了：X」。\
                          options 必须给 2..=6 个非空字符串，别用本工具问是非题之外无法\
                          列举的事。提问是只读操作（Read 档），不执行任何东西、不会弹权限窗。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "要问的问题，一句话说清"
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "maxItems": 6,
                        "description": "2..=6 个候选答案，每个都是非空字符串"
                    }
                },
                "required": ["question", "options"]
            }),
            // Read：出站只有一条事件（提问），不碰任何执行面 ⇒ 零弹窗。
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // 入参校验全部排在发事件之前：没有事件出口的会话里，坏入参也要给出
        // 入参自己的错（BadInput），而不是先撞上「不支持 question」的会话级错。
        let question = input
            .get("question")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::BadInput(
                    "question 需要非空字符串参数 question（要问的问题）".to_string(),
                )
            })?;
        let options: Vec<String> = input
            .get("options")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                ToolError::BadInput("question 需要数组参数 options（2..=6 个候选答案）".to_string())
            })?
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        ToolError::BadInput(
                            "options 每一项都必须是非空字符串（纯空白也算空）".to_string(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&options.len()) {
            return Err(ToolError::BadInput(format!(
                "options 需要 {MIN_OPTIONS}..={MAX_OPTIONS} 个候选，实得 {} 个",
                options.len()
            )));
        }

        let Some(sink) = &ctx.events else {
            return Err(ToolError::Failed(
                "当前会话不支持 question（没有事件出口）".to_string(),
            ));
        };
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<String>();
        crate::lock::guard(&self.pending).insert(id.clone(), tx);
        sink.emit(AgentEvent::Question {
            id: id.clone(),
            question: question.to_string(),
            options,
        });
        // biased + 取消臂在前：用户按停止的那一刻，取消分支先被轮询，
        // 不必等应答通道先就绪才检查取消。
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                // 摘除挂起项：取消之后迟到的 resolve 必须是 false（答案无处可去），
                // 留着死通道只会让挂起表越积越大。
                crate::lock::guard(&self.pending).remove(&id);
                Err(ToolError::Cancelled)
            }
            ans = rx => match ans {
                Ok(answer) => Ok(ToolOutput::text(format!("用户选择了：{answer}"))),
                // 接收端活着时这条只在「工具实例被丢弃」时发生（挂起表随实例消失）。
                // 不是取消也不是用户拒绝，如实报失败。
                Err(_) => Err(ToolError::Failed(
                    "提问面板已被丢弃，应答通道关闭".to_string(),
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::EventSink;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    /// 记录型事件出口（与 session.rs/gapcheck 的同名替身同形状；自含一份）。
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
    }

    fn ctx(events: Option<Arc<dyn EventSink>>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: vec![],
            write_root: None,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: None,
            events,
        }
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    /// 等到 sink 里出现第一条 Question 事件（execute 在另一个任务里跑）。
    async fn wait_question(sink: &Arc<VecSink>) -> (String, String, Vec<String>) {
        for _ in 0..500 {
            if let Some(e) = sink.events().into_iter().find_map(|e| match e {
                AgentEvent::Question {
                    id,
                    question,
                    options,
                } => Some((id, question, options)),
                _ => None,
            }) {
                return e;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("等不到 Question 事件");
    }

    fn input(question: &str, options: &[&str]) -> Value {
        json!({ "question": question, "options": options })
    }

    // ———————— 注册形状 ————————

    #[test]
    fn meta_pins_name_risk_and_schema() {
        let m = QuestionTool::new().meta();
        assert_eq!(m.name, NAME);
        assert_eq!(NAME, "question");
        assert_eq!(m.risk, Risk::Read, "提问不是执行 ⇒ 只读档，零弹窗");
        assert_eq!(
            m.input_schema,
            json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "要问的问题，一句话说清"
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "maxItems": 6,
                        "description": "2..=6 个候选答案，每个都是非空字符串"
                    }
                },
                "required": ["question", "options"]
            }),
            "发给模型的 schema 必须是声明的那一份"
        );
    }

    #[test]
    fn shared_returns_the_same_process_wide_instance() {
        assert!(
            Arc::ptr_eq(&QuestionTool::shared(), &QuestionTool::shared()),
            "挂起表必须跨装配共享：两次 shared() 是同一个实例"
        );
    }

    // ———————— 入参校验（events 为 None 也会先走校验，可以离线钉住） ————————

    #[tokio::test]
    async fn options_outside_2_to_6_or_blank_items_are_bad_input() {
        let tool = QuestionTool::new();
        for options in [
            vec!["只有一个"],
            vec!["甲", "乙", "丙", "丁", "戊", "己", "庚"], // 7 个
            vec!["甲", ""],                                 // 空串
            vec!["甲", "   "],                              // 纯空白
        ] {
            let err = tool
                .execute(input("问什么", &options), &ctx(None))
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::BadInput(_)),
                "{options:?}: {err:?}"
            );
        }
        // 非字符串数组直接拒。
        let err = tool
            .execute(
                json!({ "question": "问什么", "options": [1, 2] }),
                &ctx(None),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
        // 边界：2 与 6 都过校验（走到「没有事件出口」这一步 = 校验已放行）。
        for ok in [vec!["甲", "乙"], vec!["1", "2", "3", "4", "5", "6"]] {
            let err = tool
                .execute(input("问什么", &ok), &ctx(None))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, ToolError::Failed(m) if m.contains("没有事件出口")),
                "{ok:?} 应过校验落到缺出口的错，实得 {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_blank_or_non_string_question_is_bad_input() {
        let tool = QuestionTool::new();
        for bad in [
            json!({ "options": ["甲", "乙"] }),
            json!({ "question": "", "options": ["甲", "乙"] }),
            json!({ "question": "  ", "options": ["甲", "乙"] }),
            json!({ "question": 42, "options": ["甲", "乙"] }),
            json!({ "question": "问什么", "options": "不是数组" }),
        ] {
            let err = tool.execute(bad, &ctx(None)).await.unwrap_err();
            assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
        }
    }

    // ———————— 执行路径 ————————

    #[tokio::test]
    async fn without_an_event_sink_the_tool_fails_explicitly() {
        let tool = QuestionTool::new();
        let err = tool
            .execute(input("问什么", &["甲", "乙"]), &ctx(None))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("没有事件出口")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_resolved_question_returns_the_choice_and_emits_the_event() {
        let tool = Arc::new(QuestionTool::new());
        let sink = Arc::new(VecSink::default());
        let handle = tokio::spawn({
            let tool = tool.clone();
            let sink = sink.clone();
            // 2..=6 的上限正好把 6 个选项放进同一条：一次跑完正向流。
            let inp = input(
                "用哪个方案？",
                &["方案一", "方案二", "方案三", "方案四", "方案五", "方案六"],
            );
            async move { tool.execute(inp, &ctx(Some(sink))).await }
        });
        let (id, question, options) = wait_question(&sink).await;
        assert_eq!(question, "用哪个方案？");
        assert_eq!(options.len(), 6, "选项原样透传，不做任何改写");

        assert!(tool.resolve(&id, "选项二".into()), "应答必须送达");
        let out = handle.await.unwrap().unwrap();
        assert_eq!(text_of(out), "用户选择了：选项二");

        // 已应答的题不允许二次应答（id 一次性）。
        assert!(!tool.resolve(&id, "再答一次".into()), "id 是一次性的");
    }

    #[tokio::test]
    async fn resolve_on_an_unknown_id_is_false_not_a_panic() {
        let tool = QuestionTool::new();
        assert!(!tool.resolve("不存在的 id", "答案".into()));
        // 空挂起表上 resolve 任何东西都安全。
        assert!(!tool.resolve("", "".into()));
    }

    #[tokio::test]
    async fn cancellation_while_pending_returns_cancelled_and_drops_the_entry() {
        let tool = Arc::new(QuestionTool::new());
        let sink = Arc::new(VecSink::default());
        let token = CancellationToken::new();
        let handle = tokio::spawn({
            let tool = tool.clone();
            let sink = sink.clone();
            let token = token.clone();
            async move {
                let mut c = ctx(Some(sink));
                c.cancel = token;
                tool.execute(input("问什么", &["甲", "乙"]), &c).await
            }
        });
        let (id, _, _) = wait_question(&sink).await;
        token.cancel();

        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "{err:?}");
        // 取消摘除了挂起项：迟到的应答无处可去，resolve 如实报 false。
        assert!(
            !tool.resolve(&id, "迟到的答案".into()),
            "取消后挂起项必须已摘除"
        );
    }
}
