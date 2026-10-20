// 测试与循环验证用的两个工具。**都不注册进生产**（plan:1854）——
// Plan A 的生产工具表为空，所以 spec 5.1 的「全程零弹窗」为真。
use super::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::agent::types::{Risk, ToolOutput};
use async_trait::async_trait;
use serde_json::{Value, json};

/// echo 与它的风险替身共用同一份入参 schema，测试里按 `{"text": …}` 调用。
fn guarded_meta(name: &str, description: &str, risk: Risk) -> ToolMeta {
    ToolMeta {
        name: name.into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
        risk,
    }
}

/// 原样返回输入：让 Task 10 的循环在没有网络、没有生产工具的情况下也能整轮跑通。
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn meta(&self) -> ToolMeta {
        guarded_meta("echo", "原样返回输入，仅用于测试循环", Risk::Read)
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        Ok(ToolOutput::text(
            input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        ))
    }
}

/// 测试专用：名字是 `guarded`、执行逻辑复用 echo，但**风险由测试指定**。
///
/// Plan A 没有任何生产工具是 Write/Exec，而 Task 10 的
/// `denied_tool_fills_error_result_and_loop_continues`（plan:2015）需要一条真的会弹窗的
/// 路径；没有这个类型，那条测试就只能去改 echo 的风险 —— 那会把「Read 零弹窗」这件事
/// 本身测没。注册表侧的行为由
/// `tools::registry` 的 `risk_override_tool_reports_the_risk_the_struct_says` 钉住：
/// `risk_of("guarded")` 必须等于结构体里那个字段。
pub struct RiskOverrideTool(pub Risk);

#[async_trait]
impl Tool for RiskOverrideTool {
    fn meta(&self) -> ToolMeta {
        guarded_meta("guarded", "echo 的替身，风险等级由测试指定", self.0)
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        EchoTool.execute(input, ctx).await
    }
}
