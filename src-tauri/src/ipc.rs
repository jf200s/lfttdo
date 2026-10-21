// Task 13：Tauri 装配与 IPC 层。
//
// 本文件是 Plan A 里唯一允许 `use tauri` 的文件（`lib.rs` 只剩模块声明与一行转发）。
// 合帧逻辑与整条一轮对话的路径都**不依赖 tauri**（R106：出口抽成 `Publish`，
// 一轮抽成 `run_turn`），因为 `AppHandle` 在测试里构造不出来 —— 不抽这两层，
// 这段每次发送都要跑几百次的代码在生产里就永远没有测试。
use crate::agent::context::{ContextBuilder, ContextConfig};
use crate::agent::events::{AgentEvent, Decision, EventSink, StopReason};
use crate::agent::session::{AgentSession, SessionDeps};
use crate::agent::types::{ContentBlock, Message};
use crate::model::openai_compat::OpenAiCompat;
use crate::model::provider::ModelProvider;
use crate::permission::gate::PermissionGate;
use crate::settings::{Settings, SettingsStore};
use crate::store::db::{Db, SessionRow, events_from_messages};
use crate::tools::registry::ToolRegistry;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::ipc::InvokeBody;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_util::sync::CancellationToken;

/// 事件通道的 payload（plan:2842 / spec:285 钉死 `{ sessionId, event }`）。
/// 只有这一个结构体用 camelCase：`AgentEvent` 内部的键仍是 snake_case，
/// 前端的 `AgentPayload` 镜像（plan:3354）读的就是 `sessionId` 这个键名。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPayload {
    pub session_id: String,
    pub event: AgentEvent,
}

/// 合帧的唯一出口（R106 要求的 seam）。生产实现持 `AppHandle`，测试实现记账。
pub trait Publish: Send + Sync {
    fn publish(&self, payload: AgentPayload);
}

/// `TextDelta` 的合帧缓冲。30ms 或累计 512 字符任一命中就交出一帧。
/// 只负责「什么时候可以交出去」，交出之后发哪儿去归 `Publish`。
struct Coalescer {
    buf: Mutex<String>,
    last_flush: Mutex<Instant>,
}

impl Coalescer {
    /// 距上次 flush 的上限。超过它就发一帧，保证前端不觉得卡住。
    const MAX_WAIT: Duration = Duration::from_millis(30);
    /// 累计字符上限：命中即发，不等 30ms。长段回复时这是 IPC 次数的主要来源。
    const MAX_CHARS: usize = 512;

    fn new() -> Self {
        Self {
            buf: Mutex::new(String::new()),
            // 构造即视为「刚刚 flush 过」：第一帧因此最多等 30ms 也能主动出去。
            last_flush: Mutex::new(Instant::now()),
        }
    }

    /// 时间到 / 字数到 → 交出整帧；否则留在缓冲里给 `None`。
    fn push(&self, text: &str) -> Option<String> {
        {
            let mut b = self.buf();
            b.push_str(text);
        }
        let due = self.last_flush().elapsed() >= Self::MAX_WAIT;
        let big = self.buf().chars().count() >= Self::MAX_CHARS;
        if due || big { self.take() } else { None }
    }

    /// 无条件取空缓冲。空缓冲给 `None`（不发空帧，前端不该为它渲染一次气泡更新）。
    fn take(&self) -> Option<String> {
        let t = {
            let mut b = self.buf();
            std::mem::take(&mut *b)
        };
        if t.is_empty() {
            return None;
        }
        *self.last_flush() = Instant::now();
        Some(t)
    }

    // R109：锁中毒不 panic 整条流。这三把锁只保护一个 String / 一个 Instant，
    // 中毒后继续用是安全的（没有跨锁不变量）。
    fn buf(&self) -> std::sync::MutexGuard<'_, String> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn last_flush(&self) -> std::sync::MutexGuard<'_, Instant> {
        self.last_flush.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 生产 `EventSink`：合帧 + 加 session_id + 交给 `Publish`。
pub struct TauriEmitter {
    out: Arc<dyn Publish>,
    session_id: String,
    coalescer: Coalescer,
}

impl TauriEmitter {
    pub fn new(out: Arc<dyn Publish>, session_id: String) -> Self {
        Self {
            out,
            session_id,
            coalescer: Coalescer::new(),
        }
    }

    fn send(&self, event: AgentEvent) {
        self.out.publish(AgentPayload {
            session_id: self.session_id.clone(),
            event,
        });
    }

    /// 落帧（会话收尾、以及 turn 结束前都要调一次，否则最后半句话留在缓冲里没人发）。
    pub fn flush(&self) {
        if let Some(text) = self.coalescer.take() {
            self.send(AgentEvent::TextDelta { text });
        }
    }
}

impl EventSink for TauriEmitter {
    fn emit(&self, e: AgentEvent) {
        match e {
            AgentEvent::TextDelta { text } => {
                if let Some(frame) = self.coalescer.push(&text) {
                    self.send(AgentEvent::TextDelta { text: frame });
                }
            }
            // 唯一的一条正确性约束（brief:76）：非 TextDelta 之前必先 flush，
            // 否则 `tool_call` 会跑到未落地的文本之前到达，前端时间线倒序。
            other => {
                self.flush();
                self.send(other);
            }
        }
    }
}

// ==== 装配：状态、命令、事件出口 ====

/// 事件通道名（plan:2842）：前端只 `listen("agent")` 这一条通道。
const AGENT_CHANNEL: &str = "agent";

/// 新会话的默认标题。**自动改名只在这个值仍然成立时才进行**（R104），
/// 所以 `session_create` 与 `auto_title` 必须读同一个常量，不许各写一遍字面量。
pub const DEFAULT_TITLE: &str = "新对话";

/// R109：命令层不许 `.lock().unwrap()`。`AppState` 的锁是长命的，一次别处的 panic
/// 会把中毒的 Mutex 变成之后**每条**命令都 panic。沿用仓库既有处置
/// （`settings.rs` 的 `lock()`、`db.rs:99`）：中毒就继续用内层值。
/// 这些锁保护的全是单点状态（一个 `String`、一个令牌、一个缓冲），没有跨锁不变量，
/// 所以吞掉 poison 不会让人读到撕裂的形状。
fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 生产出口：把 payload 发进 tauri 的 `agent` 通道。
/// 这是 `Publish` 的**唯一**生产实现 —— 合帧与路由逻辑全在 `TauriEmitter` 里，与 tauri 无关。
struct AppPublisher {
    app: AppHandle,
}

impl Publish for AppPublisher {
    fn publish(&self, payload: AgentPayload) {
        // 发不出去只可能是窗口已经没了。失败**不打 payload**：事件里带用户原文和工具输出，
        // 日志不该收（spec:473 同一条理由管着 API key）。
        if let Err(e) = self.app.emit(AGENT_CHANNEL, payload) {
            tracing::warn!("agent 事件发送失败：{e}");
        }
    }
}

/// Tauri 托管的应用状态。每个字段都是长命的：`Db`/`SettingsStore` 各只允许一个实例
/// （R101 的 `SettingsStore` 锁是**实例内**的，两个实例同目录并发 save 实测 48 次里
/// 20~30 次报 `Io(os error 2)`），所以一律 `Arc` + 一次 `manage`。
pub struct AppState {
    pub db: Arc<Db>,
    pub settings: Arc<SettingsStore>,
    pub registry: Arc<ToolRegistry>,
    pub gate: Arc<PermissionGate>,
    /// 当前这一轮的取消令牌（一次性，R63）。`run_turn` 装、`interrupt` 读、轮次结束清空。
    /// brief 的 Interfaces 写的是 `Mutex<..>`；外面多一层 `Arc` 是因为 `run_turn` 要在
    /// 不借 `&AppState`（那会把 tauri 类型拖进可测路径）的前提下拿到这个槽位。
    pub cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// 正在运行的会话 id —— 单轮并发的唯一凭据。
    pub running: Arc<Mutex<Option<String>>>,
    /// 日志文件路径（R107：GUI 没有控制台，路径看不见等于日志不存在）。
    /// 装配期才知道 app data 目录，所以是 `Option`；建不出日志文件时给 `None` 而不是失败。
    pub log_path: Option<PathBuf>,
}

/// 单轮并发闸门。guard 一析构就释放槽位，所以 `?` 的每条早退路径（读设置失败、
/// 没配 key、模型构造失败、落库失败）都不会把「正在运行」永久留在状态里。
/// brief 用手动清两遍的写法，`?` 之前那几行根本没执行 —— 表现是界面从此卡在上一轮。
pub struct RunningGuard {
    slot: Arc<Mutex<Option<String>>>,
}

/// 认领这一轮。**任何一轮在跑时都不许起第二轮，同一会话也不行**：
/// brief:113-121 允许同会话重入（先 cancel 上一轮），但上一轮的 `append_message` 还没落地时，
/// 新一轮的 `prior` 已经读完了库 —— 两轮同时落库正是 R103 要消灭的那个形状。
/// 想接着聊就先按停止，等这一轮收尾。
fn claim_running(slot: &Arc<Mutex<Option<String>>>, want: &str) -> Result<RunningGuard, String> {
    let mut current = guard(slot);
    if let Some(busy) = current.clone() {
        return Err(format!(
            "会话 {busy} 正在运行。同一时刻只支持一轮对话，请先中断它。"
        ));
    }
    *current = Some(want.to_string());
    drop(current);
    Ok(RunningGuard { slot: slot.clone() })
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        *guard(&self.slot) = None;
    }
}

/// 取消令牌的槽位生命周期也交给所有权：`run_turn` 无论从哪儿退出都清空，
/// 于是 `interrupt` 永远不会打到一个已经死掉的会话的令牌上（R63 的一次性合同）。
struct TurnToken {
    slot: Arc<Mutex<Option<CancellationToken>>>,
}

impl TurnToken {
    fn install(&self, token: CancellationToken) {
        *guard(&self.slot) = Some(token);
    }
}

impl Drop for TurnToken {
    fn drop(&mut self) {
        *guard(&self.slot) = None;
    }
}

/// 一轮对话的全部依赖，**一项 tauri 类型都没有**（R106 的 seam 从出口扩到整条路径）。
pub struct TurnDeps {
    pub db: Arc<Db>,
    pub registry: Arc<ToolRegistry>,
    pub gate: Arc<PermissionGate>,
    pub model: Arc<dyn ModelProvider>,
    pub sink: Arc<dyn EventSink>,
    /// 由 `Settings::to_context_config()` 给（R100）：钳位与预算只有一处定义。
    pub context: ContextConfig,
    pub cancel: Arc<Mutex<Option<CancellationToken>>>,
}

/// 同步 SQLite / 磁盘读写一律进 `spawn_blocking` —— 这是 `db.rs:50-53` 与 `settings.rs`
/// 结构体文档（N-3）写给 T13 的交接：占住 tokio worker 或占住主线程都是界面卡死。
/// `State` 不是 `'static`，所以调用方先把 `Arc` 克隆进闭包。
async fn blocking<T>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("后台任务失败：{e}"))?
}

/// 一轮对话的完整生产路径：恢复历史 → `AgentSession::run` → **只写新增的行** → 条件自动改名。
///
/// R103 的落库所有权就落在 `prior` 这一个下标上：`session.rs:110` 的 `run()` 自己会
/// `history.push(Message::user(blocks))`（一条用户轮次 = 一行），而 `restore()` 装回来的
/// 历史本来就在库里。所以发送前一行都不写、跑完只写 `history[prior..]`。
/// brief:152-160 的写法（按块补写 + 整段重放）会把一轮的两行写成四行。
pub async fn run_turn(
    deps: TurnDeps,
    session_id: &str,
    blocks: Vec<ContentBlock>,
) -> Result<StopReason, String> {
    let TurnDeps {
        db,
        registry,
        gate,
        model,
        sink,
        context,
        cancel,
    } = deps;
    let sid = session_id.to_string();

    // 读库失败（含 R71 的「某一行解不开」）直接报错，不返回「看起来完整」的历史。
    let reader = db.clone();
    let read_sid = sid.clone();
    let history = blocking(move || reader.messages(&read_sid).map_err(|e| e.to_string())).await?;
    let prior = history.len();

    // R105：装配期没有 session_id，所以 gate 的出口每轮换一次内核（`gate.rs:77`）。
    gate.set_sink(sink.clone());

    let mut session = AgentSession::new(SessionDeps {
        model,
        registry,
        gate,
        sink,
        context: ContextBuilder::new(context),
    });
    let token = TurnToken { slot: cancel };
    token.install(session.cancel_handle());
    session.restore(history);

    let reason = session.run(blocks).await;

    let fresh = session.history()[prior..].to_vec();
    let writer = db.clone();
    let write_sid = sid.clone();
    blocking(move || append_rows(&writer, &write_sid, &fresh).map_err(|e| e.to_string())).await?;

    let typer = db.clone();
    let title_sid = sid.clone();
    blocking(move || auto_title(&typer, &title_sid).map_err(|e| e.to_string())).await?;

    Ok(reason)
}

/// R103 落库的唯一入口（一轮的新行按顺序追加）。同步 SQLite，调用方负责 `spawn_blocking`。
fn append_rows(db: &Db, session_id: &str, rows: &[Message]) -> Result<(), rusqlite::Error> {
    for m in rows {
        db.append_message(session_id, m)?;
    }
    Ok(())
}

/// R104：自动改名**只**在标题仍是默认「新对话」时发生。brief:161-163 的无条件
/// `rename_session` 会在下一次发送时把用户手工起的名字冲掉。
///
/// 默认标题走 `list_sessions`：`Db` 没有按 id 取标题的入口，而 Plan A 的会话数量级
/// 不值得为它加一条 SQL —— 一条只服务于这里的读法，就是给「谁拥有标题」再造一个答案。
/// 标题文本沿用 `latest_user_message`：它已经截到 40 字符、并且跳过工具结果行（R75），
/// 在这里再截一遍就是第二份真相（R77 同一条理由）。
fn auto_title(db: &Db, session_id: &str) -> Result<(), rusqlite::Error> {
    let still_default = db
        .list_sessions()?
        .iter()
        .any(|s| s.id == session_id && s.title == DEFAULT_TITLE);
    if !still_default {
        return Ok(());
    }
    if let Some(title) = db.latest_user_message(session_id)?
        && !title.is_empty()
    {
        db.rename_session(session_id, &title)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    app: AppHandle,
    session_id: String,
    blocks: Vec<ContentBlock>,
) -> Result<(), String> {
    let _running = claim_running(&state.running, &session_id)?;

    let store = state.settings.clone();
    let settings = blocking(move || store.load().map_err(|e| e.to_string())).await?;
    // R90：`normalize_base_url("")` 返回空串，settings.rs 不替用户编一个默认端点，
    // 所以这道守卫必须住在装配层 —— 空 base_url 到这里就停。
    if settings.base_url.trim().is_empty() {
        return Err("尚未配置模型地址，请在设置中填写".into());
    }
    // 先查地址再读凭据：端点没配时压根不该去碰凭据管理器（少一次无关的失败）。
    let key_store = state.settings.clone();
    let api_key = blocking(move || key_store.api_key().map_err(|e| e.to_string()))
        .await?
        .ok_or_else(|| "尚未配置 API key，请在设置中填写".to_string())?;

    // 明文 key 只在这一行进 `ModelConfig`：不打日志、不进事件、不回前端（spec:473）。
    let model =
        Arc::new(OpenAiCompat::new(settings.to_model_config(&api_key)).map_err(|e| e.to_string())?);
    drop(api_key);

    let emitter = Arc::new(TauriEmitter::new(
        Arc::new(AppPublisher { app }),
        session_id.clone(),
    ));
    let out = run_turn(
        TurnDeps {
            db: state.db.clone(),
            registry: state.registry.clone(),
            gate: state.gate.clone(),
            model,
            sink: emitter.clone(),
            // R100：钳位与预算只有一处定义，手搭 `ContextConfig{..}` 等于绕过 `normalize()`。
            context: settings.to_context_config(),
            cancel: state.cancel.clone(),
        },
        &session_id,
        blocks,
    )
    .await;
    // 兜底冲一次。`run()` 的每个出口都发 `Done`，而 `Done` 走「非 delta 先 flush」那一臂
    // （`coalescing_preserves_the_exact_text` 钉着），所以正常路径上这里拿不到东西；
    // 将来谁加一条不发 Done 的出口，半句话也不会凭空消失。
    emitter.flush();
    // 收尾冲完缓冲再把 StopReason 丢掉：它已经由 `done` 事件交过一次，命令返回值里再给一份
    // 就是同一个事实的两个来源（前端该读的始终是事件流）。
    out.map(|_| ())
}

/// 打断当前这一轮。令牌一次性（R63）：取消过的会话算用完，下一轮 `run_turn` 会新建会话、
/// 换新令牌，`TurnToken` 负责把旧的那枚从槽里清掉。
#[tauri::command]
pub fn interrupt(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(token) = guard(&state.cancel).clone() {
        token.cancel();
    }
    Ok(())
}

/// 前端把权限决定递回来。`Decision` 的形状由 `events.rs` 里**手写**的 `impl Deserialize`
/// 把关（R98）：极性与携带字段矛盾（例如 `{"allow":false,"scope":"session"}`）时这条命令
/// 压根收不到参数，前端只会拿到 serde 的报错 —— 拒绝不会在入程上变成放行。
#[tauri::command]
pub fn respond(
    state: State<'_, AppState>,
    request_id: String,
    decision: Decision,
) -> Result<bool, String> {
    // 返回 false = pending 里没有这个 id（已经答过、或那一轮已经收场），前端据此收起弹窗。
    Ok(state.gate.resolve(&request_id, decision))
}

#[tauri::command]
pub async fn session_list(state: State<'_, AppState>) -> Result<Vec<SessionRow>, String> {
    let db = state.db.clone();
    blocking(move || db.list_sessions().map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn session_create(state: State<'_, AppState>) -> Result<String, String> {
    let db = state.db.clone();
    blocking(move || db.create_session(DEFAULT_TITLE).map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn session_rename(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    let db = state.db.clone();
    blocking(move || db.rename_session(&id, &title).map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn session_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    // 只取消**这个**会话的那一轮：删 B 不该把 A 正在跑的一轮打断（brief:197 是无条件 cancel）。
    // 刻意不把两个 guard() 写成一条 let-chain：那样会在持有 `running` 锁的去取 `cancel` 锁，
    // 今天没有反向路径所以不会死锁，但锁序不该靠运气，也不该给将来留一个周期。
    let owns_the_running_turn = guard(&state.running).as_deref() == Some(id.as_str());
    if owns_the_running_turn {
        let token = guard(&state.cancel).clone();
        if let Some(token) = token {
            token.cancel();
        }
    }
    let db = state.db.clone();
    blocking(move || db.delete_session(&id).map_err(|e| e.to_string())).await
}

#[tauri::command]
pub async fn session_messages(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<AgentEvent>, String> {
    let db = state.db.clone();
    let registry = state.registry.clone();
    blocking(move || {
        let msgs = db.messages(&id).map_err(|e| e.to_string())?;
        // R99：第二参必须是注册表的真查表。写死 `Risk::Read` 会让 Plan B 的写工具
        // 重开后显示「自动允许」，那是界面替用户撒谎（db.rs:329）。
        Ok(events_from_messages(&msgs, &|name| registry.risk_of(name)))
    })
    .await
}

/// 设置页要的一切。**这里没有任何一个字段承载明文 key**（R83 / spec:473）：
/// 凭据只以掩码形状出这道边界。
#[derive(Serialize)]
pub struct SettingsView {
    #[serde(flatten)]
    pub settings: Settings,
    /// R99：`api_key_masked()` 返回 `Result<Option<String>, SettingsError>`，所以这里是
    /// `Option<String>` 而不是 brief 的 `String`。`None` = 未配置；「读不到」是另一回事，
    /// 由 `Err` 走 IPC 的错误通道（比在 view 里塞第二个字段诚实：前端不可能把两者读成一样）。
    pub api_key_masked: Option<String>,
    /// R102：`load()` 之后**紧接着**取走的「文件坏了、已备份为 X」通知。
    /// 这是 R93 造那个入口的唯一消费者 —— 不在这里取就等于没有。
    /// 不许在 `send_message` 里取：那会把提示从设置页抢走，那里才是该看到的地方。
    pub corrupt_backup: Option<PathBuf>,
}

#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let store = state.settings.clone();
    blocking(move || {
        let settings = store.load().map_err(|e| e.to_string())?;
        // 顺序是 R102 定的：`take_corrupt_backup` 描述的就是「刚才那次 load 的结果」，
        // 中间不许插第二次 load。
        let corrupt_backup = store.take_corrupt_backup();
        let api_key_masked = store.api_key_masked().map_err(|e| e.to_string())?;
        Ok(SettingsView {
            settings,
            api_key_masked,
            corrupt_backup,
        })
    })
    .await
}

#[tauri::command]
pub async fn settings_set(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    let store = state.settings.clone();
    blocking(move || store.save(&settings).map_err(|e| e.to_string())).await
}

/// key 进来之后只做一件事：交给凭据管理器。不打印、不写日志、不进返回值（spec:473）。
/// 空串按「清除」处理，那是 `set_api_key` 自己的合同（R88）。
#[tauri::command]
pub async fn settings_set_api_key(
    state: State<'_, AppState>,
    api_key: String,
) -> Result<(), String> {
    let store = state.settings.clone();
    blocking(move || store.set_api_key(&api_key).map_err(|e| e.to_string())).await
}

/// R110：`settings_api_key_masked` 这条命令**不实现** —— `settings_get` 已经带掩码，
/// 再开一个只读掩码的命令没有消费者（R89：加接口先给消费者）。
///
/// 消费者是 T17 的人工验收（R107/R110 都要求版本与日志路径看得见），不是界面，
/// 所以维持 brief 的 `-> String`，不新造一个没人读的 struct。
#[tauri::command]
pub fn runtime_summary(state: State<'_, AppState>) -> String {
    let log = state
        .log_path
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "日志文件未能创建".into());
    format!(
        "qwen-do {} · plan A · 日志：{log}",
        env!("CARGO_PKG_VERSION")
    )
}

/// raw body 取字节。字节只能克隆：`Request` 借的是 `InvokeMessage` 里的 `InvokeBody`，
/// 而 `blocking` 要 `'static`。T3 实测 512 KB 往返中位 7.5 ms（15/15 全 `Ok`），
/// 这次克隆在预算内，不是未量的开销。
fn take_raw_body(body: &InvokeBody) -> Result<Vec<u8>, String> {
    match body {
        InvokeBody::Raw(bytes) => Ok(bytes.clone()),
        // 不回显 value：这条字符串会进日志，而 body 内容大小不受控。
        InvokeBody::Json(_) => Err(
            "normalize_image 只接受 raw body，收到的是 json body（前端必须是 invoke(\"normalize_image\", bytes)）".into(),
        ),
    }
}

/// Task 18 Step 4：三入口拿到的字节 → `ContentBlock::Image`。
///
/// T3 的两条硬约束（实测，非推测）：
/// 1. **raw 与 JSON 字段互斥** —— 这条命令一旦加第二个具名参数，前端就没法再把它整体当
///    raw body 发。所以 max-edge / mime 这类旋钮只能内置（`image_prep::MAX_EDGE_PX`），
///    或另开一条 JSON 命令。
/// 2. `Request` 的文档注释写明 raw body 「on all platforms except Android」。本项目只发
///    Windows，但这是**已知降级面**：真要跨端，兜底是走 JSON 传 base64。
///
/// 归一化是 CPU 活（解码 + 缩放 + 降质循环），而且 R139 之后 `image` 的 `rayon` 是关的，
/// 单张 4000px 全在主线程串行 ⇒ 必须走 `blocking`，不能像 T3 探针那样写同步命令。
#[tauri::command]
pub async fn normalize_image(req: tauri::ipc::Request<'_>) -> Result<ContentBlock, String> {
    let bytes = take_raw_body(req.body())?;
    blocking(move || crate::image_prep::normalize_image(&bytes).map_err(|e| e.to_string())).await
}

/// 日志文件名。和 `settings.json`、`qwen-do.db` 同目录（R107）。
const LOG_FILE: &str = "qwen-do.log";

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join(LOG_FILE)
}

/// 共享一把 `File` 的写出端。
///
/// 为什么不直接 `with_writer(file)`：`fmt` 层的一条日志是**多笔**小 `write`，
/// 而 `MakeWriter` 每次拿到的都是一个新的写入口 —— 两个线程各带各的缓冲，两行日志会互相
/// 插进半行，搜日志的人读到的就是碎句。这里把「一条日志」收进一次 `Mutex` 持有，
/// `append` 模式下不同进程也不会互相盖。
#[derive(Clone)]
struct SharedFile(Arc<Mutex<std::fs::File>>);

impl std::io::Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedFile {
    type Writer = SharedFile;

    fn make_writer(&'a self) -> SharedFile {
        self.clone()
    }
}

/// 把日志落到 app data 目录里的文件（R107）。返回路径给 `runtime_summary` 报出去。
///
/// 建不出来（目录权限、磁盘满）时让应用**照常启动**、只把路径报成 `None`：
/// 没有日志很糟，界面起不来更糟。这一支本身也 `warn` 不出去（还没有 subscriber），
/// 所以只能 `eprintln!` —— GUI 里看不见，但也不是异常：真需要用户知道的启动期失败
/// 另有出口，就是 `run()` 末尾那个 `expect`。
fn init_logging(dir: &Path) -> Option<PathBuf> {
    let path = log_path(dir);
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("无法创建日志文件 {}：{e}", path.display());
            return None;
        }
    };
    let writer = SharedFile(Arc::new(Mutex::new(file)));
    // `try_init` 而不是 `init`：重复装配（测试、或将来谁再调一次 `run()`）不该 panic。
    match tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(writer)
        .try_init()
    {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!("日志 subscriber 已存在，本次不接管：{e}");
            None
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // `dialog:allow-open` 在 capabilities 里（spec:459 / T15 的图片附件入口），
        // 权限要对得上，插件就必须真的注册 —— 只写权限不装插件会在 build 时炸（R108）。
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let log_path = init_logging(&dir);
            let db = Arc::new(Db::open(&dir.join("qwen-do.db"))?);
            // R101：一个目录只许存在**一个** `SettingsStore`。它的 `save()` 锁是实例内的
            // （settings.rs 结构体文档 N-3），两个实例同目录并发写那个固定名 temp 实测会失败
            // 20~30/48 次。所以这里 `manage` 一次，命令一律经 `State<AppState>` 取，
            // 绝不在命令里现构。
            let settings = Arc::new(SettingsStore::new(&dir));
            // R105：装配期还没有 session_id，所以 gate 先不带 sink，由 `run_turn` 每轮开头
            // `set_sink(emitter)`（`gate.rs:77` 的可换内核就是为这一刻准备的）。
            // 这里**不留** brief Step 3 的 `NoopSink`：空 sink 意味着 Plan B 的弹窗静默不发事件。
            // Plan A 的生产工具表是空的（plan:1854，`echo` 不注册），所以本轮既没有工具也没有
            // 弹窗；而 sink 还没换上的窗口里 `check` 给的是 deny（R44：headless ≠ 全授权）。
            app.manage(AppState {
                db,
                settings,
                registry: Arc::new(ToolRegistry::new()),
                gate: Arc::new(PermissionGate::new()),
                cancel: Arc::new(Mutex::new(None)),
                running: Arc::new(Mutex::new(None)),
                log_path,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            respond,
            interrupt,
            session_list,
            session_create,
            session_rename,
            session_delete,
            session_messages,
            settings_get,
            settings_set,
            settings_set_api_key,
            normalize_image,
            runtime_summary,
        ])
        .run(tauri::generate_context!())
        .expect("error while running qwen-do");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{Risk, ToolCallView};
    use serde_json::json;

    /// 测试用的出口：把 payload 记在账上，不发往任何 tauri 对象。
    #[derive(Default)]
    struct Recorded {
        published: Mutex<Vec<AgentPayload>>,
    }

    impl Recorded {
        fn frames(&self) -> Vec<AgentEvent> {
            self.published
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.event.clone())
                .collect()
        }
        fn texts(&self) -> Vec<String> {
            self.frames()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::TextDelta { text } => Some(text),
                    _ => None,
                })
                .collect()
        }
        fn count(&self) -> usize {
            self.published.lock().unwrap().len()
        }
    }

    impl Publish for Recorded {
        fn publish(&self, payload: AgentPayload) {
            self.published.lock().unwrap().push(payload);
        }
    }

    fn emitter() -> (Arc<TauriEmitter>, Arc<Recorded>) {
        let rec = Arc::new(Recorded::default());
        let e = Arc::new(TauriEmitter::new(rec.clone(), "sess-1".into()));
        (e, rec)
    }

    fn delta(text: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            text: text.to_string(),
        }
    }

    /// R106 断言①：30ms 内的多个 delta 合成一帧，且合成后内容一字不差。
    #[test]
    fn deltas_inside_the_30ms_window_stay_in_one_frame() {
        let (e, rec) = emitter();
        e.emit(delta("你"));
        e.emit(delta("好"));
        e.emit(delta("，世界"));
        assert_eq!(rec.count(), 0, "30ms 内不该有任何一帧落地");
        e.flush();
        assert_eq!(rec.count(), 1, "三段必须合成恰好一帧");
        assert_eq!(rec.texts(), vec!["你好，世界".to_string()]);
    }

    /// R106 断言②：累计到 512 字符立刻交帧，不等 30ms。
    #[test]
    fn reaching_512_chars_flushes_immediately() {
        let (e, rec) = emitter();
        let half: String = std::iter::repeat_n('字', 256).collect();
        e.emit(delta(&half));
        assert_eq!(rec.count(), 0, "256 字符还没到上限");
        e.emit(delta(&half));
        assert_eq!(rec.count(), 1, "第 512 个字符必须当场发帧");
        assert_eq!(rec.texts()[0].chars().count(), 512);
    }

    /// R106 断言③（合帧唯一的正确性约束）：非 TextDelta 之前先把缓冲发出去。
    #[test]
    fn a_non_delta_event_always_follows_the_buffered_text() {
        let (e, rec) = emitter();
        e.emit(delta("先看一眼"));
        e.emit(AgentEvent::tool_call(ToolCallView {
            id: "c1".into(),
            name: "fs.read".into(),
            risk: Risk::Read,
            input: json!({"path": "a.txt"}),
        }));
        let frames = rec.frames();
        assert_eq!(frames.len(), 2, "缓冲那一帧 + tool_call 那一帧");
        assert_eq!(
            frames[0],
            delta("先看一眼"),
            "文本必须排在 tool_call 之前，否则前端时间线倒序"
        );
        assert!(matches!(frames[1], AgentEvent::ToolCall { .. }));
        // tool_call 之后缓冲必须是空的：不能留半句到下一帧才落地。
        e.flush();
        assert_eq!(rec.count(), 2, "tool_call 不该在缓冲里留下东西");
    }

    /// R106 断言④：flush 之后缓冲为空 —— 再 flush 不产生空帧。
    #[test]
    fn flushing_empties_the_buffer_and_never_emits_an_empty_frame() {
        let (e, rec) = emitter();
        e.flush();
        assert_eq!(rec.count(), 0, "空缓冲不许发帧");
        e.emit(delta("一句"));
        e.flush();
        e.flush();
        assert_eq!(rec.count(), 1, "第二次 flush 不该再发一遍");
        assert_eq!(rec.texts(), vec!["一句".to_string()]);
    }

    /// 时间上限那一支也要有人测：过了 30ms 的下一个 delta 就该把整帧带走。
    #[test]
    fn the_30ms_threshold_releases_the_frame_on_the_next_delta() {
        let (e, rec) = emitter();
        e.emit(delta("早"));
        assert_eq!(rec.count(), 0);
        std::thread::sleep(Coalescer::MAX_WAIT + Duration::from_millis(20));
        e.emit(delta("到"));
        assert_eq!(rec.count(), 1, "超过 30ms 后下一个 delta 必须当场发帧");
        assert_eq!(rec.texts(), vec!["早到".to_string()]);
    }

    /// payload 的线上形状（plan:2842 的 `{ sessionId, event }`）。
    /// 前端 `commands.ts` 直接读 `p.payload.sessionId`，键名改了就是一片 undefined。
    #[test]
    fn the_payload_wire_shape_is_session_id_plus_event() {
        let (_e, _) = emitter();
        let v = serde_json::to_value(AgentPayload {
            session_id: "sess-1".into(),
            event: delta("hi"),
        })
        .unwrap();
        assert_eq!(v["sessionId"], "sess-1");
        assert_eq!(v["event"]["t"], "text_delta");
        // 每一帧都自带 session_id：前端靠它把事件路由到对应会话的 reducer。
        let (e2, rec2) = emitter();
        e2.emit(delta("x"));
        e2.flush();
        assert_eq!(rec2.published.lock().unwrap()[0].session_id, "sess-1");
    }

    /// 非 delta 事件不吞文本、也不被合帧：内容逐字一致，且 `Done` 之后没有半帧残留。
    #[test]
    fn coalescing_preserves_the_exact_text() {
        let (e, rec) = emitter();
        for p in ["你好", "，", "我是", "Qwen"] {
            e.emit(delta(p));
        }
        e.emit(AgentEvent::Done {
            stop_reason: crate::agent::events::StopReason::EndTurn,
        });
        let frames = rec.frames();
        assert_eq!(frames[0], delta("你好，我是Qwen"));
        assert_eq!(
            frames.last().unwrap(),
            &AgentEvent::Done {
                stop_reason: crate::agent::events::StopReason::EndTurn
            }
        );
        assert_eq!(frames.len(), 2);
    }

    // ---- R103 / R104：落库只此一家，自动改名只覆盖默认标题 --------------------
    //
    // 这一组的夹具真的走 `AgentSession::run()`，测的是**生产路径本身**而不是抄出来的
    // 一半逻辑：`run_turn` 不碰 tauri（模型是脚本化的、sink 是 `Publish` 的记账实现），
    // 所以「重复写历史」这个 brief 里的真 bug 只有在这里才量得出来。

    use crate::agent::context::ContextConfig;
    use crate::agent::events::StopReason;
    use crate::agent::types::{ContentBlock, Message, Role};
    use crate::model::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
    use crate::permission::gate::PermissionGate;
    use crate::store::db::{Db, events_from_messages};
    use crate::tools::registry::ToolRegistry;
    use async_trait::async_trait;
    use futures::Stream;
    use tempfile::TempDir;

    /// 每轮吐一条固定回复的模型。Plan A 的生产工具表是空的，所以不需要工具轮。
    struct ReplyModel {
        replies: Mutex<std::collections::VecDeque<String>>,
    }

    #[async_trait]
    impl ModelProvider for ReplyModel {
        fn stream<'a>(
            &'a self,
            _req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            // 取完了就是夹具写错（轮数不够），不许安静地回一条空消息把测试做绿。
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("夹具轮数不够");
            Box::pin(futures::stream::iter(
                vec![ModelDelta::Text(reply)].into_iter().map(Ok),
            ))
        }
    }

    fn replies(v: &[&str]) -> Arc<dyn ModelProvider> {
        Arc::new(ReplyModel {
            replies: Mutex::new(v.iter().map(|s| s.to_string()).collect()),
        })
    }

    struct Fixture {
        db: Arc<Db>,
        registry: Arc<ToolRegistry>,
        gate: Arc<PermissionGate>,
        cancel: Arc<Mutex<Option<tokio_util::sync::CancellationToken>>>,
        _dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self {
                db: Arc::new(Db::open(&dir.path().join("qwen-do.db")).unwrap()),
                registry: Arc::new(ToolRegistry::new()),
                gate: Arc::new(PermissionGate::new()),
                cancel: Arc::new(Mutex::new(None)),
                _dir: dir,
            }
        }

        /// 组装一次真实的一轮：走生产函数 `run_turn`，夹具只提供模型与记账 sink。
        async fn turn(
            &self,
            sid: &str,
            model: Arc<dyn ModelProvider>,
            text: &str,
        ) -> (Result<StopReason, String>, Vec<AgentEvent>) {
            let rec = Arc::new(Recorded::default());
            let sink = Arc::new(TauriEmitter::new(rec.clone(), sid.to_string()));
            let reason = run_turn(
                TurnDeps {
                    db: self.db.clone(),
                    registry: self.registry.clone(),
                    gate: self.gate.clone(),
                    model,
                    sink: sink.clone(),
                    context: ContextConfig::default(),
                    cancel: self.cancel.clone(),
                },
                sid,
                vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            )
            .await;
            sink.flush();
            (reason, rec.frames())
        }
    }

    fn text_rows(msgs: &[Message], role: Role) -> Vec<String> {
        msgs.iter()
            .filter(|m| m.role == role)
            // 工具结果行的 role 也是 User（types.rs:111），按块种类排除。
            .filter(|m| {
                !m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .map(|m| m.text_of())
            .collect()
    }

    /// R103：落库只能有一个所有者。`run()` 自己会把这条用户轮次 push 进 history，
    /// 所以一轮 = 两行（一问一答），**不许**再按块补写、也不许把整段历史重放一遍。
    /// brief:152-160 的写法在这里会测出 8 行（2 + 2 + 4）而不是 4 行。
    #[tokio::test]
    async fn a_second_turn_appends_only_what_the_first_turn_left_out() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let model = replies(&["回复一", "回复二"]);

        let (reason, _) = f.turn(&sid, model.clone(), "第一句").await;
        assert_eq!(reason.unwrap(), StopReason::EndTurn);
        assert_eq!(
            f.db.messages(&sid).unwrap().len(),
            2,
            "一轮之后库里必须恰好两行"
        );

        let (reason, _) = f.turn(&sid, model, "第二句").await;
        assert_eq!(reason.unwrap(), StopReason::EndTurn);
        let msgs = f.db.messages(&sid).unwrap();
        assert_eq!(msgs.len(), 4, "第二轮只许追加两行，整段重放就是重复气泡");
        assert_eq!(
            text_rows(&msgs, Role::User).join("|"),
            "第一句|第二句",
            "一条用户轮次必须恰好一行"
        );
        assert_eq!(text_rows(&msgs, Role::Assistant).join("|"), "回复一|回复二");
    }

    /// R103 的后半边（T11「两套真相」那条红线）：关闭重开之后，重放出来的
    /// 条数与内容和直播一致 —— 一句不多、一句不少。
    #[tokio::test]
    async fn reopening_a_session_replays_exactly_what_the_live_stream_showed() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let model = replies(&["回复一", "回复二"]);
        let live1 = f.turn(&sid, model.clone(), "第一句").await.1;
        let live2 = f.turn(&sid, model, "第二句").await.1;
        let live_text: String = live1
            .iter()
            .chain(live2.iter())
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let live_turn_starts = live1
            .iter()
            .chain(live2.iter())
            .filter(|e| matches!(e, AgentEvent::TurnStart { .. }))
            .count();
        assert_eq!(live_turn_starts, 2, "两次发送各一个 turn_start");

        // 重开：换一个 Db 实例读同一个文件，等价于应用重启后重新打开会话。
        let path = f._dir.path().join("qwen-do.db");
        let reopened = Db::open(&path).unwrap();
        let msgs = reopened.messages(&sid).unwrap();
        // 注册表夹具为空：`risk_of` 只影响徽章，这里要的是「第二参必须是真函数」（R99）。
        let replay = events_from_messages(&msgs, &|n| f.registry.risk_of(n));
        let replay_text: String = replay
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let replay_users = replay
            .iter()
            .filter(|e| matches!(e, AgentEvent::UserMessage { .. }))
            .count();

        assert_eq!(replay_text, live_text, "重放的文字与直播不一致");
        assert_eq!(
            replay_users, 2,
            "一条用户轮次只许有重放里的一条 user_message"
        );
        assert_eq!(
            replay
                .iter()
                .filter(|e| matches!(e, AgentEvent::TurnStart { .. }))
                .count(),
            2
        );
    }

    /// R104 的正向半边：默认标题「新对话」会被第一句真人发言顶掉（截到 40 字符）。
    #[tokio::test]
    async fn a_default_title_adopts_the_first_user_message() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        f.turn(&sid, replies(&["好的"]), "帮我看看今天的天气")
            .await
            .0
            .unwrap();
        let title = title_of(&f.db, &sid);
        assert_eq!(title.as_deref(), Some("帮我看看今天的天气"));
    }

    /// R104（brief:161-163 的无条件 rename 会在这里转红）：用户手工改过的名字
    /// 在下一次发送时必须原样保住。
    #[tokio::test]
    async fn a_hand_named_session_keeps_its_name() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        f.db.rename_session(&sid, "我的名字").unwrap();
        // 必须 unwrap：整轮失败时标题当然还是「我的名字」，那条断言会因为**什么都没发生**而变绿。
        f.turn(&sid, replies(&["好的"]), "随便聊聊")
            .await
            .0
            .unwrap();
        assert_eq!(title_of(&f.db, &sid).as_deref(), Some("我的名字"));
    }

    fn title_of(db: &Db, sid: &str) -> Option<String> {
        db.list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == sid)
            .map(|s| s.title)
    }

    /// 收尾必须把取消令牌槽位清空：留着它，`interrupt` 会在下一轮里取消一个已经死掉的会话。
    #[tokio::test]
    async fn a_finished_turn_releases_the_cancel_slot() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        f.turn(&sid, replies(&["好的"]), "说一句").await.0.unwrap();
        assert!(
            f.cancel.lock().unwrap().is_none(),
            "一轮跑完不许把令牌留在槽里"
        );
    }

    /// 单轮并发闸门：任何一轮都在跑的时候都不许起第二轮 —— 同会话也不行，
    /// 因为第二轮的 `prior` 会在第一轮的写落地之前读库（R103 只有一个所有者才成立）。
    #[test]
    fn one_running_turn_blocks_every_other_claim() {
        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let _first = claim_running(&slot, "A").expect("第一轮应当放行");
        assert_eq!(slot.lock().unwrap().as_deref(), Some("A"));
        assert!(claim_running(&slot, "B").is_err(), "别的会话在跑时必须拒绝");
        assert!(
            claim_running(&slot, "A").is_err(),
            "同一会话的重入也必须拒绝：两轮同时落库就是把 R103 破掉"
        );
        drop(_first);
        assert!(slot.lock().unwrap().is_none(), "guard 析构即释放");
        assert!(claim_running(&slot, "A").is_ok(), "释放后必须能再起一轮");
    }

    /// R109：命令层不许 `.lock().unwrap()`。这里真的把锁弄中毒，断言闸门照常服务
    /// 而不是让之后每一条命令都 panic。
    #[test]
    fn a_poisoned_slot_still_serves_claims() {
        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let poisoning = slot.clone();
        let h = std::thread::spawn(move || {
            let _held = poisoning.lock();
            panic!("故意中毒");
        });
        let _ = h.join();
        assert!(slot.lock().is_err(), "夹具前提：这把锁现在确实是中毒的");
        let g = claim_running(&slot, "A").expect("中毒不该让命令层 panic");
        assert_eq!(
            slot.lock().unwrap_or_else(|e| e.into_inner()).as_deref(),
            Some("A")
        );
        drop(g);
        assert!(slot.lock().unwrap_or_else(|e| e.into_inner()).is_none());
    }

    // ---- Task 18 Step 4：raw body → 归一化命令 ----

    /// `InvokeBody::Raw` 里的字节必须原样交给归一化，一个字节都不许动：
    /// 图片解码按内容判定格式（`with_guessed_format`），途中任何转码/裁剪都会让它判错。
    #[test]
    fn a_raw_body_is_handed_over_byte_for_byte() {
        let bytes: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
        let got = take_raw_body(&InvokeBody::Raw(bytes.clone())).expect("raw 必须收");
        assert_eq!(got, bytes, "raw body 不许被改写");
    }

    /// JSON 分支必须**明确拒**，不许静默按空字节去解码。
    /// R141：raw 与 JSON 字段互斥 —— 前端 `invoke("normalize_image", bytes)` 一旦写成
    /// `invoke("normalize_image", { bytes })`，body 就变成 `Json`，这条测试是唯一能抓住它的地方。
    #[test]
    fn a_json_body_is_refused_and_named_as_json() {
        let err = take_raw_body(&InvokeBody::Json(json!({ "bytes": "aGVsbG8=" })))
            .expect_err("JSON body 必须拒收");
        assert!(
            err.contains("json"),
            "报错要指名收到的形态，否则前端只能对着「图片无法识别」猜：{err}"
        );
    }

    /// `generate_handler!` 是宏，漏登记不会编译报错，只会在运行时让前端收到
    /// "command normalize_image not found" —— 单测碰不到，所以按本项目既有的「源码级守卫」
    /// 一路（`tokens.test.ts` 解析 styles.css、T14 的线格式镜像）在这里直接读源码断言。
    #[test]
    fn the_normalization_command_is_registered_in_the_handler_list() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ipc.rs"));
        let list = src
            .split("tauri::generate_handler![")
            .nth(1)
            .and_then(|rest| rest.split("]").next())
            .expect("找不到 generate_handler 的登记表");
        assert!(
            list.contains("normalize_image,"),
            "命令写了但没登记，前端调用只会拿到 not found：{list}"
        );
    }
}
