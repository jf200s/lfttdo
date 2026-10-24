// 权限门：Ask / Auto / Block 的判定点，也是整个核心里唯一会去问人的地方。
// 机制在 Plan A 建齐并测试，生产工具表是空的（plan:1854 明确 `echo` 不注册进生产），
// 所以 spec 5.1 的「全程零弹窗」在 Plan A 为真；Plan B 只需注册 `fetch_url`。
use crate::agent::events::{AgentEvent, Decision, EventSink, Scope};
use crate::agent::types::{Risk, ToolCallView};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// 每台 MCP server 的信任档位（T33）。**不进线格式**（没有 serde），它只在 Rust 侧策略里活。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 静默放行：用户明确信任这台 server。
    Auto,
    /// 每次问人（spec 5.2 的默认档）。
    Ask,
    /// 不问、直接拒：理由里会点名 server。
    Block,
}

/// 会话级放行攒下来的作用域令牌集。键是 `PermissionGate::rule_key` 的产物 ——
/// **不是裸工具名**，裸工具名会把「本会话允许写一次」变成「本会话可写任意路径」。
#[derive(Default)]
pub struct RuleTable {
    auto_keys: RwLock<HashSet<String>>,
    /// server 名 ⇒ 信任档位（T33）。按 server 作用域而不是工具名：一台 server 的工具是它自己
    /// 声明的，逐个授权等于让用户去追一个随时会变的清单。
    server_trust: RwLock<HashMap<String, Verdict>>,
}

impl RuleTable {
    /// 命中即视为会话级放行。MVP 策略「Read 一律 Auto」不住在这里，住在 `check`
    /// （R45：曾经想过 `default_mvp()` 的别名，全计划零调用方，删）。
    pub fn lookup(&self, key: &str) -> Option<Decision> {
        if crate::lock::read(&self.auto_keys).contains(key) {
            return Some(Decision::allow(Scope::Session));
        }
        None
    }

    /// 只登记会话级放行；`Scope::Once` 绝不进这里，否则一次确认就买断整个会话。
    pub fn remember_session(&self, key: &str) {
        crate::lock::write(&self.auto_keys).insert(key.to_string());
    }

    /// 设定一台 server 的信任档位（T35 的 `mcp_set_trust` 是唯一写入者）。
    pub fn set_server_trust(&self, server: &str, verdict: Verdict) {
        crate::lock::write(&self.server_trust).insert(server.to_string(), verdict);
    }

    /// 读回当前档位（T35 的界面要显示它）。
    pub fn server_trust(&self, server: &str) -> Option<Verdict> {
        crate::lock::read(&self.server_trust).get(server).copied()
    }

    /// 只回答「有没有 override」（R45：默认策略不许搬进来，否则同一条策略住两处）：
    /// 先精确 `{name}|{scope}`（今天还没有生产者，留着是因为它是最窄的一档），再按 server 作用域。
    pub fn verdict_for(&self, name: &str, scope: &str) -> Option<Verdict> {
        let trust = crate::lock::read(&self.server_trust);
        if let Some(v) = trust.get(&format!("{name}|{scope}")) {
            return Some(*v);
        }
        trust.get(scope).copied()
    }
}

/// 一次调用实际覆盖的作用域令牌（R212：住在 gate，不住注册表）。
///
/// - `mcp__{server}__{tool}` ⇒ `server`。**注意这是有损的**：T30 的兜底命名把 server 段截到
///   8 个 char，长名字还原不出真名 —— 所以 `check` 里「没查到信任就默认 Ask」是必需的
///   （R321），否则长 server 名会绕过 Ask。
/// - `fetch_url` ⇒ `input.url` 的小写主机名（去端口；解析失败退回 `"read"`，绝不 panic）。
/// - 其余 ⇒ `"read"`（内置 Read 的既有行为，一个字不改）。
pub(crate) fn scope_of(name: &str, input: &serde_json::Value) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        return match rest.split_once("__") {
            Some((server, _tool)) => server.to_string(),
            // 形状不对也不许 panic：宁可给一个不会命中信任表的令牌，让 check 走 Ask。
            None => "mcp".to_string(),
        };
    }
    if name == "fetch_url"
        && let Some(url) = input.get("url").and_then(|v| v.as_str())
        && let Ok(parsed) = url::Url::parse(url)
        && let Some(host) = parsed.host_str()
    {
        return host.to_ascii_lowercase();
    }
    "read".to_string()
}

fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

#[derive(Clone, Default)]
pub struct PermissionGate {
    /// 等人应答的请求。`Arc` 让 `Clone` 出来的 gate 共享同一张表 —— 这正是
    /// 「gate 是长生命周期 `AppState.gate`」的形态，也是 `resolve` 能从 IPC 命令
    /// 打到等待中的 `check` 的原因。
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    rules: Arc<RuleTable>,
    /// 可换 sink（R42 / R3）：装配期还没有 session_id，`send_message` 每轮开头
    /// 才 `set_sink(emitter)`（plan:3106）。所以这里不能是 `Option<Arc<dyn EventSink>>`
    /// 那种只能在构造时填一次的字段。
    sink: Arc<Mutex<Option<Arc<dyn EventSink>>>>,
}

/// `select!` 的三种收场，用枚举摊开是为了让「取消」和「窗口消失」各自都必须
/// 清一次 pending：漏掉的 sender 会让表随中断次数无限增长（`check` 已经返回，
/// `rx` 也随之消失，所以漏掉的条目谁也看不见 —— 只有 `pending_len()` 量得出来）。
enum Waited {
    Cancelled,
    Answered(Decision),
    WindowGone,
}

impl PermissionGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造期装 sink：给测试和「装配即已知 sink」的调用方用；
    /// 每轮换 emitter 的调用方用 `set_sink`。
    pub fn with_sink(self, sink: Arc<dyn EventSink>) -> Self {
        self.set_sink(sink);
        self
    }

    pub fn with_rules(self, rules: Arc<RuleTable>) -> Self {
        let mut g = self;
        g.rules = rules;
        g
    }

    /// 换掉出口（Task 13 在 `send_message` 开头调用）。锁只保护「换个 Arc」这个动作，
    /// `check` 里是先把 Arc clone 出来再释放锁，**任何 await 都不跨这把锁**（R42）。
    pub fn set_sink(&self, sink: Arc<dyn EventSink>) {
        // 旧 sink 必须在锁**外**被 drop：`EventSink` 是 dyn 的，它的 `Drop` 里若有任何东西
        // 回头调本方法（或读这把锁），持锁期间就是自己等自己 —— 症状是界面卡死而不是报错。
        // guard 是这条 `let` 的临时量，语句结束即释放，`old` 才真正活到函数末尾。
        let old = crate::lock::guard(&self.sink).replace(sink);
        drop(old);
    }

    pub fn rules(&self) -> Arc<RuleTable> {
        self.rules.clone()
    }

    /// 作用域令牌：工具名 + 这一次操作实际覆盖的范围。
    ///
    /// - `Write`：目标路径的父目录（写一次 `/tmp/a` 不该买到 `/etc/a`）
    /// - `Exec`：**整条 trim 后的命令**（R46）。曾经用首个空白 token，于是
    ///   允许过 `cmd /C npx hello` 就把 `cmd /C npx anything` 一起买了 —— 一次确认
    ///   换到任意命令执行，方向正好是危险的那一侧。
    /// - 缺字段：`?` 占位，让「没有 path 的 Write」和任何真实作用域都不撞键。
    pub fn rule_key(&self, call: &ToolCallView) -> String {
        let scope_token = match call.risk {
            Risk::Read => scope_of(&call.name, &call.input),
            Risk::Write => call
                .input
                .get("path")
                .and_then(|v| v.as_str())
                .map(parent_dir)
                .unwrap_or_else(|| "?".into()),
            Risk::Exec => call
                .input
                .get("command")
                .and_then(|v| v.as_str())
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "?".into()),
        };
        format!("{}|{}", call.name, scope_token)
    }

    /// 判定一次工具调用。
    ///
    /// **`call.risk` 必须来自 `ToolRegistry::risk_of(name)`**（Task 10 的装配点就是这么写的），
    /// 本方法只信这个字段、不回查注册表：任何别处填的 risk 都等于调用方给自己降权限。
    ///
    /// `cancel` 是这层的命门（R43）：`AgentSession` 的 `select!` 打不断一个已经在
    /// `await` 的 `check`（plan:2190 就站在那之外），所以「弹窗挂着时用户按停止」
    /// 只能由这里自己收场 —— spec:226「静默挂死是权限门设计中最容易漏的一处」。
    /// `biased` 保证取消臂先于应答臂被轮询：停止按钮按下后不该再采纳任何应答。
    pub async fn check(&self, call: &ToolCallView, cancel: &CancellationToken) -> Decision {
        let key = self.rule_key(call);
        let scope = scope_of(&call.name, &call.input);
        // 优先级 1：会话级放行（用户已经在本会话里答过「都行」）。
        if let Some(d) = self.rules.lookup(&key) {
            return d; // 命中会话规则：静默放行，不发弹窗事件
        }
        // 优先级 2：per-server 信任档（T33）。必须压在下面那记 Read 自动放行**之前**，
        // 否则 MCP 的 Ask/Block 永远轮不到 —— 这正是本任务要修的那个矛盾。
        match self.rules.verdict_for(&call.name, &scope) {
            Some(Verdict::Auto) => return Decision::allow(Scope::Once),
            Some(Verdict::Block) => {
                return Decision::deny(format!("server `{scope}` 被设为 Block，调用被拒绝"));
            }
            Some(Verdict::Ask) => {}
            None => {}
        }
        // 优先级 3：MVP 默认「Read 一律 Auto」。**MCP 除外**（R321）：T30 的兜底命名把
        // server 段截到 8 个 char，信任表按名字查不到 ⇒ 落到 Auto 就等于「名字越长的 server
        // 越能绕过 Ask」。没登记的 MCP 工具默认问人（spec 5.2「添加 server 时默认 Ask」）。
        if matches!(call.risk, Risk::Read) && !is_mcp_tool(&call.name) {
            return Decision::allow(Scope::Once);
        }

        // 先 clone Arc、立刻释放锁：下面每一个 await 都不得持有 sink 的 Mutex（R42）。
        let Some(sink) = crate::lock::guard(&self.sink).clone() else {
            // headless（没接界面）绝不等于全授权（R44）。
            return Decision::deny("没有可应答的界面");
        };
        // 已经取消就不用再弹一次窗。
        if cancel.is_cancelled() {
            return Decision::deny("已取消");
        }

        let (tx, rx) = oneshot::channel();
        let id = uuid::Uuid::new_v4().to_string();
        crate::lock::guard(&self.pending).insert(id.clone(), tx);
        sink.emit(AgentEvent::permission_request(id.clone(), call.clone()));

        let waited = tokio::select! {
            biased;
            _ = cancel.cancelled() => Waited::Cancelled,
            d = rx => match d {
                Ok(d) => Waited::Answered(d),
                Err(_) => Waited::WindowGone,
            },
        };

        match waited {
            Waited::Cancelled => {
                // 必须摘掉 sender，且必须告诉前端把弹窗收掉。
                crate::lock::guard(&self.pending).remove(&id);
                let d = Decision::deny("等待授权时收到中断");
                sink.emit(AgentEvent::PermissionResolved {
                    id,
                    decision: d.clone(),
                });
                d
            }
            // 兜底：Plan A 的 gate 是长生命周期 `state.gate`，这支不可达（所以只能由
            // `drop_pending_for_test` 证明，见测试）；未来按会话建 gate 时它是「UI 关了 /
            // 前端崩了 / 会话对象被 drop」的那条保险带。
            // 与上一支的不对称是有意的：R43 要求 cancel 分支 emit，是因为那时弹窗**还挂在
            // 屏幕上**需要被收掉；这支的应答通道已经先没了，再发一条 PermissionResolved 等于
            // 替前端宣布一个它从未做过的决定。若哪天这支变得可达，连同 emit 一起补。
            Waited::WindowGone => {
                crate::lock::guard(&self.pending).remove(&id);
                Decision::deny("确认窗口已关闭")
            }
            Waited::Answered(d) => {
                // 冗余保险：`resolve` 是「先摘出来再 send」，走到这支时条目通常已经不在表里了。
                // 留着是因为它同样保证「应答过的请求不会漏在 pending 里」，而这条性质由
                // `write_risk_asks_and_resolves` 的 `pending_len() == 0` 钉住（两处一起改才会红）。
                crate::lock::guard(&self.pending).remove(&id);
                if let Decision::Allow {
                    scope: Scope::Session,
                } = d
                {
                    self.rules.remember_session(&key);
                }
                sink.emit(AgentEvent::PermissionResolved {
                    id,
                    decision: d.clone(),
                });
                d
            }
        }
    }

    /// 由 IPC 命令（Task 13 的 `respond`）调用。返回 false = 没有这个待决请求
    /// （重复应答、已取消、或 id 是伪造的），调用方据此报错而不是静默。
    pub fn resolve(&self, id: &str, d: Decision) -> bool {
        // 取出与发送分两步：这把锁的临界区只许包含「动这张表」，`tx.send` 是给别人的动作。
        // 今天 `oneshot::send` 不阻塞，所以这只是一道形状上的防线；将来谁把 pending 换成
        // 会阻塞的通道（带锁的 mpsc、或要回调前端的 send），持锁发送就变成死锁。
        let taken = crate::lock::guard(&self.pending).remove(id);
        match taken {
            Some(tx) => tx.send(d).is_ok(),
            None => false,
        }
    }

    /// 测试夹具（R43）：真的把 pending 里那条 sender 移出来 drop 掉，从而可确定地走一遍
    /// `rx` 的 `Err` 分支。`drop(gate)` 做不到这件事 —— tx 存在 `Arc` 里，而 `Arc` 被
    /// gate 的每个 Clone 共享，测试本体还留着一份，所以那条路在 Plan A 压根不可达。
    #[cfg(test)]
    pub(crate) fn drop_pending_for_test(&self, id: &str) -> bool {
        self.pending.lock().unwrap().remove(id).is_some()
    }

    /// 第二个兜底夹具（R43）：把 pending 里那条 sender **就地换掉** —— 旧 sender 被 drop
    /// （于是 `rx` 立刻拿到 `Err`），但 map 里仍留着该 id 的一条新条目。
    /// 这样「Err 分支有没有清 pending」才成为可观测的事：清了归 0，忘了清就一直挂着一条。
    /// （`drop_pending_for_test` 做不到这点 —— 它自己就把条目摘了。）
    #[cfg(test)]
    pub(crate) fn drop_sender_in_place_for_test(&self, id: &str) -> bool {
        let (tx, _rx) = oneshot::channel::<Decision>();
        self.pending
            .lock()
            .unwrap()
            .insert(id.to_string(), tx)
            .is_some()
    }

    /// 待决请求的条数。**泄漏观测点**：`check` 返回后 `rx` 已经没了，所以
    /// 「同 id 再 resolve」无论清理与否都返回 false（那条 sender 发不出去而已）——
    /// 也就是漏掉的 sender 用 `resolve` 是量不出来的，只能直接数条数。
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

/// 父目录，小写化（Windows 路径大小写不敏感，`C:/TMP` 和 `c:/tmp` 该是同一个令牌）。
/// 这是**纯字符串**运算，不碰文件系统：规则表存的是「用户同意过的作用域描述」，
/// 真正落盘时的越界检查归 `ToolCtx::resolve_within_roots`（R47）。
fn parent_dir(p: &str) -> String {
    Path::new(p)
        .parent()
        .map(|x| x.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::{AgentEvent, Decision, EventSink, Scope};
    use crate::agent::types::{Risk, ToolCallView};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// 记录型 sink：测试靠它观察事件，生产靠 Task 13 的 `TauriEmitter`。
    #[derive(Default)]
    struct VecSink(Arc<Mutex<Vec<AgentEvent>>>);

    impl EventSink for VecSink {
        fn emit(&self, e: AgentEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    impl VecSink {
        fn events(&self) -> Vec<AgentEvent> {
            self.0.lock().unwrap().clone()
        }
        fn request_ids(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::PermissionRequest { id, .. } => Some(id),
                    _ => None,
                })
                .collect()
        }
        fn resolved(&self) -> Vec<(String, Decision)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::PermissionResolved { id, decision } => Some((id, decision)),
                    _ => None,
                })
                .collect()
        }
    }

    /// 事件是异步 emit 的，测试不能靠 sleep 猜时间；只让出执行权直到攒够 n 条请求。
    async fn wait_requests(sink: &VecSink, n: usize) -> Vec<String> {
        for _ in 0..1000 {
            let ids = sink.request_ids();
            if ids.len() >= n {
                return ids;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "等不到第 {n} 条 PermissionRequest，已有 {:?}",
            sink.events()
        );
    }

    fn call(name: &str, risk: Risk) -> ToolCallView {
        ToolCallView {
            id: "c1".into(),
            name: name.into(),
            risk,
            input: json!({"path": "/tmp/x"}),
        }
    }

    /// 起一个等待中的 `check`，返回 join handle 与**第 n 条**请求的 id。
    /// gate 是 clone 进任务的（R43：任务 move 走 gate 会让测试之后再也用不了它）。
    async fn pending_request(
        gate: &PermissionGate,
        c: &ToolCallView,
        cancel: &CancellationToken,
        sink: &VecSink,
        n: usize,
    ) -> (tokio::task::JoinHandle<Decision>, String) {
        let gate2 = gate.clone();
        let c2 = c.clone();
        let cancel2 = cancel.clone();
        let h = tokio::spawn(async move { gate2.check(&c2, &cancel2).await });
        let ids = wait_requests(sink, n).await;
        let id = ids.last().unwrap().clone();
        (h, id)
    }

    // ---- MVP 策略：Read 全 Auto，零弹窗（spec 5.1） ----
    #[tokio::test]
    async fn read_risk_is_auto_and_emits_no_permission_event() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        // 包 timeout：Read 策略一旦被改回「问人」，这里要**变红**而不是把测试进程挂住
        // （第一次变异就是把 `matches!(…Read)` 改成 false，结果整个 test binary 卡死）。
        let d = tokio::time::timeout(
            Duration::from_secs(2),
            gate.check(&call("fetch_url", Risk::Read), &cancel),
        )
        .await
        .expect("Read 级调用没走 Auto：它在等人应答，MVP 的零弹窗策略被破坏了");
        assert!(d.allowed(), "MVP 必须零弹窗");
        assert!(
            sink.events().is_empty(),
            "Auto 判定不应产生 permission_request，实际 {:?}",
            sink.events()
        );
    }

    // ---- Write 走询问，人回复后才放行 ----
    #[tokio::test]
    async fn write_risk_asks_and_resolves() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        let c = call("write_file", Risk::Write);
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;
        assert_eq!(gate.pending_len(), 1, "弹窗发出后 pending 必须有这一条");
        assert!(
            gate.resolve(&id, Decision::allow(Scope::Once)),
            "resolve 必须命中 pending 里那条 sender"
        );
        let d = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("已有人应答，check() 却还挂着")
            .unwrap();
        assert_eq!(d, Decision::allow(Scope::Once));
        assert_eq!(
            gate.pending_len(),
            0,
            "应答后 pending 必须清空（漏了就是每次弹窗漏一条 sender）"
        );
        assert_eq!(
            sink.resolved(),
            vec![(id.clone(), Decision::allow(Scope::Once))],
            "应答后必须且只能发一条 permission_resolved，前端靠它收掉弹窗"
        );
        // 同一 id 二次应答不存在
        assert!(
            !gate.resolve(&id, Decision::allow(Scope::Once)),
            "重复应答必须返回 false，Task 13 的 IPC 靠这个报错而不是静默"
        );
    }

    // ---- R44：没接界面绝不等于全授权 ----
    #[tokio::test]
    async fn a_gate_without_sink_denies_write_and_exec() {
        let gate = PermissionGate::new();
        let cancel = CancellationToken::new();
        for risk in [Risk::Write, Risk::Exec] {
            let d = gate.check(&call("write_file", risk), &cancel).await;
            assert!(
                !d.allowed(),
                "无 sink 的 headless gate 对 {risk:?} 绝不能默认放行， got {d:?}"
            );
        }
    }

    // ---- 取消已经发生，就不要再弹一个没人应答的窗 ----
    #[tokio::test]
    async fn an_already_cancelled_token_never_pops_a_request() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let d = tokio::time::timeout(
            Duration::from_secs(2),
            gate.check(&call("write_file", Risk::Write), &cancel),
        )
        .await
        .expect("token 早就取消了却还去等人 —— 静默挂死的前半段");
        assert!(!d.allowed(), "已取消必须 Deny，实际 {d:?}");
        assert!(
            sink.events().is_empty(),
            "已取消不该发 PermissionRequest，实际 {:?}",
            sink.events()
        );
    }

    // ---- R43 主路径：弹窗挂着时点「停止」不能静默挂死 ----
    #[tokio::test]
    async fn cancel_stops_a_pending_request_instead_of_hanging() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        let c = call("run_command", Risk::Exec);
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;
        assert_eq!(gate.pending_len(), 1, "等人应答期间必须挂着那条 sender");

        cancel.cancel(); // 用户按「停止」：Task 10 的 select! 打断不了这里（plan:2190）

        let d = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("check() 在取消后永久挂死：select! 缺 cancel 臂")
            .unwrap();
        assert!(!d.allowed(), "取消必须判 Deny，实际 {d:?}");
        assert_eq!(
            gate.pending_len(),
            0,
            "取消分支必须把 sender 从 pending 里摘掉，否则每轮中断都漏一条"
        );
        assert!(
            !gate.resolve(&id, Decision::allow(Scope::Once)),
            "已经取消的请求不该再被迟到应答命中（IPC 侧看到的必须是 false）"
        );
        assert_eq!(
            sink.resolved(),
            vec![(id.clone(), d.clone())],
            "取消也必须发 permission_resolved，否则前端的弹窗永远收不掉"
        );
    }

    // ---- R43 兜底支：tx 被 drop 时必须兜底 Deny（用真 drop 证明，不靠读源码文本） ----
    #[tokio::test]
    async fn dropped_sender_falls_back_to_deny_instead_of_hanging() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        let c = call("run_command", Risk::Exec);
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;

        assert!(
            gate.drop_pending_for_test(&id),
            "pending 里本该存着这条 sender，测试夹具无从下手"
        ); // remove 出来即 drop -> rx 收到 Err(RecvError)

        let d = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("check() 永久挂死：缺少 oneshot 的 Err(()) 兜底")
            .unwrap();
        assert!(!d.allowed(), "确认窗口消失必须 Deny，实际 {d:?}");
    }

    /// 兜底支的另一半：那条路也得清 pending。用「就地换掉 sender」的夹具做 ——
    /// `drop_pending_for_test` 自己就把条目摘了，量不出这件事（M1b 变异验证过：
    /// 少了清理动作时 `resolve` 断言仍然是绿的，因为 `check` 返回后 `rx` 已经不存在）。
    #[tokio::test]
    async fn the_deny_fallback_also_clears_pending() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        let c = call("run_command", Risk::Exec);
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;

        assert!(
            gate.drop_sender_in_place_for_test(&id),
            "夹具前提：pending 里有这条 sender"
        ); // 旧 sender 被 drop，map 里留下的是一条新的

        let d = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("兜底支没走到：check() 挂死了")
            .unwrap();
        assert!(!d.allowed(), "实际 {d:?}");
        assert_eq!(
            gate.pending_len(),
            0,
            "Err 兜底分支也得把 pending 清空，否则这类事件每次都漏一条 sender"
        );
    }

    // ---- R42：sink 是装配后可换的（plan:3106 的 set_sink） ----
    #[tokio::test]
    async fn set_sink_installs_the_emitter_after_assembly() {
        let gate = PermissionGate::new();
        let cancel = CancellationToken::new();
        let sink = Arc::new(VecSink::default());
        gate.set_sink(sink.clone());
        let c = call("write_file", Risk::Write);
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;
        gate.resolve(&id, Decision::allow(Scope::Once));
        assert!(h.await.unwrap().allowed());
        assert_eq!(sink.request_ids().len(), 1);
    }

    /// `with_rules` 是 Interfaces 承诺的注入口（Task 13 可能拿一张预填的表进来），
    /// 它必须共享同一张表而不是拷一份 —— 这条就钉这件事。
    #[tokio::test]
    async fn with_rules_shares_one_table_across_gate_clones() {
        let rules = Arc::new(RuleTable::default());
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new()
            .with_sink(sink.clone())
            .with_rules(rules.clone());
        let cancel = CancellationToken::new();
        let c = call("write_file", Risk::Write);
        let key = gate.rule_key(&c);
        // 从表这一侧写入，gate 那一侧必须立刻看得见（同一个 Arc）
        rules.remember_session(&key);
        let d = tokio::time::timeout(Duration::from_secs(2), gate.check(&c, &cancel))
            .await
            .expect("注入的规则表没被读到：gate 又去问人了");
        assert_eq!(d, Decision::allow(Scope::Session));
        assert_eq!(
            gate.rules().lookup(&key),
            Some(Decision::allow(Scope::Session))
        );
        assert!(
            sink.events().is_empty(),
            "命中注入的规则表不该弹窗，实际 {:?}",
            sink.events()
        );
    }

    // ---- R45：会话规则按作用域令牌存，不按裸工具名 ----
    #[tokio::test]
    async fn session_allow_is_scoped_by_root_not_bare_tool_name() {
        let gate = PermissionGate::new();
        let a = call("write_file", Risk::Write);
        let key = gate.rule_key(&a);
        gate.rules().remember_session(&key);
        assert!(gate.rules().lookup(&key).is_some(), "同目录应命中");
        let b = {
            let mut b = a.clone();
            b.input = json!({"path": "C:/Windows/system32/x"});
            b
        };
        assert_ne!(gate.rule_key(&a), gate.rule_key(&b), "跨目录必须是不同令牌");
        assert!(
            gate.rules().lookup(&gate.rule_key(&b)).is_none(),
            "跨目录必须重新询问"
        );
    }

    // ---- R45：走真路径的会话放行（check -> Allow{Session} -> 第二次不再弹窗） ----
    #[tokio::test]
    async fn session_allow_short_circuits_the_next_identical_call() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        let a = call("write_file", Risk::Write);
        let (h, id) = pending_request(&gate, &a, &cancel, &sink, 1).await;
        gate.resolve(&id, Decision::allow(Scope::Session));
        assert!(h.await.unwrap().allowed());

        // 完全相同的作用域：自动放行，且不得再发第二条 PermissionRequest。
        // 包 timeout 是刻意的：规则表一旦不命中，这次调用就会去等人，
        // 没有兜底的话本测试是挂死而不是变红。
        let second = tokio::time::timeout(Duration::from_secs(2), gate.check(&a, &cancel))
            .await
            .expect("会话规则没命中：第二次同 key 的调用又去问人了（应当静默放行）");
        assert_eq!(second, Decision::allow(Scope::Session));
        assert_eq!(
            sink.request_ids().len(),
            1,
            "会话级放行后同 key 的调用必须命中规则表、静默放行"
        );

        // 换了作用域令牌仍要问
        let b = {
            let mut b = a.clone();
            b.input = json!({"path": "/etc/hosts"});
            b
        };
        let (hb, idb) = pending_request(&gate, &b, &cancel, &sink, 2).await;
        gate.resolve(&idb, Decision::deny("这次不写"));
        assert!(!hb.await.unwrap().allowed());
        assert_eq!(sink.request_ids().len(), 2, "不同目录必须重新询问");
    }

    // ---- R46：Exec 的令牌是整条命令，首词相同绝不等于同一作用域 ----
    #[tokio::test]
    async fn exec_rule_key_uses_the_whole_trimmed_command() {
        let gate = PermissionGate::new();
        let a = {
            let mut a = call("run_command", Risk::Exec);
            a.input = json!({"command": "cmd /C npx hello"});
            a
        };
        let b = {
            let mut b = call("run_command", Risk::Exec);
            b.input = json!({"command": "cmd /C npx anything"});
            b
        };
        assert_ne!(
            gate.rule_key(&a),
            gate.rule_key(&b),
            "首词相同就复用令牌 = 一次确认换到任意命令执行"
        );
        // 同一意图（前后空白不同）应视为同一令牌
        let c = {
            let mut c = call("run_command", Risk::Exec);
            c.input = json!({"command": "  cmd /C npx hello  "});
            c
        };
        assert_eq!(
            gate.rule_key(&a),
            gate.rule_key(&c),
            "命令两端空白不该造出新令牌"
        );
        // 缺 command 字段不得和任何真实命令撞键
        let d = call("run_command", Risk::Exec);
        assert!(
            gate.rule_key(&d).ends_with("|?"),
            "没有 command 的 Exec 调用要用占位令牌，实际 {}",
            gate.rule_key(&d)
        );
    }

    /// Task 13 要把 gate 放进 `AppState`（必须 `Send + Sync`），这件事现在只有编译器
    /// 知道 —— 一旦哪天往结构里塞进 `Rc` 或裸 `dyn EventSink`（没有 Send），那是一句
    /// 到 T13 才炸的错。这里提前钉住。
    #[test]
    fn permission_gate_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PermissionGate>();
        assert_send_sync::<RuleTable>();
        assert_send_sync::<Arc<dyn EventSink>>();
    }

    // ───────────────────────── T33：per-server 信任压在 Read 自动放行之前 ─────────────────────────

    #[tokio::test]
    async fn a_server_defaulted_to_ask_pops_exactly_one_request() {
        let sink = Arc::new(VecSink::default());
        let rules = Arc::new(RuleTable::default());
        rules.set_server_trust("search", Verdict::Ask);
        let gate = PermissionGate::new()
            .with_sink(sink.clone())
            .with_rules(rules);
        let cancel = CancellationToken::new();
        let c = ToolCallView {
            id: "c1".into(),
            name: "mcp__search__web_search".into(),
            risk: Risk::Read,
            input: json!({"q": "x"}),
        };

        // `check` 在 Ask 路径上是**等人应答才返回**的，直接 await 会挂住测试进程。
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;

        assert_eq!(
            gate.rule_key(&c),
            "mcp__search__web_search|search",
            "键必须带 server 作用域；仍是 `read` 就说明 MCP 工具共用一个键，问一次买断所有 server"
        );
        assert_eq!(gate.pending_len(), 1, "Ask 的真相是挂在待答表里");

        gate.resolve(&id, Decision::allow(Scope::Once));
        let d = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("resolve 之后 check 仍不返回：应答臂断了")
            .expect("check 任务不该 panic");
        assert!(d.allowed(), "答了 Allow 就该放行");
        assert_eq!(
            sink.request_ids().len(),
            1,
            "一次调用只发一条 permission_request"
        );
    }

    #[tokio::test]
    async fn a_blocked_server_denies_without_waiting() {
        let sink = Arc::new(VecSink::default());
        let rules = Arc::new(RuleTable::default());
        rules.set_server_trust("evil", Verdict::Block);
        let gate = PermissionGate::new()
            .with_sink(sink.clone())
            .with_rules(rules);
        let cancel = CancellationToken::new();

        // Block **不 await**：判据是「它在 2 秒内自己收了场」，而不是「看起来返回了 Deny」。
        let d = tokio::time::timeout(
            Duration::from_secs(2),
            gate.check(&call("mcp__evil__x", Risk::Read), &cancel),
        )
        .await
        .expect("Block 走成了等人应答：Block 的整个意义就是不问");

        assert!(
            matches!(&d, Decision::Deny { reason } if reason.contains("evil")),
            "拒绝理由必须指名是哪个 server：{d:?}"
        );
        assert!(
            sink.events().is_empty(),
            "Block 不许产生任何 permission_request"
        );
        assert_eq!(gate.pending_len(), 0, "Block 不该在待答表里留下条目");
    }

    #[test]
    fn scope_of_takes_the_server_for_mcp_and_the_host_for_fetch() {
        assert_eq!(
            scope_of("fetch_url", &json!({"url": "https://News.Example:443/a"})),
            "news.example",
            "域名要小写、去端口"
        );
        assert_eq!(
            scope_of("fetch_url", &json!({"url": "not a url"})),
            "read",
            "解析失败退回默认令牌，不许 panic"
        );
        assert_eq!(scope_of("mcp__search__web_search", &json!({})), "search");
        assert_eq!(
            scope_of("read_file", &json!({"path": "/tmp/x"})),
            "read",
            "内置 Read 的既有行为一个字不许变"
        );

        let gate = PermissionGate::new();
        let fetch = ToolCallView {
            id: "c1".into(),
            name: "fetch_url".into(),
            risk: Risk::Read,
            input: json!({"url": "https://News.Example:443/a"}),
        };
        assert_eq!(gate.rule_key(&fetch), "fetch_url|news.example");
        assert_eq!(
            gate.rule_key(&ToolCallView {
                id: "c2".into(),
                name: "mcp__search__web_search".into(),
                risk: Risk::Read,
                input: json!({}),
            }),
            "mcp__search__web_search|search"
        );
    }

    /// R321：**没有信任记录**的 MCP 工具也必须问人，不许落到 Read 自动放行。
    /// 这一条针对 T30 的兜底命名：server 段被截到 8 个 char，gate 无法从名字还原真名，
    /// 于是信任表按名字查不到 —— 若默认是 Auto，就是「越长的 server 名越能绕过 Ask」。
    /// 名字取的是真实的兜底形状（T30 的 `mcp__a-very-l__…_<7 hex>`）。
    #[tokio::test]
    async fn an_unknown_mcp_tool_is_asked_not_auto_allowed() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let cancel = CancellationToken::new();
        let c = ToolCallView {
            id: "c1".into(),
            name: "mcp__a-very-l__get_the_current_weather_39a9f58".into(),
            risk: Risk::Read,
            input: json!({}),
        };
        let (h, id) = pending_request(&gate, &c, &cancel, &sink, 1).await;
        assert_eq!(gate.pending_len(), 1, "未登记的 MCP 工具必须弹窗");
        gate.resolve(&id, Decision::deny("测试收尾"));
        let _ = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("resolve 之后 check 仍不返回");
    }

    /// 反方向的钉子：已登记 `Auto` 的 server 静默放行、`read_file` 仍然零弹窗。
    #[tokio::test]
    async fn a_trusted_mcp_server_is_silent_while_builtin_reads_stay_auto() {
        let sink = Arc::new(VecSink::default());
        let rules = Arc::new(RuleTable::default());
        rules.set_server_trust("search", Verdict::Auto);
        let gate = PermissionGate::new()
            .with_sink(sink.clone())
            .with_rules(rules);
        let cancel = CancellationToken::new();
        let d = gate
            .check(&call("mcp__search__web_search", Risk::Read), &cancel)
            .await;
        assert!(d.allowed(), "登记 Auto 的 server 必须静默放行：{d:?}");
        let d = gate.check(&call("read_file", Risk::Read), &cancel).await;
        assert!(d.allowed(), "内置 Read 的零弹窗不许被本任务改坏：{d:?}");
        assert!(sink.events().is_empty(), "两条都不该产生弹窗事件");
        assert_eq!(gate.pending_len(), 0);
    }
}
