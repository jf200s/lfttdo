// 事件线上格式说明（前端契约）：
// `Decision` 的序列化是**手写**的（见下方 `impl Serialize`）：变体里不再有 `allow: bool` 字段，
// 线上的 `allow` 由 `self.allowed()` 在序列化那一刻算出，所以 Rust 侧的变体名和前端读到的布尔
// 是同一个真相，不可能再各说一套。线上字节与 R16 钉住的形状完全一致：
//   {"allow":true,"scope":"session"} 和 {"allow":false,"reason":"用户拒绝"}
// 后续注意：`Decision` 今天**不派生 `Deserialize`**。若 Task 13 需要反序列化，必须同样手写，
// 不能挂 `#[serde(untagged)]` 的 derive —— 派生的 untagged 反序列化器是按"哪个键存在"挑变体的，
// `{"allow":false,"scope":…}` 会被静默解析成 `Allow`：一次权限极性的翻转就发生在入程上。
use super::types::*;
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

// `AgentEvent` 只到 `PartialEq` 为止：它传递性持有 `ToolCallView.input: serde_json::Value`
// （可以是 f64，NaN ≠ NaN 让自反性不成立）和 `Decision`，这两者今天都不是 `Eq`，
// 所以该派生不可能成立（实测报错：`the trait ToolCallView: Eq is not satisfied`）。
// Tasks 9/13 用 `assert_eq!(event, expected_event)` 比较事件已经够用。
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStart {
        turn: u32,
    },
    TextDelta {
        text: String,
    },
    /// 用户说的话（R68，Task 11 新增）。**只有重放路径会发这一支**：live 时这句话由前端
    /// 乐观插入（plan:4335），事件流里从来没有它。少了这一支，重开会话后用户的话只能
    /// 塞进 `text_delta` ⇒ 落进助手气泡，而 spec:421 要的正是一条能重建「这句话是谁说的」
    /// 的重放流。交接 T14：TS 联合类型与 reducer 都要有 `user_message` 分支。
    UserMessage {
        blocks: Vec<ContentBlock>,
    },
    /// 不变量：`risk` 必须等于 `call.risk`，两者来自注册表的一次查表 —— 请用 `AgentEvent::tool_call` 构造。
    ToolCall {
        call: ToolCallView,
        risk: Risk,
    },
    /// 不变量：`risk` 必须等于 `call.risk`，两者来自注册表的一次查表 —— 请用 `AgentEvent::permission_request` 构造。
    PermissionRequest {
        id: String,
        call: ToolCallView,
        risk: Risk,
    },
    PermissionResolved {
        id: String,
        decision: Decision,
    },
    ToolResult {
        call_id: String,
        is_error: bool,
        ms: u64,
        preview: String,
    },
    Usage {
        input_tokens: u32,
        output_tokens: u32,
    },
    ContextCompacted {
        dropped: usize,
        summary_chars: usize,
    },
    Error {
        #[serde(rename = "where")]
        at: ErrorWhere,
        message: String,
        fatal: bool,
    },
    Done {
        stop_reason: StopReason,
    },
}

impl AgentEvent {
    /// 带 `risk` 的两个事件的唯一认可构造路径：顶层 `risk` 直接取自 `call.risk`，
    /// 注册表的一次查表只写一次，前端读到两个键也必然相等。
    pub fn tool_call(call: ToolCallView) -> Self {
        Self::ToolCall {
            risk: call.risk,
            call,
        }
    }

    /// 同上：顶层 `risk` 由 `call.risk` 填充，不接受第二个独立的 risk 入参。
    pub fn permission_request(id: String, call: ToolCallView) -> Self {
        Self::PermissionRequest {
            id,
            risk: call.risk,
            call,
        }
    }
}

/// 事件出口的抽象（Task 9 建机制，Task 13 接实现）。
/// 住在事件层而不是权限层：`PermissionGate` 要它，`AgentEvent` 也要它，放别处会形成
/// `permission -> agent` 之外的反向依赖。实现方两件：生产的 `TauriEmitter`（持 app_handle
/// 与 session_id）和测试的 `VecSink`。`Send + Sync` 是硬要求 —— gate 是 `Arc` 共享、
/// 跨任务调用的，emit 会在任意一个 await 点上被调用。
pub trait EventSink: Send + Sync {
    fn emit(&self, e: AgentEvent);
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorWhere {
    Model,
    Tool,
    Mcp,
    Loop,
    Store,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTurns,
    Interrupted,
    Error,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Once,
    Session,
}

/// 权限决定。变体即真相：不存在 `Allow { allow: false }` 这种状态。
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Allow { scope: Scope },
    Deny { reason: String },
}

impl Decision {
    pub fn allow(scope: Scope) -> Self {
        Self::Allow { scope }
    }
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// 手写序列化：线上形状保持 R16 钉死的两个字段，`allow` 写在最前，
/// 取值来自 `self.allowed()`，所以 `allowed()` 和前端读的是同一个真相。
impl Serialize for Decision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Decision", 2)?;
        state.serialize_field("allow", &self.allowed())?;
        match self {
            Decision::Allow { scope } => state.serialize_field("scope", scope)?,
            Decision::Deny { reason } => state.serialize_field("reason", reason)?,
        }
        state.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_event_uses_t_as_tag_and_snake_case() {
        let e = AgentEvent::TextDelta {
            text: "你好".into(),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"text_delta","text":"你好"}"#
        );
    }

    #[test]
    fn error_event_field_serializes_as_where_not_at() {
        let e = AgentEvent::Error {
            at: ErrorWhere::Model,
            message: "boom".into(),
            fatal: false,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["where"], "model");
        assert!(json.get("at").is_none(), "at 必须被重命名为 where");
    }

    #[test]
    fn stop_reason_and_risk_are_lowercase() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            r#""end_turn""#
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

    /// 钉住 I-1：两个带 risk 的事件，序列化后顶层 `risk` 与 `call.risk` 在 JSON 里必须一致
    /// （前端读的是 JSON，不是 Rust 字段）。
    #[test]
    fn risk_bearing_events_repeat_the_call_risk_in_json() {
        let call = ToolCallView {
            id: "c1".into(),
            name: "fs.write".into(),
            risk: Risk::Write,
            input: json!({"path": "a.txt"}),
        };
        for e in [
            AgentEvent::tool_call(call.clone()),
            AgentEvent::permission_request("perm_1".into(), call.clone()),
        ] {
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(
                v["risk"], v["call"]["risk"],
                "顶层 risk 与 call.risk 在 JSON 里分叉了：{v}"
            );
            assert_eq!(v["risk"], "write");
        }
    }

    /// 钉住 I-2：`allow` 由变体算出，与 R16 的字节完全一致，且永远不与变体矛盾。
    #[test]
    fn decision_allow_field_always_agrees_with_its_variant() {
        for d in [
            Decision::allow(Scope::Once),
            Decision::allow(Scope::Session),
            Decision::deny("用户拒绝"),
        ] {
            let v = serde_json::to_value(&d).unwrap();
            assert_eq!(v["allow"], json!(d.allowed()));
        }
        assert_eq!(
            serde_json::to_value(Decision::allow(Scope::Once)).unwrap(),
            json!({"allow": true, "scope": "once"})
        );
    }

    /// 钉住全部 11 个事件的 `t` 标签：本任务的交付物就是线上格式，不能只测两个变体。
    #[test]
    fn every_agent_event_variant_serializes_its_t_tag() {
        let call = ToolCallView {
            id: "c1".into(),
            name: "fs.read".into(),
            risk: Risk::Read,
            input: json!(null),
        };
        let events = [
            AgentEvent::TurnStart { turn: 0 },
            AgentEvent::TextDelta {
                text: String::new(),
            },
            AgentEvent::UserMessage { blocks: vec![] },
            AgentEvent::tool_call(call.clone()),
            AgentEvent::permission_request("p1".into(), call.clone()),
            AgentEvent::PermissionResolved {
                id: "p1".into(),
                decision: Decision::allow(Scope::Once),
            },
            AgentEvent::ToolResult {
                call_id: "c1".into(),
                is_error: false,
                ms: 0,
                preview: String::new(),
            },
            AgentEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            AgentEvent::ContextCompacted {
                dropped: 0,
                summary_chars: 0,
            },
            AgentEvent::Error {
                at: ErrorWhere::Model,
                message: String::new(),
                fatal: false,
            },
            AgentEvent::Done {
                stop_reason: StopReason::EndTurn,
            },
        ];
        let tags = [
            "turn_start",
            "text_delta",
            "user_message",
            "tool_call",
            "permission_request",
            "permission_resolved",
            "tool_result",
            "usage",
            "context_compacted",
            "error",
            "done",
        ];
        assert_eq!(
            (events.len(), tags.len()),
            (11, 11),
            "AgentEvent 有 11 个变体，标签表必须一一对应；新增事件要同步补进这张表"
        );
        for (e, tag) in events.iter().zip(tags) {
            let v = serde_json::to_value(e).unwrap();
            assert_eq!(v["t"], tag, "{e:?} 的 t 标签不符");
        }
    }
}
