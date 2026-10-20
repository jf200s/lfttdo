// 事件线上格式说明（前端契约）：
// `Decision` 用 `#[serde(untagged)]`，所以变体名不会出现在 JSON 里；前端只能靠字段分派。
// 因此 `allow: bool` 虽然冗余于变体名，仍是必须落在线上的字段 —— Task 14 的 TS 镜像与
// Task 16 的分派逻辑都读 `allow`。构造请一律走 `Decision::allow` / `Decision::deny`，
// 它们保证 `allow` 与变体一致；直接写字面量有可能拼出 `Allow { allow: false }` 这种谎话。
use super::types::*;
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStart { turn: u32 },
    TextDelta { text: String },
    ToolCall { call: ToolCallView, risk: Risk },
    PermissionRequest { id: String, call: ToolCallView, risk: Risk },
    PermissionResolved { id: String, decision: Decision },
    ToolResult { call_id: String, is_error: bool, ms: u64, preview: String },
    Usage { input_tokens: u32, output_tokens: u32 },
    ContextCompacted { dropped: usize, summary_chars: usize },
    Error {
        #[serde(rename = "where")]
        at: ErrorWhere,
        message: String,
        fatal: bool,
    },
    Done { stop_reason: StopReason },
}

#[derive(Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ErrorWhere { Model, Tool, Mcp, Loop, Store }

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason { EndTurn, MaxTurns, Interrupted, Error }

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope { Once, Session }

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Decision {
    Allow { allow: bool, scope: Scope },
    Deny { allow: bool, reason: String },
}

impl Decision {
    pub fn allow(scope: Scope) -> Self { Self::Allow { allow: true, scope } }
    pub fn deny(reason: impl Into<String>) -> Self { Self::Deny { allow: false, reason: reason.into() } }
    pub fn allowed(&self) -> bool { matches!(self, Self::Allow { .. }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_uses_t_as_tag_and_snake_case() {
        let e = AgentEvent::TextDelta { text: "你好".into() };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"text_delta","text":"你好"}"#
        );
    }

    #[test]
    fn error_event_field_serializes_as_where_not_at() {
        let e = AgentEvent::Error {
            at: ErrorWhere::Model, message: "boom".into(), fatal: false,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["where"], "model");
        assert!(json.get("at").is_none(), "at 必须被重命名为 where");
    }

    #[test]
    fn stop_reason_and_risk_are_lowercase() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(), r#""end_turn""#
        );
        assert_eq!(serde_json::to_string(&Risk::Read).unwrap(), r#""read""#);
    }

    #[test]
    fn decision_allow_carries_scope() {
        let d = Decision::allow(Scope::Session);
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"allow":true,"scope":"session"}"#
        );
    }

    #[test]
    fn decision_deny_carries_reason_and_allow_false() {
        let d = Decision::deny("用户拒绝");
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"allow":false,"reason":"用户拒绝"}"#
        );
    }
}
