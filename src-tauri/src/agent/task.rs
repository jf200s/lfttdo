//! G16：`SubagentRunner` 的 agent 层实现 —— 用与会话同一套 model/registry/gate 起
//! 一个**独立的子 `AgentSession`** 跑完 prompt，把最后一条 assistant 文本带回去。
//!
//! 为什么实现住这里而不是 tools 层：起子会话要动 `AgentSession`/`SessionDeps` 这些
//! agent 具体类型，而 tools 树不许反向依赖 agent（Plan A 依赖方向）；tools 只认识
//! `tools/task.rs::SubagentRunner` 这个接缝，装配由 `session::run` 在构造 `ToolCtx`
//! 时完成。
use crate::agent::context::{ContextBuilder, ContextConfig};
use crate::agent::events::{AgentEvent, EventSink};
use crate::agent::session::{AgentProfile, AgentSession, SessionConfig, SessionDeps};
use crate::agent::types::{ContentBlock, Role};
use crate::model::provider::ModelProvider;
use crate::permission::gate::PermissionGate;
use crate::tools::registry::{ToolCtx, ToolError, ToolRegistry};
use crate::tools::task::SubagentRunner;
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 子会话的事件出口 —— **空实现**。MVP 取舍：子会话的 ToolCall/ToolResult/文本流
/// 事件**不转发**给 UI，用户只见 TaskTool 最终带回的那段文本（取舍与扩展点：转发
/// 需要事件回流 + 会话 id 关联 + 前端 reducer 分支，是一整条管线；「看子代理在
/// 干嘛」的需求真出现了再加）。注意权限门仍是**父的那只**（gate 的 sink 在装配期
/// 注入），子会话里该弹的授权窗照常弹到父的界面 —— NullSink 只吞子会话循环自己的
/// 循环事件。
struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _e: AgentEvent) {}
}

/// 子代理运行器：持有与会话相同的外部依赖（`SessionDeps` 的镜像子集），
/// 每次调用现起一个全新子会话。
pub(crate) struct LoopRunner {
    pub(crate) model: Arc<dyn ModelProvider>,
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) gate: Arc<PermissionGate>,
    pub(crate) temperature: Option<f32>,
    /// 装配时的上下文配置（经 `ContextBuilder::config()` 取回）：子会话用同一份
    /// 预算/滑窗/人设。calibration 等运行时状态不带走 —— 那是父会话本轮的校准。
    pub(crate) context_cfg: ContextConfig,
    /// G22：task 可点名的 agent 档案表（来自父会话的 `SessionConfig.agents`）。
    /// 点名时在这里查找：找到 ⇒ 子会话换成该档案的人设与工具白名单；找不到 ⇒ 报错。
    pub(crate) agents: Vec<AgentProfile>,
}

#[async_trait]
impl SubagentRunner for LoopRunner {
    async fn run(
        &self,
        prompt: String,
        agent: Option<String>,
        ctx: &ToolCtx,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError> {
        // G22：点名档案就在表里找。找不到必须明确报错（fail-closed）—— 静默回落成
        // 全工具子代理会让「派只读调研员」变成「派全能代理」，白名单就成了摆设。
        let profile = match &agent {
            Some(name) => Some(
                self.agents
                    .iter()
                    .find(|p| &p.name == name)
                    .ok_or_else(|| ToolError::BadInput(format!("没有这个 agent 档案: {name}")))?,
            ),
            None => None,
        };
        // 子会话与父的差异只有两处，都是刻意的：
        // · **独立 history** —— 从 prompt 重新开始，不带父会话的任何消息（父的对话
        //   对子代理不可见，这是 task 工具的语义：prompt 必须把任务交代完整）；
        // · `SessionConfig::default()` + `NullSink` —— plan/弹窗护栏与事件出口不继承
        //   （子代理是一段受控的自主工作；权限门共享，该弹的窗照弹）。
        // 其余全部同源：model/registry/gate/temperature/预算滑窗（context_cfg）、
        // 文件作用域（roots 与 write_root 都取自父的 ToolCtx）。
        // G22：点名档案且档案 prompt 非空 ⇒ 系统提示词**整段替换**成档案人设
        // （「只读调研员」模式，与用户人设同一语义）；否则沿用装配时的人设。
        let context_cfg = match profile.and_then(|p| {
            p.prompt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }) {
            Some(system_prompt) => ContextConfig {
                system_prompt,
                ..self.context_cfg.clone()
            },
            None => self.context_cfg.clone(),
        };
        let mut child = AgentSession::new(SessionDeps {
            model: self.model.clone(),
            registry: self.registry.clone(),
            gate: self.gate.clone(),
            sink: Arc::new(NullSink),
            context: ContextBuilder::new(context_cfg),
            temperature: self.temperature,
        })
        .with_roots(ctx.roots.clone())
        .with_output_dir(ctx.write_root.clone());
        // G22：点名档案（且找得到）才给子会话换配置 —— 只带该档案的工具白名单。
        // `agents` 传**空表**是取舍而不是偷懒：孙代理意味着档案身份要么靠透传（父根本
        // 看不见孙代理在用谁的身份）、要么给委派深度开了没有自然上限的暗门；本项规格
        // 只到「父 → 子」一层，「子代理再派子代理」的需求真出现时，连同深度上限一起加。
        if let Some(p) = profile {
            child = child.with_config(SessionConfig {
                allowed_tools: p.tools.clone(),
                agents: Vec::new(),
                ..SessionConfig::default()
            });
        }
        // 父停 = 子停：会话的取消合同是一次性自有令牌（R63）+ `cancel_handle`，
        // 没有「外来令牌」的注入口，所以用转发任务把父令牌接到子令牌上。
        // 子会话收场后立刻 abort 转发任务 —— 否则它会陪着父令牌挂到进程结束
        // （父令牌从不取消时，那个 future 永远在等）。
        let forwarder = {
            let child_token = child.cancel_handle();
            let parent = cancel.clone();
            tokio::spawn(async move {
                parent.cancelled().await;
                child_token.cancel();
            })
        };
        // 收场原因（EndTurn/Interrupted/Error）不外抛：结果文本按下面的规则提取，
        // 拿不到正文就给占位 —— 父模型读得懂「子代理没有产出」。
        let _stop = child.run(vec![ContentBlock::Text { text: prompt }]).await;
        forwarder.abort();
        let result = child
            .history()
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.text_of())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "（子代理没有输出）".to_string());
        Ok(result)
    }
}
