// Task 13：Tauri 装配与 IPC 层。
//
// 本文件是 Plan A 里唯一允许 `use tauri` 的文件（`lib.rs` 只剩模块声明与一行转发）。
// 合帧逻辑与整条一轮对话的路径都**不依赖 tauri**（R106：出口抽成 `Publish`，
// 一轮抽成 `run_turn`），因为 `AppHandle` 在测试里构造不出来 —— 不抽这两层，
// 这段每次发送都要跑几百次的代码在生产里就永远没有测试。
use crate::agent::context::{ContextBuilder, ContextConfig};
use crate::agent::events::{AgentEvent, Decision, EventSink, StopReason};
use crate::agent::session::{AgentSession, SessionDeps};
use crate::agent::summarize::{SUMMARY_OMITTED, collect_summary, summary_note, summary_request};
use crate::agent::types::{ContentBlock, Message};
use crate::model::openai_compat::{OpenAiCompat, is_loopback_endpoint};
use crate::model::provider::ModelProvider;
use crate::permission::gate::PermissionGate;
use crate::settings::{Settings, SettingsStore, effective_roots};
use crate::store::db::{Db, SessionRow, events_from_messages};
use crate::tools::fetch::FetchTool;
use crate::tools::read_file::ReadFileTool;
use crate::tools::registry::ToolRegistry;
use crate::window_state::{self, Ev, WindowGeom, should_persist};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::InvokeBody;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WindowEvent};
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

/// `TextDelta` 的合帧缓冲。两个上限（30ms / 累计 512 字符）**都只在下一个 delta 到达时**
/// 由 `push` 检查一次 ⇒ 这里没有定时器，谁也别指望「时间到了自己会发一帧」。
/// 只负责「什么时候可以交出去」，交出之后发哪儿去归 `Publish`。
struct Coalescer {
    buf: Mutex<String>,
    last_flush: Mutex<Instant>,
}

impl Coalescer {
    /// 距上次 flush 的时间上限（30ms）。**它自己不会发帧**：`push` 在下一次被调用时拿
    /// `elapsed()` 与它比，比过了才当场交出整帧（实测由
    /// `the_30ms_threshold_releases_the_frame_on_the_next_delta` 钉住）。
    /// 所以「保证前端不觉得卡住」这句话的真相是：流还在动的时候最迟下一个 delta 就落地；
    /// 流的最后一帧没有下一个 delta，靠收尾的 `flush()` 兜底。
    const MAX_WAIT: Duration = Duration::from_millis(30);
    /// 累计字符上限：命中即发，不等 30ms。长段回复时这是 IPC 次数的主要来源。
    const MAX_CHARS: usize = 512;

    fn new() -> Self {
        Self {
            buf: Mutex::new(String::new()),
            // 构造即把 `last_flush` 拨到「现在」，计时从这一刻起算。诚实的说法：
            // 30ms 之内到达的第一个 delta **不会**因为时间命中而出帧（`elapsed() < MAX_WAIT`），
            // 它要么攒够 `MAX_CHARS`，要么等之后的某个 delta，要么由收尾的 `flush()` 带走。
            // 这里没有定时器，所以不存在「最多等 30ms 也能主动出去」那件事（F3/m3 改的就是这句）。
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
        guard(&self.buf)
    }
    fn last_flush(&self) -> std::sync::MutexGuard<'_, Instant> {
        guard(&self.last_flush)
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

// R109 的处置本身住在 `crate::lock`（与 `settings.rs`、`store/db.rs`、
// `permission/gate.rs`、`tools/registry.rs` 共用一份）。命令层的锁全是长命的，一次别处的
// panic 不许变成之后每条命令都 panic；这些锁保护的全是单点状态（一个 `String`、一个令牌、
// 一个缓冲），没有跨锁不变量，所以吞掉 poison 不会让人读到撕裂的形状。
use crate::lock::guard;

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
    /// 由 `Settings::to_model_config()` 给（T39）：`None` = 请求体不带 `temperature`，
    /// 端点自己定。和 `context` 同样理由 —— 会话不许自己去读 `Settings`。
    pub temperature: Option<f32>,
    /// T26（R276）：`read_file` 能读的那张目录表，**每轮从 `Settings` 现取**（`send_message`
    /// 头里那一次 `store.load()` 是它唯一的来源，不缓存 —— 用户改了设置，下一轮就该生效）。
    ///
    /// 为什么它是 `TurnDeps` 的一项、而不是 `SessionDeps` 上再加一个字段：`SessionDeps` 那条
    /// 路已经有 builder 方法 `AgentSession::with_roots`（`session.rs:86`），两处都能收 roots
    /// 就是「一个设置两个门」，两边给的值可以不一致而没人报错。R276 因此把它钉成一条线：
    /// `TurnDeps.roots` → `.with_roots(...)` → `ToolCtx.roots`，只此一处。
    /// T26 修复轮 1（R284）：这一项交的是**用户写的那份原表**（读写两侧不再裁盘），
    /// 「今天哪几条解得开」由 `run_turn` 在 `.with_roots(..)` 那一处经
    /// `settings::effective_roots` 现算 —— 使用点是文件系统依赖的唯一落点。
    /// 空表是**允许值**而且是默认值：那时 `read_file` 一律拒绝，那是设计（R90 取向），
    /// 不是这一层漏了什么。
    pub roots: Vec<PathBuf>,
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

/// 两次发送之间的摘要压缩（T41）。
///
/// 为什么它是一段**独立**的 `pub(crate)` 函数、而不是 `run_turn` 里的一串行：
/// `#[tauri::command]` 在单测里调不动（Plan A T13 实测），而 `run_turn` 要凑齐 `TurnDeps`
/// 那八项才谈得上一次调用。把压缩的真相收进一个不碰 `State` 的签名，Step 1–4 的测试直接调
/// 它（brief §5），`run_turn` 剩下的只有「按顺序装配」。
///
/// 三步，全部在「越了线」这个判定里面（`an_under_threshold_history_costs_no_model_call…`
/// 守的就是这条 —— 把 IO 挪到判定外，用户每次发消息都要白烧一次摘要调用）：
/// 1) 问构建器要不要压、压哪一段：纯决策，不调模型不做 IO（T40）；
/// 2) 用被丢掉的那一段换一句摘要。摘要调用报错就降级成 `SUMMARY_OMITTED` 并**照压**
///    （spec:365：降级只换正文，不换事务）；用户在摘要还挂着的时候按了停止是另一件事 ——
///    事务压根还没开始，一个字节都不写，原样交回；
/// 3) 一次事务删旧行 + 落一条 `Role::System` 备注（`db.rs` 的 `compact_session`），
///    然后把库**重读**回来。
///
/// 交回的是重读后的历史：调用方必须在这份历史之上才取 `prior`（brief §4 的第一个陷阱）。
/// 交回的事件只带两个数（`dropped` / `summary_chars`），摘要文本一个字都不在其中 ——
/// 它唯一的去处是库里那条 `Role::System` 备注，而那条备注在模型侧可见、用户侧不可见（R69）。
pub(crate) async fn compact_before_turn(
    db: &Arc<Db>,
    session_id: &str,
    model: &dyn ModelProvider,
    builder: &ContextBuilder,
    history: Vec<Message>,
    cancel: &CancellationToken,
) -> Result<(Vec<Message>, Option<AgentEvent>), String> {
    let Some(plan) = builder.compaction_plan(&history) else {
        return Ok((history, None));
    };

    let body = match collect_summary(model, summary_request(&plan.source), cancel).await {
        Ok(text) => text,
        // 取消优先于报错。`collect_summary` 的 `Err` 在这一臂既可能是「用户按了停止」也可能
        // 是端点报错，所以判据看令牌、不看错误文本（错误文本里那句「已取消」是它的兜底说法，
        // 不是事实来源）。没压成，也就没什么可宣布的。
        Err(_) if cancel.is_cancelled() => return Ok((history, None)),
        // 降级。原因只进日志、不进事件：`ContextCompacted` 的载荷是两个数，前端那句
        // 「已压缩 N 轮」不该被一条端点文案污染；日志才是它该去的地方（R107，GUI 没有控制台）。
        Err(reason) => {
            tracing::warn!("会话 {session_id} 的摘要调用失败，本轮改为省略占位：{reason}");
            SUMMARY_OMITTED.to_string()
        }
    };

    let note = summary_note(&body);
    let dropped = plan.dropped;
    // 字数按字符算不是按字节：中文备注用 `len()` 会显示成 3 倍（brief §4 的第三条陷阱）。
    let summary_chars = note.chars().count();
    let writer = db.clone();
    let write_sid = session_id.to_string();
    blocking(move || {
        writer
            .compact_session(&write_sid, &plan, &note)
            .map_err(|e| e.to_string())
    })
    .await?;

    // 重读，不在内存里自己拼那份「删了若干行 + 加了一条备注」的结果：
    // 库里是什么样就以库为准，`run_turn` 之后的 `restore`/`append_rows` 才有同一个基准。
    let reader = db.clone();
    let read_sid = session_id.to_string();
    let reread = blocking(move || reader.messages(&read_sid).map_err(|e| e.to_string())).await?;
    Ok((
        reread,
        Some(AgentEvent::ContextCompacted {
            dropped,
            summary_chars,
        }),
    ))
}

/// 一轮对话的完整生产路径：读历史 →（越线就）摘要压缩 → `AgentSession::run` →
/// **只写新增的行** → 条件自动改名。
///
/// R103 的落库所有权就落在 `prior` 这一个下标上：`session.rs:110` 的 `run()` 自己会
/// `history.push(Message::user(blocks))`（一条用户轮次 = 一行），而 `restore()` 装回来的
/// 历史本来就在库里。所以发送前一行都不写、跑完只写 `history[prior..]`。
/// brief:152-160 的写法（按块补写 + 整段重放）会把一轮的两行写成四行。
///
/// T41 加的那一段就插在 `prior` 之前，顺序是硬性的（brief §4 的三条陷阱）：
/// ① `prior` 取在压缩段**重读之后**那份历史之上 —— 取在压缩前就是 `history()[40..]` 越界；
/// ② 构建器只造一次，压缩段借 `&builder`，同一个 `builder` 随后 move 进 `SessionDeps`
///    （造两次就是两份阈值、两份校准，压缩判定和装配判定从此可以分叉）；
/// ③ `ContextCompacted` 在写本轮新行**之前**emit（顺序在用户那边是「先说压缩、再答这句」），
///    也在 `sink` move 进 `SessionDeps` 之前用借用发。
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
        temperature,
        roots,
        cancel,
    } = deps;
    let sid = session_id.to_string();

    // 读库失败（含 R71 的「某一行解不开」）直接报错，不返回「看起来完整」的历史。
    let reader = db.clone();
    let read_sid = sid.clone();
    let history = blocking(move || reader.messages(&read_sid).map_err(|e| e.to_string())).await?;

    // 构建器只在这里造一次（②），压缩段与本轮用的是同一个。
    let builder = ContextBuilder::new(context);

    // 摘要调用挂在流上的那几秒也必须能被「停止」打到，所以先把**这一只**令牌装进槽位。
    // `AgentSession::new` 自己造令牌（`session.rs:74`）且不接受外来的，只能先装这只、
    // 装配完再换成会话那只，并把已经按下的停止传染过去。
    let compaction = CancellationToken::new();
    let token = TurnToken { slot: cancel };
    token.install(compaction.clone());

    let (history, compacted) =
        compact_before_turn(&db, &sid, &*model, &builder, history, &compaction).await?;
    // ③：先宣布，再答这句。此刻本轮的一行都还没写。
    if let Some(event) = compacted {
        sink.emit(event);
    }
    // ①：这一行必须在压缩段之后，量的才是「restore 回来的那份历史」。
    let prior = history.len();

    // R105：装配期没有 session_id，所以 gate 的出口每轮换一次内核（`gate.rs:77`）。
    gate.set_sink(sink.clone());

    let mut session = AgentSession::new(SessionDeps {
        model,
        registry,
        gate,
        sink,
        context: builder,
        temperature,
    })
    // T26（R276）：**全仓库唯一一处**调用 `AgentSession::with_roots` 的生产代码。
    // 在它之前这一行不存在，于是 `ToolCtx.roots` 恒空、T25 的 `read_file` 对任何路径都
    // fail-closed —— 缺陷不补，切片 3 交付的第二个工具就是死的。
    // T26 修复轮 1（R284）：这里拿的是**有效表**，不是 `TurnDeps.roots` 那份原表。
    // 文件系统依赖的裁剪（绝对性 + canonicalize）从读写两侧搬到了这个使用点，每轮现算一次
    // （R276 的「每轮从 `Settings` 现取、不缓存」因此更硬了：今天的答案只在今天有效）。
    // 过滤仍然**只此一处** —— `settings.rs::effective_roots` 是它的唯一实现，本行是它唯一的
    // 生产调用点；第二处过滤意味着两处答案可以不一致，那正是 R276 把它钉成一条线的原因。
    .with_roots(effective_roots(&roots));
    let session_cancel = session.cancel_handle();
    token.install(session_cancel.clone());
    if compaction.is_cancelled() {
        // 停止是按在摘要上的：会话带着同一个事实早退（`session.rs:114` 直接收尾成
        // `Interrupted`，连 user 行都不 push），而不是在这里另发明一套「按了停止还跑不跑」。
        // `fresh` 因此是空的，本轮一个字都不写 —— 库里那一刻还是压缩前的完整历史。
        session_cancel.cancel();
    }
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

/// 装配层的 key 闸门：`None` 时究竟是拒绝还是放行，判据只有一个 —— 端点是不是回环。
///
/// 为什么这道闸住在装配层而不是 provider 里：`OpenAiCompat::new` 早就接受空 `api_key`
/// （`new_rejects_blank_base_url_but_not_blank_api_key` 钉着），拦住本机模型的一直是
/// 这里那句 `ok_or_else`。反过来，provider 也不该拥有「要不要凭据」这个决定 —— 它只是
/// 把 key 塞进 header，配不配是设置与端点的事。
///
/// 判据**只能窄到回环**：放错的代价是把一个无凭据的推理端点交给任意网络（SSRF 同一条
/// 红线）。非回环分支保持与抽函数之前逐字一致，包括「`Some("")` 也照样放行」这一条 ——
/// 那是既有行为，收紧它是另一件事，不在本轮范围。
fn resolve_api_key(api_key: Option<String>, base_url: &str) -> Result<String, String> {
    match api_key {
        Some(key) => Ok(key),
        None if is_loopback_endpoint(base_url) => Ok(String::new()),
        None => Err("尚未配置 API key，请在设置中填写".to_string()),
    }
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
    let api_key = resolve_api_key(
        blocking(move || key_store.api_key().map_err(|e| e.to_string())).await?,
        &settings.base_url,
    )?;

    // 明文 key 只在这一行进 `ModelConfig`：不打日志、不进事件、不回前端（spec:473）。
    // T39：温度从**同一个** `ModelConfig` 里取出来交给会话 —— 这里读一次 `Settings`，
    // `AgentSession` 只收现成的值，配置真相仍然只有一份。
    let model_cfg = settings.to_model_config(&api_key);
    let temperature = model_cfg.temperature;
    let model = Arc::new(OpenAiCompat::new(model_cfg).map_err(|e| e.to_string())?);
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
            // T39：同上，温度的唯一来源是上面那个 `ModelConfig`（同一个 `normalize()` 出来的）。
            temperature,
            // T26（R276）：roots 和上面两项同源 —— 同一个 `store.load()` 的每一轮现取，
            // 因此**用户改完设置发下一条消息就生效**，这里没有任何缓存可以过期。
            // R284：交的是设置文件里那份**原表**（不再是在读盘时被裁过的），裁剪在
            // `run_turn` 的 `.with_roots(..)` 那一处、每轮一次。
            // 这一行没法单测（`#[tauri::command]` 在单测里调不动，见本模块测试头 R103 段
            // 的说明）：它保证的是「来源是本轮那份 `Settings`」，而「值真的抵达 `ToolCtx`」
            // 由下面的 `run_turn_hands_the_effective_roots_not_the_persisted_list` 钉。
            roots: settings.roots.clone(),
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
/// T3 的两条硬约束（brief:394，实测，非推测）是下面 1、2 两条 —— 它们都是「前端怎么写、
/// 命令体就怎么收，谁也察觉不到谁」的那种，所以只能写在 doc 里让 T19 读得到。
/// 第 3 条不是硬约束，是平台面。
/// 1. **raw 与 JSON 字段互斥** —— 这条命令一旦加第二个具名参数，前端就没法再把它整体当
///    raw body 发。所以 max-edge / mime 这类旋钮只能内置（`image_prep::MAX_EDGE_PX`），
///    或另开一条 JSON 命令。
/// 2. **永远不要把 `subarray` 切出来的那段视图的 `.buffer` 交出去**（brief:394 T3 第 2 条，
///    实测；T19 的前端就靠这一条活着）：若 `bytes` 是一个切过的视图，`bytes.buffer` 带回的是
///    底层**整个** ArrayBuffer 而不是那一段 ⇒ 字节照发、长度照报，命令体这边**察觉不到多收**
///    —— `take_raw_body` 只做「把收到的字节原样克隆出去」（`a_raw_body_is_handed_over_byte_for_byte`
///    钉的就是这个），它没有任何办法知道「前端其实只想发中间那 3KB」。
///    前端要传切片就得把它重建成独立缓冲（`view.slice(0)`，或 `new Uint8Array(view)`），
///    不许 `view.buffer`。
/// 3. `Request` 的文档注释写明 raw body 「on all platforms except Android」。本项目只发
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
        guard(&self.0).write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        guard(&self.0).write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        guard(&self.0).flush()
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

/// 把 `Monitor::work_area()` 摊平成 `window_state::{sane,pick}` 要的 `(x, y, width, height)`。
/// 之所以在 `ipc.rs` 而不是 `window_state.rs` 里展开：那个模块不许依赖 Tauri
/// （`lib.rs:4-6` 的边界规则），顺带换来它的 6 条测试不需要运行时也不需要接显示器。
fn work_area(monitor: &tauri::Monitor) -> (i32, i32, u32, u32) {
    // `tauri::Monitor` 的字段是 `pub(crate)`，只给访问器（`window/mod.rs:56-101`），
    // 而 `work_area()` 返回的是 `&PhysicalRect`，那个结构体的字段才是公开的。
    let area = monitor.work_area();
    (
        area.position.x,
        area.position.y,
        area.size.width,
        area.size.height,
    )
}

/// T46b：按存档几何恢复窗口。**任何一步不成立就退回 conf 里的默认尺寸**，
/// 全程不许 panic —— 一个存坏的 `window.json` 不该有能力让应用打不开。
fn restore_window_geometry(handle: &AppHandle) {
    // conf 里的窗口是在 `setup` 钩子**之前**创建的（`tauri-2.12.4/src/app.rs:2692` 的
    // `WebviewWindowBuilder::from_config`），所以到这里才拿得到 `main`。
    // 这也是 Plan A 写死的那个 `"label": "main"` 第一次真的有消费者。
    let Some(window) = handle.get_webview_window("main") else {
        return;
    };
    let Ok(dir) = handle.path().app_data_dir() else {
        return;
    };
    let Some(saved) = window_state::load(&dir.join(window_state::STATE_FILE)) else {
        return;
    };
    // 枚举不到任何显示器就**不**恢复：无法证明「某块屏上看得见」的几何比默认尺寸更危险
    // —— 拔掉副屏后重启、窗口落在 x=3000 上就是这么来的。
    // 判定要对着**每一块**屏做，而不是只问 `current_monitor()`：这时窗口还是系统刚按默认位置
    // 摆出来的那块（通常主屏），摆在副屏上的存档会被误判成越界丢掉，症状是「每次重启都从
    // 副屏被拽回主屏」—— 等于这条需求对双屏用户白做。
    let Ok(monitors) = window.available_monitors() else {
        return;
    };
    let works: Vec<(i32, i32, u32, u32)> = monitors.iter().map(work_area).collect();
    let Some(g) = window_state::pick(saved, &works) else {
        return;
    };
    let restored = window
        .set_size(PhysicalSize::new(g.width, g.height))
        .and_then(|_| window.set_position(PhysicalPosition::new(g.x, g.y)));
    if let Err(e) = restored {
        tracing::warn!("窗口几何未能恢复，留在默认尺寸：{e}");
    }
}

/// 关闭事件发生时把当前几何写回存档；取不到就什么也不写。
fn persist_window_geometry(window: &tauri::Window) {
    // 单窗口应用（spec §6.6）：别的 label 的窗口不该覆盖这份存档。
    if window.label() != "main" {
        return;
    }
    let Ok(dir) = window.path().app_data_dir() else {
        return;
    };
    // `Destroyed` 这一臂很可能已经取不到几何了，那是「不写」而不是「写 (0,0,0,0)」：
    // 写一份 zeros 进存档会把一次正常的关闭变成下一次启动要靠 `sane` 兜底的脏数据。
    // 位置取 **outer**、尺寸取 **inner**，这个不对称是被真窗口逼出来的：恢复侧的
    // `set_position` 与 `set_size` 分别按 outer 位置、inner 尺寸生效（tao 的 `resize`
    // 改的是客户区）。两边都用 `outer_size` 时，存档的 1000×800 会被当成客户区尺寸重放，
    // 实测每次启动净增一圈边框 —— 在这台 2560×1600 的屏上是 +22 宽 / +56 高，连开三个
    // 窗口依次 1000×800 → 1022×856 → 1044×912，一路涨到被 `sane` 夹满工作区为止。
    let (Ok(position), Ok(size)) = (window.outer_position(), window.inner_size()) else {
        return;
    };
    // 最小化状态下关窗，Windows 报的是 (-32000, -32000) 一类的哨兵位置。这里**不加特例**：
    // 照原样写进去，由下次启动的 `sane` 判成越界 ⇒ 回到默认尺寸。
    // 在此识别「现在是 minimized」要多引一层窗口状态查询，换来的只是同一次重启少丢一个位置。
    let geom = WindowGeom {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    if let Err(e) = window_state::save(&dir.join(window_state::STATE_FILE), geom) {
        tracing::warn!("窗口位置未能保存：{e}");
    }
}

/// 生产工具表的**唯一**装配点（Task 27）。
///
/// 零参是刻意的：两把工具都不需要 `AppState`，运行期要的东西（`http`、`cancel`、`roots`）
/// 由会话每轮现构的 `ToolCtx` 给（R276 的单一入口）。一旦它开始收参数，「全进程装配一次」
/// 就退化成「每个调用点各装配一次」，而 `R101` 那条「一个目录只许一个 `SettingsStore`」
/// 的教训说的正是这种重复。
///
/// 名字序由 `ToolRegistry` 里的 `BTreeMap` 决定（字典序），**不是**这里的注册顺序；
/// `production_registry_ships_fetch_url_and_read_file` 钉的是那份字面名单。
///
/// 两把都是 `Risk::Read`，spec 5.1「内置工具全为 Read ⇒ 全程零弹窗」到这一轮仍然成立。
///
/// 与 R286 的分工：这里**只管注册**，roots 为空时 `read_file` 照样在表里 —— 模型真喊它
/// 就走闸门被拒。要不要把它的**声明**交给模型，由 `agent/session.rs::request()` 按
/// `read_file::usable_with_roots` 决定（那是省 token 的事，不是授权的事）。
pub fn build_production_registry() -> Arc<ToolRegistry> {
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Arc::new(FetchTool::new()));
    registry.register(Arc::new(ReadFileTool));
    registry
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 没有任何外部插件：图片附件入口是原生 `<input type="file">`（R144），
        // 前端因此既不需要 dialog 也不需要 fs 权限。曾经的 `tauri_plugin_dialog`
        // 与 capabilities 里的 `dialog:allow-open` 一起在评审 #29 摘掉，理由写在
        // `Cargo.toml` 与 `capabilities/default.json` 里。
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
            // Task 27 起生产表里有两把工具（见 `build_production_registry`），两把都是
            // `Risk::Read` ⇒ 自动放行仍然成立、用户侧仍然零弹窗；而 sink 还没换上的窗口里
            // `check` 给的是 deny（R44：headless ≠ 全授权）。
            app.manage(AppState {
                db,
                settings,
                registry: build_production_registry(),
                gate: Arc::new(PermissionGate::new()),
                cancel: Arc::new(Mutex::new(None)),
                running: Arc::new(Mutex::new(None)),
                log_path,
            });
            // T46b：几何恢复放在 `manage` 之后、`Ok(())` 之前——`app_data_dir()` 与
            // `create_dir_all` 已经在前头跑过，存档目录这时一定存在。
            restore_window_geometry(app.handle());
            Ok(())
        })
        // T46b：落盘时机（计划 Step 2 的裁定）。一次拖拽会发来几百个 `Resized`/`Moved`，
        // 全部写盘等于把「记住位置」做成「每移一次写一次磁盘」。**不为此引入定时器去抖**：
        // 那是给「关窗时定时器还在写」新造一类竞态。决定本身抽成纯函数 `should_persist`，
        // 于是这条取舍有测试而不是只有注释。代价如实记在这里：崩溃或强杀会丢窗口位置，
        // 症状是「下一次回到默认尺寸」，不是数据丢失。
        .on_window_event(|window, event| {
            let ev = match event {
                WindowEvent::Resized(_) => Ev::Resized,
                WindowEvent::Moved(_) => Ev::Moved,
                // `CloseRequested` 是带 `api` 字段的变体，而 `WindowEvent` 整个枚举
                // `#[non_exhaustive]`（`app.rs:111`）⇒ 结构体变体要写 `{ .. }`、且必须有兜底臂。
                WindowEvent::CloseRequested { .. } => Ev::CloseRequested,
                WindowEvent::Destroyed => Ev::Destroyed,
                _ => return,
            };
            if should_persist(ev) {
                persist_window_geometry(window);
            }
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
        // R108 / T18 m2 —— `tauri.conf.json` 里那句 `"label": "main"` 的理由（R108 要求每处
        // conf 改动写清理由，而这份 conf 里放不下理由，原因写在下面）：
        // 默认 label 本来就是 `main`，这行显式改动**冗余无害**，留着它的价值在于
        // `capabilities/default.json` 的 `"windows": ["main"]` 是按 **label** 指名窗口的 ——
        // label 写死在 conf 里，「主窗口叫什么」就只有一份答案，不用去赌某个依赖版本的默认值。
        // 为什么理由落在 Rust 里而不是 conf 里（实测两次 `cargo check` 均 rc=101）：这份 conf 由
        // `tauri-build` 的 build script 在编译期解析，`//` 注释得到 `key must be a string at
        // line 15 column 9`，自由字段 `"_comment"` 得到 `unknown field _comment` —— JSON 里没有
        // 任何合法的注释位。红线：交出去的 conf 必须能被解析（`cargo check` rc=0 为准）。
        // T46b 之后 conf 里那两行 `"width": 800, "height": 600` **保留**，它的身份是
        // 「没有存档时的尺寸」：删掉它换到的是 Tauri 自己的默认尺寸，那才是第三份真相
        //（存档 / conf / 框架默认），而 `window_state::sane` 丢掉一份越界存档之后
        // 落到的正是这一行。
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
                    // 夹具不带温度 = 请求体里不带这个键，与 T39 之前的行为逐字相同
                    // （`run_turn` 侧的那条线由 `session.rs` 的
                    // `the_session_temperature_reaches_the_chat_request` 钉）。
                    temperature: None,
                    // 夹具默认不带 roots：除了那条专门量 roots 的测试（它自己搭 `TurnDeps`），
                    // 其余测试走的是生产的空表形状 —— 表空时 `read_file` 会拒。
                    // T27（R286）之后这里还要多说一句：`build_production_registry()` 的表里
                    // **有** `read_file`，但空 roots 那一轮交给模型的声明表里没有它，
                    // 而夹具的 registry 本来就是空的 —— 两件事各自成立，别混着读。
                    // 生产装配点那行由 `the_assembly_wires_the_production_registry` 钉（M4：
                    // 把 `app.manage` 换回空表，405 条单测全绿，所以判据只能是源码级守卫）。
                    roots: Vec::new(),
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

    /// 事件的 serde 标签名，只用来让行序断言失败时能一眼看出「谁排在了谁前面」
    /// （直接比 `Vec<AgentEvent>` 的话，失败信息是两大屏结构体字段）。
    fn event_tag(e: &AgentEvent) -> &'static str {
        match e {
            AgentEvent::TurnStart { .. } => "turn_start",
            AgentEvent::TextDelta { .. } => "text_delta",
            AgentEvent::UserMessage { .. } => "user_message",
            AgentEvent::ToolCall { .. } => "tool_call",
            AgentEvent::PermissionRequest { .. } => "permission_request",
            AgentEvent::PermissionResolved { .. } => "permission_resolved",
            AgentEvent::ToolResult { .. } => "tool_result",
            AgentEvent::Usage { .. } => "usage",
            AgentEvent::ContextCompacted { .. } => "context_compacted",
            AgentEvent::Error { .. } => "error",
            AgentEvent::Done { .. } => "done",
        }
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

        // F1（T13 m1）：**行序**。上面与下面这几条断言全都是「按角色筛一遍再比」或
        // 「数个数」，所以一轮内部整段倒序在它们眼里看不出来 —— 评审变异 M6 实证过：
        // `append_rows` 的迭代一旦改成反向，230 条测试全绿，而 `messages()` 是
        // `ORDER BY id`（写入顺序，db.rs:202），落库顺序就是重放顺序 ⇒ 助手回复会排在
        // 用户提问之前。这里按**落库行的原始顺序**逐行断言角色与文本，谁把追加顺序弄倒
        // 就必须在这里红。
        assert_eq!(
            msgs.iter()
                .map(|m| (m.role.clone(), m.text_of()))
                .collect::<Vec<_>>(),
            vec![
                (Role::User, "第一句".to_string()),
                (Role::Assistant, "回复一".to_string()),
                (Role::User, "第二句".to_string()),
                (Role::Assistant, "回复二".to_string()),
            ],
            "落库行序必须是「一问一答」交替；T20 的重放直接吃这个顺序"
        );
        // 重放侧同样要按行序交替：剔除 `usage` 与 `done` 两支后的**全事件序列**。
        // `turn_start` 排在 `user_message` 之后不是笔误 —— `events_from_messages`
        // 是在助手行才发它（db.rs:395），顺序本身就是行序的镜像。
        let replay_tags: Vec<&str> = replay
            .iter()
            .filter(|e| !matches!(e, AgentEvent::Usage { .. } | AgentEvent::Done { .. }))
            .map(event_tag)
            .collect();
        assert_eq!(
            replay_tags,
            vec![
                "user_message",
                "turn_start",
                "text_delta",
                "user_message",
                "turn_start",
                "text_delta",
            ],
            "重放的事件顺序就是行序：user_message 必须排在它那条回复之前"
        );

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

    // ==== Task 26：装配层真的把**有效表**交给了会话（R276 的唯一入口 + R284 的使用点）====
    //
    // 为什么这条测试非得住在 ipc 里：`session.rs` 那条
    // `run_turn_passes_configured_roots_into_tool_ctx` 量的是 `AgentSession::with_roots`
    // **这个方法本身**能用；而本任务修的是「它在生产里从未被调用」。把 `run_turn` 里
    // `AgentSession::new(..).with_roots(effective_roots(&roots))` 那一行删掉（现 :438），
    // `session.rs` 全绿、`settings.rs` 全绿，只有这里红 ——
    // 判据必须能被打红，不然它不构成一条线（R56 的老规矩）。
    // R284 之后这里还多钉一件事：把那一行的 `effective_roots(&roots)` 换回 `roots`，
    // 同一处当场红（够不着的那条 root 就又能把闸门整体拖死了）。
    //
    // 取证方式和 `session.rs` 那边一致：从**工具自己手里**读 `ctx.roots`。`ToolCtx` 是
    // 会话内部造的（`session.rs:123`），从 `AgentSession` 的字段读回来属于自证。

    use crate::agent::types::ToolOutput;
    use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
    // `ModelDelta`/`ModelError`/`ChatRequest`/`ModelProvider`/`Stream`/`async_trait`/
    // `VecDeque` 都由本模块后头那组 `use`（Task 24/41 段）带进来了 —— 模块级 `use` 与
    // 书写顺序无关，这里再写一遍就是 E0252 重复导入。

    /// 记下**执行那一刻**的 `ctx.roots`，每次调用一条。
    #[derive(Default)]
    struct RootsSpy {
        seen: Mutex<Vec<Vec<PathBuf>>>,
    }

    #[async_trait]
    impl Tool for RootsSpy {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "roots_spy".into(),
                description: "记下执行那一刻 ctx.roots 的取证工具".into(),
                input_schema: json!({"type": "object"}),
                // Read 档是有意的：这条测试量的是 roots 到没到。Ask 档要在测试里搭一套
                // 应答任务，那会把「roots 没接上」的读数埋进应答时序里。
                risk: Risk::Read,
            }
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            self.seen.lock().unwrap().push(ctx.roots.clone());
            Ok(ToolOutput::text("记下了"))
        }
    }

    impl RootsSpy {
        fn seen(&self) -> Vec<Vec<PathBuf>> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// 按轮吐剧本的模型。本模块原有的两只都不会发 `tool_call`（Plan A 的生产工具表是空的），
    /// 而 roots 的唯一消费者就是工具 —— 不让模型真的调一次工具，「传到执行点」就无从取证。
    /// **取空了就是夹具写错**，不许安静地回一条空消息把测试做绿。
    struct Scripted {
        rounds: Mutex<VecDeque<Vec<ModelDelta>>>,
    }

    #[async_trait]
    impl ModelProvider for Scripted {
        fn stream<'a>(
            &'a self,
            _req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            let round = self
                .rounds
                .lock()
                .unwrap()
                .pop_front()
                .expect("夹具轮数不够");
            Box::pin(futures::stream::iter(
                round.into_iter().map(Ok).collect::<Vec<_>>(),
            ))
        }
    }

    /// 一轮工具调用：id 带轮次（R57 —— 全复用 `"c1"` 的话配对断言会 latch 到早先那次）。
    fn tool_round(round: usize, name: &str) -> Vec<ModelDelta> {
        vec![
            ModelDelta::ToolCallStart {
                index: 0,
                id: Some(format!("r{round}c0")),
                name: Some(name.to_string()),
            },
            ModelDelta::ToolCallArgs {
                index: 0,
                fragment: "{}".to_string(),
            },
        ]
    }

    /// brief Step 5 的生产侧那条线：`TurnDeps.roots`（= 用户写的那张表）必须**在交给会话的
    /// 那一刻**被裁成有效表，然后一路走到 `ToolCtx.roots`。R284 把「这条今天还在不在」的
    /// 判定从读写两侧搬到这里，所以这条测试钉的两件事是：**使用点裁剪**和 **R276 的单一入口**
    /// —— 把 `run_turn` 里 `.with_roots(..)` 那一行删掉这里当场红（表变空）。
    #[tokio::test]
    async fn run_turn_hands_the_effective_roots_not_the_persisted_list() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let spy = Arc::new(RootsSpy::default());
        // 注册在**夹具的** registry 上（`Fixture::new` 每轮现构一只空表），
        // 生产那只在 `build_production_registry` 函数体里装配、由 `app.manage` 那一行接管
        // —— 夹具**不**改用它：这里的测试只量自己注册的那把替身工具。
        f.registry.register(spy.clone());

        // 两条真目录 + 一条**不存在**的路径 + 一条**相对**路径，原样塞进 `TurnDeps.roots` ——
        // 那是设置文件里的形状（R284 之后读写两侧不再裁盘，所以到这一层的表就是用户写的那份）。
        // 把不存在的那条塞进来是故意的，它就是 Important-1 的现场：用户拔了 U 盘，那条 root 照旧在
        // 设置里，但这一轮不许把它交给闸门 —— 规则 4 会整体拒绝，连带让别的合法 root 也读不到东西。
        // 相对那条钉的是 R285 的**使用点**一半：`"."` 是解得开的（就解在进程 CWD 上），
        // 所以只有绝对性这条判据能拦住它，而拦住它的地方必须是交闸门之前，不只是 settings 层。
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ghost = a.path().join("never-created");
        let roots = vec![
            a.path().to_path_buf(),
            b.path().to_path_buf(),
            ghost.clone(),
            PathBuf::from("."),
        ];

        let model = Arc::new(Scripted {
            rounds: Mutex::new(VecDeque::from(vec![
                tool_round(0, "roots_spy"),
                vec![ModelDelta::Text("看完了".into())],
            ])),
        });
        let rec = Arc::new(Recorded::default());
        let sink = Arc::new(TauriEmitter::new(rec.clone(), sid.clone()));
        let reason = run_turn(
            TurnDeps {
                db: f.db.clone(),
                registry: f.registry.clone(),
                gate: f.gate.clone(),
                model,
                sink: sink.clone(),
                context: ContextConfig::default(),
                temperature: None,
                roots: roots.clone(),
                cancel: f.cancel.clone(),
            },
            &sid,
            vec![ContentBlock::Text {
                text: "读一下我的笔记".into(),
            }],
        )
        .await
        .expect("带一次工具调用的一轮必须整体成功");
        sink.flush();
        assert_eq!(reason, StopReason::EndTurn, "剧本第二轮就是收尾文本");

        let seen = spy.seen();
        assert_eq!(
            seen.len(),
            1,
            "夹具只安排了一次工具调用，取证点就得恰好记下一条；0 条说明工具压根没跑，那条断言是空的"
        );
        assert_eq!(
            seen[0],
            vec![
                a.path().canonicalize().unwrap(),
                b.path().canonicalize().unwrap()
            ],
            "交到执行点的必须是**有效表**（绝对 + 解得开 + canonical 形态），不是设置文件里 \
             那份原表；这一条同时守住 R276 的单一入口 —— 删掉 `run_turn` 里 `.with_roots(..)` \
             那一行，这里就空了。夹具里那条 `\".\"` 钉的是绝对性：它 canonicalize 得开（就解在进程 \
             CWD 上），所以拦得住它的只有「绝对」这一条判据（R285），而拦截必须发生在交闸门之前 —— \
             删掉 `effective_roots` 里的 `is_absolute()`，这里就多出一条 `\\\\?\\<进程 CWD>`，\
             等于程序替用户选了一个他没指过的目录；实得 {:?}（原表 {:?}）",
            seen[0],
            roots
        );
        assert!(
            !seen[0].iter().any(|p| p == &ghost),
            "暂时够不着的那条 root 不许进这一轮的使用表（R284）；实得 {:?}",
            seen[0]
        );
    }

    /// 空表也要**原样**传下去：装配层不许在 roots 为空时替用户补一个「看起来合理」的目录
    /// （进程 cwd、可执行文件目录、app data 目录都在此列）。R90 的空 `base_url` 是同一个
    /// 取向 —— 那一支真被补过，症状是把会话内容发给用户没选过的端点；这一支补了的代价是
    /// 模型能读用户没批准过的目录。两条都是「界面替用户做主」，只是方向相反。
    #[tokio::test]
    async fn an_empty_roots_table_is_handed_over_empty_instead_of_a_default_directory() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let spy = Arc::new(RootsSpy::default());
        f.registry.register(spy.clone());

        let model = Arc::new(Scripted {
            rounds: Mutex::new(VecDeque::from(vec![
                tool_round(0, "roots_spy"),
                vec![ModelDelta::Text("没读到东西".into())],
            ])),
        });
        let rec = Arc::new(Recorded::default());
        let sink = Arc::new(TauriEmitter::new(rec.clone(), sid.clone()));
        run_turn(
            TurnDeps {
                db: f.db.clone(),
                registry: f.registry.clone(),
                gate: f.gate.clone(),
                model,
                sink: sink.clone(),
                context: ContextConfig::default(),
                temperature: None,
                roots: Vec::new(),
                cancel: f.cancel.clone(),
            },
            &sid,
            vec![ContentBlock::Text {
                text: "读一下我的笔记".into(),
            }],
        )
        .await
        .expect("空 roots 不是错误，一轮照常跑完");
        sink.flush();

        let seen = spy.seen();
        assert_eq!(seen.len(), 1, "同上：取证点必须真的跑过一次");
        assert!(
            seen[0].is_empty(),
            "roots 为空时执行点却拿到了 {:?} —— 那是装配层替用户做的决定",
            seen[0]
        );
    }

    // ---- Task 27：生产 registry 的装配判据 ----

    /// 生产工具表**就是这份名单**：多一把、少一把都算装配错，而这一条是它唯一的钉子
    /// （Plan A 交的是空表，Plan B 到此才开始有真工具进生产）。
    /// `echo` / `guarded` 这类替身只该活在 `#[cfg(test)]` 里 —— 它们的名字一旦进生产，
    /// 请求体里就多出一条用户看不懂、实现也不是给用户的工具。
    #[test]
    fn production_registry_ships_fetch_url_and_read_file() {
        let r = build_production_registry();
        let names: Vec<String> = r.defs().into_iter().map(|d| d.name).collect();
        // 这个字面顺序由 `ToolRegistry` 的 `BTreeMap`（字典序）决定，**不是**注册顺序。
        // 刻意不加 `.sort()`、也不换成集合比较：T33 的 MCP 工具重挂之后仍然靠「名字序不变」
        // 保证界面与模型侧的工具列表不因重连而抖动，把这条性质藏起来就没法发现它被改坏。
        assert_eq!(
            names,
            vec!["fetch_url", "read_file"],
            "模型看到的工具集必须是这份名单；echo/guarded 这类测试工具绝不能进生产（它们只活在 #[cfg(test)]）"
        );
        assert_eq!(
            r.risk_of("fetch_url"),
            Risk::Read,
            "内置工具全为 Read ⇒ 才谈得上零弹窗"
        );
        assert_eq!(
            r.risk_of("read_file"),
            Risk::Read,
            "read_file 进生产的前提就是它仍是只读档（spec 5.1）；它变成 Write 就会开始弹窗"
        );
        assert_eq!(
            r.risk_of("delete_everything"),
            Risk::Exec,
            "未知工具按最严一档（R158）"
        );
    }

    /// R282：`SettingsView` 的线上形状 —— `roots` 落在**顶层**、是**字符串数组**。
    ///
    /// brief Step 5 写的是「`SettingsView` 加 `roots: Vec<String>`」，而那是**空转**：
    /// `settings` 那一项是 `#[serde(flatten)]`（:655），新字段自己就冒到顶层来了，
    /// 手加一个 `roots` 字段反而是同一份设置的两个来源（R276 要避免的那个形状，另一个方向）。
    /// 但「不用改代码」不等于「不用测」：flatten 一旦被人改成嵌套（或者给这个 struct 加了
    /// `rename_all`），前端 `settings.roots` 就静默变成 `undefined`，而 `tsc` 对着 Rust 侧
    /// 什么都看不见。这条测试钉的就是这个形状，前端镜像（`src/types.ts`）由它兜住。
    #[test]
    fn the_settings_view_carries_roots_at_the_top_level_as_strings() {
        let view = SettingsView {
            settings: Settings {
                // `\\?\` 前缀是刻意选的：`PathBuf` 的 serde 实现原样交字符串，
                // 于是这条测试同时也在钉「Windows 长路径前缀不会被拆成结构体」。
                roots: vec![
                    PathBuf::from(r"\\?\C:\notes"),
                    PathBuf::from(r"\\?\D:\code"),
                ],
                ..Default::default()
            },
            api_key_masked: None,
            corrupt_backup: None,
        };
        let v = serde_json::to_value(&view).unwrap();
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("view 必须序列化成一个对象，实得 {v:?}"));
        assert!(
            obj.contains_key("roots"),
            "`roots` 没在顶层 —— flatten 臂被改坏了，前端读到的会是 undefined。顶层键：{:?}",
            obj.keys().collect::<Vec<_>>()
        );
        let arr = v["roots"]
            .as_array()
            .unwrap_or_else(|| panic!("roots 必须是 JSON 数组，实得 {:?}", v["roots"]));
        assert_eq!(
            arr.iter()
                .map(|x| x.as_str().unwrap_or("<非字符串>"))
                .collect::<Vec<_>>(),
            vec![r"\\?\C:\notes", r"\\?\D:\code"],
            "每一项都得是字符串：TS 镜像声明的是 string[]，混进对象就是镜像骗人"
        );
        // 视图自带的两项与 flatten 进来的十三项**共处一层**（`roots` 不许被挤进 `settings` 里），
        // 而明文 key 依旧不许出现在任何一个键名上（R83 / spec:473）。
        assert!(obj.contains_key("api_key_masked") && obj.contains_key("corrupt_backup"));
        assert!(
            !obj.contains_key("settings"),
            "顶层冒出了嵌套的 `settings` 对象，前端镜像是按顶层读的"
        );
        assert!(
            !obj.contains_key("api_key"),
            "明文 key 不许出现在这条边界上（这里能出现 `api_key_masked`，不能出现 `api_key`）"
        );
        // 默认值那一支也得有键：空表序列化成 `[]` 而不是整个键消失，
        // 否则前端的 `settings.roots.map(...)` 在生产默认配置下直接抛。
        let empty = serde_json::to_value(SettingsView {
            settings: Settings::default(),
            api_key_masked: None,
            corrupt_backup: None,
        })
        .unwrap();
        assert_eq!(
            empty["roots"].as_array().map(|a| a.len()),
            Some(0),
            "空 roots 必须是可见的空数组，不是缺键"
        );
    }

    // ==== Task 41：两次发送之间的摘要压缩 ====
    //
    // 本组的形状由 brief §5 定死：`#[tauri::command]` 在单测里调不动（Plan A T13 实测过），
    // 所以压缩段的全部真相住在**不依赖 `State`** 的 `compact_before_turn` 里，Step 1–4 的
    // 测试一律直接调它、一处都不碰 `run_turn`。唯一例外是
    // `prior_is_recounted_after_compaction`：`prior` 那一行下标只存在于 `run_turn` 内部，
    // 判据 (a)「把 prior 放回压缩前」要红的就是它，理由写在那条测试的注释里。
    //
    // 假模型本地写：`session.rs` 的 `VecSink`/`ScriptedModel`/`HangingModel` 是那个私有测试
    // 模块的（:356/:467/:523），跨不过模块边界，也**不许**为了让它们跨过来改成 `pub`。

    use crate::agent::summarize::{
        SUMMARY_INSTRUCTION, SUMMARY_OMITTED, SUMMARY_PREFIX, summary_note,
    };
    use async_stream::stream;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 摘要调用与本轮回答共用**同一只**模型 —— 生产就是这样（`TurnDeps.model` 只有一个
    /// `Arc`，`compact_before_turn` 与 `AgentSession` 拿的是它），所以这个替身按请求头部分流：
    /// 头一条是 `SUMMARY_INSTRUCTION` 就走摘要那一支，否则走回答那一支。
    /// `calls` 逐次记账（R57：「模型实际看到了什么」只能从模型这一侧取证）。
    struct FakeModel {
        /// 摘要调用吐出的分片（失败那一支里它是「报错之前已经到的半句」）
        summary_parts: Vec<String>,
        /// `true` = 摘要调用先吐 `summary_parts` 再回一个 `Api` 错误（429 那一类）
        summary_fails: bool,
        /// `true` = 摘要调用吐完分片后**挂住不回**，等测试按停止
        summary_stalls: bool,
        /// 本轮回答的队列（只有走 `run_turn` 的那条测试用得到）
        replies: Mutex<VecDeque<String>>,
        calls: Mutex<Vec<ChatRequest>>,
        /// 挂住夹具的同步点：置位 = 最后一个分片已经抵达消费者。不靠 sleep 猜时间。
        stalled: Arc<AtomicBool>,
    }

    impl FakeModel {
        fn build(parts: &[&str], answers: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                summary_parts: parts.iter().map(|s| s.to_string()).collect(),
                summary_fails: false,
                summary_stalls: false,
                replies: Mutex::new(answers.iter().map(|s| s.to_string()).collect()),
                calls: Mutex::new(vec![]),
                stalled: Arc::new(AtomicBool::new(false)),
            })
        }
        fn summarizing(parts: &[&str]) -> Arc<Self> {
            Self::build(parts, &[])
        }
        /// 报错之前先吐出 `parts` 那半句：半截摘要不是摘要。
        fn failing(parts: &[&str]) -> Arc<Self> {
            let mut raw = Self::build(parts, &[]);
            // 构造期改一次：`Arc` 之后不再可变，所以这里用 `get_mut`（唯一持有者）。
            Arc::get_mut(&mut raw).unwrap().summary_fails = true;
            raw
        }
        /// 吐完 `parts` 之后永久挂住：测试在 `stalled` 置位的那一刻按停止。
        fn stalling(parts: &[&str]) -> Arc<Self> {
            let mut raw = Self::build(parts, &[]);
            Arc::get_mut(&mut raw).unwrap().summary_stalls = true;
            raw
        }
        /// 走完整 `run_turn` 时用：摘要与回答都要能答。
        fn summarizing_and_replying(parts: &[&str], answers: &[&str]) -> Arc<Self> {
            Self::build(parts, answers)
        }
        fn calls(&self) -> Vec<ChatRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ModelProvider for FakeModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.calls.lock().unwrap().push(req.clone());
            let is_summary = req
                .messages
                .first()
                .is_some_and(|m| m.role == Role::System && m.text_of() == SUMMARY_INSTRUCTION);
            if !is_summary {
                // 回答那一支：取完了就是夹具写错（轮数不够），不许安静地回一条空消息把测试做绿。
                let reply = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("夹具的回答轮数不够");
                return Box::pin(futures::stream::iter(
                    vec![ModelDelta::Text(reply)].into_iter().map(Ok),
                ));
            }
            if self.summary_fails {
                let head = self.summary_parts.clone();
                return Box::pin(futures::stream::iter(
                    head.into_iter()
                        .map(|t| Ok(ModelDelta::Text(t)))
                        .chain(std::iter::once(Err(ModelError::Api(
                            "429 rate limited".to_string(),
                        )))),
                ));
            }
            if self.summary_stalls {
                let head: Vec<ModelDelta> = self
                    .summary_parts
                    .iter()
                    .cloned()
                    .map(ModelDelta::Text)
                    .collect();
                let ready = self.stalled.clone();
                return Box::pin(stream! {
                    for d in head {
                        yield Ok(d);
                    }
                    ready.store(true, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                });
            }
            Box::pin(futures::stream::iter(
                self.summary_parts
                    .iter()
                    .cloned()
                    .map(ModelDelta::Text)
                    .map(Ok)
                    .collect::<Vec<_>>(),
            ))
        }
    }

    /// 40 条 user/assistant 交替 = **20 轮**，每条**恰好 800 个 ASCII 字符** ⇒ 单条
    /// `4 + 800/4 = 204 token`，与 `context.rs` 的 `mod compaction_planning` 同一个夹具口径。
    /// 内容带行号（`000…012`），所以「哪一段被丢、哪一段留下」能逐字对撞，而不是靠数条数。
    fn long_history() -> Vec<Message> {
        (0..40)
            .map(|i| Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                blocks: vec![ContentBlock::Text {
                    text: format!("{i:0>800}"),
                }],
            })
            .collect()
    }

    /// 上面那份夹具在这组参数下**必然**越线，且 `compaction_plan` 恰好压掉最老的 10 轮 = 20 条
    /// （`40/2 = 20`、`keep_turns: 10` ⇒ 刀口 `starts[10] = 20`）。算术本身归 `context.rs` 测，
    /// 这里要的只是「压缩一定会发生」这个前提。
    fn compacting_cfg() -> ContextConfig {
        ContextConfig {
            budget_tokens: 1_024,
            reserve_output: 0,
            keep_turns: 10,
            ..Default::default()
        }
    }

    fn seed(db: &Db, session_id: &str, msgs: &[Message]) {
        for m in msgs {
            db.append_message(session_id, m).unwrap();
        }
    }

    /// Step 1–4 共用的一次压缩。`ContextBuilder` 与生产同一处造出来（造一次、然后 move 进
    /// `SessionDeps`），所以这里也是 `new(cfg)` 之后再借 `&builder`（§2a 的形状）。
    async fn compact_once(
        f: &Fixture,
        model: &Arc<FakeModel>,
        sid: &str,
        history: Vec<Message>,
        cfg: ContextConfig,
        cancel: &CancellationToken,
    ) -> (Vec<Message>, Option<AgentEvent>) {
        compact_before_turn(
            &f.db,
            sid,
            &**model,
            &ContextBuilder::new(cfg),
            history,
            cancel,
        )
        .await
        .expect("压缩段本身不许报错（摘要失败的降级由另一条测试管）")
    }

    /// 摘要文本**绝不进用户可见的事件流**（brief §1 的那句话）。
    /// 用户可见的出口只有两处，这里两处都查：① `compact_before_turn` 交回、由 `run_turn`
    /// emit 的那一条事件；② 重放投影 `events_from_messages`（前端重开会话走的那条路）。
    /// 摘要调用压根拿不到 sink —— 它只在模型的返回流里存在过一次。
    #[tokio::test]
    async fn the_summary_call_never_leaks_into_the_event_stream() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let all = long_history();
        seed(&f.db, &sid, &all);
        let before = f.db.messages(&sid).unwrap();
        let model = FakeModel::summarizing(&["早期要点：用户叫林，", "卡号尾 4021"]);
        let (_, event) = compact_once(
            &f,
            &model,
            &sid,
            before,
            compacting_cfg(),
            &CancellationToken::new(),
        )
        .await;

        // 压了就要说，而且只说这一件事（两个数：dropped / summary_chars）。
        let event = event.expect("越线的历史必须交回一条 ContextCompacted");
        assert!(matches!(event, AgentEvent::ContextCompacted { .. }));
        let wire = serde_json::to_string(&event).unwrap();
        assert!(
            !wire.contains("早期要点"),
            "摘要文本混进了 emit 的事件里：{wire}"
        );

        // 落库那条是 `Role::System` ⇒ 重放里压根没有它（R69 那一臂；判据 (b) 打的就是这里）。
        let replay =
            events_from_messages(&f.db.messages(&sid).unwrap(), &|n| f.registry.risk_of(n));
        let visible = serde_json::to_string(&replay).unwrap();
        assert!(
            !visible.contains("早期要点"),
            "摘要进了重放事件流 ⇒ 用户会看到一句莫名其妙的小结"
        );
        assert!(
            !replay
                .iter()
                .any(|e| matches!(e, AgentEvent::ContextCompacted { .. })),
            "压缩事件是直播时发的一条通知，不是历史（库里没有这一列，重放也不该编造它）"
        );
        assert_eq!(model.calls().len(), 1, "一次压缩恰好一次摘要调用");
    }

    /// §9「存在一条覆盖被丢弃区间的摘要消息」的可测形式，加上「不重复」。
    /// 覆盖范围从**模型那一侧**取证（R57）：摘要调用的输入必须逐字等于被删掉的那一段 ——
    /// 只断言「库里有一条备注」是不够的，输入对不上就等于覆盖范围没兜住。
    #[tokio::test]
    async fn the_summary_row_is_written_once_and_covers_the_dropped_range() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let all = long_history();
        seed(&f.db, &sid, &all);
        let before = f.db.messages(&sid).unwrap();
        let model = FakeModel::summarizing(&["早期要点：", "用户叫林，卡号尾 4021"]);
        let (history, _) = compact_once(
            &f,
            &model,
            &sid,
            before,
            compacting_cfg(),
            &CancellationToken::new(),
        )
        .await;

        let after = f.db.messages(&sid).unwrap();
        let notes: Vec<&Message> = after.iter().filter(|m| m.role == Role::System).collect();
        assert_eq!(
            notes.len(),
            1,
            "每次压缩一条备注；两条备注意味着同一段历史被摘要了两遍"
        );
        assert_eq!(
            notes[0].text_of(),
            summary_note("早期要点：用户叫林，卡号尾 4021"),
            "备注 = 前缀 + 摘要正文（分片拼接后的原文）"
        );
        assert!(
            notes[0].text_of().starts_with(SUMMARY_PREFIX),
            "前缀必须有，而且只加一次：`build()` 把 System 行当 notes 原样带上"
        );
        assert!(
            after.len() < all.len(),
            "被压掉的行必须真的没了，否则下一轮还是全量读"
        );
        assert_eq!(after.len(), 21, "40 删 20 加 1 条备注");

        let req = model.calls().remove(0);
        assert_eq!(
            req.messages.len(),
            1 + 20,
            "摘要输入 = 被丢掉的那 20 条，一条不多一条不少"
        );
        assert_eq!(
            &req.messages[1..],
            &all[..20],
            "摘要输入必须逐字是被丢的那一段（最老 10 轮）"
        );
        let conv: Vec<&Message> = after.iter().filter(|m| m.role != Role::System).collect();
        assert_eq!(
            conv,
            all[20..].iter().collect::<Vec<_>>(),
            "留下的必须是刀口之后那 20 条，原样原序"
        );
        assert_eq!(history, after, "交回给调用方的历史必须就是重读回来的那份");
    }

    /// spec :365 的降级：摘要调用报错（429、断流）时**照压**，正文换成 `[早期对话已省略]`。
    /// 关键词是「照压」—— 不许有第二条「丢弃」路径，落库走的还是同一条 `compact_session`，
    /// 该删的行照删（否则下一轮照样超预算，而这次连摘要都没了）。
    #[tokio::test]
    async fn a_failing_summary_still_frees_space_and_says_so() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let all = long_history();
        seed(&f.db, &sid, &all);
        let before = f.db.messages(&sid).unwrap();
        // 报错之前先到了半句：那半句**不是**摘要，也不许落进备注。
        let model = FakeModel::failing(&["早期要点：用户叫"]);
        let (history, event) = compact_once(
            &f,
            &model,
            &sid,
            before,
            compacting_cfg(),
            &CancellationToken::new(),
        )
        .await;

        let after = f.db.messages(&sid).unwrap();
        let notes: Vec<&Message> = after.iter().filter(|m| m.role == Role::System).collect();
        assert_eq!(notes.len(), 1, "降级也只落一条备注");
        assert_eq!(
            notes[0].text_of(),
            summary_note(SUMMARY_OMITTED),
            "降级只换正文，不换事务"
        );
        assert!(
            !notes[0].text_of().contains("早期要点"),
            "半截摘要被当成了摘要落库：{}",
            notes[0].text_of()
        );
        let conv: Vec<&Message> = after.iter().filter(|m| m.role != Role::System).collect();
        assert_eq!(
            conv,
            all[20..].iter().collect::<Vec<_>>(),
            "摘要失败也不能把该删的留着撑爆下一轮"
        );
        assert_eq!(after.len(), 21);

        // 「压了」这件事照样要说，而且说的就是删掉的行数与备注的字数。
        match event.expect("降级路径同样必须 emit") {
            AgentEvent::ContextCompacted {
                dropped,
                summary_chars,
            } => {
                assert_eq!(dropped, 20);
                assert_eq!(
                    summary_chars,
                    summary_note(SUMMARY_OMITTED).chars().count(),
                    "字数是 chars().count() 不是 len()：前端文案「摘要 {{n}} 字」，\
                     用字节数中文会显示成 3 倍"
                );
            }
            other => panic!("降级也要说清楚压了多少，拿到的是 {other:?}"),
        }
        assert_eq!(history, after);
    }

    /// 取消发生在「模型还在写摘要」时：事务压根还没开始，所以库必须**一字未改**（原子性）。
    /// 半截摘要同样不许当成成功（那条合同在 `summarize.rs` 的
    /// `cancelling_a_summary_returns_an_error_and_keeps_nothing_back`）。
    #[tokio::test]
    async fn cancelling_mid_compaction_leaves_a_consistent_store() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let all = long_history();
        seed(&f.db, &sid, &all);
        let before = f.db.messages(&sid).unwrap();
        let model = FakeModel::stalling(&["早期要点：用户叫林，"]);
        let cancel = CancellationToken::new();
        let ready = model.stalled.clone();
        let builder = ContextBuilder::new(compacting_cfg());
        // 同步点而非 sleep：`ready` 置位时分片已经抵达消费者，摘要此刻确实还挂在流上。
        let (got, _) = tokio::join!(
            compact_before_turn(&f.db, &sid, &*model, &builder, before.clone(), &cancel),
            async {
                while !ready.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                cancel.cancel();
            }
        );
        let (history, event) = got.expect("取消不是失败：压缩段必须正常返回");
        assert!(event.is_none(), "没压成就别说压了：{event:?}");
        assert_eq!(
            history, before,
            "交回的历史必须是原样那份，否则调用方的 `prior` 会跟着错"
        );
        assert_eq!(
            f.db.messages(&sid).unwrap(),
            before,
            "取消不许留下半条摘要、也不许留下已经删掉的行"
        );
        assert_eq!(
            model.calls().len(),
            1,
            "夹具前提：摘要调用确实发起过并被取消"
        );
    }

    /// 没越线 ⇒ 一次模型调用都不发起、一个字节都不写。
    /// 这条看着像废话，但它守着的是「压缩段的 IO 全部在 `if let Some(plan)` 里面」：
    /// 把 `collect_summary` 挪到判定之外，用户每次发消息都要白烧一次摘要调用（花钱、且慢）。
    #[tokio::test]
    async fn an_under_threshold_history_costs_no_model_call_and_writes_nothing() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let short = vec![
            Message::user(vec![ContentBlock::Text { text: "一".into() }]),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "二".into() }],
            },
        ];
        seed(&f.db, &sid, &short);
        let model = FakeModel::summarizing(&["不该被调用的摘要"]);
        let (history, event) = compact_once(
            &f,
            &model,
            &sid,
            short.clone(),
            ContextConfig::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(event.is_none(), "没压就不许 emit：{event:?}");
        assert_eq!(history, short, "不压时交回的就是进来的那份");
        assert_eq!(
            model.calls().len(),
            0,
            "没越线一次都不该调模型：摘要调用要花钱"
        );
        assert_eq!(
            f.db.messages(&sid).unwrap(),
            short,
            "不压时库里一个字都不动"
        );
    }

    /// Step 6.3 的判据，也是本组**唯一**走 `run_turn` 的一条。
    /// 为什么只能走它：`prior` 这一行下标住在 `run_turn` 里（原 :304 / :323），
    /// brief §4 点名的失败模式（`prior` 停在压缩前的 40，而压缩后的历史只有 21 行
    /// ⇒ `session.history()[prior..]` 当场越界）发生在装配层的 move/重读**顺序**上，
    /// 抽出来的 `compact_before_turn` 里压根没有这个下标 —— 判据 (a)「把 prior 放回压缩前」
    /// 要红的就是这条测试。
    #[tokio::test]
    async fn prior_is_recounted_after_compaction() {
        let f = Fixture::new();
        let sid = f.db.create_session(DEFAULT_TITLE).unwrap();
        let all = long_history();
        seed(&f.db, &sid, &all);
        let model = FakeModel::summarizing_and_replying(&["早期要点：用户叫林"], &["本轮的回复"]);
        let rec = Arc::new(Recorded::default());
        let sink = Arc::new(TauriEmitter::new(rec.clone(), sid.clone()));
        let reason = run_turn(
            TurnDeps {
                db: f.db.clone(),
                registry: f.registry.clone(),
                gate: f.gate.clone(),
                model: model.clone(),
                sink: sink.clone(),
                context: compacting_cfg(),
                temperature: None,
                roots: Vec::new(),
                cancel: f.cancel.clone(),
            },
            &sid,
            vec![ContentBlock::Text {
                text: "本轮的提问".into(),
            }],
        )
        .await
        .expect("压缩 + 一轮发送必须整体成功");
        sink.flush();
        assert_eq!(reason, StopReason::EndTurn);

        let after = f.db.messages(&sid).unwrap();
        // 压缩后 21 行 + 本轮 2 行 = 23。`prior` 停在压缩前的 40 时，
        // `session.history()[40..]` 在 23 行上直接 panic（测试表现为失败），
        // 而「少附」的写法会把本轮那两行整轮丢掉 —— 两种退化都过不了下面三条。
        assert_eq!(
            after.len(),
            21 + 2,
            "总行数 = 压缩后行数 + 本轮新增两行；实际 {}",
            after.len()
        );
        assert_eq!(
            after[after.len() - 2].text_of(),
            "本轮的提问",
            "倒数第二行必须是本轮的用户行"
        );
        assert_eq!(
            after[after.len() - 1].text_of(),
            "本轮的回复",
            "最后一行必须是本轮的助手行"
        );
        assert!(
            after[..21]
                .iter()
                .all(|m| m.role != Role::User || m.blocks.len() == 1),
            "留下那 21 行的形状不许被本轮污染"
        );

        // 事件顺序（Step 6.5 / brief §4）：`context_compacted` 在写新行之前发，
        // 也就是排在本轮的回答之前；且摘要文本不在任何一帧里。
        let tags: Vec<&str> = rec.frames().iter().map(event_tag).collect();
        assert_eq!(
            tags.first().copied(),
            Some("context_compacted"),
            "压缩通知必须排在整轮回答之前，实际顺序 {tags:?}"
        );
        let wire = serde_json::to_string(&rec.frames()).unwrap();
        assert!(
            !wire.contains("早期要点"),
            "摘要文本进了用户可见的事件流：{wire}"
        );
        assert_eq!(model.calls().len(), 2, "一次摘要 + 一次回答");
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
        // F6-N4：原来写的是 `list.contains("normalize_image,")` —— 它硬依赖尾逗号，
        // 登记项哪天被挪成末项（末项无逗号）就**假红**。按分隔符拆开、逐项 trim 再比，
        // 对空白与尾逗号都不敏感；少登记仍然是红（拆出来的集合里根本没有这一项）。
        let registered: Vec<&str> = list
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        assert!(
            registered.contains(&"normalize_image"),
            "命令写了但没登记，前端调用只会拿到 not found：{registered:?}"
        );
    }

    // ---- resolve_api_key：本机模型免 key 的唯一闸门 ----
    //
    // 这三条断言是一组「放宽不许越界」的锁：第一条是新行为，后两条是既有行为，
    // 谁哪天把分支写成 `if base_url.contains("localhost")` 之类，后两条之一必红。

    #[test]
    fn a_loopback_endpoint_needs_no_stored_key() {
        // ollama / LM Studio 的默认形态。空串而不是 None 是刻意的：provider 侧
        // 只认「有 key 就带 header」，空串正好等于不带。
        assert_eq!(
            resolve_api_key(None, "http://127.0.0.1:11434/v1"),
            Ok(String::new())
        );
        assert_eq!(
            resolve_api_key(None, "http://localhost:11434/v1"),
            Ok(String::new())
        );
    }

    #[test]
    fn a_remote_endpoint_still_rejects_a_missing_key() {
        for base_url in [
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "https://api.openai.com/v1",
            // 前缀骗人：字符串看像 127.，实际是 DNS 名。
            "http://127.0.0.1.evil.test/v1",
            // 0.0.0.0 语义是「所有网卡」，不算本机。
            "http://0.0.0.0:11434/v1",
        ] {
            let err = resolve_api_key(None, base_url)
                .expect_err("非回环端点没有 key 必须停在这里，不能带着空 header 发出去");
            assert_eq!(
                err, "尚未配置 API key，请在设置中填写",
                "报文是前端直接展示的文案，不许漂"
            );
        }
    }

    #[test]
    fn a_stored_key_passes_through_byte_identical_on_both_paths() {
        for base_url in ["http://127.0.0.1:11434/v1", "https://remote.test/v1"] {
            assert_eq!(
                resolve_api_key(Some("sk-secret".into()), base_url),
                Ok("sk-secret".into())
            );
            // 抽函数之前 `Some("")` 就是能过的。这里维持原样：本轮只做「None 且回环」
            // 这一件事，顺手收紧空串会让非回环路径的行为不再是「逐字不变」。
            assert_eq!(
                resolve_api_key(Some(String::new()), base_url),
                Ok(String::new())
            );
        }
    }

    /// T27 的**接线**判据（评审 Important-1 → R287）。为什么只能是这样：
    /// `app.manage(AppState{…})` 在 Tauri 的 setup 闭包里，单测既造不出 `App` 也进不去那个闭包，
    /// 于是 `build_production_registry()` 自己绿了 405 条，**装配那行退回空表却一条都抓不到**
    /// （实现者亲跑的 M4 就是这个形状：405 passed / 0 failed）。
    /// 本项目对「宏/闭包造不出来的接线」已有同一路判例 —— `the_normalization_command_is_registered_in_the_handler_list`
    /// （`:2435`，`generate_handler!` 是宏）与前端 `tokens.test.ts`（解析 styles.css），
    /// 所以这里读自己的源码，不新开机制，只沿用既有那条线。
    /// 取法：**逐行**取 `registry:` 那一行并按行剥掉注释与尾逗号。为什么不按 `,` 拆整个字面量 ——
    /// 本文件是 CRLF，按 `,` 拆会把行尾注释和下一行粘成一段（M4 第一次跑就是这样露出来的），
    /// 而 `rustfmt` 对这个 7 字段字面量恒按「一字段一行」排；不硬依赖尾逗号则是 F6-N4 的教训。
    #[test]
    fn the_assembly_wires_the_production_registry() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ipc.rs"));
        let manage = src
            .split("app.manage(AppState {")
            .nth(1)
            .and_then(|rest| rest.split("});").next())
            .expect("找不到 app.manage(AppState { … }) 那段装配");
        let wired: Vec<&str> = manage
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("registry:"))
            .map(|line| line.split("//").next().unwrap_or("").trim())
            .map(|line| line.trim_end_matches(',').trim())
            .collect();
        assert_eq!(
            wired.len(),
            1,
            "装配段里 `registry:` 那一行必须**恰好一条**（0 条=换了写法或没了，2 条=两条装配路径）；\
             实得 {wired:?}"
        );
        assert_eq!(
            wired[0], "registry: build_production_registry()",
            "装配点没接上生产工具表：模型永远只有 0 把工具，而其余 405 条单测全绿 \
             （`production_registry_ships_fetch_url_and_read_file` 量的是那个函数自己，\
             不是这行接线 —— 实现者亲跑的 M4 就是这个形状）；实得 {:?}",
            wired[0]
        );
        // 第二半：`#[cfg(test)]` 之前**只许**有一处 `Arc::new(ToolRegistry::new())`，而且它必须在
        // `build_production_registry` 的函数体里 —— 出现第二处就等于有了第二条装配路径，
        // 而 R101（一个目录只许一个 `SettingsStore`）说的正是这种重复会各自漂移。
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("找不到 #[cfg(test)] 边界");
        let empty_constructions: Vec<&str> = production
            .lines()
            .filter(|line| line.contains("Arc::new(ToolRegistry::new())"))
            .map(str::trim)
            .collect();
        assert_eq!(
            empty_constructions.len(),
            1,
            "生产侧只许在 `build_production_registry` 体内空构造注册表一次；实得 {empty_constructions:?}"
        );
        assert!(
            empty_constructions[0].starts_with("let registry ="),
            "那唯一一处空构造必须是 `build_production_registry` 的第一行，不是别处的第二条装配路径；\
             实得 {:?}",
            empty_constructions[0]
        );
    }
}
