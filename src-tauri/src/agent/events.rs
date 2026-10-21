// 事件线上格式说明（前端契约）：
// `Decision` 的序列化是**手写**的（见下方 `impl Serialize`）：变体里不再有 `allow: bool` 字段，
// 线上的 `allow` 由 `self.allowed()` 在序列化那一刻算出，所以 Rust 侧的变体名和前端读到的布尔
// 是同一个真相，不可能再各说一套。线上字节与 R16 钉住的形状完全一致：
//   {"allow":true,"scope":"session"} 和 {"allow":false,"reason":"用户拒绝"}
// 后续注意：`Decision` 的反序列化**也是手写的**（R98，Task 13 落地，见下方 `impl Deserialize`）：
// 不能挂 `#[serde(untagged)]` 的 derive —— 派生的 untagged 反序列化器是按"哪个键存在"挑变体的，
// `{"allow":false,"scope":…}` 会被静默解析成 `Allow`：一次权限极性的翻转就发生在入程上。
use super::types::*;
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

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

/// 只有两个无字段变体，derive 的 `Deserialize` 与手写的 `Serialize` 是同一个映射表，
/// 未知取值直接报错（`decision_round_trips_through_the_hand_written_pair` +
/// `a_broken_decision_errors_instead_of_choosing_a_variant_by_key_presence` 各盯一半）。
/// **这不能推广到 `Decision`**：那里的风险是变体选择，不是取值解析（R98）。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
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

/// R98：手写反序列化，**这是 `respond` 命令唯一的入程形状守卫**。
///
/// 规则只有一条：变体由 `allow` 这个布尔决定，绝不由「哪个键存在」决定。
/// 中间结构体 `Wire` 是实现手段而不是漏洞 —— 它把三个键收成平坦字段，
/// `allow` 是必填（缺了就报），`scope`/`reason` 是 `Option`，
/// 挑哪个变体全看 `allow`，`deny_unknown_fields` 再挡掉协议外的键。
/// derive 出来的 `untagged` 恰好相反：它逐个变体试解，`{"allow":false,"scope":…}`
/// 会在 `Allow` 那一支先解通 —— 一次拒绝在前程上变成一次放行。
///
/// 形状与上面的 `impl Serialize` 严格互逆（R16 钉死的两个键，顺序也不重要）。
impl<'de> Deserialize<'de> for Decision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            allow: bool,
            #[serde(default)]
            scope: Option<Scope>,
            #[serde(default)]
            reason: Option<String>,
        }

        let w = Wire::deserialize(deserializer)?;
        match (w.allow, w.scope, w.reason) {
            (true, Some(scope), None) => Ok(Decision::Allow { scope }),
            (false, None, Some(reason)) => Ok(Decision::Deny { reason }),
            // 带 scope 的一切 allow=false —— 含「scope + reason 同时出现」—— 都拒绝。
            // 这一支的报错文案刻意说「极性矛盾」：它是唯一一种**前端不可能是善意**的输入。
            (false, Some(_), _) | (true, Some(_), Some(_)) => Err(de::Error::custom(
                "allow 与携带的字段矛盾：Allow 只带 scope，Deny 只带 reason",
            )),
            (true, None, _) => Err(de::Error::custom(
                "allow=true 缺少 scope（once 或 session）",
            )),
            (false, None, None) => Err(de::Error::custom(
                "allow=false 缺少 reason（拒绝必须给理由）",
            )),
        }
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

    /// 钉住 R98：反序列化是手写的，变体由 `allow` 这个布尔决定，**不是**由「哪个键存在」决定。
    /// 线上三个畸形形状都必须报错 —— 任何一支被静默解析成 `Allow` 都是一次权限极性翻转，
    /// 而 `respond` 正是本计划里唯一能让工具真的跑起来的入口。
    #[test]
    fn a_broken_decision_errors_instead_of_choosing_a_variant_by_key_presence() {
        // ① allow=false 却带 scope：derive(untagged) 会按「scope 存在」挑 Allow ⇒ 拒绝变成放行。
        for broken in [
            r#"{"allow":false,"scope":"session"}"#,
            // ② 只有 allow，另一侧字段缺失：不许用默认值兜出一个变体。
            r#"{"allow":true}"#,
            r#"{"allow":false}"#,
            // ③ allow 与携带的字段自相矛盾（两个方向都查）。
            r#"{"allow":true,"reason":"用户拒绝"}"#,
            r#"{"allow":false,"scope":"once","reason":"不"}"#,
            // ④ 压根没有 allow：极性不明，必须由入程报错，而不是猜一个。
            r#"{"scope":"session"}"#,
            r#"{"reason":"不"}"#,
            // ⑤ scope 拼错：不许降级成 Once（下限未定 = 未知，未知就该拒绝）。
            r#"{"allow":true,"scope":"sessionn"}"#,
        ] {
            let e = serde_json::from_str::<Decision>(broken);
            assert!(e.is_err(), "{broken} 本应报错，实际解出了 {:?}", e.ok());
        }
    }

    /// R98 的正向半边：手写序列化与手写反序列化必须互为逆运算，
    /// 否则前端 `respond(requestId, decision)` 递回来的东西会解不开。
    #[test]
    fn decision_round_trips_through_the_hand_written_pair() {
        for d in [
            Decision::allow(Scope::Once),
            Decision::allow(Scope::Session),
            Decision::deny("用户拒绝"),
            Decision::deny(""),
        ] {
            let json = serde_json::to_string(&d).unwrap();
            let back: Decision = serde_json::from_str(&json).unwrap();
            assert_eq!(back, d, "{json} 往返后变了形状");
        }
        // 线上字节仍是 R16 那一套（反序列化不许顺手改协议）。
        assert_eq!(
            serde_json::to_string(&Decision::allow(Scope::Session)).unwrap(),
            r#"{"allow":true,"scope":"session"}"#
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

    /// R160（F9 欠的债）：上面那条只断言 `v["t"]`，**嵌套层从来没有生产侧守卫** ——
    /// `tool_call.call{id,name,risk,input}`、`ContentBlock` 的 `type` 标签、`ToolOutput` 的
    /// `kind` 标签、`Decision` 手写的两个可选键（R98），目前全靠 T14 读一遍 Rust 源码。
    /// 这里对 11 个变体各取实例，`to_value` 后与 `json!` 字面量**整块相等**：
    /// 改名、挪层、加 `flatten` 都会打红，而逐字段取值比较对这三件事是聋的。
    /// `permission_resolved` 破例给三份 decision —— `Deny` 的 `scope` 是**缺席**而不是
    /// `null`，`Allow` 的 `reason` 同理，这是全事件里唯一有可选键的地方。
    #[test]
    fn every_event_serializes_its_full_json_shape() {
        let view = || ToolCallView {
            id: "c1".into(),
            name: "fetch_url".into(),
            risk: Risk::Read,
            input: json!({ "url": "https://example.com/a" }),
        };
        // 四种块全放进去：`user_message` 因此顺带钉住 `ContentBlock` 的 `type` 标签、
        // `ToolOutput` 的 `kind` 标签两支（文本与图片）和 `ImageMime` 的小写线上名。
        // 事件的 `tool_result` 臂只有 `preview: String`，`kind` 标签只能从这条路进线。
        let blocks = || {
            vec![
                ContentBlock::Text {
                    text: "你好".into(),
                },
                ContentBlock::Image {
                    data: "AAAA".into(),
                    mime: ImageMime::Png,
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "fetch_url".into(),
                    input: json!({ "n": 1 }),
                },
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    is_error: false,
                    output: ToolOutput::text("抓到 42 字"),
                },
                ContentBlock::ToolResult {
                    call_id: "c2".into(),
                    is_error: true,
                    output: ToolOutput::Image {
                        data: "BBBB".into(),
                        mime: ImageMime::Jpeg,
                    },
                },
            ]
        };
        let cases: Vec<(AgentEvent, serde_json::Value)> = vec![
            (
                AgentEvent::TurnStart { turn: 3 },
                json!({ "t": "turn_start", "turn": 3 }),
            ),
            (
                AgentEvent::TextDelta {
                    text: "你好".into(),
                },
                json!({ "t": "text_delta", "text": "你好" }),
            ),
            (
                AgentEvent::UserMessage { blocks: blocks() },
                json!({
                    "t": "user_message",
                    "blocks": [
                        { "type": "text", "text": "你好" },
                        { "type": "image", "data": "AAAA", "mime": "png" },
                        { "type": "tool_call", "id": "c1", "name": "fetch_url", "input": { "n": 1 } },
                        { "type": "tool_result", "call_id": "c1", "is_error": false,
                          "output": { "kind": "text", "text": "抓到 42 字" } },
                        { "type": "tool_result", "call_id": "c2", "is_error": true,
                          "output": { "kind": "image", "data": "BBBB", "mime": "jpeg" } },
                    ]
                }),
            ),
            (
                AgentEvent::tool_call(view()),
                json!({
                    "t": "tool_call",
                    "call": { "id": "c1", "name": "fetch_url", "risk": "read",
                              "input": { "url": "https://example.com/a" } },
                    "risk": "read"
                }),
            ),
            (
                AgentEvent::permission_request("p1".into(), view()),
                json!({
                    "t": "permission_request",
                    "id": "p1",
                    "call": { "id": "c1", "name": "fetch_url", "risk": "read",
                              "input": { "url": "https://example.com/a" } },
                    "risk": "read"
                }),
            ),
            (
                AgentEvent::PermissionResolved {
                    id: "p1".into(),
                    decision: Decision::allow(Scope::Once),
                },
                json!({ "t": "permission_resolved", "id": "p1",
                        "decision": { "allow": true, "scope": "once" } }),
            ),
            (
                AgentEvent::PermissionResolved {
                    id: "p1".into(),
                    decision: Decision::allow(Scope::Session),
                },
                json!({ "t": "permission_resolved", "id": "p1",
                        "decision": { "allow": true, "scope": "session" } }),
            ),
            (
                // `scope` 必须**不出现**：写成 `"scope": null` 会让 TS 镜像的
                // `{allow:false;reason}` 分支多一个它没声明的键，而 R98 的反序列化会因为
                // `(false, Some(_), _)` 那一臂直接拒绝这条应答。
                AgentEvent::PermissionResolved {
                    id: "p1".into(),
                    decision: Decision::deny("用户拒绝"),
                },
                json!({ "t": "permission_resolved", "id": "p1",
                        "decision": { "allow": false, "reason": "用户拒绝" } }),
            ),
            (
                AgentEvent::ToolResult {
                    call_id: "c1".into(),
                    is_error: false,
                    ms: 1234,
                    preview: "抓到 42 字".into(),
                },
                json!({ "t": "tool_result", "call_id": "c1", "is_error": false, "ms": 1234,
                        "preview": "抓到 42 字" }),
            ),
            (
                AgentEvent::Usage {
                    input_tokens: 900,
                    output_tokens: 21,
                },
                json!({ "t": "usage", "input_tokens": 900, "output_tokens": 21 }),
            ),
            (
                AgentEvent::ContextCompacted {
                    dropped: 7,
                    summary_chars: 512,
                },
                json!({ "t": "context_compacted", "dropped": 7, "summary_chars": 512 }),
            ),
            (
                // `at` 这个 Rust 字段名在线上叫 `where`（`#[serde(rename)]`）——
                // 整块相等才守得住这条重命名，取 `json["where"]` 比是聋的。
                AgentEvent::Error {
                    at: ErrorWhere::Mcp,
                    message: "spawn 失败".into(),
                    fatal: true,
                },
                json!({ "t": "error", "where": "mcp", "message": "spawn 失败", "fatal": true }),
            ),
            (
                AgentEvent::Done {
                    stop_reason: StopReason::EndTurn,
                },
                json!({ "t": "done", "stop_reason": "end_turn" }),
            ),
        ];
        // 13 条 = 11 个变体 + `permission_resolved` 的另外两份 decision。
        // 这条去重断言是「别漏变体」的守卫：新增事件而不在这里补形状，红的是这里。
        let mut seen: Vec<&str> = cases
            .iter()
            .filter_map(|(_, want)| want["t"].as_str())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            11,
            "11 个变体必须各有至少一份全形状样本，漏一个就等于那一支的嵌套层没上锁"
        );
        for (e, want) in &cases {
            assert_eq!(
                &serde_json::to_value(e).unwrap(),
                want,
                "{e:?} 的整块线上形状不符（改名/挪层/加 flatten 都会走到这里）"
            );
        }
    }
}
