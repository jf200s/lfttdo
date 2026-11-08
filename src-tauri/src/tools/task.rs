//! task：派一个子代理独立完成一段工作（对齐 OpenCode 的同名工具）。
//!
//! 依赖方向（Plan A）：本文件只定义 [`SubagentRunner`] 接缝与工具壳 —— 「起子会话
//! 跑循环」的真正实现住在 agent 层（`agent/task.rs::LoopRunner`）。tools 树不许反向
//! 依赖 agent 的具体类型，所以这里只有一个 trait：实现方由会话装配进
//! `ToolCtx.subagents`，工具执行时拿接缝调用。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const NAME: &str = "task";

/// 子代理运行器的接缝（tools→agent 的唯一通道）。实现方（agent 层的 `LoopRunner`）
/// 用与会话相同的 model/registry/gate 起一个独立的子 `AgentSession` 跑完 prompt。
#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// 跑完一段自主工作，返回子代理的最终文本。
    ///
    /// · `ctx`：文件作用域（roots/write_root）与子代理一致 —— 子代理能碰的目录
    ///   就是父会话能碰的目录，不另开权限。
    /// · `cancel`：父会话的取消令牌。**父停 = 子停**：实现方必须把它接到子会话的
    ///   取消上，父按下停止时正在跑的子代理不许继续烧请求。
    async fn run(
        &self,
        prompt: String,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError>;
}

/// task 的 ToolError 文案：没装配 runner 的会话调用必须明确失败。
const NO_RUNNER_NOTE: &str = "当前会话没有装配子代理运行器";
/// prompt 为空 / 纯空白：schema 有 `minLength: 1`，这里是运行时同规守卫。
const EMPTY_PROMPT_NOTE: &str = "prompt 不能为空：告诉子代理要完成什么";

/// task 工具：把 prompt 交给子代理跑一段自主循环，把最终文本带回给父模型。
/// Risk 是 **Write**：子代理会连续烧多轮模型请求并可能改文件，这一下必须弹窗 ——
/// 用户要看得见、拦得住；降成 Read 等于绕开权限门。
pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.into(),
            description: "派一个子代理独立完成一段工作：它带着同样的工具自主跑若干轮，\
              最终只把一段文本结果带回来。用 prompt 把任务、边界与期望产出交代清楚。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "minLength": 1,
                        "description": "交给子代理的任务描述（它看不到你这里的对话，只能看到这段话）"
                    }
                },
                "required": ["prompt"]
            }),
            risk: Risk::Write,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // fail-closed：runner 只由会话循环装配（`session::run` 的 ToolCtx）。没有它
        // 的调用方（未来可能的旁路执行点）必须明确报错，不许静默当空操作。
        let Some(runner) = &ctx.subagents else {
            return Err(ToolError::Failed(NO_RUNNER_NOTE.into()));
        };
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if prompt.is_empty() {
            return Err(ToolError::Failed(EMPTY_PROMPT_NOTE.into()));
        }
        // 取消令牌就是 ctx.cancel（会话每轮现构 ToolCtx 时填的正是自己的令牌）——
        // 两个参数在这里指同一个令牌，签名分开是为了让实现方不必从 ctx 里「猜」。
        let result = runner.run(prompt.to_string(), ctx, &ctx.cancel).await?;
        Ok(ToolOutput::text(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 记录型 runner：记下收到的 prompt，回一句固定文本。
    #[derive(Default)]
    struct RecordingRunner(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl SubagentRunner for RecordingRunner {
        async fn run(
            &self,
            prompt: String,
            _ctx: &ToolCtx,
            _cancel: &CancellationToken,
        ) -> Result<String, ToolError> {
            self.0.lock().unwrap().push(prompt);
            Ok("子结果".to_string())
        }
    }

    impl RecordingRunner {
        fn prompts(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    fn ctx_with(runner: Option<Arc<dyn SubagentRunner>>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: vec![],
            write_root: None,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: runner,
            events: None,
        }
    }

    /// 名字 / 风险档 / schema 的钉子：task 必须是 Write（弹窗），prompt 必填且非空。
    #[test]
    fn meta_pins_name_risk_and_schema() {
        let m = TaskTool.meta();
        assert_eq!(m.name, "task");
        assert_eq!(
            m.risk,
            Risk::Write,
            "自主循环值得一次弹窗；降 Read 等于绕开权限门"
        );
        assert_eq!(m.input_schema["type"], "object");
        assert_eq!(m.input_schema["properties"]["prompt"]["type"], "string");
        assert_eq!(m.input_schema["properties"]["prompt"]["minLength"], 1);
        assert_eq!(m.input_schema["required"], json!(["prompt"]));
    }

    /// 没装配 runner ⇒ 明确报错（fail-closed），不是静默空操作。
    #[tokio::test]
    async fn without_a_runner_it_fails_loudly() {
        let err = TaskTool
            .execute(json!({"prompt": "做事"}), &ctx_with(None))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(s) if s.contains("没有装配子代理运行器")),
            "{err}"
        );
    }

    /// 空 prompt 在**碰 runner 之前**就被拒：runner 在场也不能被空调用惊动。
    #[tokio::test]
    async fn an_empty_prompt_is_rejected_before_touching_the_runner() {
        let runner = Arc::new(RecordingRunner::default());
        let ctx = ctx_with(Some(runner.clone()));
        for bad in [json!({}), json!({"prompt": ""}), json!({"prompt": "   "})] {
            let err = TaskTool
                .execute(bad, &ctx)
                .await
                .expect_err("空 prompt 必须报错");
            assert!(
                matches!(&err, ToolError::Failed(s) if s.contains("prompt 不能为空")),
                "{err}"
            );
        }
        assert!(
            runner.prompts().is_empty(),
            "校验失败不许惊动 runner：{:?}",
            runner.prompts()
        );
    }

    /// 正常路径：trim 后的 prompt 原样进 runner，返回文本成为工具输出。
    #[tokio::test]
    async fn the_prompt_reaches_the_runner_and_the_result_comes_back() {
        let runner = Arc::new(RecordingRunner::default());
        let ctx = ctx_with(Some(runner.clone()));
        let out = TaskTool
            .execute(json!({"prompt": "  去查 X  "}), &ctx)
            .await
            .unwrap();
        assert_eq!(runner.prompts(), vec!["去查 X".to_string()], "trim 后透传");
        match out {
            ToolOutput::Text { text } => assert_eq!(text, "子结果"),
            other => panic!("应当是纯文本输出：{other:?}"),
        }
    }
}
