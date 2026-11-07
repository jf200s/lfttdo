// 一轮对话的编排循环（spec 4.6）：`context` 组装 → `model` 流式 → 权限门 → 工具执行
// → 结果回填，直到模型自己收尾、用户打断、模型报错，或轮数触顶。
//
// 本文件不得 `use tauri`（Plan A 约束，见 `agent/mod.rs`）：整条循环靠脚本化模型 +
// 记录型 sink 就能端到端跑完，文件末尾的测试就是它唯一的运行方式验证。
// Plan A 的生产工具表是**空的**（plan:1854），所以工具路径的正确性完全由这些测试撑着
// ——不要以「生产里没有工具」为理由简化循环本身。
use super::context::{ContextBuilder, TOOL_POLICY};
use super::events::{AgentEvent, Decision, ErrorWhere, EventSink, StopReason};
use super::types::*;
use crate::model::assistant_acc::AssistantAcc;
use crate::model::provider::{ChatRequest, ModelDelta, ModelProvider, ToolDef};
use crate::permission::gate::PermissionGate;
use crate::tools::edit_file;
use crate::tools::experiment;
use crate::tools::glob;
use crate::tools::grep;
use crate::tools::todo;
use crate::tools::undo;
use crate::tools::workflow;
use crate::tools::read_file;
use crate::tools::registry::{ToolCtx, ToolRegistry};
use crate::tools::use_skill;
use crate::tools::write_file;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// 打断 / 报错时补进 history 的那条工具结果的内容（R56）。
/// 两个出口共用一个作用（「这个调用没执行」），但归因写清楚，模型才不至于把
/// 「被打断」当成「工具坏了」再试一遍。
const INTERRUPTED_NOTE: &str = "本轮被打断，工具未执行";
const MODEL_FAILURE_NOTE: &str = "本轮在模型流中断处失败，工具未执行";
/// 上下文溢出重试（G05）时，给已流出的半截回复补的那条说明。
const OVERFLOW_RETRY_NOTE: &str = "上一段响应因上下文超限中断，将收缩上下文后重新回答";
/// 触顶总结轮（G06）追加在 system 尾的指令：只进那一次请求，不落库。
const MAX_STEPS_DIRECTIVE: &str = "## 收尾\n\
 本轮工作步数已到上限，不能再调用任何工具。请基于**已经完成**的工作写一段简短总结：\
 做了什么、结果在哪（文件/路径）、还剩什么没做。不要尝试调用工具。";
/// 触顶后模型仍凭空喊出的工具调用，统一回这条错误结果（不执行）。
const MAX_TURNS_NOTE: &str = "已到本轮步数上限，这个调用没有执行";
/// doom-loop 防护（G07）触发时的错误结果文案：打断自我循环，但把出路写清楚。
const DOOM_LOOP_NOTE: &str = "这已经是连续第 3 次发起完全相同的调用（工具与入参一字未改），本次没有执行。\
 上一轮的工具结果里通常已经有你需要的信息；请先读它。确需重试请换一种做法（改入参、换工具），\
 或先用文字向用户说明你在等什么。";
/// 工具**已经开始执行**之后被取消：不能断言它没执行完（副作用可能已经发生），
/// 只能说这份结果没有被采用。
const TOOL_CANCELLED_NOTE: &str = "本轮已中断，工具结果未采用";
/// G15：plan 模式拦下 Write/Exec 调用时的错误结果文案。三件事都在：这是什么模式、
/// 为什么被拒、出路是什么（完成调研给方案 / 退出 plan 模式再改）。
const PLAN_MODE_NOTE: &str = "Plan 模式：当前为只读研究，不能执行写入/命令类操作。请先完成调研并给出方案；退出 plan 模式后再执行修改。";

/// G17：AGENTS.md 注入段落的标题（拼接格式见 `AgentSession::agents_md_section`）。
const AGENTS_MD_HEADING: &str = "## 项目规则（AGENTS.md）";
/// G17：单个 AGENTS.md 的字节上限 —— 防一个巨型文件顶爆预算（截断在 UTF-8 字符
/// 边界上，见 `floor_char_boundary`）。
const AGENTS_MD_MAX_BYTES: usize = 8 * 1024;
/// G17：截断标记 —— 模型与测试都靠它知道「内容不完整」。
const AGENTS_MD_TRUNCATED_NOTE: &str = "\n\n（AGENTS.md 超过 8 KiB，已截断）";

/// 这条模型错误是不是「上下文超长」。判据只看错误文本：错误**分类**住 provider 层
/// （`ModelError` 四个变体按「用户该去查什么」划分，不按内容），而溢出是端点自己的
/// 4xx 文案 —— OpenAI 系 "maximum context length"、Ollama 系 "context window"，
/// 同一件事各家措辞不同，大小写不敏感地认这几个词；中文关键词兜底本地端点的报错。
fn is_context_overflow(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("context length")
        || m.contains("context window")
        || m.contains("context overflow")
        || message.contains("上下文长度")
        || message.contains("上下文超")
}

/// 循环的护栏。`max_turns` 是防模型自我循环的保险丝（spec 4.6）：日常对话 1 轮收尾，
/// 带工具的多为 2–5 轮，25 是上限而不是目标；触顶时发 `Done{MaxTurns}` 让 UI 明确提示，
/// 而不是静默停止。
///
/// G15：`plan_mode` 把会话变成 OpenCode 的「只读研究」agent —— 模型只能看和读
/// （Read 级工具照常），写入/命令类调用在工具循环里直接拒绝：不执行、不走权限门，
/// 错误结果里写明退出方式。生产值来自 `Settings.plan_mode`（`ipc.rs::run_turn` 每轮
/// 现取：改设置下一条消息生效）；不给就是 false（普通会话，行为一个字不变）。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub max_turns: u32,
    pub plan_mode: bool,
    /// G18：诊断命令（`write_file`/`edit_file` 成功落盘后每轮跑一次，输出作为
    /// 【自动诊断】合成 user 轮反馈给模型）。生产值来自 `Settings.diagnostics_command`
    /// （`ipc.rs::run_turn` 每轮现取：改设置下一条消息生效）；`None`/空串 = 关闭
    /// （历史组装与没有本功能时逐字节一致）。
    pub diagnostics_command: Option<String>,
    /// G19：agent 工具白名单（激活 agent 档案后生效）。`Some` 时双层生效：
    /// 声明表只留名单内的工具（`filtered_tool_defs` 再滤一层），白名单外的调用在
    /// 循环里被拒（不执行、不走权限门）。`None` = 不限制，一切照旧。
    pub allowed_tools: Option<Vec<String>>,
    /// G19：激活档案的名字（拒绝理由里点名「Agent「X」」用）。与 allowed_tools
    /// 同源同设（装配点一起给）；None 时拒绝理由退化为「当前 Agent」。
    pub active_agent_name: Option<String>,
    /// G22：task 工具可点名的 agent 档案表（`task(agent:"explore")` 时在表里查找）。
    /// 子代理的身份由 task 调用**显式点名**，父会话自己的 allowed_tools / 人设**不**
    /// 自动传给子会话 —— 「审计员主会话」≠「审计员子代理」：主会话的白名单管的是
    /// 主会话这一轮，子代理是谁、能干什么，是派活那一刻才定的事。Default 空表 =
    /// 无档案可点名，task 的行为与从前逐字节一致。
    pub agents: Vec<AgentProfile>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_turns: 25,
            plan_mode: false,
            diagnostics_command: None,
            allowed_tools: None,
            active_agent_name: None,
            agents: Vec::new(),
        }
    }
}

/// G22：task 工具可点名的子代理档案（OpenCode 的 explore/general 模式）。住 agent 层的
/// **中性结构**，不放 settings.rs —— agent 层不许依赖 settings（Plan A 依赖方向），
/// `AgentProfileJson → AgentProfile` 的映射归 ipc 装配层（`ipc.rs::agent_profiles`）。
/// `prompt` 非空 ⇒ 子会话系统提示词整段替换（「只读调研员」这类人设）；
/// `tools` 有表 ⇒ 子会话的工具白名单；两个 `None` 各自表示「不替换 / 不限制」。
/// description（`AgentProfileJson` 里有）是给人看的档案说明，模型用不上，故不在此结构中。
#[derive(Clone, Debug, PartialEq)]
pub struct AgentProfile {
    pub name: String,
    pub prompt: Option<String>,
    pub tools: Option<Vec<String>>,
}

/// 会话的全部外部依赖，由装配点（Task 13）注入。每一项都是 `Arc`（`context` 除外），
/// 所以「每轮新建一个会话」是廉价的 —— `cancel_handle` 的合同正建立在这上面。
pub struct SessionDeps {
    pub model: Arc<dyn ModelProvider>,
    pub registry: Arc<ToolRegistry>,
    pub gate: Arc<PermissionGate>,
    pub sink: Arc<dyn EventSink>,
    pub context: ContextBuilder,
    /// 端点级别的采样温度，来自 `Settings::to_model_config`（装配点给，见 `ipc.rs`）。
    /// `None` = 请求体里不带这个键，由端点自己定。**不在这里读 `Settings`**：
    /// 那会造出第二份配置真相，Settings 的读取归装配点（Plan A 约束）。
    pub temperature: Option<f32>,
}

/// 一轮对话的编排者。一次 `run` 只跑一轮（`&mut self`），历史留在结构体里，
/// 由 Task 11 落库、Task 13 经 `restore` 装回来。
pub struct AgentSession {
    deps: SessionDeps,
    history: Vec<Message>,
    cfg: SessionConfig,
    cancel: CancellationToken,
    http: reqwest::Client,
    roots: Vec<PathBuf>,
    output_dir: Option<PathBuf>,
    /// 技能包目录（`skills/<名字>/SKILL.md`）。每轮请求现扫，用户改完文件不用重启。
    skills_dir: Option<PathBuf>,
    /// 用户在面板里停用的技能文件夹（按文件夹名，稳定键）。
    skills_disabled: Vec<String>,
    /// 本轮已经自动加载过正文的技能：`(展示名, [小写技能名, 小写文件夹名])`。
    /// 只服务一件事 —— 模型再喊一次 `use_skill` 时不重读，回一句提示。
    turn_skills: Vec<(String, Vec<String>)>,
    /// 本轮**用户原话**（`run()` 入口那一句）。不能用「最近一条 user 消息」代替：
    /// 工具返回图片时 `image_followup` 会再补一条 user 轮，那样会让技能在第二次请求里
    /// 突然从上下文消失。
    turn_user_text: String,
    /// 发出**当前这次请求**时 `build()` 报的粗估输入 token（切片 5 的校准分母）。
    last_est_input: u32,
    /// 生产装配把它置真：`Done` 由调用方在**本轮落库成功之后**再发，
    /// 这样「界面已显示完成、库里却没有这一轮」的窗口不存在。测试默认假（会话自己发）。
    defer_done: bool,
}

/// 从工具输出里摘出图片块（R319）：`Text` 不要，`Image` 原样，`Multi` 递归摊平。
fn images_of(output: &ToolOutput) -> Vec<ContentBlock> {
    match output {
        ToolOutput::Image { data, mime } => vec![ContentBlock::Image {
            data: data.clone(),
            mime: mime.clone(),
        }],
        ToolOutput::Multi { parts } => parts.iter().flat_map(images_of).collect(),
        ToolOutput::Text { .. } => Vec::new(),
    }
}

/// 工具返回过图片时，给模型补一条 **user** 轮消息（R319）。没有图片 ⇒ `None`，
/// 纯文本工具的输出一个字节都不多加。
fn image_followup(images: &[ContentBlock]) -> Option<Message> {
    // 同一张图只送一次：模型一轮里重复调用同一把工具时，`Multi` 会把同一份 base64
    // 出现两次，而图片是按张计费的（context.rs 的 TOKEN 估算也是按张）。
    let mut unique: Vec<&ContentBlock> = Vec::new();
    for image in images {
        let ContentBlock::Image { data, mime } = image else {
            continue;
        };
        if unique.iter().any(|kept| match kept {
            ContentBlock::Image {
                data: kept_data,
                mime: kept_mime,
            } => kept_data == data && kept_mime == mime,
            _ => false,
        }) {
            continue;
        }
        unique.push(image);
    }
    if unique.is_empty() {
        return None;
    }
    let mut blocks = vec![ContentBlock::Text {
        text: TOOL_IMAGE_FOLLOWUP_NOTE.to_string(),
    }];
    blocks.extend(unique.into_iter().cloned());
    Some(Message::user(blocks))
}

/// G17：UTF-8 安全的截断点。`String::truncate` 在非字符边界会 panic，而 AGENTS.md 是
/// 用户可手改的文件 —— 8 KiB 这刀不许让它炸。从 `max` 往回退到最近的字符边界
/// （`is_char_boundary(0)` 恒真，循环必然终止）。
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// `request()` 与预算估算共用这一份，避免「算的是全量注册表、发的是过滤后的表」。
/// 隐藏只影响声明表、省一轮注定失败的调用，**不是**授权：模型凭空喊这两个名字
/// 照样会走到工具自己的拒绝路径。
fn filtered_tool_defs(
    registry: &ToolRegistry,
    roots: &[PathBuf],
    has_skills: bool,
    allowed_tools: Option<&[String]>,
) -> Vec<ToolDef> {
    let mut tools = registry.defs();
    if !glob::usable_with_roots(roots) {
        tools.retain(|d| d.name != glob::NAME);
    }
    if !grep::usable_with_roots(roots) {
        tools.retain(|d| d.name != grep::NAME);
    }
    if !read_file::usable_with_roots(roots) {
        tools.retain(|d| d.name != read_file::NAME);
    }
    if !write_file::usable_with_roots(roots) {
        tools.retain(|d| d.name != write_file::NAME);
    }
    if !edit_file::usable_with_roots(roots) {
        tools.retain(|d| d.name != edit_file::NAME);
    }
    if !todo::usable_with_roots(roots) {
        tools.retain(|d| d.name != todo::NAME);
    }
    if !undo::usable_with_roots(roots) {
        tools.retain(|d| d.name != undo::NAME);
    }
    // G24：workflow 的定义面在 workspace 的 workflows/ 下（write_root/roots[0]），
    // 没有工作目录时声明了也只会收到一句报错 —— 与 todo/undo 同一条隐藏判据。
    if !workflow::usable_with_roots(roots) {
        tools.retain(|d| d.name != workflow::NAME);
    }
    // G25：experiment 的命令与 git 都以 write_root/roots[0] 为 cwd，没有工作目录
    // 时三个 action 全是死路 —— 同一条隐藏判据。
    if !experiment::usable_with_roots(roots) {
        tools.retain(|d| d.name != experiment::NAME);
    }
    if !has_skills {
        tools.retain(|d| d.name != use_skill::NAME);
    }
    // G19：agent 工具白名单再滤一层 —— 只留名单内的名字。与上面的既有过滤**叠加**
    // （roots 空表隐藏 read_file 等照旧）：两层都是「这一轮到底给不给模型看」的
    // 判据，隐藏不是授权，白名单外的调用另有循环拦截兜底。
    if let Some(allowed) = allowed_tools {
        tools.retain(|d| allowed.iter().any(|a| a == &d.name));
    }
    tools
}

/// 装配层在压缩判定之前要用的版本：扫描一次技能目录，判据与 `request()` 一致
/// （只认启用中的技能）。这样预算里计的工具声明就是真正会发出去的那一批。
pub fn tool_defs_for_turn(
    registry: &ToolRegistry,
    roots: &[PathBuf],
    skills_dir: Option<&Path>,
    skills_disabled: &[String],
    allowed_tools: Option<&[String]>,
) -> Vec<ToolDef> {
    let has_skills = skills_dir
        .map(crate::skills::discover)
        .unwrap_or_default()
        .iter()
        .any(|e| !skills_disabled.iter().any(|f| f == &e.folder));
    filtered_tool_defs(registry, roots, has_skills, allowed_tools)
}

impl AgentSession {
    pub fn new(deps: SessionDeps) -> Self {
        Self {
            deps,
            history: Vec::new(),
            cfg: SessionConfig::default(),
            cancel: CancellationToken::new(),
            http: reqwest::Client::builder()
                .user_agent("lfttdo/0.1")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            roots: Vec::new(),
            output_dir: None,
            skills_dir: None,
            skills_disabled: Vec::new(),
            turn_skills: Vec::new(),
            turn_user_text: String::new(),
            last_est_input: 0,
            defer_done: false,
        }
    }

    /// 把 `Done` 的发送权交给调用方（生产路径：落库成功后再发）。
    /// 只影响 `finish()` 的那一条事件，循环的其余行为一个字不变。
    pub fn defer_done(&mut self) {
        self.defer_done = true;
    }

    /// 文件类工具的作用域根（Plan A 没有生产工具，所以默认是空表；
    /// `ToolCtx::resolve_within_roots` 拿到空表时是**全拒**，不是全放行）。
    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        self
    }

    /// G15：覆盖默认护栏。装配点（`ipc.rs::run_turn`）从 `Settings.plan_mode` 每轮现取
    /// 后经这里注入。与 `with_roots` 同一形态 —— builder，不进 `SessionDeps`：这是
    /// 「每轮现取的运行配置」，不是外部依赖（同 roots 的归属理由）。
    pub fn with_config(mut self, cfg: SessionConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn with_output_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.output_dir = dir;
        self
    }

    /// 技能目录。`None` 与「目录为空」等价：索引为空 ⇒ `use_skill` 不交给模型。
    pub fn with_skills_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.skills_dir = dir;
        self
    }

    /// 停用列表（文件夹名）。停用的技能既进不了索引，也读不出正文。
    pub fn with_skills_disabled(mut self, disabled: Vec<String>) -> Self {
        self.skills_disabled = disabled;
        self
    }

    /// 模型喊的 `use_skill` 若指向本轮已经加载过正文的技能，返回那句替代输出。
    /// 匹配键是技能名与文件夹名（都小写），与 `use_skill` 工具自己认的那一套一致。
    fn already_loaded_note(&self, input: &serde_json::Value) -> Option<String> {
        let want = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?
            .to_lowercase();
        self.turn_skills
            .iter()
            .find(|(_, keys)| keys.contains(&want))
            .map(|(name, _)| {
                format!("技能「{name}」的完整说明已经在本轮上下文里，直接按它执行，不必重复读取。")
            })
    }

    /// 这一轮之后模型应该看到的完整历史。事件流给 UI，这个给 Task 11 落库。
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// 装回历史（Task 13 每轮新建会话时靠它续上多轮对话）。
    pub fn restore(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// 停止用的令牌。**一次性**（R63）：`cancel()` 之后这个会话只会再产出一个
    /// `Interrupted` 收尾，`run()` 内部**不会**复位它 —— 复位会让前端手里那个
    /// handle 静默失效，比原竞态更糟。
    /// 合同：取消过的会话算用完，想继续聊就带着 `history()` 新建一个会话
    /// （`SessionDeps` 全是 `Arc`，Task 13 正是每轮新建）。
    pub fn cancel_handle(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// 跑完这一轮。返回值同时是 `Done{..}` 事件里的 `stop_reason`，两边不可能分叉
    /// （只有 `finish` 一处发 `Done`）。
    pub async fn run(&mut self, blocks: Vec<ContentBlock>) -> StopReason {
        // 取消发生在开始之前：这一轮压根没发生，连 user 消息都不该留下（R63 的第二条 run 同理）。
        if self.cancel.is_cancelled() {
            return self.finish(StopReason::Interrupted);
        }
        let user = Message::user(blocks);
        // 「本轮已加载」和「用户原话」都是**这一轮**的事：下一轮重新按那句话决定。
        self.turn_user_text = user.text_of();
        self.turn_skills.clear();
        self.history.push(user);
        // 工具上下文建一次就够：三个字段整轮不变，而 `CancellationToken` 的克隆共享
        // 同一个状态，所以中途按下停止照样被 `ToolRegistry::execute` 的守卫看见。
        let ctx = ToolCtx {
            write_root: self.output_dir.clone(),
            http: self.http.clone(),
            cancel: self.cancel.clone(),
            roots: self.roots.clone(),
            skills_dir: self.skills_dir.clone(),
            skills_disabled: self.skills_disabled.clone(),
            // G16：子代理运行器 —— 用同一套 model/registry/gate/温度/预算起独立子会话
            // （agent/task.rs::LoopRunner）。文件作用域随 TaskTool 调用时的 ctx 传下去。
            // G22：档案表从本会话的配置带下去 —— task 调用点名哪个档案（`task(agent:"explore")`），
            // 子会话就换成哪个身份；表本身不自动传给子会话（孙代理不支持，取舍见 LoopRunner）。
            subagents: Some(Arc::new(crate::agent::task::LoopRunner {
                model: self.deps.model.clone(),
                registry: self.deps.registry.clone(),
                gate: self.deps.gate.clone(),
                temperature: self.deps.temperature,
                context_cfg: self.deps.context.config(),
                agents: self.cfg.agents.clone(),
            })),
            // G20：事件出口原样交给工具（question 发多选题用）。与上面同一份 sink，
            // 事件线只有一条；question 不经权限门，门与出口在这里就已分流。
            events: Some(self.deps.sink.clone()),
        };

        // G05：一次 run() 只自动恢复一次溢出。第二次仍超限 = 预算收缩到下限也装不下，
        // 走既有失败出口把控制权交回用户（改设置/换端点），而不是无限收缩。
        let mut overflow_retried = false;
        // G07：doom-loop 计数器，跨轮存活（「连续」以整次 run() 计）；
        // 签名 = 工具名 + 入参的 JSON 序列化（serde_json 的 Map 是 BTreeMap，
        // 键序确定 ⇒ 签名可复现）。出现**任何不同**的调用即复位。
        let mut doom_sig: Option<String> = None;
        let mut doom_count: u32 = 0;
        for turn in 1..=self.cfg.max_turns {
            // 上一轮的工具跑完之后又被点了停止：成对状态已经落好，直接收尾。
            if self.cancel.is_cancelled() {
                return self.finish(StopReason::Interrupted);
            }
            self.deps.sink.emit(AgentEvent::TurnStart { turn });

            // `request()` 要 `&mut self`（记下校准分母）：请求先落地，流的消费在
            // `consume()` 里完成（借 model 的借用随返回值一起结束）。
            let req = self.request();
            let (acc, interrupted, model_error) = self.consume(req).await;

            let assistant = acc.finish();
            if let Some(message) = model_error {
                // G05：上下文超限先收缩重试一次，而不是整轮失败。半截回复已经流给 UI，
                // 按 R56 的同一逻辑提交进 history（带归因说明），重试的回答接在它后面。
                if is_context_overflow(&message) && !overflow_retried {
                    overflow_retried = true;
                    self.deps.context.halve_budget();
                    self.deps.sink.emit(AgentEvent::Error {
                        at: ErrorWhere::Loop,
                        message: format!("模型报上下文超限，已收缩上下文并重试：{message}"),
                        fatal: false,
                    });
                    self.commit_partial(assistant, OVERFLOW_RETRY_NOTE);
                    continue;
                }
                // R61：Plan A 里模型错误就结束本轮，所以 fatal 是 true。
                self.deps.sink.emit(AgentEvent::Error {
                    at: ErrorWhere::Model,
                    message,
                    fatal: true,
                });
                self.commit_partial(assistant, MODEL_FAILURE_NOTE);
                return self.finish(StopReason::Error);
            }
            if interrupted {
                self.commit_partial(assistant, INTERRUPTED_NOTE);
                return self.finish(StopReason::Interrupted);
            }
            if assistant.blocks.is_empty() {
                // R58：零块的 assistant 消息发不出去（`assistant_acc.rs:75-79` 明确要求
                // 调用方自己判），所以它既不进 history，也不许被下一轮当成输入。
                // 报一条非致命的 loop 错误收尾：静默的「模型没说话」和静默的统计归零
                // 一样，只有 UI 上那一行能被发现。
                self.deps.sink.emit(AgentEvent::Error {
                    at: ErrorWhere::Loop,
                    message: "模型返回空响应".into(),
                    fatal: false,
                });
                return self.finish(StopReason::EndTurn);
            }

            let calls = assistant.tool_calls();
            self.history.push(assistant);
            if calls.is_empty() {
                return self.finish(StopReason::EndTurn);
            }

            // 工具**顺序**执行，不并行（spec 4.6 的刻意取舍：并行难追踪「哪个写操作改了
            // 哪个文件」，也和分级确认的交互顺序冲突）。
            let mut tool_images: Vec<ContentBlock> = Vec::new();
            // G18：本轮是否有 write_file/edit_file **成功落盘** —— 有就等全部工具结果
            // 处理完之后跑一次诊断命令（命令往往秒级，别每个文件各跑一遍）。
            let mut wrote_file = false;
            for (id, name, input) in calls {
                // risk 只从注册表查（`ToolCallView` 的注释就是这条）：模型或前端递来的
                // 任何「我是 Read」都不作数 —— `ToolDef` 里压根没有 risk 字段。
                let risk = self.deps.registry.risk_of(&name);
                let view = ToolCallView {
                    id: id.clone(),
                    name: name.clone(),
                    risk,
                    input: input.clone(),
                };
                // 构造函数负责钉住「顶层 risk == call.risk」这个不变量（agent/events.rs:75）。
                self.deps.sink.emit(AgentEvent::tool_call(view.clone()));

                // G07：doom-loop 防护。完全相同的「工具名 + 入参」连续第 3 次：不再执行，
                // 回一条可读的错误结果打断自我循环。调用卡照发（UI 仍然看得见这次尝试）；
                // 不走权限门 —— 门是「该不该执行」的闸，这里已经裁定不执行。
                {
                    let sig = format!("{name}\u{0}{input}");
                    if doom_sig.as_deref() == Some(sig.as_str()) {
                        doom_count += 1;
                    } else {
                        doom_sig = Some(sig);
                        doom_count = 1;
                    }
                    if doom_count >= 3 {
                        let started = Instant::now();
                        let (preview, _) = ToolOutput::text(DOOM_LOOP_NOTE).event_view();
                        self.deps.sink.emit(AgentEvent::ToolResult {
                            call_id: id.clone(),
                            is_error: true,
                            ms: started.elapsed().as_millis() as u64,
                            preview,
                            images: vec![],
                        });
                        self.history
                            .push(Message::tool(&id, ToolOutput::text(DOOM_LOOP_NOTE), true));
                        continue;
                    }
                }

                // G15：plan 模式 = 只读研究 agent。Write/Exec 级调用在这里就被拒：
                // 不执行、不走权限门 —— 门是「该不该执行」的闸，plan 模式已经裁定
                // 不执行（与上面 doom 守卫同一归属：拦截发生在门之前）。
                // 调用卡照发（上面的 tool_call 事件）：UI 仍然看得见模型尝试了什么；
                // 模型也收到这条错误结果，可以接着做只读调研或改出方案。R57 配对：
                // ToolResult 事件与 history 各记一条，结尾不停在 assistant 上。
                if self.cfg.plan_mode && risk != Risk::Read {
                    let started = Instant::now();
                    let (preview, _) = ToolOutput::text(PLAN_MODE_NOTE).event_view();
                    self.deps.sink.emit(AgentEvent::ToolResult {
                        call_id: id.clone(),
                        is_error: true,
                        ms: started.elapsed().as_millis() as u64,
                        preview,
                        images: vec![],
                    });
                    self.history
                        .push(Message::tool(&id, ToolOutput::text(PLAN_MODE_NOTE), true));
                    continue;
                }

                // G19：agent 工具白名单的第二层（第一层是声明表过滤）。插在 plan 拦截
                // 之后、gate 之前 —— 与 plan 同一归属（会话级护栏已经裁定不执行，轮不到
                // 权限门），doom 守卫必须仍在其前：白名单内允许的调用照样要被计数，
                // 白名单外被拒的调用不该污染 doom 计数。调用卡照发（UI 可见这次尝试），
                // R57 配对不破。
                if let Some(allowed) = &self.cfg.allowed_tools
                    && !allowed.iter().any(|a| a == &name)
                {
                    let agent = self
                        .cfg
                        .active_agent_name
                        .as_deref()
                        .unwrap_or("当前 Agent");
                    let note = format!(
                        "Agent「{agent}」不允许使用工具 {name}（白名单: {}）",
                        allowed.join(", ")
                    );
                    let started = Instant::now();
                    let (preview, _) = ToolOutput::text(&note).event_view();
                    self.deps.sink.emit(AgentEvent::ToolResult {
                        call_id: id.clone(),
                        is_error: true,
                        ms: started.elapsed().as_millis() as u64,
                        preview,
                        images: vec![],
                    });
                    self.history.push(Message::tool(&id, ToolOutput::text(note), true));
                    continue;
                }

                // 本轮已经把正文加载进上下文的技能又被点名：不重读，只回一句提示。
                // 8B 实测会把同一份正文连读 2–3 遍，每遍都是白烧的输入 token；
                // 工具卡照常发出去，界面上仍然看得见这一次调用。
                let already = if name == use_skill::NAME {
                    self.already_loaded_note(&input)
                } else {
                    None
                };
                let started = Instant::now();
                let (output, is_error) = match already {
                    Some(note) => (ToolOutput::text(note), false),
                    None => {
                        // 取消臂住在 gate 内部（R43）：弹窗挂着的时候按停止，`select!` 打不断
                        // 一个已经在 await 的 `check`，只能由它自己收场。
                        let decision = self.deps.gate.check(&view, &ctx.cancel).await;
                        match decision {
                            Decision::Allow { .. } => {
                                // 取消必须能打断**正在执行**的工具，而不只是打断模型流：
                                // 一次抓取可能在一跳上停 20 秒，不 select 的话「停止」要等
                                // 工具自己回来才生效（验收线要求 1 秒内）。
                                // drop 掉执行中的 future 对只读工具是安全的（清理归各自的
                                // RAII）；这也是 `ToolCtx.cancel` 存在的意义。
                                tokio::select! {
                                    biased;
                                    _ = ctx.cancel.cancelled() => {
                                        (ToolOutput::text(TOOL_CANCELLED_NOTE), true)
                                    }
                                    result = self.deps.registry.execute(&name, input, &ctx) => {
                                        match result {
                                            // R64：工具失败只由 `ToolResult{is_error:true}` 承载，
                                            // 不再补一条 `Error{at:Tool}` —— 同一个事实报两遍，
                                            // 前端会长出两行（那条分支是 Plan B/C 的 MCP/循环错误用的）。
                                            Ok(output) => (output, false),
                                            Err(e) => (ToolOutput::text(e.to_string()), true),
                                        }
                                    }
                                }
                            }
                            // R59：内容就是拒绝理由本身。`ToolError::Failed` 的 Display 前缀是
                            // 「工具执行失败: 」，套上去模型读到的是双重包装（spec:263 说的是理由）。
                            //
                            // 归属必须看令牌，不能一律写「用户拒绝执行」：门里的 `已取消` /
                            // `等待授权时收到中断` 是**用户按了停止**，不是用户否决了这个诉求。写成
                            // 否决会让模型以为这条路被禁止而不再重试；「中断」才是它能接着做的事实。
                            // 竞态方向是安全的：用户应答完 deny 再按停止会被归到「本轮已中断」，两句
                            // 都仍为真，只影响落库措辞；反过来（把停止说成否决）才是说谎。
                            // headless 的 `没有可应答的界面` 仍会带上「用户拒绝执行」——Plan A 生产里
                            // 不可达（T13 装配必然 `with_sink`），等它真可达时再连同归属一起改。
                            Decision::Deny { reason } => {
                                let who = if ctx.cancel.is_cancelled() {
                                    "本轮已中断"
                                } else {
                                    "用户拒绝执行"
                                };
                                (ToolOutput::text(format!("{who}：{reason}")), true)
                            }
                        }
                    }
                };
                let (preview, images) = output.event_view();
                // Cache images off the async worker; the event itself must keep its order,
                // so only the disk part is moved to a blocking thread.
                let images = if let Some(dir) = &self.output_dir {
                    let call_id = id.clone();
                    let cache = dir.join(crate::image_cache::CACHE_SUBDIR);
                    tokio::task::spawn_blocking(move || {
                        let mut images = images;
                        crate::image_cache::enrich_images(&call_id, &mut images, &cache);
                        images
                    })
                    .await
                    .unwrap_or_default()
                } else {
                    images
                };
                self.deps.sink.emit(AgentEvent::ToolResult {
                    call_id: id.clone(),
                    is_error,
                    ms: started.elapsed().as_millis() as u64,
                    preview,
                    images,
                });
                tool_images.extend(images_of(&output));
                self.history.push(Message::tool(&id, output, is_error));
                // G18：诊断反馈的触发记账。is_error 的写不触发 —— 落盘都没发生，
                // 诊断的是旧文件，反馈只会误导；被 plan 模式 / doom 守卫拦下的调用
                // 走 `continue`，根本到不了这里。
                if matches!(name.as_str(), "write_file" | "edit_file") && !is_error {
                    wrote_file = true;
                }
            }
            // R319：工具回的图片对**模型**不可见（`role:"tool"` 只能带文本，
            // openai_compat.rs:387-392）。要让 vision 模型真的看到，只能在 tool 消息之后
            // 补一条 **user** 轮的 Image 块。放在同一轮末尾、所有 tool_result 之后：
            // R57 的配对不变量（N 个调用紧跟 N 条结果）因此不受影响。
            if let Some(followup) = image_followup(&tool_images) {
                self.history.push(followup);
            }
            // G18：诊断反馈 —— OpenCode 把「编辑后的诊断（编译/LSP 错误）」送回模型的
            // 离线等价物：配置了命令且本轮有写工具成功落盘时，**全部工具结果处理完之后
            // 跑一次**，输出非空就补一条合成 user 轮。与 image_followup 同一先例：
            // tool 消息之后给模型的补充上下文，不进 UI 事件流。命令失败/超时/被取消/
            // 输出为空都不追加（不编造）；没有配置命令时这里一个字节都不动 —— 历史与
            // 没有本功能时逐字节一致（有测试钉住）。
            if wrote_file
                && let Some(command) = self.cfg.diagnostics_command.as_deref()
                && !command.trim().is_empty()
                && let Some(diag) = self.run_diagnostics(command, &ctx).await
            {
                self.history.push(Message::user(vec![ContentBlock::Text {
                    text: format!("【自动诊断】{diag}"),
                }]));
            }
        }

        // ---- G06：步数触顶的强制收尾 ----
        // 直接终止会让工作停在半截（用户只看到一个 MaxTurns）。OpenCode 的做法是
        // **收回工具声明**再要一次总结：`summary_request()` 的 tools 为空表，模型在
        // 物理上开不了新调用。若它仍凭空喊出工具名，逐个回错误结果（不执行）并按
        // MaxTurns 收尾 —— 调用与结果保持 R57 的配对，下一轮 history 不会破。
        self.deps.sink.emit(AgentEvent::TurnStart {
            turn: self.cfg.max_turns + 1,
        });
        let req = self.summary_request();
        let (acc, interrupted, model_error) = self.consume(req).await;
        let assistant = acc.finish();
        if interrupted {
            self.commit_partial(assistant, INTERRUPTED_NOTE);
            return self.finish(StopReason::Interrupted);
        }
        if let Some(message) = model_error {
            self.deps.sink.emit(AgentEvent::Error {
                at: ErrorWhere::Model,
                message,
                fatal: true,
            });
            self.commit_partial(assistant, MODEL_FAILURE_NOTE);
            return self.finish(StopReason::Error);
        }
        if assistant.blocks.is_empty() {
            // 与主循环同一形状（R58）：空响应不进 history，报非致命 loop 错误收尾。
            self.deps.sink.emit(AgentEvent::Error {
                at: ErrorWhere::Loop,
                message: "模型返回空响应".into(),
                fatal: false,
            });
            return self.finish(StopReason::MaxTurns);
        }
        let calls = assistant.tool_calls();
        self.history.push(assistant);
        if calls.is_empty() {
            // 模型交出文本总结：按正常收尾处理（工作没白做，界面上是一段可读的交付）。
            return self.finish(StopReason::EndTurn);
        }
        for (id, _, _) in calls {
            self.deps.sink.emit(AgentEvent::ToolResult {
                call_id: id.clone(),
                is_error: true,
                ms: 0,
                preview: MAX_TURNS_NOTE.to_string(),
                images: vec![],
            });
            self.history.push(Message::tool(&id, ToolOutput::text(MAX_TURNS_NOTE), true));
        }
        self.finish(StopReason::MaxTurns)
    }

    /// 消费一次模型流（spec 4.6 的「模型说话」半步）：流尽 / 取消 / 出错三出口。
    /// `req` 由调用方先建好（`request()`/`summary_request()` 都要 `&mut self`），
    /// 这里只负责把 delta 灌进累加器。三出口返回后借 `self.deps.model` 的借用即告结束。
    ///
    /// R55：`tokio::select!` + `biased`（取消臂先轮询）；R60：Usage 里的 token 在进
    /// `AssistantAcc` 之前取走做校准，分母是**发出这次请求时**的 est。
    /// `biased` 的代价是一条已知且良性的竞态：流干净收尾的那次 poll 里若取消同时就绪，
    /// 取消臂先赢 —— 内容不丢（`commit_partial` 照原样提交），只是 `stop_reason`
    /// 记成用户做的那件事。
    async fn consume(
        &mut self,
        req: ChatRequest,
    ) -> (AssistantAcc, bool, Option<String>) {
        let mut acc = AssistantAcc::default();
        let mut deltas = self.deps.model.stream(req);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return (acc, true, None),
                item = deltas.next() => match item {
                    None => return (acc, false, None),
                    Some(Ok(ModelDelta::Usage { input_tokens, output_tokens })) => {
                        // 两道零闸留在 session 侧：`set_calibration` 只挡非有限值，
                        // 而 0.0 是**有限**的、会被 clamp 成 0.2 —— 一次「端点报了 0 token」
                        // 不该把估算压到地板（`a_zero_or_absurd_usage_...` 钉这条）。
                        if input_tokens > 0 && self.last_est_input > 0 {
                            self.deps.context.set_calibration(
                                input_tokens as f64 / self.last_est_input as f64,
                            );
                        }
                        self.deps.sink.emit(AgentEvent::Usage { input_tokens, output_tokens });
                    }
                    Some(Ok(d)) => {
                        if let Some(text) = acc.apply(d) {
                            self.deps.sink.emit(AgentEvent::TextDelta { text });
                        }
                    }
                    Some(Err(e)) => return (acc, false, Some(e.to_string())),
                }
            }
        }
    }

    /// 触顶总结请求：与 [`Self::request`] 同一条组装路径（预算、校准分母、历史窗口
    /// 全部照算），只有两处不同 —— 工具声明清空、system 尾换成 [`MAX_STEPS_DIRECTIVE`]。
    /// 技能索引与自动加载**不带**：没有工具可用，技能正文也是噪声。
    fn summary_request(&mut self) -> ChatRequest {
        let built = self
            .deps
            .context
            .build_with_extra(&self.history, MAX_STEPS_DIRECTIVE);
        self.last_est_input = built.est_input_tokens;
        ChatRequest {
            messages: built.messages,
            tools: Vec::new(),
            temperature: self.deps.temperature,
        }
    }

    /// 每个出口都只经这里：`Done` 恰好一条，且带的就是返回值本身。
    /// `defer_done` 模式下不发 —— 调用方在落库成功后用同一个 `r` 补发。
    fn finish(&self, r: StopReason) -> StopReason {
        if !self.defer_done {
            self.deps.sink.emit(AgentEvent::Done { stop_reason: r });
        }
        r
    }

    /// R56：把**已经流给 UI 的部分**提交进 history。
    ///
    /// 事件流是 UI 的真相源（Task 14 合帧，Task 11 重放），`history` 是下一轮模型与
    /// 落库的真相源，两边必须说同一件事：半句话在 UI 上出现过，就必须也在库里 ——
    /// 否则「停止 → 关闭会话 → 重开」那段文字就凭空消失。
    ///
    /// 另一半同样必要：给这条消息里每个 `ToolCall` 补一条未执行的结果。只提交
    /// `assistant(ToolCall)` 会让 history 结束在调用上，下一轮模型看到的请求里那条
    /// `role:"tool"` 就没有对应的调用 —— 正是 Task 8 在窗口边界修掉的 mid-session 400
    /// 从这个入口复发。（spec:242 的草图没有这一步，R56 以裁定身份补上。）
    fn commit_partial(&mut self, assistant: Message, note: &str) {
        if assistant.blocks.is_empty() {
            return; // R58：零块消息不进 history
        }
        let calls = assistant.tool_calls();
        self.history.push(assistant);
        for (id, _, _) in calls {
            self.history
                .push(Message::tool(&id, ToolOutput::text(note), true));
        }
    }

    /// G17：扫 roots 下**根目录一级**的 `AGENTS.md`，拼出「项目规则」段。
    /// `request()` 每轮调用 —— 每轮现读，用户改文件下一轮即生效，不落库
    /// （与技能索引同一条 tail 组装路径，经 `build_with_extra` 天然参与预算）。
    ///
    /// · **只读根目录一级**，不做嵌套目录递归 —— MVP 取舍：OpenCode 的嵌套发现要沿
    ///   目录树走且每个子目录一个作用域，lfttdo 的 roots 本来就是平铺的授权目录；
    ///   子目录级规则等真有消费者（monorepo 场景）再加，扩展点就在这个循环里。
    /// · 失败静默跳过：AGENTS.md 缺失不是错误（可选约定），读不动（权限/解码失败）
    ///   同路。roots 已闸门化 —— 这里读的是用户授权的目录，不需要第二道 containment。
    /// · 每份内容截到 [`AGENTS_MD_MAX_BYTES`] 并附截断标记（防巨型文件顶爆预算）。
    /// · 多根时每段带 `### <根目录名>` 小标题分隔（归属可辨）；单根不带。
    /// · 一个都没有（或全是空文件）⇒ `None`，tail 不加这段 —— 系统提示与功能
    ///   不存在时逐字节一致（有测试钉）。
    fn agents_md_section(&self) -> Option<String> {
        let multi = self.roots.len() > 1;
        let mut parts: Vec<String> = Vec::new();
        for root in &self.roots {
            let Ok(content) = std::fs::read_to_string(root.join("AGENTS.md")) else {
                continue;
            };
            if content.trim().is_empty() {
                // 空文件 / 纯空白不产生空段落（全部为空时整段消失）。
                continue;
            }
            let mut content = content;
            if content.len() > AGENTS_MD_MAX_BYTES {
                let cut = floor_char_boundary(&content, AGENTS_MD_MAX_BYTES);
                content.truncate(cut);
                content.push_str(AGENTS_MD_TRUNCATED_NOTE);
            }
            if multi {
                let name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.to_string_lossy().into_owned());
                parts.push(format!("### {name}\n\n{content}"));
            } else {
                parts.push(content);
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!("{AGENTS_MD_HEADING}\n\n{}", parts.join("\n\n")))
    }

    /// G18：跑一次诊断命令，返回 Some(text) = 有非空输出要反馈给模型。
    ///
    /// · cwd：会话输出目录（`ctx.write_root`）；没有就退回 `roots[0]` —— 诊断命令
    ///   通常要相对某个项目根才有意义。两处都没有 ⇒ 放弃（在任意进程 cwd 下跑
    ///   用户没指定位置的命令，输出无法解释）。
    /// · 15s 超时、随父令牌取消立即放弃（`select!` biased 取消臂优先）。
    /// · **退出码不参与判定**：非零恰恰是「有诊断」（编译器报错走非零退出）；
    ///   spawn 失败 / 超时才给 `None` —— 反馈只来自真实输出，不编造。
    /// · stdout+stderr 合并，截到 4000 字符（按字符，UTF-8 安全）—— 这是给模型的
    ///   反馈摘要，不是完整日志。
    async fn run_diagnostics(&self, command: &str, ctx: &ToolCtx) -> Option<String> {
        let mut parts = command.split_whitespace();
        let program = parts.next()?;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(parts);
        let cwd = ctx
            .write_root
            .clone()
            .or_else(|| ctx.roots.first().cloned())?;
        cmd.current_dir(cwd);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return None,
            res = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output()) => {
                // 超时（Elapsed）或 spawn 失败：没有可反馈的输出。
                res.ok()?.ok()?
            }
        };
        let mut merged = String::from_utf8_lossy(&output.stdout).into_owned();
        merged.push_str(&String::from_utf8_lossy(&output.stderr));
        let merged = merged.trim();
        if merged.is_empty() {
            return None;
        }
        Some(merged.chars().take(4000).collect())
    }

    /// 交给模型的请求：滑窗组装出来的消息 + 注册表的工具声明。
    /// `&mut self` 只为记下这次请求的粗估（校准的分母）；`run()` 是它唯一的调用方。
    fn request(&mut self) -> ChatRequest {
        // 外壳规矩 + 技能索引 + 本轮命中的技能正文，一起追加在人设**之后**：
        // `ContextConfig::system_prompt` 非空时是整段替换内置提示词的，工具与技能的调用
        // 规矩不该跟着人设一起被抹掉（本地小模型「有工具却凭记忆答」的主因之一）。
        let mut skills = self
            .skills_dir
            .as_deref()
            .map(crate::skills::discover)
            .unwrap_or_default();
        skills.retain(|e| !self.skills_disabled.iter().any(|f| f == &e.folder));
        let mut tail = String::from(TOOL_POLICY);
        // 技能索引只在请求上、不进历史：用户往 skills 里加/删文件，下一轮就生效。
        if let Some(section) = crate::skills::prompt_section(&skills) {
            tail.push_str("\n\n");
            tail.push_str(&section);
        }
        // 用户这句话里出现技能名或触发词，就直接把正文放进本轮：按技能的步骤做这件事
        // 不该取决于 8B 模型记不记得要先调 `use_skill`。没命中的技能仍然只留一行描述。
        let hits = crate::skills::matched(
            &skills,
            &self.turn_user_text,
            crate::skills::MAX_LOADED_PER_TURN,
        );
        let mut loaded: Vec<crate::skills::Skill> = Vec::new();
        // 只有**完整**加载的正文才进短路表：被 16 KiB 截断的那份，模型必须还能用
        // `use_skill` 把剩下的读回来（截断提示就是这么写的）。
        let mut loaded_keys: Vec<(String, Vec<String>)> = Vec::new();
        for entry in hits {
            let Ok(skill) = crate::skills::body_of_entry(entry) else {
                continue;
            };
            if skill.body.len() <= crate::skills::MAX_LOADED_BYTES {
                loaded_keys.push((
                    entry.name.clone(),
                    vec![entry.name.to_lowercase(), entry.folder.to_lowercase()],
                ));
            }
            loaded.push(skill);
        }
        self.turn_skills = loaded_keys;
        if let Some(block) = crate::skills::loaded_section(&loaded) {
            tail.push_str("\n\n");
            tail.push_str(&block);
        }
        if let Some(dir) = &self.output_dir {
            tail.push_str("\n\n## 工作目录\n本次会话的输出目录是：");
            tail.push_str(&crate::tools::fs_write::display_path(dir));
            tail.push_str("。生成的图片、视频和文档都写到这里；不要写到别的目录。");
        }
        // G17：AGENTS.md 每轮现读（格式与取舍见 `agents_md_section`）—— 用户改文件
        // 下一轮即生效，不落库；与技能索引同一条 tail 组装路径，走 build_with_extra
        // 的 tail 参数参与预算（先 build 再往外贴会让请求凭空超线，同下）。
        if let Some(section) = self.agents_md_section() {
            tail.push_str("\n\n");
            tail.push_str(&section);
        }
        // 追加段交给 ContextBuilder 一起算预算：先 build 再往外贴会让请求凭空超线。
        let built = self.deps.context.build_with_extra(&self.history, &tail);
        self.last_est_input = built.est_input_tokens;
        // `built.dropped` 在 Plan A 没有消费者：`ContextCompacted` 属切片 5，
        // `context.rs:44` 明确要求 Task 10 不为它造事件。
        // R286：声明表按「这一轮到底能不能用」过滤。roots 为空时 `read_file` 对**任何**路径
        // 都会被闸门规则 1 拒，把它挂在请求里等于邀请模型多烧一轮 token、界面上多一张必然
        // 红色的卡；判据住在工具自己家（`read_file::usable_with_roots`），这里只问一次。
        // 边界要说清：这**只影响声明表，不影响闸门** —— 模型凭空喊 `read_file` 照样走到
        // `resolve_within_roots` 被拒（`hiding_a_tool_does_not_weaken_the_gate` 钉的就是这条）。
        // 隐藏是省 token，不是授权，越权那道墙永远在闸门里。
        // R286：声明表按「这一轮到底能不能用」过滤，且与预算估算共用同一个函数
        // —— 算的是全量、发的是过滤后，会让估算虚高、校准系数偏低。
        // 隐藏不是授权：模型凭空喊这两个名字照样会走工具自己的拒绝路径。
        let tools = filtered_tool_defs(
            &self.deps.registry,
            &self.roots,
            !skills.is_empty(),
            self.cfg.allowed_tools.as_deref(),
        );
        ChatRequest {
            messages: built.messages,
            // R66：`defs()` 已经是 `Vec<model::provider::ToolDef>`（registry.rs:143），
            // 这里没有跨层转换要做，逐字段搬运只会让人以为 risk 能在这里被加工。
            tools,
            // T39：Plan A 这里硬写 `None`，于是设置里的温度到 `ModelConfig` 就断了线
            // （`openai_compat.rs` 的发送侧一直在等这个 `Some`）。
            temperature: self.deps.temperature,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Task 10 的六条路径（plan:2010 起）+ 控制器裁定 R56–R63 补上的
    //! 中断 / 配对 / 用量 / 空响应路径。Plan A 的生产工具表是**空的**
    //! （plan:1854：`echo` 不注册进生产），所以工具路径的真相只能在这里建立：
    //! 每条测试都用真注册表、真权限门、真滑窗，只有模型是脚本化的。
    use super::*;
    use crate::agent::context::ContextConfig;
    use crate::agent::events::{Decision, ErrorWhere, Scope, StopReason};
    use crate::agent::types::{ContentBlock, ImageMime, Message, Risk, Role, ToolOutput};
    use crate::model::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
    use crate::permission::gate::PermissionGate;
    use crate::tools::echo::{EchoTool, RiskOverrideTool};
    use crate::tools::registry::{Tool, ToolError, ToolMeta, ToolRegistry};
    use async_stream::stream;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // ---- 测试替身 ----

    /// 记录型事件出口：整条时间线只能从这里取证（生产用的是 `ipc.rs` 里的 TauriEmitter）。
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

        /// 事件的线上 `t` 标签序列。走 serde 而不是 match：前端读的就是这个标签。
        /// 多数测试对它做**精确**断言——`contains` 那种断言挡不住「多发一个事件」，
        /// 而 R60 的占位 usage、R64 的重复报错正好都是多发型 bug。
        fn kinds(&self) -> Vec<String> {
            self.events()
                .iter()
                .map(|e| {
                    serde_json::to_value(e).unwrap()["t"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        }

        fn done(&self) -> Option<StopReason> {
            self.events().into_iter().find_map(|e| match e {
                AgentEvent::Done { stop_reason } => Some(stop_reason),
                _ => None,
            })
        }

        /// `(where, message, fatal)`，按事件顺序。
        fn errors(&self) -> Vec<(ErrorWhere, String, bool)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::Error { at, message, fatal } => Some((at, message, fatal)),
                    _ => None,
                })
                .collect()
        }

        fn usage(&self) -> Vec<(u32, u32)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => Some((input_tokens, output_tokens)),
                    _ => None,
                })
                .collect()
        }

        fn permission_ids(&self) -> Vec<String> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::PermissionRequest { id, .. } => Some(id),
                    _ => None,
                })
                .collect()
        }

        /// `(call_id, is_error, preview)`，按事件顺序：工具路径的成败与**顺序**都看这里。
        fn tool_results(&self) -> Vec<(String, bool, String)> {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::ToolResult {
                        call_id,
                        is_error,
                        preview,
                        ..
                    } => Some((call_id, is_error, preview)),
                    _ => None,
                })
                .collect()
        }

        fn texts(&self) -> String {
            self.events()
                .into_iter()
                .filter_map(|e| match e {
                    AgentEvent::TextDelta { text } => Some(text),
                    _ => None,
                })
                .collect()
        }
    }

    /// 模型侧取证点：每个 `ChatRequest` 在被调用那一刻记下。
    /// R57——「模型实际看到了什么」只能从模型这一侧拿，从 history 反推等于自证。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<ChatRequest>>>);

    impl Recorder {
        fn push(&self, r: ChatRequest) {
            self.0.lock().unwrap().push(r);
        }
        fn recorded(&self) -> Vec<ChatRequest> {
            self.0.lock().unwrap().clone()
        }
    }

    /// 每轮对话吐一组预录 delta；Plan B/C 的测试同样用它。
    struct ScriptedModel {
        rounds: Mutex<Vec<Vec<ModelDelta>>>,
        recorder: Recorder,
    }

    impl ScriptedModel {
        fn new(rounds: Vec<Vec<ModelDelta>>) -> Arc<Self> {
            Arc::new(Self {
                rounds: Mutex::new(rounds),
                recorder: Recorder::default(),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            // 夹具前提：轮数必须够循环用完。`remove(0)` 空了就 panic，
            // 而不是让循环安静地少跑一轮——那会把「触顶」测成「提前收尾」。
            let next = self.rounds.lock().unwrap().remove(0);
            self.recorder.push(req);
            Box::pin(futures::stream::iter(next.into_iter().map(Ok)))
        }
    }

    /// 一问就失败的模型。`ModelError::Api` = 已连通的 429（provider.rs 的分类表）。
    /// `head` 是报错之前先吐出来的分片：留空就是「第一个 delta 之前就断了」。
    struct FailingModel {
        head: Vec<ModelDelta>,
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for FailingModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(req);
            let head = self.head.clone();
            Box::pin(futures::stream::iter(head.into_iter().map(Ok).chain(
                std::iter::once(Err(ModelError::Api("429 rate limited".into()))),
            )))
        }
    }

    /// 吐出 `head` 之后**永久挂住**的模型：模拟一个还连着的流式响应。
    /// 关键是 `ready` 的时机：`stream!` 的每次 `yield` 都把执行权交给消费者，
    /// 所以 `ready` 置位时最后一个 head 分片**已经抵达累加器**——
    /// 测试于是能在「半条回复 + 一个未执行的 tool_call 已经在 acc 里」的那一刻才按停止，
    /// 不靠 sleep 猜时间，也不会打断在流结束之后（那就不叫打断流式了）。
    struct HangingModel {
        head: Vec<ModelDelta>,
        ready: Arc<AtomicBool>,
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for HangingModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(req);
            let head = self.head.clone();
            let ready = self.ready.clone();
            Box::pin(stream! {
                for d in head {
                    yield Ok(d);
                }
                ready.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            })
        }
    }

    /// 记账工具：Write 级，所以每次调用都得先过权限门——
    /// 「门没放行就绝不执行工具」这件事才有观察点（只看返回内容是证不出的：
    /// echo 的替身被拒后也会回显同样的文本）。
    #[derive(Clone, Default)]
    struct CountingTool(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl Tool for CountingTool {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "counted".into(),
                description: "记下自己被执行过几次".into(),
                input_schema: json!({"type": "object"}),
                risk: Risk::Write,
            }
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push(input.to_string());
            Ok(ToolOutput::text("执行了"))
        }
    }

    impl CountingTool {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// 永远不自己收尾的只读工具：取消必须能把它打断，而不是等它 30 秒后回来。
    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "slow".into(),
                description: "睡 30 秒才返回".into(),
                input_schema: json!({"type": "object"}),
                risk: Risk::Read,
            }
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(ToolOutput::text("slow done"))
        }
    }

    /// T26 Step 4 的取证点：记下**执行那一刻**的 `ctx.roots`。
    /// 判据只能从工具这一侧拿 —— `ToolCtx` 是 `run()` 内部构造的（`session.rs:120`），
    /// 从 `AgentSession` 的字段读回来等于自证，看不见「构造时到底填了什么」。
    /// Read 档是有意的：这条测试要量的是 roots 到没到，弹窗那条线 `gate.rs` 自己钉着，
    /// 这里不想为了取证再搭一套应答任务（那会把「roots 没接上」的读数埋进应答时序里）。
    #[derive(Clone, Default)]
    struct AssertRootsTool(Arc<Mutex<Vec<Vec<PathBuf>>>>);

    #[async_trait]
    impl Tool for AssertRootsTool {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "assert_roots".into(),
                description: "记下执行那一刻 ctx.roots 的假工具".into(),
                input_schema: json!({"type": "object"}),
                risk: Risk::Read,
            }
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            self.0.lock().unwrap().push(ctx.roots.clone());
            Ok(ToolOutput::text("记下了"))
        }
    }

    impl AssertRootsTool {
        fn roots_seen(&self) -> Vec<Vec<PathBuf>> {
            self.0.lock().unwrap().clone()
        }
    }

    // ---- 装配 ----

    /// 一套完整装配：真注册表、真权限门、真滑窗，模型是脚本化的。
    /// 每个 rig 都注册 `echo`(Read) 与 `guarded`(Write)：guarded 不调用就不弹窗，
    /// 所以「Read 零弹窗」这类断言不受它影响，而需要 Ask 路径的测试可以直接调它。
    struct Rig {
        session: AgentSession,
        sink: Arc<VecSink>,
        gate: Arc<PermissionGate>,
        recorder: Recorder,
    }

    impl Rig {
        fn new(rounds: Vec<Vec<ModelDelta>>) -> Rig {
            Rig::with_context(ContextConfig::default(), rounds)
        }

        fn with_context(context: ContextConfig, rounds: Vec<Vec<ModelDelta>>) -> Rig {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            Rig::assembled(model, context, recorder, vec![])
        }

        /// 装配 + 一个记账工具（名字 `counted`，Write 级）。
        fn with_counted(rounds: Vec<Vec<ModelDelta>>) -> (Rig, Arc<CountingTool>) {
            Self::with_counted_config(SessionConfig::default(), rounds)
        }

        /// G15：`with_counted` 的带 `SessionConfig` 变体 —— plan 模式要拦的正是
        /// Write 级的 `counted`。
        fn with_counted_config(
            cfg: SessionConfig,
            rounds: Vec<Vec<ModelDelta>>,
        ) -> (Rig, Arc<CountingTool>) {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            let tool = Arc::new(CountingTool::default());
            let (session, sink, gate) = parts(
                model,
                ContextConfig::default(),
                None,
                vec![tool.clone()],
                Vec::new(),
            );
            let rig = Rig {
                session: session.with_config(cfg),
                sink,
                gate,
                recorder,
            };
            (rig, tool)
        }

        /// G15：与 `new()` 唯一的区别是注入自定义 `SessionConfig`（echo/guarded 照常注册，
        /// Read 级的对照测试用）。
        fn with_config(cfg: SessionConfig, rounds: Vec<Vec<ModelDelta>>) -> Rig {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            let (session, sink, gate) = parts(model, ContextConfig::default(), None, vec![], Vec::new());
            Rig {
                session: session.with_config(cfg),
                sink,
                gate,
                recorder,
            }
        }

        /// 装配 + 一条 roots 表 + 额外的工具（T26 Step 4）。
        /// 夹具经的是**生产同一个入口** `AgentSession::with_roots`（R276：roots 进 `ToolCtx`
        /// 只有这一条路），所以这条测试量到的就是那条线本身，而不是夹具在 `SessionDeps`
        /// 上另开的第三条路。
        fn with_roots(
            rounds: Vec<Vec<ModelDelta>>,
            roots: Vec<PathBuf>,
            extra_tools: Vec<Arc<dyn Tool>>,
        ) -> Rig {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            let (session, sink, gate) = parts(
                model,
                ContextConfig::default(),
                None,
                extra_tools,
                // 温度这一支与 `assembled` 同样给 `None`：这条测试不量温度，
                // 温度那条线由 `the_session_temperature_reaches_the_chat_request` 钉。
                roots,
            );
            Rig {
                session,
                sink,
                gate,
                recorder,
            }
        }

        fn assembled(
            model: Arc<dyn ModelProvider>,
            context: ContextConfig,
            recorder: Recorder,
            extra_tools: Vec<Arc<dyn Tool>>,
        ) -> Rig {
            let (session, sink, gate) = parts(model, context, None, extra_tools, Vec::new());
            Rig {
                session,
                sink,
                gate,
                recorder,
            }
        }

        /// R286 的夹具：注册表里**只有**传进来的那些工具（`parts()` 会附带 `echo` 与
        /// `guarded`，那是给权限门路径用的替身）。
        /// 这三条量的是「模型在生产工具集下看到的声明名单」，名单要能写成字面值，
        /// 夹具自己塞进去的替身就会把这件事变成猜测。roots 与 `parts()` 同一条入口
        /// （`AgentSession::with_roots`，R276），不在 `SessionDeps` 上另开第二个字段。
        fn with_only(
            rounds: Vec<Vec<ModelDelta>>,
            roots: Vec<PathBuf>,
            tools: Vec<Arc<dyn Tool>>,
        ) -> Rig {
            let model = ScriptedModel::new(rounds);
            let recorder = model.recorder.clone();
            let sink = Arc::new(VecSink::default());
            let registry = Arc::new(ToolRegistry::new());
            for t in tools {
                registry.register(t);
            }
            let gate = Arc::new(PermissionGate::new().with_sink(sink.clone()));
            let session = AgentSession::new(SessionDeps {
                model,
                registry,
                gate: gate.clone(),
                sink: sink.clone(),
                context: ContextBuilder::new(ContextConfig::default()),
                temperature: None,
            })
            .with_roots(roots);
            Rig {
                session,
                sink,
                gate,
                recorder,
            }
        }

        /// 挂住的模型：`ready` 置位 = head 已全部抵达累加器。
        fn hanging(head: Vec<ModelDelta>) -> (Rig, Arc<AtomicBool>) {
            let ready = Arc::new(AtomicBool::new(false));
            let recorder = Recorder::default();
            let model = Arc::new(HangingModel {
                head,
                ready: ready.clone(),
                recorder: recorder.clone(),
            });
            (
                Rig::assembled(model, ContextConfig::default(), recorder, vec![]),
                ready,
            )
        }

        fn failing() -> Rig {
            Rig::failing_after(Vec::new())
        }

        /// 先吐 `head` 再报错的模型。「流里已经攒下一个完整 tool_call，然后断了」这条
        /// 组合只能由它造出来 —— 报错出口和取消出口共用 `commit_partial`，而 R56 的
        /// 裁定只写在「取消」上，没测过就等于另一半路径没走过。
        fn failing_after(head: Vec<ModelDelta>) -> Rig {
            let recorder = Recorder::default();
            let model = Arc::new(FailingModel {
                head,
                recorder: recorder.clone(),
            });
            Rig::assembled(model, ContextConfig::default(), recorder, vec![])
        }

        async fn run(&mut self, text: &str) -> StopReason {
            self.session.run(user(text)).await
        }

        fn reqs(&self) -> Vec<ChatRequest> {
            self.recorder.recorded()
        }
    }

    /// 温度作为第 3 个入参而不是藏在夹具默认里：`the_session_temperature_reaches_the_chat_request`
    /// 要能换着值喂它，而其它测试要的「不带温度」= 生产上端点自己定那个状态，写明白比默认好。
    ///
    /// `roots` 是 T26 加的第 5 个入参，同样是「显式而不是默认」：绝大多数 rig 要的是**空表**
    /// （生产默认，`read_file` 因此全拒），只有 `run_turn_passes_configured_roots_into_tool_ctx`
    /// 要一张非空的表。把空表写成每个调用点的 `vec![]` 而不是塞进默认值，为的是让
    /// 「这条测试到底在什么 roots 下跑」在读测试的人眼前就是一个具体值。
    fn parts(
        model: Arc<dyn ModelProvider>,
        context: ContextConfig,
        temperature: Option<f32>,
        extra_tools: Vec<Arc<dyn Tool>>,
        roots: Vec<PathBuf>,
    ) -> (AgentSession, Arc<VecSink>, Arc<PermissionGate>) {
        let sink = Arc::new(VecSink::default());
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(RiskOverrideTool(Risk::Write)));
        for t in extra_tools {
            registry.register(t);
        }
        let gate = Arc::new(PermissionGate::new().with_sink(sink.clone()));
        let session = AgentSession::new(SessionDeps {
            model,
            registry,
            gate: gate.clone(),
            sink: sink.clone(),
            context: ContextBuilder::new(context),
            temperature,
        })
        // R276：roots 进 `ToolCtx` 只有这一个入口 —— builder 方法，不在 `SessionDeps` 上
        // 再开第二个字段。表**原样**送下去，这里不做任何过滤：今天哪几条能用是
        // `settings.rs::effective_roots` 在**使用点**（`ipc.rs::run_turn`）定的事，
        // 执行点只负责按交进来的表放行（R284 把裁剪从读写两侧搬到了这里）。
        .with_roots(roots);
        (session, sink, gate)
    }

    // ---- 夹具 ----

    fn user(text: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: text.into() }]
    }

    /// 纯文本回复：按片段分 delta（逐字符只会把同一条路径重复几十遍）。
    fn deltas(parts: &[&str]) -> Vec<ModelDelta> {
        parts
            .iter()
            .map(|p| ModelDelta::Text((*p).to_string()))
            .collect()
    }

    /// 一轮工具调用，`calls` 是 (工具名, arguments 原文)，index 升序。
    /// **id 每轮唯一**（R57）：全都复用 `"c1"` 的话，「找得到对应调用」的断言会
    /// latch 到早先那次调用上，配对真的错了也照样绿。
    fn tool_round(round: usize, calls: &[(&str, &str)]) -> Vec<ModelDelta> {
        let mut out = Vec::new();
        for (index, (name, args)) in calls.iter().enumerate() {
            let id = format!("r{round}c{index}");
            out.push(ModelDelta::ToolCallStart {
                index: index as u32,
                id: Some(id),
                name: Some((*name).to_string()),
            });
            out.push(ModelDelta::ToolCallArgs {
                index: index as u32,
                fragment: (*args).to_string(),
            });
        }
        out
    }

    /// 用替身的话名字和 `meta()` 都得由夹具自己编，那就变成夹具在测自己。
    /// 与 `ipc::build_production_registry` 是同一批类型，但这里不直接调它 ——
    /// `tools/` 与 `agent/` 整棵子树都不许依赖装配层（`lib.rs` 的边界约束），
    /// 测试里反向够一次 `crate::ipc` 同样是那条约束的破口。
    fn production_tools() -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(crate::tools::fetch::FetchTool::new()),
            Arc::new(crate::tools::read_file::ReadFileTool),
            Arc::new(crate::tools::use_skill::UseSkillTool),
            Arc::new(crate::tools::write_file::WriteFileTool),
            Arc::new(crate::tools::edit_file::EditFileTool),
        ]
    }

    /// 一份最小技能包。返回的 `TempDir` 必须活到会话跑完（技能是在请求时才读盘的）。
    fn skill_fixture(folder: &str, name: &str, description: &str, body: &str) -> tempfile::TempDir {
        skill_fixture_with_keywords(folder, name, description, "", body)
    }

    fn skill_fixture_with_keywords(
        folder: &str,
        name: &str,
        description: &str,
        keywords: &str,
        body: &str,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(folder);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let frontmatter = if keywords.is_empty() {
            format!("---\nname: {name}\ndescription: {description}\n---\n")
        } else {
            format!("---\nname: {name}\ndescription: {description}\nkeywords: {keywords}\n---\n")
        };
        std::fs::write(skill_dir.join("SKILL.md"), format!("{frontmatter}{body}\n")).unwrap();
        dir
    }

    /// **每一次**请求里的工具名（按注册表的字典序原样交出，不排序：顺序本身就是被测物）。
    /// 取全部请求而不是只看第一次：工具循环里的第二次请求同样不许把藏起来的工具再端出去。
    fn declared_names(reqs: &[ChatRequest]) -> Vec<Vec<String>> {
        reqs.iter()
            .map(|r| r.tools.iter().map(|d| d.name.clone()).collect())
            .collect()
    }

    /// 轮询等第 n 条 `PermissionRequest` 落地。用 sleep 而不是 `yield_now`：
    /// 被等的那一轮正跑在**同一个** current-thread 运行时上，纯忙等要么饿死它、
    /// 要么在 `await` 点之前读到空表（gate.rs 的同类循环是跨任务 spawn，那里够用）。
    /// 5 秒上限，超时即 panic——挂死不是通过。
    async fn wait_requests(sink: &VecSink, n: usize) -> Vec<String> {
        for _ in 0..500 {
            let ids = sink.permission_ids();
            if ids.len() >= n {
                return ids;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "等不到第 {n} 条 PermissionRequest，已有事件 {:?}",
            sink.kinds()
        );
    }

    /// 后台应答第一条弹窗。`run()` 会在权限门上阻塞，所以应答任务必须**先 spawn**。
    fn answer_first(
        gate: &Arc<PermissionGate>,
        sink: &Arc<VecSink>,
        d: Decision,
    ) -> tokio::task::JoinHandle<bool> {
        let gate = gate.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let id = wait_requests(&sink, 1).await.remove(0);
            gate.resolve(&id, d)
        })
    }

    /// 应答**所有**弹窗（G07 夹具用）：每出现一条新请求就放行（Once），
    /// 放行 `total` 条后收工。5 秒上限，超时 panic —— 挂死不是通过。
    fn answer_all(
        gate: &Arc<PermissionGate>,
        sink: &Arc<VecSink>,
        total: usize,
    ) -> tokio::task::JoinHandle<()> {
        let gate = gate.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let mut answered = 0usize;
            for _ in 0..500 {
                let ids = sink.permission_ids();
                while answered < ids.len().min(total) {
                    gate.resolve(&ids[answered], Decision::allow(Scope::Once));
                    answered += 1;
                }
                if answered >= total {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("应答不到 {total} 条 PermissionRequest，只应答了 {answered} 条");
        })
    }

    /// 请求里所有工具结果消息的 `(call_id, is_error, 文本)`。
    fn tool_results_of(msgs: &[Message]) -> Vec<(String, bool, String)> {
        let mut out = Vec::new();
        for m in msgs {
            for b in &m.blocks {
                if let ContentBlock::ToolResult {
                    call_id,
                    is_error,
                    output,
                } = b
                {
                    let text = match output {
                        ToolOutput::Text { text } => text.clone(),
                        ToolOutput::Image { .. } => "<image>".into(),
                        // R317：多块结果按块摊平，测试辅助只关心「模型看见了什么文本」。
                        ToolOutput::Multi { parts } => parts
                            .iter()
                            .map(|part| match part {
                                ToolOutput::Text { text } => text.clone(),
                                ToolOutput::Image { .. } => "<image>".into(),
                                ToolOutput::Multi { .. } => "<multi>".into(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    out.push((call_id.clone(), *is_error, text));
                }
            }
        }
        out
    }

    // ---- R319：工具返回的图片要作为 user 轮送进模型 ----

    #[test]
    fn tool_images_become_exactly_one_user_turn_and_text_only_adds_nothing() {
        let img = ContentBlock::Image {
            data: "QUJD".into(),
            mime: ImageMime::Png,
        };
        assert!(
            image_followup(&[]).is_none(),
            "纯文本工具的输出不许平白多一条 user 消息"
        );
        let m = image_followup(&[img]).expect("有图就要补一条");
        assert_eq!(m.role, Role::User, "必须是 user 轮：tool 角色只吃文本");
        assert!(
            matches!(m.blocks[0], ContentBlock::Text { .. }),
            "先有一句说明，模型才知道这条消息是工具图片"
        );
        assert!(
            matches!(m.blocks[1], ContentBlock::Image { .. }),
            "图必须是真正的 Image 块，而不是一句 [图片]"
        );
        assert_eq!(m.blocks.len(), 2, "一句说明 + 一张图，不加别的");

        // Multi 递归摊平：text 丢掉、image 留下。
        let multi = ToolOutput::Multi {
            parts: vec![
                ToolOutput::text("x"),
                ToolOutput::Image {
                    data: "QkNE".into(),
                    mime: ImageMime::Png,
                },
            ],
        };
        assert_eq!(images_of(&multi).len(), 1);
        assert!(images_of(&ToolOutput::text("无图")).is_empty());

        // 同一张图出现两次 ⇒ 只送一次（图片按张计费）。
        let same = ContentBlock::Image {
            data: "QUJD".into(),
            mime: ImageMime::Png,
        };
        let deduped = image_followup(&[same.clone(), same]).expect("仍有图");
        assert_eq!(deduped.blocks.len(), 2, "一句说明 + 一张（去重后的）图");
    }

    // ---- R57 的不变量：模型看到的每个请求都得配对 ----

    /// 可执行形式的 R56/R57 不变量，作用在**模型实际收到的请求**上：
    /// 1. 每个 `ToolResult` 的 `call_id` 必须匹配一条**在前的** assistant `ToolCall`；
    /// 2. 带 N 个调用的 assistant 消息必须紧跟 N 条结果，id 集合完全相同
    ///    （结果没回填 = 下一轮模型看不见结果，这条直接红）；
    /// 3. 调用 id 在单个请求内唯一——这是前两条不 latch 的**夹具前提**，
    ///    写进断言而不是只写在注释里，夹具漂移时是这条先红。
    fn assert_paired(label: &str, req: &ChatRequest) {
        let msgs = &req.messages;
        // 当前 assistant 轮还欠着的结果 id；空 = 没有待回填的调用。
        let mut owed: Vec<String> = Vec::new();
        let mut owed_at = 0usize;
        let mut seen: Vec<String> = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            let calls: Vec<String> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            let results: Vec<String> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                })
                .collect();

            for id in &calls {
                assert!(
                    !seen.contains(id),
                    "{label} 第 {i} 条：调用 id {id} 重复出现——「找得到对应调用」的断言会 latch 到早先那次，什么都测不出（夹具前提）"
                );
                seen.push(id.clone());
            }
            for id in &results {
                assert!(
                    seen.contains(id),
                    "{label} 第 {i} 条：工具结果 {id} 前面没有任何匹配的调用，这个请求上线就是 400"
                );
            }

            let has_calls = !calls.is_empty();
            let has_results = !results.is_empty();

            if has_calls {
                assert!(
                    owed.is_empty(),
                    "{label} 第 {i} 条：上一条 assistant（第 {owed_at} 条）的调用 {owed:?} 还没等到结果，又来了一条带调用的消息"
                );
                owed = calls;
                owed_at = i;
            }
            if has_results {
                assert!(
                    !owed.is_empty(),
                    "{label} 第 {i} 条：结果 {results:?} 不属于任何待回填的调用（重复回填或切口切错了）"
                );
                for id in &results {
                    assert!(
                        owed.iter().any(|c| c == id),
                        "{label}：第 {i} 条结果 {id} 不属于第 {owed_at} 条 assistant 的调用 {owed:?}"
                    );
                    owed.retain(|c| c != id);
                }
            }
            // 严格相邻：调用与它的结果之间不许插别的消息（system 备注恒排在最前，不会走到这里）。
            if !has_calls && !has_results && !owed.is_empty() && m.role != Role::System {
                panic!(
                    "{label} 第 {i} 条：第 {owed_at} 条 assistant 的调用 {owed:?} 还没等到结果，中间插进了这条消息"
                );
            }
        }
        assert!(
            owed.is_empty(),
            "{label}：第 {owed_at} 条 assistant 的调用 {owed:?} 到请求结尾都没有配对结果——history 结束在 tool_call 上，下一轮就是 mid-session 400（R56）"
        );
    }

    /// 整段对话跑完后，把不变量 over 每一个模型收到过的请求。
    fn assert_every_request_paired(rig: &Rig) {
        for (n, req) in rig.reqs().iter().enumerate() {
            assert_paired(&format!("请求 #{n}"), req);
        }
    }

    // ---- 路径 1：纯文本 ----

    #[tokio::test]
    async fn plain_text_turn_emits_turn_start_delta_done_end_turn() {
        let mut rig = Rig::new(vec![deltas(&["你好", "，我是 Qwen Do"])]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);

        // 精确序列：多一个事件就红。特别是 `usage`——plan Step 2 里那句
        // `Usage{0,0}` 占位 emit 会把前端的 token 计数每轮归零（R60 要求删掉）。
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "text_delta", "text_delta", "done"]
        );
        assert_eq!(rig.sink.texts(), "你好，我是 Qwen Do");
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_eq!(rig.sink.usage(), Vec::<(u32, u32)>::new());

        // 纯文本轮只有 user + assistant 两条（Step 3 修正了 plan 里写成 3 的断言）。
        let hist = rig.session.history();
        assert_eq!(hist.len(), 2, "{hist:?}");
        assert_eq!(hist[1].text_of(), "你好，我是 Qwen Do");

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].messages.len(), 2, "system + user");
        assert_eq!(reqs[0].messages[0].role, Role::System);
        // R66：注册表的声明原样进请求，不在这里做字段搬运。
        let names: Vec<String> = reqs[0].tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, ["echo", "guarded"], "工具声明必须来自注册表");
        assert_paired("纯文本请求", &reqs[0]);
    }

    // ---- 路径 2：工具调用 + 回填 + 第二轮 ----

    #[tokio::test]
    async fn tool_call_is_executed_and_backfilled_then_second_round() {
        let mut rig = Rig::new(vec![
            tool_round(0, &[("echo", r#"{"text":"hi"}"#)]),
            deltas(&["拿到结果了"]),
        ]);
        assert_eq!(rig.run("用工具").await, StopReason::EndTurn);

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ],
            "Read 级工具必须零弹窗（spec 5.1），且回填后要再发一次 turn_start 问第二轮"
        );
        assert_eq!(
            rig.sink.tool_results(),
            vec![("r0c0".into(), false, "hi".into())]
        );
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_eq!(rig.sink.texts(), "拿到结果了");

        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            4,
            "user, assistant(ToolCall), tool, assistant：{hist:?}"
        );
        assert_eq!(
            tool_results_of(hist),
            vec![("r0c0".into(), false, "hi".into())]
        );

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2, "工具结果回填后必须再问一轮");
        assert_eq!(reqs[0].messages.len(), 2, "第一次调用时模型还看不到工具");
        assert_eq!(tool_results_of(&reqs[0].messages), vec![]);
        assert_eq!(
            reqs[1].messages.len(),
            4,
            "system + user + assistant + tool"
        );
        assert_eq!(
            tool_results_of(&reqs[1].messages),
            vec![("r0c0".into(), false, "hi".into())],
            "模型必须真的收到那条工具结果，否则第二轮是凭空回答"
        );
        assert_every_request_paired(&rig);
    }

    #[tokio::test]
    async fn two_parallel_calls_each_get_exactly_one_backfilled_result() {
        // 只测一个调用的话，「跑完第一个就 break」这种退化是绿的——
        // 而它留下的正是一条没有结果的 tool_call（R57 的「匹配数量」那一半）。
        let mut rig = Rig::new(vec![
            tool_round(
                0,
                &[
                    ("echo", r#"{"text":"第一"}"#),
                    ("echo", r#"{"text":"第二"}"#),
                ],
            ),
            deltas(&["两个都拿到了"]),
        ]);
        assert_eq!(rig.run("并行调两个").await, StopReason::EndTurn);

        // 顺序执行（spec 4.6）：第 0 个调用的结果必须先于第 1 个调用出现。
        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ]
        );
        assert_eq!(
            rig.sink.tool_results(),
            vec![
                ("r0c0".into(), false, "第一".into()),
                ("r0c1".into(), false, "第二".into())
            ]
        );
        assert_eq!(
            tool_results_of(rig.session.history()),
            vec![
                ("r0c0".into(), false, "第一".into()),
                ("r0c1".into(), false, "第二".into())
            ]
        );
        assert_every_request_paired(&rig);
    }

    // ---- 路径 3：用户拒绝 ----

    #[tokio::test]
    async fn denied_tool_fills_error_result_and_loop_continues() {
        let mut rig = Rig::new(vec![
            tool_round(0, &[("guarded", r#"{"text":"x"}"#)]),
            deltas(&["被拒了，我换法子"]),
        ]);
        let answered = answer_first(&rig.gate, &rig.sink, Decision::deny("我不想让你写"));

        assert_eq!(
            rig.run("写个文件").await,
            StopReason::EndTurn,
            "拒绝不是错误：循环必须继续到模型自己收尾（spec 4.6）"
        );
        assert!(
            answered.await.unwrap(),
            "弹窗没进 pending：拒绝路径压根没经过权限门"
        );

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "permission_request",
                "permission_resolved",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ]
        );
        // R64：工具这一侧的事实只由 tool_result 承载，不再补一条 Error{at:Tool}。
        assert_eq!(rig.sink.errors(), Vec::<(ErrorWhere, String, bool)>::new());

        // R59：内容就是拒绝理由本身，外面不许再套 `ToolError::Failed`
        // （它的 Display 前缀是「工具执行失败: 」，套上去模型会读到双重包装）。
        let refused = vec![(
            "r0c0".to_string(),
            true,
            "用户拒绝执行：我不想让你写".to_string(),
        )];
        assert_eq!(tool_results_of(rig.session.history()), refused);
        assert_eq!(rig.sink.tool_results(), refused);
        assert_eq!(rig.sink.texts(), "被拒了，我换法子");

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            tool_results_of(&reqs[1].messages),
            refused,
            "模型必须看得见这条拒绝，才能自己换路"
        );
        assert_every_request_paired(&rig);
    }

    #[tokio::test]
    async fn an_allowed_call_executes_once_and_a_denied_call_not_at_all() {
        // 「门没放行就不执行」只看返回内容证不出来（guarded 被拒后也会回显同样的字），
        // 所以这里用一次真的执行计数。
        let (mut rig, tool) = Rig::with_counted(vec![
            tool_round(0, &[("counted", r#"{"n":1}"#)]),
            deltas(&["好"]),
        ]);
        let answered = answer_first(&rig.gate, &rig.sink, Decision::allow(Scope::Once));
        assert_eq!(rig.run("放行").await, StopReason::EndTurn);
        assert!(answered.await.unwrap(), "Write 级调用必须弹窗等人");
        assert_eq!(
            tool.calls(),
            vec![r#"{"n":1}"#.to_string()],
            "放行后恰好执行一次，参数原样抵达工具"
        );
        assert_eq!(
            tool_results_of(rig.session.history()),
            vec![("r0c0".into(), false, "执行了".into())]
        );

        let (mut rig2, tool2) = Rig::with_counted(vec![
            tool_round(0, &[("counted", r#"{"n":2}"#)]),
            deltas(&["被拒了"]),
        ]);
        let answered = answer_first(&rig2.gate, &rig2.sink, Decision::deny("不许"));
        assert_eq!(rig2.run("拒绝").await, StopReason::EndTurn);
        assert!(answered.await.unwrap());
        assert!(
            tool2.calls().is_empty(),
            "被拒的工具仍然执行了：权限门只是摆设，实际 {:?}",
            tool2.calls()
        );
        assert_eq!(
            tool_results_of(rig2.session.history()),
            vec![("r0c0".into(), true, "用户拒绝执行：不许".into())]
        );
        assert_every_request_paired(&rig2);
    }

    // ---- 路径 4：注册表报错 ----

    #[tokio::test]
    async fn an_unknown_tool_reports_only_a_tool_result_event() {
        let mut rig = Rig::new(vec![
            tool_round(0, &[("nope", "{}")]),
            deltas(&["没这个工具，我自己来"]),
        ]);
        // 未知工具按 Exec（最严一档，registry.rs:137），所以这里必须有人应答。
        let answered = answer_first(&rig.gate, &rig.sink, Decision::allow(Scope::Once));
        assert_eq!(rig.run("调个不存在的").await, StopReason::EndTurn);
        assert!(answered.await.unwrap());

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "permission_request",
                "permission_resolved",
                "tool_result",
                "turn_start",
                "text_delta",
                "done"
            ]
        );
        let got = rig.sink.tool_results();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1, "工具失败必须由 tool_result 的 is_error 承载");
        assert!(
            got[0].2.contains("工具 nope 未注册"),
            "报错文本必须可归因，实际 {:?}",
            got[0].2
        );
        // R64：同一个事实不许再由 Error{at:Tool} 报第二遍（前端会长出两行）。
        assert_eq!(rig.sink.errors(), Vec::<(ErrorWhere, String, bool)>::new());
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_every_request_paired(&rig);
    }

    // ---- 路径 5：用量与空响应 ----

    #[tokio::test]
    async fn a_usage_delta_reaches_the_ui_as_a_usage_event() {
        // R60：`AssistantAcc::apply` 会**静默丢弃** Usage 里的 token
        // （assistant_acc.rs:37-39 的文档就是这句），循环是唯一能把它们抢救回来的地方。
        // 这条测试是「统计静默为 0」的唯一防线。
        let mut rig = Rig::new(vec![vec![
            ModelDelta::Text("答".into()),
            ModelDelta::Usage {
                input_tokens: 1234,
                output_tokens: 56,
            },
        ]]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        assert_eq!(rig.sink.usage(), vec![(1234, 56)]);
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "text_delta", "usage", "done"],
            "usage 事件只能来自流里的那个 Usage delta"
        );
        assert_paired("用量请求", &rig.reqs()[0]);
    }

    // ---- 路径 5b：usage → 校准（切片 5 的第一根线） ----

    #[tokio::test]
    async fn real_usage_calibrates_the_estimator_in_the_right_direction() {
        // 端点真实回报 = 粗估的一半 ⇒ 系数必须是 真实 ÷ 粗估 = 0.5（context.rs:65 的语义）。
        // 接反成 粗估 ÷ 真实 = 2.0 不会报错，但估算会一直朝危险方向偏 —— 这条测试存在的唯一理由。
        // 分母不许由测试自己造（R110：不给 ContextBuilder 加测试专用 setter），
        // 而是用真 `ContextConfig` + 真历史，把 `built.est_input_tokens` **读出来**当分母。
        let hist = vec![Message::user(user("hi"))];
        let est = ContextBuilder::new(ContextConfig::default())
            .build(&hist)
            .est_input_tokens;
        assert!(
            est > 1,
            "夹具前提：粗估必须非零，否则除不出 0.5（est={est}）"
        );
        // 会话把「工作方式」段并进系统提示一起算预算；分母要按同一条路重算。
        let denominator = ContextBuilder::new(ContextConfig::default())
            .build_with_extra(&hist, TOOL_POLICY)
            .est_input_tokens;
        let mut rig = Rig::new(vec![vec![
            ModelDelta::Text("好".into()),
            ModelDelta::Usage {
                input_tokens: denominator / 2,
                output_tokens: 7,
            },
        ]]);
        // `run()` 在 build 之前 push 的正是这条 `user("hi")` ⇒ 请求那一刻的历史就是 `hist`，
        // 所以 `request()` 当时看到的 est 就是上面这个 `est`。
        assert_eq!(rig.session.deps.context.build(&hist).est_input_tokens, est);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        // 校准只能从 ContextBuilder 侧观测：给它**同一份**历史再 build 一次，看 est 落下来了。
        let again = rig.session.deps.context.build(&hist).est_input_tokens;
        assert!(
            again < est,
            "真实比粗估省 ⇒ 之后的估算必须跟着降：{again} vs {est}"
        );
        assert!(
            (again as i64 - (est / 2) as i64).abs() <= 1,
            "系数必须是真实÷粗估（接反成 2.0 时这条红）：{again} vs 期望 {}",
            est / 2
        );
    }

    #[tokio::test]
    async fn a_zero_or_absurd_usage_leaves_the_budget_gate_functioning() {
        // 两种坏法都要守：(a) input_tokens=0、(b) input_tokens=u32::MAX。
        // 可观测面只有 `build()` 的返回（裁定：**不新增** `calibration_for_test`），
        // 所以断言一律写成行为式。判据一句话：**坏 usage 之后预算闸还在工作**。
        let hist = vec![Message::user(user("hi"))];
        let before = ContextBuilder::new(ContextConfig::default())
            .build(&hist)
            .est_input_tokens;
        assert!(before > 0);
        for actual in [0u32, u32::MAX] {
            let mut rig = Rig::new(vec![vec![
                ModelDelta::Text("好".into()),
                ModelDelta::Usage {
                    input_tokens: actual,
                    output_tokens: 3,
                },
            ]]);
            assert_eq!(rig.run("hi").await, StopReason::EndTurn);
            let after = rig.session.deps.context.build(&hist).est_input_tokens;
            assert!(
                after > 0,
                "估算被清零 ⇒ 预算永不生效，正是 context.rs:69 禁止的后果"
            );
            assert!(
                after <= before * 3 && after >= before / 5 || after == before,
                "系数必须落在 0.2..=3.0（actual={actual}：{after} vs {before}）"
            );
            // 带内断言单独**抓不到**「session 侧那道闸被删」：0.0 是有限因子，
            // `set_calibration` 会把它 clamp 成 floor 0.2，数值仍贴着 0.2 的下界。
            // 所以这里各自钉死精确值：0 报告整个被忽略，爆表报告被 clamp 到天花板。
            if actual == 0 {
                assert_eq!(
                    after, before,
                    "input_tokens=0 不许动系数（actual={actual}）"
                );
            } else {
                assert_eq!(after, before * 3, "u32::MAX 必须被 clamp 到 3.0 天花板");
            }
        }
    }

    #[tokio::test]
    async fn an_empty_model_response_pushes_nothing_and_reports_the_loop() {
        // R58：`AssistantAcc::finish()` 对空流交回一条**零块**消息，
        // 而零块的 assistant 消息发不出去（assistant_acc.rs:75-79）：
        // 既不进 history，也不许被下一轮当成输入。
        let mut rig = Rig::new(vec![vec![]]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        assert_eq!(
            rig.session.history().len(),
            1,
            "只留那条 user 消息：空 assistant 轮不进 history，也不补工具结果：{:?}",
            rig.session.history()
        );
        assert_eq!(
            rig.sink.errors(),
            vec![(ErrorWhere::Loop, "模型返回空响应".into(), false)]
        );
        assert_eq!(rig.sink.kinds(), ["turn_start", "error", "done"]);
        assert_eq!(rig.sink.done(), Some(StopReason::EndTurn));
        assert_eq!(rig.reqs().len(), 1, "空响应不该再问第二遍");
    }

    // ---- 路径 6：模型错误 ----

    #[tokio::test]
    async fn a_model_error_ends_the_turn_with_a_fatal_model_error() {
        // R61：Plan A 里模型错误就收尾，所以 fatal 是 true。
        // plan 的测试名叫 non_fatal 而 body 发的是 fatal:true —— 按 body 走，名字跟着改。
        let mut rig = Rig::failing();
        assert_eq!(rig.run("hi").await, StopReason::Error);
        assert_eq!(
            rig.sink.errors(),
            vec![(
                ErrorWhere::Model,
                "模型返回错误: 429 rate limited".into(),
                true
            )]
        );
        assert_eq!(rig.sink.kinds(), ["turn_start", "error", "done"]);
        assert_eq!(rig.sink.done(), Some(StopReason::Error));
        assert_eq!(
            rig.session.history().len(),
            1,
            "错误在第一个 delta 之前就到了：没有任何内容已经流给 UI，所以不提交空轮"
        );
        assert_eq!(rig.reqs().len(), 1);
    }

    /// G05 夹具：**第一次请求**报 `error_message`，之后按序吐 `then` 各轮；
    /// `then` 耗尽后继续报同一个错（测「重试仍溢出」）。FailingModel 的文案
    /// 固定是 429，认不出「溢出」，测不了重试分支。
    struct OverflowModel {
        error_message: String,
        then: Mutex<Vec<Vec<ModelDelta>>>,
        errored: AtomicBool,
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for OverflowModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
        {
            self.recorder.push(req);
            let already_errored = self.errored.swap(true, Ordering::SeqCst);
            let mut q = self.then.lock().unwrap();
            if already_errored && !q.is_empty() {
                let next = q.remove(0);
                return Box::pin(futures::stream::iter(next.into_iter().map(Ok)));
            }
            Box::pin(futures::stream::iter(std::iter::once(Err(
                ModelError::Api(self.error_message.clone()),
            ))))
        }
    }

    /// G05 的正向半边：第一次溢出 → 非致命的 Loop 说明 + 收缩上下文 + 重试一次，
    /// 重试的回答正常收尾。
    #[tokio::test]
    async fn an_overflow_error_recovers_by_shrinking_and_retrying_once() {
        let model = Arc::new(OverflowModel {
            error_message: "400 This model's maximum context length is exceeded".into(),
            then: Mutex::new(vec![vec![ModelDelta::Text("修好了".into())]]),
            errored: AtomicBool::new(false),
            recorder: Recorder::default(),
        });
        let recorder = model.recorder.clone();
        let mut rig = Rig::assembled(model, ContextConfig::default(), recorder, vec![]);
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        assert!(rig.sink.texts().contains("修好了"), "重试的回答要送达");
        assert_eq!(
            rig.sink.errors(),
            vec![(
                ErrorWhere::Loop,
                "模型报上下文超限，已收缩上下文并重试：模型返回错误: 400 This model's maximum context length is exceeded".into(),
                false
            )],
            "只有一条非致命的收缩说明，没有 fatal"
        );
        assert_eq!(rig.reqs().len(), 2, "溢出后恰好重试一次");
    }

    /// G05 的另一半：重试仍然溢出 = 走既有失败出口，不能无限收缩。
    #[tokio::test]
    async fn a_second_overflow_still_ends_with_a_fatal_error() {
        let model = Arc::new(OverflowModel {
            error_message: "400 maximum context length exceeded".into(),
            then: Mutex::new(vec![]),
            errored: AtomicBool::new(false),
            recorder: Recorder::default(),
        });
        let recorder = model.recorder.clone();
        let mut rig = Rig::assembled(model, ContextConfig::default(), recorder, vec![]);
        assert_eq!(rig.run("hi").await, StopReason::Error);
        let errors = rig.sink.errors();
        assert_eq!(errors.len(), 2, "一次收缩说明 + 一次 fatal：{errors:?}");
        assert!(!errors[0].2, "第一次是非致命说明");
        assert!(errors[1].2, "第二次是 fatal");
        assert_eq!(rig.sink.done(), Some(StopReason::Error));
        assert_eq!(rig.reqs().len(), 2, "重试恰好一次，之后收尾");
    }

    /// G07 正向半边：完全相同的「工具名 + 入参」连续第 3 次不再执行，
    /// 回一条可读的错误结果；前两次照常执行。
    #[tokio::test]
    async fn a_third_identical_call_is_interrupted_with_an_error_result() {
        let (rig, counter) = Rig::with_counted(vec![
            tool_round(1, &[("counted", r#"{"k":1}"#)]),
            tool_round(2, &[("counted", r#"{"k":1}"#)]),
            tool_round(3, &[("counted", r#"{"k":1}"#)]),
            deltas(&["收到"]),
        ]);
        let mut rig = rig;
        let _answers = answer_all(&rig.gate, &rig.sink, 2);
        assert_eq!(rig.run("别打转").await, StopReason::EndTurn);
        assert_eq!(counter.calls().len(), 2, "第 3 次不许再执行");
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 3);
        assert!(!results[0].1 && !results[1].1, "前两次正常执行");
        assert!(results[2].1, "第 3 次以错误结果回给模型：{:?}", results[2]);
        assert!(
            results[2].2.contains("连续第 3 次"),
            "错误结果要指出这是重复调用：{:?}",
            results[2]
        );
        // R57 配对：history 结尾停在工具结果上。
        assert_every_request_paired(&rig);
    }

    /// G07 另一半：出现**任何不同**的调用即复位 —— 两次相同 + 一次不同 + 再来原来的，
    /// 最后这次是第 1 次而不是第 3 次，照常执行。
    #[tokio::test]
    async fn a_changed_argument_resets_the_doom_counter() {
        let (rig, counter) = Rig::with_counted(vec![
            tool_round(1, &[("counted", r#"{"k":1}"#)]),
            tool_round(2, &[("counted", r#"{"k":1}"#)]),
            tool_round(3, &[("counted", r#"{"k":2}"#)]),
            tool_round(4, &[("counted", r#"{"k":1}"#)]),
            deltas(&["收到"]),
        ]);
        let mut rig = rig;
        let _answers = answer_all(&rig.gate, &rig.sink, 4);
        assert_eq!(rig.run("换着来").await, StopReason::EndTurn);
        assert_eq!(counter.calls().len(), 4, "全部执行，没有误伤");
        let results = rig.sink.tool_results();
        assert!(results.iter().all(|(_, err, _)| !err), "{results:?}");
    }

    // ───────────────────────── G15：plan 模式（只读研究 agent） ─────────────────────────

    /// plan 模式下 Write 级调用被拦：工具**未执行**、不走权限门（无 PermissionRequest），
    /// 错误结果写明「Plan 模式」与退出方式；调用卡照发（UI 看得见这次尝试），
    /// 模型收到错误结果后正常收尾。R57 配对不破。
    #[tokio::test]
    async fn plan_mode_rejects_write_tools_without_touching_the_gate() {
        let (rig, counter) = Rig::with_counted_config(
            SessionConfig {
                plan_mode: true,
                ..Default::default()
            },
            vec![
                tool_round(1, &[("counted", r#"{"k":1}"#)]),
                deltas(&["方案如下"]),
            ],
        );
        let mut rig = rig;
        // 不 spawn 应答任务：plan 模式必须拦在权限门**之前**，一旦这里挂死等弹窗
        // 就说明拦截没生效（run 卡在 gate 上，测试超时红）。
        assert_eq!(rig.run("改一下").await, StopReason::EndTurn);
        assert!(
            counter.calls().is_empty(),
            "plan 模式下 Write 工具绝不能真的执行：{:?}",
            counter.calls()
        );
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1);
        assert!(results[0].1, "以错误结果回给模型：{:?}", results[0]);
        assert!(
            results[0].2.contains("Plan 模式") && results[0].2.contains("退出 plan 模式"),
            "错误结果要点名模式与出路：{:?}",
            results[0]
        );
        assert!(
            rig.sink.permission_ids().is_empty(),
            "不走权限门：不该有任何 PermissionRequest，实际 {:?}",
            rig.sink.permission_ids()
        );
        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "tool_result",
                "turn_start", // 第二轮请求：模型拿到拒绝结果后改出文本方案
                "text_delta",
                "done"
            ],
            "调用卡照发（tool_call 在 tool_result 之前），模型回复正常送达：{:?}",
            rig.sink.kinds()
        );
        assert_every_request_paired(&rig);
    }

    /// plan 模式的另一半：Read 级照常执行（只读研究的「看和读」半边），零弹窗语义不变。
    #[tokio::test]
    async fn plan_mode_still_runs_read_tools() {
        let mut rig = Rig::with_config(
            SessionConfig {
                plan_mode: true,
                ..Default::default()
            },
            vec![
                tool_round(1, &[("echo", r#"{"text":"调研记录"}"#)]),
                deltas(&["读完"]),
            ],
        );
        assert_eq!(rig.run("看看").await, StopReason::EndTurn);
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1);
        assert!(!results[0].1, "Read 级照常执行：{:?}", results[0]);
        assert!(
            results[0].2.contains("调研记录"),
            "echo 的输出原样回给模型：{:?}",
            results[0]
        );
        assert!(
            rig.sink.permission_ids().is_empty(),
            "Read 零弹窗语义不变：{:?}",
            rig.sink.permission_ids()
        );
        assert_every_request_paired(&rig);
    }

    // ───────────────────────── G17：AGENTS.md 注入 ─────────────────────────

    /// 根下有 AGENTS.md ⇒ 系统提示含「项目规则」标题与文件内容。
    #[tokio::test]
    async fn agents_md_in_the_root_reaches_the_system_prompt() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "永远用中文回复").unwrap();
        let mut rig = Rig::with_roots(
            vec![deltas(&["好的"])],
            vec![root.path().to_path_buf()],
            vec![],
        );
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        let system = rig.reqs()[0].messages[0].text_of();
        assert!(system.contains(AGENTS_MD_HEADING), "要有段落标题：{system}");
        assert!(
            system.contains("永远用中文回复"),
            "文件内容要进系统提示：{system}"
        );
    }

    /// 两个根各有 AGENTS.md ⇒ 两个都进且带 `### <根目录名>` 小标题，顺序随 roots。
    #[tokio::test]
    async fn two_roots_each_get_a_named_subsection_in_root_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("AGENTS.md"), "A 的规矩").unwrap();
        std::fs::write(b.path().join("AGENTS.md"), "B 的规矩").unwrap();
        let mut rig = Rig::with_roots(
            vec![deltas(&["好的"])],
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            vec![],
        );
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        let system = rig.reqs()[0].messages[0].text_of();
        let a_name = a.path().file_name().unwrap().to_string_lossy();
        let b_name = b.path().file_name().unwrap().to_string_lossy();
        assert!(
            system.contains(&format!("### {a_name}")) && system.contains(&format!("### {b_name}")),
            "多根要用根目录名做小标题分隔：{system}"
        );
        assert!(
            system.contains("A 的规矩") && system.contains("B 的规矩"),
            "两份内容都要进：{system}"
        );
        let a_pos = system.find("A 的规矩").unwrap();
        let b_pos = system.find("B 的规矩").unwrap();
        assert!(
            a_pos < b_pos,
            "roots 的顺序就是拼接顺序（可预期的归属）：{system}"
        );
    }

    /// 没有 AGENTS.md ⇒ 系统提示与功能不存在时**逐字节一致**（两个 rig 对比）。
    /// roots 本身不许把任何别的东西带进 tail —— 带进来这里就红了。
    #[tokio::test]
    async fn an_empty_root_keeps_the_system_prompt_byte_identical() {
        let root = tempfile::tempdir().unwrap(); // 空目录：roots 非空但没有 AGENTS.md
        let mut with_root = Rig::with_roots(
            vec![deltas(&["好的"])],
            vec![root.path().to_path_buf()],
            vec![],
        );
        let mut without = Rig::with_roots(vec![deltas(&["好的"])], vec![], vec![]);
        assert_eq!(with_root.run("hi").await, StopReason::EndTurn);
        assert_eq!(without.run("hi").await, StopReason::EndTurn);
        assert_eq!(
            with_root.reqs()[0].messages[0].text_of(),
            without.reqs()[0].messages[0].text_of(),
            "缺 AGENTS.md 时系统提示必须与功能不存在时逐字节一致"
        );
    }

    /// 内容 > 8 KiB ⇒ 截断标记出现、8 KiB 之后的字节不进提示、截断前的内容保留。
    #[tokio::test]
    async fn an_oversized_agents_md_is_truncated_with_a_marker() {
        let root = tempfile::tempdir().unwrap();
        let mut big = "x".repeat(AGENTS_MD_MAX_BYTES + 100);
        big.push_str("结尾的独有标记");
        std::fs::write(root.path().join("AGENTS.md"), &big).unwrap();
        let mut rig = Rig::with_roots(
            vec![deltas(&["好的"])],
            vec![root.path().to_path_buf()],
            vec![],
        );
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        let system = rig.reqs()[0].messages[0].text_of();
        assert!(
            system.contains(AGENTS_MD_TRUNCATED_NOTE),
            "截断标记要出现：{system}"
        );
        assert!(
            !system.contains("结尾的独有标记"),
            "8 KiB 之后的字节不许进提示：{system}"
        );
        assert!(
            system.contains(&"x".repeat(64)),
            "截断前的内容要保留：{system}"
        );
    }

    /// 截断点落在多字节字符中间时按字符边界退让：不许 panic、不许出现半个字符。
    #[test]
    fn truncation_respects_utf8_char_boundaries() {
        // 8191 处是边界，8192 落在第一个「中」（3 字节）的中间。
        let mut s = "x".repeat(AGENTS_MD_MAX_BYTES - 1);
        s.push_str("中中中");
        let cut = floor_char_boundary(&s, AGENTS_MD_MAX_BYTES);
        assert_eq!(cut, AGENTS_MD_MAX_BYTES - 1, "退让到最近的字符边界");
        let mut t = s.clone();
        t.truncate(cut); // 非边界会 panic，这一行本身就是断言的一半
        t.push_str(AGENTS_MD_TRUNCATED_NOTE);
        assert!(t.ends_with(AGENTS_MD_TRUNCATED_NOTE), "{t:?}");
    }

    /// 每轮现读：第二轮请求同样带上；**改文件后下一轮请求反映新内容**（不落库 ——
    /// 系统提示只活在请求上，历史里没有它）。
    #[tokio::test]
    async fn agents_md_is_reread_each_round_and_reflects_edits() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "第一版规矩").unwrap();
        let mut rig = Rig::with_roots(
            vec![
                tool_round(1, &[("echo", r#"{"text":"记下了"}"#)]),
                deltas(&["好"]),
                deltas(&["继续办"]), // 第二次 run 的那一轮
            ],
            vec![root.path().to_path_buf()],
            vec![],
        );
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2, "工具轮 + 收尾轮");
        assert!(
            reqs[0].messages[0].text_of().contains("第一版规矩"),
            "第一轮要带上：{}",
            reqs[0].messages[0].text_of()
        );
        assert!(
            reqs[1].messages[0].text_of().contains("第一版规矩"),
            "第二轮同样现读带上（不是装配时读一次缓存住）：{}",
            reqs[1].messages[0].text_of()
        );

        // 改文件 → 同一会话再 run 一次：新文本出现、旧文本消失。
        std::fs::write(root.path().join("AGENTS.md"), "第二版规矩").unwrap();
        assert_eq!(rig.run("继续").await, StopReason::EndTurn);
        let reqs = rig.reqs();
        assert!(reqs.len() >= 3, "第二次 run 至少多一条请求：{}", reqs.len());
        let system = reqs[2].messages[0].text_of();
        assert!(
            system.contains("第二版规矩"),
            "改文件后下一轮要反映新内容：{system}"
        );
        assert!(!system.contains("第一版规矩"), "旧内容不许残留：{system}");
    }

    // ───────────────────────── G16：task 子代理 ─────────────────────────

    /// LoopRunner 起独立子会话：父第 1 轮 task 调用（Write 弹窗放行这一次）→ 子请求
    /// 返回「子结果」→ 父第 2 轮文本收尾。断言：父的 ToolResult 带回子结果；
    /// 子会话历史隔离（子请求带 prompt、不带父的 user 原话）；全程恰好 3 次模型请求
    /// （父子共用同一只脚本模型，请求序 = 父1、子、父2）。
    #[tokio::test]
    async fn the_task_tool_runs_a_child_session_and_brings_back_its_text() {
        let model = ScriptedModel::new(vec![
            tool_round(1, &[("task", r#"{"prompt":"去查 X 的资料"}"#)]),
            deltas(&["子结果"]),
            deltas(&["收尾"]),
        ]);
        let recorder = model.recorder.clone();
        let mut rig = Rig::assembled(
            model,
            ContextConfig::default(),
            recorder,
            vec![Arc::new(crate::tools::task::TaskTool)],
        );
        // task 是 Write 档：后台应答放行这 1 次弹窗（子会话只跑一轮文本，没有别的弹窗）。
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("帮我派个子代理查资料").await, StopReason::EndTurn);

        // 恰好 3 次模型请求：父(工具轮) + 子(子代理那轮) + 父(收尾轮)。
        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 3, "父子共用同一只脚本模型：{}", reqs.len());

        // 父的 ToolResult 把子代理的文本带了回来。
        let results = rig.sink.tool_results();
        assert!(
            results
                .iter()
                .any(|(_, err, text)| !err && text.contains("子结果")),
            "ToolResult 要带回子结果：{results:?}"
        );

        // 历史隔离：子请求（第 2 次）带着 prompt，但没有任何一条消息含父的 user 原话。
        let child_joined: String = reqs[1].messages.iter().map(|m| m.text_of()).collect();
        assert!(
            child_joined.contains("去查 X 的资料"),
            "子请求要带上 prompt：{child_joined}"
        );
        assert!(
            !child_joined.contains("帮我派个子代理查资料"),
            "子会话是独立 history，不许泄漏父会话的消息：{child_joined}"
        );

        // R57 配对（子会话自己的请求也在同一只 recorder 里，一起钉）。
        assert_every_request_paired(&rig);
    }

    // ───────────────────────── G22：受限子代理（task 的 agent 档案） ─────────────────────────

    /// G22 夹具：注册表恰好含 task + 两把「白名单外」的工具（write_file / run_command）
    /// + 三把「白名单内」的（read_file / grep / glob），roots 指向真目录。
    /// 「含/不含」的断言必须有真工具在场才有意义 —— 工具没注册的话「不含」是空真。
    fn restricted_agent_rig(cfg: SessionConfig, rounds: Vec<Vec<ModelDelta>>, root: &Path) -> Rig {
        let model = ScriptedModel::new(rounds);
        let recorder = model.recorder.clone();
        let sink = Arc::new(VecSink::default());
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(crate::tools::task::TaskTool));
        registry.register(Arc::new(crate::tools::read_file::ReadFileTool));
        registry.register(Arc::new(crate::tools::write_file::WriteFileTool));
        registry.register(Arc::new(crate::tools::grep::GrepTool));
        registry.register(Arc::new(crate::tools::glob::GlobTool));
        registry.register(Arc::new(crate::tools::shell::ShellTool::new(Arc::new(
            std::sync::RwLock::new(crate::tools::shell::ShellCatalog::from_parts(vec![], "")),
        ))));
        let gate = Arc::new(PermissionGate::new().with_sink(sink.clone()));
        let session = AgentSession::new(SessionDeps {
            model,
            registry,
            gate: gate.clone(),
            sink: sink.clone(),
            context: ContextBuilder::new(ContextConfig::default()),
            temperature: None,
        })
        .with_roots(vec![root.to_path_buf()])
        .with_config(cfg);
        Rig {
            session,
            sink,
            gate,
            recorder,
        }
    }

    /// 点名 `agent:"explore"` ⇒ 子会话换档案身份：system 含档案人设「只读调研员」，
    /// 声明表含白名单内的 read_file、不含白名单外的 write_file / run_command；
    /// 父子历史隔离与从前一致。三次请求 = 父1（task 调用）+ 子1 + 父2（收尾）。
    #[tokio::test]
    async fn a_task_agent_profile_swaps_the_child_session_identity() {
        let root = tempfile::tempdir().unwrap();
        let mut rig = restricted_agent_rig(
            SessionConfig {
                agents: vec![AgentProfile {
                    name: "explore".into(),
                    prompt: Some("你是只读调研员".into()),
                    tools: Some(vec!["read_file".into(), "grep".into(), "glob".into()]),
                }],
                ..Default::default()
            },
            vec![
                tool_round(
                    1,
                    &[("task", r#"{"prompt":"去查 X","agent":"explore"}"#)],
                ),
                deltas(&["子结果"]),
                deltas(&["收尾"]),
            ],
            root.path(),
        );
        // task 是 Write 档：后台应答放行这 1 次弹窗（子会话只跑一轮文本，没有别的调用）。
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("派 explore 去查").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 3, "父1 + 子1 + 父2：{}", reqs.len());

        // 子会话（第 2 次请求）的人设来自档案 prompt —— 整段替换生效。
        let child = &reqs[1];
        assert_eq!(child.messages[0].role, Role::System);
        assert!(
            child.messages[0].text_of().contains("你是只读调研员"),
            "子会话 system 要含档案人设：{}",
            child.messages[0].text_of()
        );
        // 工具白名单：名单内的在、名单外的不在（注册表里确实有那两把，见夹具注释）。
        let names: Vec<&str> = child.tools.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&"read_file"),
            "白名单内的 read_file 要在子会话声明表里：{names:?}"
        );
        for banned in ["write_file", "run_command"] {
            assert!(
                !names.contains(&banned),
                "{banned} 不许进受限子会话的声明表：{names:?}"
            );
        }

        // 历史隔离不变：子请求带 prompt、不带父的 user 原话。
        let child_joined: String = child.messages.iter().map(|m| m.text_of()).collect();
        assert!(
            child_joined.contains("去查 X"),
            "子请求要带上 prompt：{child_joined}"
        );
        assert!(
            !child_joined.contains("派 explore 去查"),
            "档案不改 task 的独立 history 语义：{child_joined}"
        );
        assert_every_request_paired(&rig);
    }

    /// 点名一个不存在的档案 ⇒ 父的 ToolResult 是错误（含「没有这个 agent 档案」），
    /// 子会话压根没被创建：总请求数 2，全是父的（fail-closed，绝不静默回落成全工具子代理）。
    #[tokio::test]
    async fn an_unknown_agent_name_fails_without_creating_a_child_session() {
        let root = tempfile::tempdir().unwrap();
        let mut rig = restricted_agent_rig(
            SessionConfig {
                agents: vec![AgentProfile {
                    name: "explore".into(),
                    prompt: Some("你是只读调研员".into()),
                    tools: Some(vec!["read_file".into()]),
                }],
                ..Default::default()
            },
            vec![
                tool_round(
                    1,
                    &[("task", r#"{"prompt":"查一下","agent":"不存在"}"#)],
                ),
                deltas(&["收尾"]),
            ],
            root.path(),
        );
        // task 本身仍要过 Write 弹窗（拦截发生在档案查找**之后**的工具执行里）。
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("派一个不存在的档案").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2, "父两次、无子请求：{}", reqs.len());
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].1 && results[0].2.contains("没有这个 agent 档案"),
            "ToolResult 要是错误且点名档案缺失：{:?}",
            results[0]
        );
        assert_every_request_paired(&rig);
    }

    /// 回归：不点名 agent ⇒ 与现状一致 —— 子会话不吃任何白名单，
    /// 声明表是注册表经 roots 过滤后的全量（write_file / run_command 都在）。
    #[tokio::test]
    async fn a_task_call_without_an_agent_keeps_the_full_toolset_child() {
        let root = tempfile::tempdir().unwrap();
        let mut rig = restricted_agent_rig(
            SessionConfig::default(),
            vec![
                tool_round(1, &[("task", r#"{"prompt":"去查 X"}"#)]),
                deltas(&["子结果"]),
                deltas(&["收尾"]),
            ],
            root.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("派子代理去查").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 3, "父1 + 子1 + 父2：{}", reqs.len());
        let names: Vec<&str> = reqs[1].tools.iter().map(|d| d.name.as_str()).collect();
        for expected in ["read_file", "write_file", "run_command"] {
            assert!(
                names.contains(&expected),
                "无 agent 名 = 现状全量，{expected} 该在子会话声明表里：{names:?}"
            );
        }
        assert!(
            !reqs[1].messages[0].text_of().contains("你是只读调研员"),
            "无档案时子会话沿用默认人设：{}",
            reqs[1].messages[0].text_of()
        );
        assert_every_request_paired(&rig);
    }

    // ───────────────────────── G18：诊断反馈 ─────────────────────────

    /// G18 夹具：只注册 write_file 的 rig；输出目录指向 out_dir（写工具唯一允许的
    /// 写入面）；SessionConfig 原样注入（diagnostics_command 在 cfg 里）。
    fn diagnostics_rig(cfg: SessionConfig, rounds: Vec<Vec<ModelDelta>>, out_dir: &Path) -> Rig {
        let model = ScriptedModel::new(rounds);
        let recorder = model.recorder.clone();
        // roots 与 write_root（= 会话输出目录）都指向 out_dir：写工具先过
        // `resolve_within_roots`（空表全拒）、再过 `ensure_within_write_root`，两个
        // 闸门必须同时放行同一个目录。
        let (session, sink, gate) = parts(
            model,
            ContextConfig::default(),
            None,
            vec![Arc::new(crate::tools::write_file::WriteFileTool)],
            vec![out_dir.to_path_buf()],
        );
        Rig {
            session: session
                .with_config(cfg)
                .with_output_dir(Some(out_dir.to_path_buf())),
            sink,
            gate,
            recorder,
        }
    }

    /// 跨平台守卫与 shell.rs 同款：脚本用例只在 unix 下跑（需要 shebang + 可执行位）。
    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Windows 镜像夹具（与 unix 版同形状；Windows CI 上验证）：run_diagnostics 是
    /// **直接 spawn**（按 whitespace 切 program+args，不经 shell），Windows 上最贴近
    /// 的可执行形状是 .cmd 文件（std 的 Command 对 .bat/.cmd 走 cmd.exe 转发）——
    /// 诊断反馈的逻辑跨平台，脚本形状平台各异（shebang+x 位 ↔ .cmd）。
    #[cfg(windows)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// 诊断脚本输出一行 ⇒ write_file 成功落盘后历史里出现【自动诊断】合成 user 轮
    /// （位置：tool 结果之后、模型下一轮之前），且下一次请求的 messages 带上该文本。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_diagnostics_command_appends_a_synthetic_user_turn() {
        let out = tempfile::tempdir().unwrap();
        let script = write_script(out.path(), "diag.sh", "printf 'error: missing semicolon'");
        let file = out.path().join("note.txt");
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let mut rig = diagnostics_rig(
            SessionConfig {
                diagnostics_command: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            vec![tool_round(1, &[("write_file", args.as_str())]), deltas(&["收到"])],
            out.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("写一下").await, StopReason::EndTurn);

        // 历史形状：user → assistant(调用) → tool(结果) → user(【自动诊断】) → assistant。
        let hist = rig.session.history();
        assert_eq!(hist.len(), 5, "合成 user 轮恰好多出一条：{hist:?}");
        assert_eq!(hist[3].role, Role::User);
        let diag_text = hist[3].text_of();
        assert!(
            diag_text.contains("【自动诊断】") && diag_text.contains("missing semicolon"),
            "诊断轮要带标记与脚本输出：{diag_text}"
        );

        // 下一次请求的 messages 里模型读得到这段反馈。
        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2);
        let joined: String = reqs[1].messages.iter().map(|m| m.text_of()).collect();
        assert!(
            joined.contains("【自动诊断】") && joined.contains("missing semicolon"),
            "第二次请求要带上诊断反馈：{joined}"
        );
        // R57 配对不受合成 user 轮影响（它在全部 tool 结果之后）。
        assert_every_request_paired(&rig);
    }

    /// 诊断命令成功但**输出为空** ⇒ 不追加任何消息（没有可说的就不编造）。
    #[cfg(unix)]
    #[tokio::test]
    async fn an_empty_diagnostics_output_appends_nothing() {
        let out = tempfile::tempdir().unwrap();
        let script = write_script(out.path(), "diag.sh", "exit 0");
        let file = out.path().join("note.txt");
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let mut rig = diagnostics_rig(
            SessionConfig {
                diagnostics_command: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            vec![tool_round(1, &[("write_file", args.as_str())]), deltas(&["收到"])],
            out.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("写一下").await, StopReason::EndTurn);
        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            4,
            "user → assistant(调用) → tool(结果) → assistant，没有诊断轮：{hist:?}"
        );
        assert!(
            hist.iter().all(|m| !m.text_of().contains("【自动诊断】")),
            "空输出不许出现诊断轮"
        );
    }

    /// 没有配置命令 ⇒ 与现状**逐字节一致**：两个同配置 rig 跑同一脚本轮，
    /// 历史完全相等，且都没有诊断轮。
    #[tokio::test]
    async fn without_a_command_the_history_is_byte_identical() {
        let out = tempfile::tempdir().unwrap();
        let file = out.path().join("note.txt");
        // 预建目标文件：两个 rig 共用同一目录，若不预建，第一次写入回「新建」、
        // 第二次回「覆盖」，结果文案不同会让逐字节比较失真。预建后两次都是覆盖。
        std::fs::write(&file, "hello").unwrap();
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let rounds = vec![
            tool_round(1, &[("write_file", args.as_str())]),
            deltas(&["收到"]),
        ];
        let mut rig_a = diagnostics_rig(SessionConfig::default(), rounds.clone(), out.path());
        let mut rig_b = diagnostics_rig(SessionConfig::default(), rounds, out.path());
        let _answers_a = answer_all(&rig_a.gate, &rig_a.sink, 1);
        let _answers_b = answer_all(&rig_b.gate, &rig_b.sink, 1);
        assert_eq!(rig_a.run("写一下").await, StopReason::EndTurn);
        assert_eq!(rig_b.run("写一下").await, StopReason::EndTurn);
        assert_eq!(
            rig_a.session.history(),
            rig_b.session.history(),
            "无命令时历史必须与现状逐字节一致"
        );
        assert!(
            rig_a
                .session
                .history()
                .iter()
                .all(|m| !m.text_of().contains("【自动诊断】")),
            "无命令不许出现诊断轮"
        );
    }

    /// 命令失败（非零退出）且**无输出** ⇒ 不追加 —— 反馈只来自真实输出，不编造。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_silent_command_appends_nothing() {
        let out = tempfile::tempdir().unwrap();
        let script = write_script(out.path(), "diag.sh", "exit 3");
        let file = out.path().join("note.txt");
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let mut rig = diagnostics_rig(
            SessionConfig {
                diagnostics_command: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            vec![tool_round(1, &[("write_file", args.as_str())]), deltas(&["收到"])],
            out.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("写一下").await, StopReason::EndTurn);
        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            4,
            "失败且无输出 = 没有诊断轮：{hist:?}"
        );
        assert!(
            hist.iter().all(|m| !m.text_of().contains("【自动诊断】")),
            "不许把退出码编造成诊断文本"
        );
    }

    // ───────────────── G18 的 Windows 镜像（逻辑跨平台；Windows CI 上验证） ─────────────────
    // 脚本形状换成 .cmd（见 write_script 的 Windows 版）；断言与 unix 版逐条对应。

    /// Windows 镜像：诊断脚本输出一行 ⇒【自动诊断】合成 user 轮照常落历史与请求。
    #[cfg(windows)]
    #[tokio::test]
    async fn a_diagnostics_command_appends_a_synthetic_user_turn() {
        let out = tempfile::tempdir().unwrap();
        let script = write_script(out.path(), "diag.cmd", "@echo error: missing semicolon");
        let file = out.path().join("note.txt");
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let mut rig = diagnostics_rig(
            SessionConfig {
                diagnostics_command: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            vec![tool_round(1, &[("write_file", args.as_str())]), deltas(&["收到"])],
            out.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("写一下").await, StopReason::EndTurn);

        let hist = rig.session.history();
        assert_eq!(hist.len(), 5, "合成 user 轮恰好多出一条：{hist:?}");
        assert_eq!(hist[3].role, Role::User);
        let diag_text = hist[3].text_of();
        assert!(
            diag_text.contains("【自动诊断】") && diag_text.contains("missing semicolon"),
            "诊断轮要带标记与脚本输出：{diag_text}"
        );
        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2);
        let joined: String = reqs[1].messages.iter().map(|m| m.text_of()).collect();
        assert!(
            joined.contains("【自动诊断】") && joined.contains("missing semicolon"),
            "第二次请求要带上诊断反馈：{joined}"
        );
        assert_every_request_paired(&rig);
    }

    /// Windows 镜像：诊断命令成功但输出为空 ⇒ 不追加任何消息。
    #[cfg(windows)]
    #[tokio::test]
    async fn an_empty_diagnostics_output_appends_nothing() {
        let out = tempfile::tempdir().unwrap();
        let script = write_script(out.path(), "diag.cmd", "@exit /b 0");
        let file = out.path().join("note.txt");
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let mut rig = diagnostics_rig(
            SessionConfig {
                diagnostics_command: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            vec![tool_round(1, &[("write_file", args.as_str())]), deltas(&["收到"])],
            out.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("写一下").await, StopReason::EndTurn);
        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            4,
            "user → assistant(调用) → tool(结果) → assistant，没有诊断轮：{hist:?}"
        );
        assert!(
            hist.iter().all(|m| !m.text_of().contains("【自动诊断】")),
            "空输出不许出现诊断轮"
        );
    }

    /// Windows 镜像：命令失败（非零退出）且无输出 ⇒ 不追加。
    #[cfg(windows)]
    #[tokio::test]
    async fn a_failing_silent_command_appends_nothing() {
        let out = tempfile::tempdir().unwrap();
        let script = write_script(out.path(), "diag.cmd", "@exit /b 3");
        let file = out.path().join("note.txt");
        let args = json!({ "path": file, "content": "hello" }).to_string();
        let mut rig = diagnostics_rig(
            SessionConfig {
                diagnostics_command: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            vec![tool_round(1, &[("write_file", args.as_str())]), deltas(&["收到"])],
            out.path(),
        );
        let _answers = answer_all(&rig.gate, &rig.sink, 1);
        assert_eq!(rig.run("写一下").await, StopReason::EndTurn);
        let hist = rig.session.history();
        assert_eq!(hist.len(), 4, "失败且无输出 = 没有诊断轮：{hist:?}");
        assert!(
            hist.iter().all(|m| !m.text_of().contains("【自动诊断】")),
            "不许把退出码编造成诊断文本"
        );
    }

    // ───────────────────────── G19：agent 注册表（工具白名单） ─────────────────────────

    /// allowed_tools 限 [echo] ⇒ 白名单外的 counted（Write）调用被拒：不执行、
    /// 不走权限门（无 PermissionRequest —— 故意不 spawn 应答任务，拦截失效就挂死变红）；
    /// 错误结果点名 Agent 与白名单；声明表只留 echo（第一层过滤）；
    /// 白名单内的 echo 照常执行。R57 配对不破。
    #[tokio::test]
    async fn an_agent_allowlist_rejects_tools_outside_the_whitelist() {
        let (rig, counter) = Rig::with_counted_config(
            SessionConfig {
                allowed_tools: Some(vec!["echo".into()]),
                active_agent_name: Some("研究员".into()),
                ..Default::default()
            },
            vec![
                tool_round(1, &[("counted", r#"{"k":1}"#)]),
                tool_round(2, &[("echo", r#"{"text":"白名单内"}"#)]),
                deltas(&["收到"]),
            ],
        );
        let mut rig = rig;
        assert_eq!(rig.run("做点事").await, StopReason::EndTurn);

        assert!(
            counter.calls().is_empty(),
            "白名单外的 Write 工具绝不能执行：{:?}",
            counter.calls()
        );
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 2);
        assert!(results[0].1, "counted 以错误结果回给模型：{:?}", results[0]);
        assert!(
            results[0].2.contains("不允许使用工具")
                && results[0].2.contains("研究员")
                && results[0].2.contains("白名单: echo"),
            "拒绝理由要点名 Agent 与白名单：{:?}",
            results[0]
        );
        assert!(!results[1].1, "白名单内的 echo 照常执行：{:?}", results[1]);
        assert!(
            rig.sink.permission_ids().is_empty(),
            "白名单拦截不走权限门，echo 零弹窗：{:?}",
            rig.sink.permission_ids()
        );
        // 声明表（第一层）只剩 echo。
        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 3);
        for req in &reqs {
            let names: Vec<&str> = req.tools.iter().map(|d| d.name.as_str()).collect();
            assert_eq!(names, vec!["echo"], "声明表只留白名单内的工具：{names:?}");
        }
        assert_every_request_paired(&rig);
    }

    /// allowed_tools = None ⇒ 与现状逐字节一致：两个同配置 rig 跑同一脚本，
    /// counted 照常弹窗放行并执行，历史完全相等、没有任何白名单拒绝文案。
    #[tokio::test]
    async fn without_an_allowlist_the_history_is_byte_identical() {
        let rounds = vec![
            tool_round(1, &[("counted", r#"{"k":1}"#)]),
            deltas(&["收到"]),
        ];
        let (mut rig_a, counter_a) =
            Rig::with_counted_config(SessionConfig::default(), rounds.clone());
        let (mut rig_b, counter_b) = Rig::with_counted_config(SessionConfig::default(), rounds);
        let _answers_a = answer_all(&rig_a.gate, &rig_a.sink, 1);
        let _answers_b = answer_all(&rig_b.gate, &rig_b.sink, 1);
        assert_eq!(rig_a.run("写一下").await, StopReason::EndTurn);
        assert_eq!(rig_b.run("写一下").await, StopReason::EndTurn);
        assert_eq!(
            rig_a.session.history(),
            rig_b.session.history(),
            "无白名单时历史必须与现状逐字节一致"
        );
        assert_eq!(counter_a.calls().len(), 1, "counted 照常执行");
        assert_eq!(counter_b.calls().len(), 1);
        assert!(
            rig_a
                .session
                .history()
                .iter()
                .all(|m| !m.text_of().contains("不允许使用工具")),
            "无白名单不许出现拒绝文案"
        );
    }

    /// 溢出识别只认端点文案里的这几个词；普通 4xx/网络错误不许误判成溢出。
    #[test]
    fn overflow_classification_matches_the_common_endpoint_wording() {
        assert!(is_context_overflow(
            "400 This model's maximum context length is exceeded"
        ));
        assert!(is_context_overflow("model requires a larger context window"));
        assert!(is_context_overflow("请求失败：上下文长度超限"));
        assert!(!is_context_overflow("模型返回错误: 401 Unauthorized invalid_api_key"));
        assert!(!is_context_overflow("无法连接模型端点 https://x.test: dns error"));
    }

    /// R56 的另外半张脸：取消出口被 `interrupting_mid_stream_...` 钉住了，报错出口用的是
    /// 同一个 `commit_partial`，但「流里已经攒下一个完整 tool_call，然后断了」这条组合此前
    /// 只有代码路径、没有断言。它造的形态正是下一轮会 400 的那个：history 结尾停在
    /// `assistant(ToolCall)`。而本轮终止之后，这条 history 是要由 Task 11 落库、由 Task 13
    /// 带着它开下一轮会话的 —— 所以「报错时的 history」和「取消时的 history」一样得配对。
    #[tokio::test]
    async fn a_model_error_after_an_accumulated_tool_call_backfills_it_too() {
        let mut rig = Rig::failing_after(tool_round(0, &[("echo", r#"{"text":"x"}"#)]));
        assert_eq!(rig.run("hi").await, StopReason::Error);

        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            3,
            "user + assistant(带调用) + 补出来的结果，一条都不能少：{hist:?}"
        );
        assert_eq!(
            tool_results_of(hist),
            vec![("r0c0".to_string(), true, MODEL_FAILURE_NOTE.to_string())],
            "补的结果必须归因到「流中断」，而不是「工具坏了」——后者会诱使模型重试同一个调用"
        );
        assert_eq!(
            hist.last().map(|m| &m.role),
            Some(&Role::User),
            "结尾必须是工具结果，不能停在带 tool_call 的 assistant 上"
        );
        // 工具一次都没执行：调用只到了累加器，还没走到注册表。
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "error", "done"],
            "报错轮里不许出现 tool_call / tool_result / permission_request：{kinds:?}",
            kinds = rig.sink.kinds()
        );
        assert_eq!(rig.reqs().len(), 1, "报错就收尾，不许再问一遍");

        // 这段 history 会被落库并用于下一个会话轮次，所以它必须自己能上线。
        let next = ChatRequest {
            messages: hist.to_vec(),
            tools: Vec::new(),
            temperature: None,
        };
        assert_paired("报错之后待续聊的历史", &next);
    }

    // ---- 触顶 ----

    #[tokio::test]
    async fn max_turns_cap_is_reached_not_silently_stopped() {
        let rounds: Vec<Vec<ModelDelta>> = (0..30usize)
            .map(|n| tool_round(n, &[("echo", r#"{"text":"loop"}"#)]))
            .collect();
        let mut rig = Rig::new(rounds);
        let cap = SessionConfig::default().max_turns;
        assert_eq!(
            rig.run("无限循环").await,
            StopReason::MaxTurns,
            "默认 max_turns={cap}，给了 30 轮必须触顶"
        );
        assert_eq!(
            rig.sink.done(),
            Some(StopReason::MaxTurns),
            "触顶必须显式告知，不可静默"
        );
        // G06：触顶后要多请求一次做总结（工具声明收回）。夹具的第 26 轮仍是一个
        // tool_call，它会被回错误结果而不执行，随后按 MaxTurns 收尾。
        assert_eq!(
            rig.reqs().len(),
            cap as usize + 1,
            "触顶轮 + 一次强制总结请求"
        );
        assert!(
            rig.reqs().last().unwrap().tools.is_empty(),
            "总结请求必须收回全部工具声明"
        );
        assert_eq!(rig.sink.kinds().first().unwrap(), "turn_start");
        assert_eq!(rig.sink.kinds().last().unwrap(), "done");
        assert_eq!(
            rig.sink
                .kinds()
                .iter()
                .filter(|k| *k == "turn_start")
                .count(),
            cap as usize + 1,
            "总结轮也发 turn_start"
        );

        // 触顶时 history 也必须停在成对状态：最后一轮的调用已经有结果，
        // 总结轮里模型凭空喊出的调用也有回执（错误结果），R57 配对不破。
        assert_every_request_paired(&rig);
        let hist = rig.session.history();
        assert_eq!(
            hist.last().map(|m| &m.role),
            Some(&Role::User),
            "最后一条必须是工具结果，不能是带 tool_call 的 assistant"
        );
        assert_eq!(
            tool_results_of(hist).len(),
            cap as usize + 1,
            "cap 个正常结果 + 总结轮 1 个错误回执"
        );
    }

    /// G06 的正向半边：总结轮交出的是文本 ⇒ 按正常收尾（EndTurn），工作不白做。
    #[tokio::test]
    async fn the_step_cap_summary_turn_ends_normally_when_the_model_text_back() {
        let mut rounds: Vec<Vec<ModelDelta>> = (0..25usize)
            .map(|n| tool_round(n, &[("echo", r#"{"text":"loop"}"#)]))
            .collect();
        rounds.push(vec![ModelDelta::Text("总结收尾".into())]);
        let mut rig = Rig::new(rounds);
        assert_eq!(rig.run("跑满").await, StopReason::EndTurn);
        assert_eq!(rig.reqs().len(), 26);
        assert!(rig.reqs().last().unwrap().tools.is_empty());
        assert!(rig.sink.texts().contains("总结收尾"));
    }

    // ---- 取消 ----

    #[tokio::test]
    async fn cancel_before_run_returns_interrupted_and_emits_only_done() {
        // R62：plan 那条测试在 run() **之前**取消，所以它测的其实只是这条路径；
        // 真正的「打断流式」由下面那条负责。这里断言精确到「只有一个 done」。
        let mut rig = Rig::new(vec![deltas(&["不会用到"])]);
        rig.session.cancel_handle().cancel();
        assert_eq!(rig.run("hi").await, StopReason::Interrupted);
        assert_eq!(rig.sink.kinds(), ["done"]);
        assert_eq!(rig.sink.done(), Some(StopReason::Interrupted));
        assert!(
            rig.session.history().is_empty(),
            "取消发生在开始之前：这一轮压根没发生，连 user 消息都不该留下：{:?}",
            rig.session.history()
        );
        assert!(rig.reqs().is_empty(), "取消在开始之前：一次都不该问模型");
    }

    #[tokio::test]
    async fn a_cancelled_session_refuses_a_second_turn() {
        // R63：CancellationToken 是一次性的，`run()` 里**不许**复位
        // （那会让前端手里那个 handle 静默失效，比原来的竞态更糟）。
        // 这里钉住合同：取消后的会话对象只多一个 done，别的都不动，
        // 想继续聊就带着 history() 换一个新会话（Task 13 每轮新建，SessionDeps 全是 Arc）。
        let mut rig = Rig::new(vec![deltas(&["第一轮"])]);
        assert_eq!(rig.run("一").await, StopReason::EndTurn);
        let before = rig.sink.kinds();
        let hist_before = rig.session.history().len();

        rig.session.cancel_handle().cancel();
        assert_eq!(rig.run("二").await, StopReason::Interrupted);
        assert_eq!(&rig.sink.kinds()[before.len()..], ["done"]);
        assert_eq!(
            rig.session.history().len(),
            hist_before,
            "第二次 run 不许留下任何消息"
        );
        assert_eq!(rig.reqs().len(), 1, "取消后不再问模型");
    }

    #[tokio::test]
    async fn interrupting_mid_stream_keeps_the_partial_reply_and_backfills_the_open_tool_call() {
        // R56 + R62：打断流式时，已经流给 UI 的半句话必须进 history
        // （否则 Task 11 重放后文字凭空消失：UI 有、库里没有＝两套真相）；
        // 而那条尚未执行的 tool_call 必须补一条结果——光提交 assistant(ToolCall)
        // 会让下一轮撞上 mid-session 400，正是 Task 8 在窗口边界修掉的那个 bug 从这里复发。
        let (mut rig, ready) = Rig::hanging(vec![
            ModelDelta::Text("前半句".into()),
            ModelDelta::Text("还没说完".into()),
            ModelDelta::ToolCallStart {
                index: 0,
                id: Some("hangc0".into()),
                name: Some("echo".into()),
            },
            ModelDelta::ToolCallArgs {
                index: 0,
                fragment: r#"{"text":"永远不会执行"}"#.into(),
            },
        ]);
        let cancel = rig.session.cancel_handle();
        let canceller = tokio::spawn(async move {
            for _ in 0..500 {
                if ready.load(Ordering::SeqCst) {
                    cancel.cancel();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("模型的头几个 delta 一直没抵达累加器，打断点没能建立");
        });

        assert_eq!(rig.run("说一半").await, StopReason::Interrupted);
        canceller.await.unwrap();

        // UI 确实看到了半句话——这才叫打断流式，而不是「压根没开始」。
        assert_eq!(rig.sink.texts(), "前半句还没说完");
        assert_eq!(
            rig.sink.kinds(),
            ["turn_start", "text_delta", "text_delta", "done"],
            "工具没执行，所以不该有 tool_call / tool_result / permission_request 事件"
        );
        assert_eq!(rig.sink.done(), Some(StopReason::Interrupted));

        let hist = rig.session.history();
        assert_eq!(
            hist.len(),
            3,
            "user + 半句 assistant + 补出来的结果：{hist:?}"
        );
        assert_eq!(
            hist[1].text_of(),
            "前半句还没说完",
            "半句话不能只活在 UI 里"
        );
        assert_eq!(
            tool_results_of(hist),
            vec![("hangc0".into(), true, "本轮被打断，工具未执行".into())]
        );
        assert_eq!(
            hist.last().map(|m| &m.role),
            Some(&Role::User),
            "history 结尾必须是工具结果，不能是带 tool_call 的 assistant"
        );
        assert_eq!(rig.reqs().len(), 1);

        // R56 的动机要真的验一遍：接手这段 history 的下一个会话，
        // 发给模型的第一个请求依然配对（Task 13 每轮新建会话 + restore）。
        let carried = rig.session.history().to_vec();
        let mut cont = Rig::new(vec![deltas(&["接着说"])]);
        cont.session.restore(carried);
        assert_eq!(cont.run("继续").await, StopReason::EndTurn);
        assert_eq!(cont.sink.kinds(), ["turn_start", "text_delta", "done"]);
        assert_eq!(cont.reqs().len(), 1);
        assert_paired("打断后接手的请求", &cont.reqs()[0]);
    }

    #[tokio::test]
    async fn cancel_while_a_popup_is_pending_backfills_the_call_and_says_interrupted() {
        // 弹窗挂着时按停止，收场的是**权限门自己**（R43：session 的 `select!` 打不断一个
        // 已经在 await 的 `check`）。这条组合路径此前只测到门的边界（`gate.rs` 自己的测试
        // 看不到 history），而它正是 R56 那个悬空 `assistant(ToolCall)` 的第三个入口：
        // 调用已经完整攒下、门回了 Deny、循环继续走到轮首才发现被打断。
        // 这里同时钉两件事：①配对不变式在 Ask 入口也成立；②模型读到的归属是「中断」而不是
        // 「用户拒绝执行」——用户按的是停止，把它写成否决会让模型以为诉求被禁而不再重试。
        let mut rig = Rig::new(vec![
            tool_round(0, &[("guarded", r#"{"text":"x"}"#)]),
            deltas(&["不该走到这里"]),
        ]);
        let sink = rig.sink.clone();
        let cancel = rig.session.cancel_handle();
        let canceller = tokio::spawn(async move {
            // 等到弹窗真的抵达 sink 才取消：否则测的是 `check` 里「已取消就不弹」那支。
            wait_requests(&sink, 1).await;
            cancel.cancel();
        });

        assert_eq!(rig.run("写个文件").await, StopReason::Interrupted);
        canceller.await.unwrap();

        assert_eq!(
            rig.sink.kinds(),
            [
                "turn_start",
                "tool_call",
                "permission_request",
                "permission_resolved",
                "tool_result",
                "done"
            ],
            "弹窗要被收回、调用要补结果，且中断之后不许再问第二轮"
        );
        assert_eq!(rig.sink.done(), Some(StopReason::Interrupted));
        assert_eq!(
            tool_results_of(rig.session.history()),
            vec![(
                "r0c0".to_string(),
                true,
                "本轮已中断：等待授权时收到中断".to_string()
            )],
            "理由取自权限门的取消臂，归属由 session 改成中断"
        );
        assert_eq!(rig.reqs().len(), 1, "取消之后不再问模型");
        assert_eq!(
            rig.session.history().last().map(|m| &m.role),
            Some(&Role::User),
            "history 结尾必须是工具结果，不能是带 tool_call 的 assistant"
        );

        // 和中断流式那条一样，动机也要验一遍：接手这段历史的会话第一个请求仍然配对。
        let carried = rig.session.history().to_vec();
        let mut cont = Rig::new(vec![deltas(&["接着说"])]);
        cont.session.restore(carried);
        assert_eq!(cont.run("继续").await, StopReason::EndTurn);
        assert_paired("弹窗中断后接手的请求", &cont.reqs()[0]);
    }

    // ---- R57 的后一半：配对不变量必须扛过滑窗切口 ----

    /// 60 条成对历史 + `window_messages: 4`：窗口起点算出来是第 57 条，正是 `r28c0` 的
    /// **工具结果**（成对历史里偶数下标是调用、奇数下标是结果），所以切口一定落在孤儿结果上，
    /// 必须靠 Task 8 的 advance 前进一格才合法。窗口边界和循环边界是同一个 400 的两个入口，
    /// 所以这条不变量得在**模型实际看到的请求**上再验一遍。
    #[tokio::test]
    async fn a_windowed_long_history_still_pairs_every_tool_result_with_its_call() {
        let mut carried: Vec<Message> = Vec::new();
        for n in 0..30usize {
            carried.push(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: format!("r{n}c0"),
                    name: "echo".into(),
                    input: json!({"text": "ok"}),
                }],
            });
            carried.push(Message::tool(
                &format!("r{n}c0"),
                ToolOutput::text("ok"),
                false,
            ));
        }
        assert_eq!(carried.len(), 60);

        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 100_000,
            window_messages: 4,
            ..Default::default()
        };
        let mut rig = Rig::with_context(
            cfg,
            vec![
                tool_round(30, &[("echo", r#"{"text":"新结果"}"#)]),
                deltas(&["看到了"]),
            ],
        );
        rig.session.restore(carried);
        assert_eq!(rig.run("接着用工具").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2);
        // 精确条数而不是 `<=`：4 = system + advance 之后剩下的 3 条。这个数同时钉住
        // advance 的算术——起点 57 是孤儿结果，被跳过后窗口只剩 3 条历史。
        assert_eq!(
            reqs[0].messages.len(),
            4,
            "窗口 4 条 + system，切口必须真的裁掉 60 条历史并把孤儿结果跳掉，实际 {:?}",
            reqs[0].messages.iter().map(|m| &m.role).collect::<Vec<_>>()
        );
        assert_eq!(
            reqs[0].messages[1].role,
            Role::Assistant,
            "advance 生效的话窗口第一条是 assistant(ToolCall)，不是孤儿工具结果：{:?}",
            reqs[0].messages[1]
        );
        assert_eq!(
            reqs[1].messages.len(),
            4,
            "第二轮同样受窗口约束，实际 {}",
            reqs[1].messages.len()
        );
        // 新那一轮的调用与结果必须都在请求里（窗口不许把刚跑完的这一对裁掉）。
        assert!(
            tool_results_of(&reqs[1].messages)
                .iter()
                .any(|(id, _, text)| id == "r30c0" && text == "新结果"),
            "本轮的工具结果没进第二个请求：模型第二轮是凭空回答，{:?}",
            tool_results_of(&reqs[1].messages)
        );
        for (n, r) in reqs.iter().enumerate() {
            assert_paired(&format!("窗口后的请求 #{n}"), r);
        }
        assert_eq!(
            rig.session.history().len(),
            64,
            "窗口只裁请求，不裁 history：60 条 + user + assistant(调用) + tool(结果) + assistant(收尾)"
        );
    }

    /// `SessionDeps.context` 必须是活的：循环每轮都拿它组装请求，而不是把整个
    /// history 原样发给模型（那会让 Task 8 的窗口与预算在循环里等于没接线）。
    #[tokio::test]
    async fn the_context_window_governs_what_the_model_is_shown() {
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 100_000,
            window_messages: 3,
            ..Default::default()
        };
        let mut rig = Rig::with_context(cfg, vec![deltas(&["只看了最近三条"])]);
        let carried: Vec<Message> = (0..20)
            .map(|i| Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: format!("第 {i} 条"),
                }],
            })
            .collect();
        rig.session.restore(carried);
        assert_eq!(rig.run("最新一条").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].messages.len(), 4, "system + 窗口里的 3 条");
        assert_eq!(reqs[0].messages[1].text_of(), "第 18 条");
        assert_eq!(reqs[0].messages.last().unwrap().text_of(), "最新一条");
        assert_eq!(
            rig.session.history().len(),
            22,
            "20 条恢复的 + 这一轮的 user + assistant：history 不被窗口裁"
        );
    }

    /// `defer_done` 的唯一合同：会话自己不发明 `Done`，把最终事件留给调用方
    /// （生产路径用它把「落库成功」排在「界面显示完成」之前）。
    #[tokio::test]
    async fn deferred_done_is_left_to_the_caller() {
        let mut rig = Rig::new(vec![deltas(&["hi"])]);
        rig.session.defer_done();
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);
        assert_eq!(rig.sink.done(), None, "defer 模式下会话自己不许发 Done");
        assert!(rig.sink.texts().contains("hi"), "文字流不受影响");
    }

    /// 取消必须打断**正在执行**的工具：不 select 的话这一轮要等工具自己回来，
    /// 「中断 1 秒内生效」的验收线不成立。
    #[tokio::test]
    async fn cancelling_during_a_tool_stops_without_waiting_for_it() {
        let mut rig = Rig::with_roots(
            vec![tool_round(1, &[("slow", "{}")]), deltas(&["done"])],
            vec![],
            vec![Arc::new(SlowTool)],
        );
        let cancel = rig.session.cancel_handle();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let reason = tokio::time::timeout(std::time::Duration::from_secs(2), rig.run("跑慢工具"))
            .await
            .expect("取消必须打断正在执行的工具，而不是等它 30 秒");
        assert_eq!(reason, StopReason::Interrupted);
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1, "被取消的那次调用必须有结果：{results:?}");
        assert!(results[0].1, "被取消的调用必须标成错误：{results:?}");
        assert!(
            results[0].2.contains("中断"),
            "预览必须说明是被中断的：{results:?}"
        );
    }

    // ---- T39：请求构造的最后一段线 ----

    /// 前置事实 3 说温度那条管线早就通了，缺的只有本文件那句 `temperature: None`。
    /// `openai_compat.rs` 的 `request_declares_tools_and_temperature` 钉的是「`Some(t)` 会上线」，
    /// 这里钉的是**我们真的把设置里那个数交出去**。三个取值一起跑（含 `None`），
    /// 所以「在 `request()` 里硬写一个常数」这种实现会当场红。
    #[test]
    fn the_session_temperature_reaches_the_chat_request() {
        for want in [None, Some(0.3), Some(1.25)] {
            let (mut session, _sink, _gate) = parts(
                ScriptedModel::new(vec![]),
                ContextConfig::default(),
                want,
                vec![],
                Vec::new(),
            );
            assert_eq!(
                session.request().temperature,
                want,
                "{want:?} 必须原样进请求"
            );
        }
    }

    // ---- T26：roots 必须真的送到执行点 ----

    /// brief Step 4。这条测试存在的理由写在断言消息里：`with_roots` 在本任务之前**从未被调用**
    /// ⇒ `ToolCtx.roots` 恒空 ⇒ T25 交付的 `read_file` 在生产里对任何路径都 fail-closed。
    /// 循环这一侧的线只有在这里能量到（生产那一侧由 `ipc.rs` 的
    /// `run_turn_hands_the_effective_roots_not_the_persisted_list` 钉，装配点不在本文件）。
    #[tokio::test]
    async fn run_turn_passes_configured_roots_into_tool_ctx() {
        // 夹具是**真目录**（R277 的口径：不注入假的 `exists`），外加一条真的不存在的路径。
        // 那条幽灵路径是故意的，而且**必须照样抵达**：会话是传送带，不是第二把尺子 ——
        // 「今天哪几条解得开」归 `settings.rs::effective_roots` 一处，由 `ipc.rs::run_turn`
        // 每轮在交给会话那一刻算一次（R284 把它从读写两侧搬到了使用点）。
        // 谁哪天在循环里顺手 filter 一遍，这条会红，而那正是「同一判据长出两个强度版本」的开头。
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ghost = a.path().join("never-created");
        assert!(!ghost.exists(), "夹具前提：这条路径必须真的不存在");
        let roots = vec![
            a.path().to_path_buf(),
            ghost.clone(),
            b.path().to_path_buf(),
        ];

        let tool = Arc::new(AssertRootsTool::default());
        let mut rig = Rig::with_roots(
            vec![
                tool_round(0, &[("assert_roots", "{}")]),
                deltas(&["收到了"]),
            ],
            roots.clone(),
            vec![tool.clone()],
        );
        assert_eq!(rig.run("列一下 roots").await, StopReason::EndTurn);

        let seen = tool.roots_seen();
        // 先确认工具**真的被跑过**：一次都没执行的话，下面那条断言是空的真（绿色但什么都没说）。
        assert_eq!(
            seen.len(),
            1,
            "假工具一次都没被执行 ⇒ 这条测试什么都没量到，事件序列 {:?}",
            rig.sink.kinds()
        );
        assert_eq!(
            seen[0], roots,
            "roots 没传到执行点 ⇒ read_file 永远 fail-closed，这是本任务存在的唯一理由"
        );
    }

    /// 同一条线的**默认那一头**：不配 roots 时抵达执行点的必须还是空表，
    /// 于是 `read_file` 当场拒读一条真实存在的文件。
    /// 这条不是多余的对称：把「空表就补个默认目录」写成实现的人，会让上面那条照绿、这条红。
    /// 这里注册的是**真的 `ReadFileTool`**（不是假工具），所以拒的是那个用户真正会看到的错误，
    /// 而不是某个替身自己编的说法。
    #[tokio::test]
    async fn an_unconfigured_session_reaches_the_tool_ctx_with_no_roots_and_read_file_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "真的想读我".as_bytes()).unwrap();
        // 参数走 `json!` 而不是手拼字符串：路径里的反斜杠要按 JSON 规则转义，
        // 手抄那份转义在别的机器上就是一份随夹具漂移的暗雷（`settings.rs` 同一条理由）。
        let args = json!({ "path": file }).to_string();

        let mut rig = Rig::with_roots(
            vec![
                tool_round(0, &[("read_file", args.as_str())]),
                deltas(&["被拒了"]),
            ],
            Vec::new(),
            vec![Arc::new(crate::tools::read_file::ReadFileTool)],
        );
        assert_eq!(rig.run("读那个文件").await, StopReason::EndTurn);

        let results = rig.sink.tool_results();
        assert_eq!(
            results.len(),
            1,
            "工具没跑 ⇒ 什么都没量到：{:?}",
            rig.sink.kinds()
        );
        let (id, is_error, preview) = &results[0];
        assert_eq!(id, "r0c0");
        assert!(
            *is_error,
            "空 roots 下 read_file 必须**报错而不是读到内容**（读到就是越权）：{preview}"
        );
        assert!(
            preview.contains("越界"),
            "拒绝要归因到 containment 门（「越界」），而不是碰巧的 IO 失败：{preview}"
        );
    }

    // ---- R286：声明表按可用性过滤（只省 token，不是安全措施） ----

    /// 危害是具体的而不是抽象的：`Settings.roots` 默认空（R276 明令不许替用户决定），
    #[tokio::test]
    async fn filesystem_tools_are_not_offered_when_no_root_is_configured() {
        let mut rig = Rig::with_only(
            vec![deltas(&["还没有可读的目录"])],
            Vec::new(),
            production_tools(),
        );
        assert_eq!(rig.run("读一下我的笔记").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(
            reqs.len(),
            1,
            "夹具只安排了一次请求，取证点就得恰好记下一条；0 条说明这条什么都没量到"
        );
        // 断**名单**而不是长度：长度对、名字错（比如把 `fetch_url` 藏了留下 `read_file`）
        // 是同一个 bug 的另一半，只比长度就放得过。
        assert_eq!(
            declared_names(&reqs),
            vec![vec!["fetch_url".to_string()]],
            "空 roots 这一轮的声明表里不该有任何文件工具（R286）"
        );
    }

    /// 少了这一条，「藏工具」的实现可以退化成「永远藏住」，上面那条照样绿 ——
    /// 而用户配好目录之后会发现工具从此不再出现，症状和没修之前一样难查。
    #[tokio::test]
    async fn filesystem_tools_are_offered_once_a_root_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut rig = Rig::with_only(
            vec![deltas(&["读到啦"])],
            vec![dir.path().to_path_buf()],
            production_tools(),
        );
        assert_eq!(rig.run("读一下我的笔记").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(
            declared_names(&reqs),
            vec![vec![
                "edit_file".to_string(),
                "fetch_url".to_string(),
                "read_file".to_string(),
                "write_file".to_string(),
            ]],
            "有一条可用 root ⇒ 读与写工具都必须出现，否则它们对用户永久隐形"
        );
    }

    // ---- G20：question 的答案回到对话 ----

    /// 完整 run()：第 1 轮模型调 question（Read 档 ⇒ 零弹窗，不需要权限应答任务），
    /// 测试从 rig 的 sink 取 Question 事件、经共享实例 `shared().resolve` 应答，
    /// execute 的输出「用户选择了：选项二」在第 2 轮请求的 messages 里回到模型。
    /// 注册表只装 question 一把（with_only）：这条量的是问答闭环，不是工具名单。
    #[tokio::test]
    async fn a_question_is_answered_through_the_shared_instance_and_reaches_the_next_request() {
        let mut rig = Rig::with_only(
            vec![
                tool_round(
                    1,
                    &[(
                        "question",
                        r#"{"question":"用哪个方案？","options":["选项一","选项二"]}"#,
                    )],
                ),
                deltas(&["好，按选项二来"]),
            ],
            Vec::new(),
            vec![crate::tools::question::QuestionTool::shared()],
        );
        // 后台应答：等 sink 里出现 Question 事件（此刻 execute 已挂起），把选择送回去。
        let answerer = tokio::spawn({
            let sink = rig.sink.clone();
            async move {
                for _ in 0..500 {
                    if let Some(id) = sink.events().into_iter().find_map(|e| match e {
                        AgentEvent::Question { id, .. } => Some(id),
                        _ => None,
                    }) {
                        crate::tools::question::QuestionTool::shared()
                            .resolve(&id, "选项二".to_string());
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                panic!("等不到 Question 事件");
            }
        });

        assert_eq!(rig.run("帮我拿个主意").await, StopReason::EndTurn);
        answerer.await.unwrap();

        // Read 档：提问全程不经过权限门（gate 的 sink 与事件线是同一份，取证无死角）。
        assert!(
            rig.sink.permission_ids().is_empty(),
            "question 是 Read 档，不该有任何 PermissionRequest：{:?}",
            rig.sink.permission_ids()
        );
        // 问答闭环的模型侧证据：第 2 次请求的 messages 带着工具结果（答案）。
        // `text_of` 只收 Text 块，ToolResult 块要单独拆（gapcheck2 同一条经验）。
        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2, "第 1 轮 question + 第 2 轮收尾：{}", reqs.len());
        let results: Vec<String> = reqs[1]
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    output: ToolOutput::Text { text },
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            results.iter().any(|t| t.contains("用户选择了：选项二")),
            "答案要回到第 2 轮请求：{results:?}"
        );
    }

    // ---- 技能包：索引进提示、正文按需读、没技能时隐藏 ----

    #[tokio::test]
    async fn skills_are_indexed_in_the_system_prompt_and_offered_as_a_tool() {
        let skills = skill_fixture("translate", "翻译助手", "中英互译", "先识别语言，再翻译。");
        let mut rig = Rig::with_only(vec![deltas(&["好的"])], Vec::new(), production_tools());
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("你会什么").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 1);
        let system = reqs[0].messages[0].text_of();
        assert!(
            system.contains("- 翻译助手：中英互译"),
            "技能索引必须进系统提示，模型才知道有这个技能：{system}"
        );
        assert!(
            system.contains("use_skill"),
            "索引要教模型用哪把工具读正文：{system}"
        );
        assert_eq!(
            declared_names(&reqs),
            vec![vec!["fetch_url".to_string(), "use_skill".to_string()]],
            "skills 存在时声明表要有 use_skill；空 roots 下 read_file 仍然隐藏"
        );
    }

    #[tokio::test]
    async fn an_empty_skills_dir_adds_no_prompt_section_and_hides_use_skill() {
        let empty = tempfile::tempdir().unwrap();
        let mut rig = Rig::with_only(vec![deltas(&["没有技能"])], Vec::new(), production_tools());
        rig.session = rig
            .session
            .with_skills_dir(Some(empty.path().to_path_buf()));
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        let system = reqs[0].messages[0].text_of();
        assert!(
            system.starts_with(crate::agent::context::SYSTEM_PROMPT),
            "内置提示词仍是开头那一段：{system}"
        );
        assert!(
            system.contains(TOOL_POLICY),
            "没有技能时「工作方式」段照样要在（它不属于技能，属于外壳）：{system}"
        );
        assert!(
            !system.contains("## 技能"),
            "没有技能时不许留一个空标题：{system}"
        );
        assert_eq!(
            declared_names(&reqs),
            vec![vec!["fetch_url".to_string()]],
            "没有技能却把 use_skill 挂在请求里，等于邀请一次注定失败的调用"
        );
    }

    /// 预算估算与 `request()` 必须用同一份**过滤后**的工具声明。
    /// 两者分叉时估算会虚高（校准系数随之偏低），而真正的请求里根本没有那几把工具。
    #[tokio::test]
    async fn the_budgeting_tool_set_matches_the_declared_tool_set() {
        let skills = skill_fixture("translate", "翻译助手", "中英互译", "先识别语言。");
        let dir = tempfile::tempdir().unwrap();
        let roots = vec![dir.path().to_path_buf()];
        let mut rig = Rig::with_only(vec![deltas(&["好的"])], roots.clone(), production_tools());
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("hi").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        let declared = declared_names(&reqs).remove(0);

        // 重建一份同构注册表：`tool_defs_for_turn` 是装配层的输入，不偷 Rig 的内部状态。
        let registry = ToolRegistry::new();
        for t in production_tools() {
            registry.register(t);
        }
        let expected: Vec<String> =
            tool_defs_for_turn(&registry, &roots, Some(skills.path()), &[], None)
                .into_iter()
                .map(|d| d.name)
                .collect();
        assert_eq!(
            declared, expected,
            "预算里算的工具声明必须就是请求里发出去的那一批"
        );
        assert!(declared.contains(&"read_file".to_string()));
        assert!(declared.contains(&"use_skill".to_string()));

        // 停用唯一那份技能 ⇒ use_skill 与预算里的声明同时消失。
        let disabled = tool_defs_for_turn(
            &registry,
            &roots,
            Some(skills.path()),
            &["translate".to_string()],
            None,
        );
        assert!(
            !disabled.iter().any(|d| d.name == "use_skill"),
            "没有启用中的技能时，预算里也不许再算 use_skill"
        );
    }

    #[tokio::test]
    async fn the_tool_policy_survives_a_custom_persona() {
        // 人设是**整段替换**内置提示词的；工具规矩要是跟着人设走，换成人设的当天
        // 本地模型就「有工具也凭记忆答」。这一段必须由会话追加，替换不掉。
        let mut rig = Rig::with_context(
            ContextConfig {
                system_prompt: "你是小宋祺。".into(),
                ..Default::default()
            },
            vec![deltas(&["嗯"])],
        );
        assert_eq!(rig.run("在干嘛").await, StopReason::EndTurn);
        let system = rig.reqs()[0].messages[0].text_of();
        assert!(
            system.starts_with("你是小宋祺。"),
            "人设必须仍在最前（追加不许把它挤到后面）：{system}"
        );
        assert!(
            system.contains(TOOL_POLICY),
            "人设不许把工具与技能的使用规矩一起抹掉：{system}"
        );
    }

    #[tokio::test]
    async fn naming_a_skill_loads_its_body_without_waiting_for_a_tool_call() {
        let skills = skill_fixture_with_keywords(
            "translate",
            "翻译助手",
            "中英互译",
            "润色, 译成英文",
            "先识别语言，再翻译。",
        );
        let mut rig = Rig::with_only(
            vec![deltas(&["This endpoint times out after 30 seconds."])],
            Vec::new(),
            production_tools(),
        );
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        // 面板「用这个技能」填的就是这句话；用户自己打出来也该同样生效。
        assert_eq!(
            rig.run("使用「翻译助手」技能：这个接口默认超时 30 秒。")
                .await,
            StopReason::EndTurn
        );

        let system = rig.reqs()[0].messages[0].text_of();
        assert!(
            system.contains("### 本轮已加载的技能"),
            "点了技能就该把正文带进本轮，而不是赌模型记得调 use_skill：{system}"
        );
        assert!(system.contains("先识别语言，再翻译。"), "{system}");
        assert!(
            system.contains("## 技能"),
            "索引仍然要在（其它技能还得能自动命中）：{system}"
        );
    }

    #[tokio::test]
    async fn a_keyword_hit_loads_the_body() {
        let skills = skill_fixture_with_keywords(
            "translate",
            "翻译助手",
            "中英互译",
            "润色, 译成英文",
            "先识别语言，再翻译。",
        );
        let mut rig = Rig::with_only(vec![deltas(&["改好了"])], Vec::new(), production_tools());
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("帮我把这段润色一下").await, StopReason::EndTurn);

        let system = rig.reqs()[0].messages[0].text_of();
        assert!(
            system.contains("先识别语言，再翻译。"),
            "触发词命中却不加载正文 = 又回到赌模型自觉：{system}"
        );
    }

    #[tokio::test]
    async fn a_second_use_skill_call_for_a_loaded_skill_does_not_reread_the_body() {
        // 自动加载已经生效的那一轮里，模型还是习惯性地喊了一次 use_skill：
        // 正文不该再被交一遍（8B 实测会连读 2–3 次，每遍都是白烧的输入 token）。
        let skills = skill_fixture_with_keywords(
            "translate",
            "翻译助手",
            "中英互译",
            "润色",
            "先识别语言，再翻译。",
        );
        let mut rig = Rig::with_only(
            vec![
                tool_round(0, &[("use_skill", r#"{"name":"翻译助手"}"#)]),
                deltas(&["改好了"]),
            ],
            Vec::new(),
            production_tools(),
        );
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("帮我把这段润色一下").await, StopReason::EndTurn);

        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1, "{:?}", rig.sink.kinds());
        assert!(!results[0].1, "短路不是失败：{}", results[0].2);
        assert!(
            results[0].2.contains("已经在本轮上下文里"),
            "要明说别重复读：{}",
            results[0].2
        );
        assert!(
            !results[0].2.contains("先识别语言"),
            "正文不该再被交一遍：{}",
            results[0].2
        );
        assert_eq!(
            rig.sink.kinds()[..3],
            ["turn_start", "tool_call", "tool_result"],
            "卡片仍然要发给界面（用户看得见这一次调用）"
        );
    }

    #[tokio::test]
    async fn a_truncated_auto_load_still_allows_use_skill_to_read_the_rest() {
        // 自动加载只放前 16 KiB；被截断的那份**不能**进短路表，否则模型照着提示
        // 去调 use_skill 时会被回一句「已经在本轮上下文里」，剩下的正文永远读不到。
        let body = "甲".repeat(6000) + "尾部标记";
        let skills =
            skill_fixture_with_keywords("translate", "翻译助手", "中英互译", "润色", &body);
        let mut rig = Rig::with_only(
            vec![
                tool_round(0, &[("use_skill", r#"{"name":"翻译助手"}"#)]),
                deltas(&["读完了"]),
            ],
            Vec::new(),
            production_tools(),
        );
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("帮我把这段润色一下").await, StopReason::EndTurn);

        let system = rig.reqs()[0].messages[0].text_of();
        assert!(
            system.contains("需要完整步骤请调用 use_skill"),
            "截断提示必须在：{system}"
        );
        let full = tool_results_of(rig.session.history());
        assert_eq!(full.len(), 1, "{:?}", rig.sink.kinds());
        assert!(
            full[0].2.contains("尾部标记"),
            "被截断的技能必须还能把全文读回来：{}",
            full[0].2
        );
        assert!(
            !full[0].2.contains("已经在本轮上下文里"),
            "截断的那份不许被短路：{}",
            full[0].2
        );
        std::fs::remove_dir_all(skills.path()).ok();
    }

    #[tokio::test]
    async fn the_system_prompt_names_the_session_output_directory() {
        let out = tempfile::tempdir().unwrap();
        let mut rig = Rig::with_only(vec![deltas(&["好"])], Vec::new(), Vec::new());
        rig.session = rig.session.with_output_dir(Some(out.path().to_path_buf()));
        assert_eq!(rig.run("你好").await, StopReason::EndTurn);
        let reqs = rig.reqs();
        let system = reqs[0].messages[0].text_of();
        assert!(
            system.contains("本次会话的输出目录是"),
            "必须告诉模型产物该写哪里：{system}"
        );
        assert!(
            system.contains(&out.path().display().to_string()),
            "输出目录要写进提示词：{system}"
        );
    }

    #[tokio::test]
    async fn an_image_tool_result_reaches_the_ui_event() {
        struct ImageTool;
        #[async_trait]
        impl Tool for ImageTool {
            fn meta(&self) -> ToolMeta {
                ToolMeta {
                    name: "picture".into(),
                    description: "回一张图".into(),
                    input_schema: json!({"type": "object"}),
                    risk: Risk::Read,
                }
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::Image {
                    data: "QUJD".into(),
                    mime: ImageMime::Png,
                })
            }
        }

        let out = tempfile::tempdir().unwrap();
        let mut rig = Rig::with_only(
            vec![tool_round(0, &[("picture", "{}")]), deltas(&["看到了"])],
            Vec::new(),
            vec![Arc::new(ImageTool)],
        );
        rig.session = rig.session.with_output_dir(Some(out.path().to_path_buf()));
        assert_eq!(rig.run("来张图").await, StopReason::EndTurn);

        let (images, preview) = rig
            .sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult {
                    images, preview, ..
                } => Some((images, preview)),
                _ => None,
            })
            .expect("这一轮必须有 tool_result 事件");
        assert_eq!(images.len(), 1, "工具回的图片必须进 UI 事件");
        assert_eq!(images[0].data, "QUJD");
        assert_eq!(images[0].mime, ImageMime::Png);
        let path = images[0].path.as_deref().expect("有输出目录时必须缓存图片");
        assert!(
            std::path::Path::new(path)
                .canonicalize()
                .unwrap()
                .starts_with(out.path().join("images").canonicalize().unwrap()),
            "图片必须落在输出目录的 images/：{path}"
        );
        assert!(std::path::Path::new(path).is_file());
        assert!(
            preview.contains("[图片 png]"),
            "文本预览仍要说明结果里有一张图：{preview}"
        );
        assert!(
            !preview.contains("QUJD"),
            "base64 不许塞进文本预览：{preview}"
        );
    }

    #[tokio::test]
    async fn the_ui_image_cap_does_not_change_the_model_retention_cap() {
        struct ManyImages;
        #[async_trait]
        impl Tool for ManyImages {
            fn meta(&self) -> ToolMeta {
                ToolMeta {
                    name: "pictures".into(),
                    description: "回五张图".into(),
                    input_schema: json!({"type": "object"}),
                    risk: Risk::Read,
                }
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::Multi {
                    parts: (0..5)
                        .map(|i| ToolOutput::Image {
                            data: format!("IMG{i}"),
                            mime: ImageMime::Png,
                        })
                        .collect(),
                })
            }
        }

        let mut rig = Rig::with_only(
            vec![tool_round(0, &[("pictures", "{}")]), deltas(&["看到了"])],
            Vec::new(),
            vec![Arc::new(ManyImages)],
        );
        assert_eq!(rig.run("来五张图").await, StopReason::EndTurn);

        let (images, preview) = rig
            .sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult {
                    images, preview, ..
                } => Some((images, preview)),
                _ => None,
            })
            .expect("这一轮必须有 tool_result 事件");
        assert_eq!(images.len(), 4, "卡片只显示前四张");
        assert!(
            preview.contains("另有 1 张图片未在卡片中显示"),
            "省略数量要写在预览里：{preview}"
        );

        let reqs = rig.reqs();
        let model_images = reqs[1]
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert_eq!(
            model_images,
            crate::agent::context::KEEP_IMAGE_RECENT,
            "界面限量不许反向改变模型侧的图片保留策略"
        );
    }

    #[tokio::test]
    async fn an_image_followup_does_not_drop_the_loaded_skill_on_the_next_request() {
        // 工具回图片时 `image_followup` 会补一条 user 轮；如果按「最近一条 user」取用户原话，
        // 第二次请求就会把技能正文丢掉 —— 这里钉住「本轮原话」只认 run() 入口那一句。
        struct ImageTool;
        #[async_trait]
        impl Tool for ImageTool {
            fn meta(&self) -> ToolMeta {
                ToolMeta {
                    name: "picture".into(),
                    description: "回一张图".into(),
                    input_schema: json!({"type": "object"}),
                    risk: Risk::Read,
                }
            }
            async fn execute(
                &self,
                _input: serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::Image {
                    data: "QUJD".into(),
                    mime: ImageMime::Png,
                })
            }
        }

        let skills = skill_fixture_with_keywords(
            "translate",
            "翻译助手",
            "中英互译",
            "润色",
            "先识别语言，再翻译。",
        );
        let mut rig = Rig::with_only(
            vec![tool_round(0, &[("picture", "{}")]), deltas(&["看到了"])],
            Vec::new(),
            vec![Arc::new(ImageTool)],
        );
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("帮我把这段润色一下").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert_eq!(reqs.len(), 2, "{:?}", rig.sink.kinds());
        assert!(
            reqs[0].messages[0]
                .text_of()
                .contains("先识别语言，再翻译。"),
            "第一请求应已加载正文"
        );
        assert!(
            reqs[1].messages[0]
                .text_of()
                .contains("先识别语言，再翻译。"),
            "图片回填之后的第二次请求仍然要带着技能正文：{}",
            reqs[1].messages[0].text_of()
        );
        std::fs::remove_dir_all(skills.path()).ok();
    }

    #[tokio::test]
    async fn an_unrelated_turn_loads_no_skill_body() {
        let skills = skill_fixture_with_keywords(
            "translate",
            "翻译助手",
            "中英互译",
            "润色, 译成英文",
            "先识别语言，再翻译。",
        );
        let mut rig = Rig::with_only(vec![deltas(&["晴"])], Vec::new(), production_tools());
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("今天天气怎么样").await, StopReason::EndTurn);

        let system = rig.reqs()[0].messages[0].text_of();
        assert!(system.contains("- 翻译助手：中英互译"), "{system}");
        assert!(
            !system.contains("### 本轮已加载的技能"),
            "无关的一轮不该把正文拖进上下文：{system}"
        );
    }

    #[tokio::test]
    async fn use_skill_reads_the_body_through_the_tool_ctx_the_session_built() {
        let skills = skill_fixture("translate", "翻译助手", "中英互译", "先识别语言，再翻译。");
        let mut rig = Rig::with_only(
            vec![
                tool_round(0, &[("use_skill", r#"{"name":"翻译助手"}"#)]),
                deltas(&["按技能做了"]),
            ],
            Vec::new(),
            production_tools(),
        );
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()));
        assert_eq!(rig.run("用翻译技能").await, StopReason::EndTurn);

        let results = rig.sink.tool_results();
        assert_eq!(
            results.len(),
            1,
            "工具没跑 ⇒ 这条什么都没量到：{:?}",
            rig.sink.kinds()
        );
        assert!(!results[0].1, "读技能不该失败：{}", results[0].2);
        assert!(
            results[0].2.contains("先识别语言，再翻译。"),
            "技能正文没有经 ToolCtx 抵达工具：{}",
            results[0].2
        );
        assert_eq!(rig.sink.texts(), "按技能做了");
        assert_every_request_paired(&rig);
    }

    #[tokio::test]
    async fn a_disabled_skill_leaves_the_index_and_cannot_be_read() {
        let skills = skill_fixture("translate", "翻译助手", "中英互译", "先识别语言。");
        let mut rig = Rig::with_only(
            vec![
                tool_round(0, &[("use_skill", r#"{"name":"翻译助手"}"#)]),
                deltas(&["没用上"]),
            ],
            Vec::new(),
            production_tools(),
        );
        rig.session = rig
            .session
            .with_skills_dir(Some(skills.path().to_path_buf()))
            .with_skills_disabled(vec!["translate".to_string()]);
        assert_eq!(rig.run("用技能").await, StopReason::EndTurn);

        let reqs = rig.reqs();
        assert!(
            !reqs[0].messages[0].text_of().contains("## 技能"),
            "停用后索引里不许再出现这一段：{}",
            reqs[0].messages[0].text_of()
        );
        for (i, names) in declared_names(&reqs).iter().enumerate() {
            assert!(
                !names.iter().any(|n| n == "use_skill"),
                "第 {i} 次请求的声明表里混进了 use_skill（全停用时该藏起来）：{names:?}"
            );
        }
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1, "{:?}", rig.sink.kinds());
        assert!(results[0].1, "停用的技能直接喊名字也必须拒");
        assert!(
            results[0].2.contains("已停用"),
            "拒绝要能归因到「已停用」：{}",
            results[0].2
        );
    }

    #[tokio::test]
    async fn hiding_use_skill_does_not_weaken_its_failure_without_a_directory() {
        let mut rig = Rig::with_only(
            vec![
                tool_round(0, &[("use_skill", r#"{"name":"翻译助手"}"#)]),
                deltas(&["没读到"]),
            ],
            Vec::new(),
            production_tools(),
        );
        assert_eq!(rig.run("用技能").await, StopReason::EndTurn);

        for (i, names) in declared_names(&rig.reqs()).iter().enumerate() {
            assert!(
                !names.iter().any(|n| n == "use_skill"),
                "第 {i} 次请求的声明表里混进了 use_skill（没有技能目录该藏起来）：{names:?}"
            );
        }
        let results = rig.sink.tool_results();
        assert_eq!(results.len(), 1, "{:?}", rig.sink.kinds());
        assert!(results[0].1, "没有技能目录时必须报错，而不是静默成功");
        assert!(
            results[0].2.contains("技能目录"),
            "失败要可归因：{}",
            results[0].2
        );
    }

    /// R286 的**安全半边**：隐藏只省 token，**不是**授权。
    /// 声明表里没有 `read_file`，而模型凭空喊它（历史里有过、换个大小写猜过、或者干脆幻觉）
    /// 照样要走到 `resolve_within_roots` 并被拒。把这条写出来是为了钉住一个具体的错法：
    /// 谁把「不在声明表里」当成「不可调用」去 registry 里提前返回 NotFound，或者反过来
    /// 因为「已经藏了」就顺手放宽闸门，这两边都得在这里当场红。
    /// 与 `an_unconfigured_session_reaches_the_tool_ctx_with_no_roots_and_read_file_refuses`
    /// 的分工：那条钉的是**没藏**时的拒绝，这条钉的是**藏了之后**的拒绝。
    #[tokio::test]
    async fn hiding_a_tool_does_not_weaken_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret.txt");
        std::fs::write(&file, "不该被读到的正文").unwrap();
        // 参数走 `json!`：路径里的反斜杠要按 JSON 规则转义，手抄那份转义是随机器漂移的暗雷。
        let args = json!({ "path": file }).to_string();

        let mut rig = Rig::with_only(
            vec![
                tool_round(0, &[("read_file", args.as_str())]),
                deltas(&["被拒了"]),
            ],
            Vec::new(),
            production_tools(),
        );
        assert_eq!(rig.run("读那个文件").await, StopReason::EndTurn);

        // 夹具前提：这一轮**真的**把 read_file 藏掉了。不钉这一句，下面的拒绝就只是
        // 那条早就存在的闸门测试的重复，「藏了还拒」这件事根本没被量到。
        let reqs = rig.reqs();
        assert_eq!(
            reqs.len(),
            2,
            "工具循环该跑两次请求，夹具的轮数对不上时这里先红"
        );
        for (i, names) in declared_names(&reqs).iter().enumerate() {
            assert!(
                !names.iter().any(|n| n == "read_file"),
                "第 {i} 次请求的声明表里混进了 read_file（空 roots 该藏起来）：{names:?}"
            );
        }

        let results = rig.sink.tool_results();
        assert_eq!(
            results.len(),
            1,
            "工具没跑 ⇒ 什么都没量到：{:?}",
            rig.sink.kinds()
        );
        let (id, is_error, preview) = &results[0];
        assert_eq!(id, "r0c0");
        assert!(
            is_error,
            "声明表里没有 read_file 不等于可以读：拒读必须还是错误，实得 preview {preview}"
        );
        assert!(
            preview.contains("越界"),
            "拒绝要归因到 containment 门（「越界」），不是碰巧的 IO 失败，也不是「工具没注册」：{preview}"
        );
        assert!(
            !preview.contains("不该被读到的正文"),
            "读到内容了 —— 隐藏声明表被当成了授权：{preview}"
        );
    }

    // ---- 预览：一次大输出不许压垮 IPC 与前端 ----

    #[tokio::test]
    async fn a_long_tool_output_reaches_the_ui_as_a_bounded_preview() {
        let long = "测".repeat(500);
        let args = format!(r#"{{"text":"{long}"}}"#);
        let mut rig = Rig::new(vec![
            tool_round(0, &[("echo", args.as_str())]),
            deltas(&["读完了"]),
        ]);
        assert_eq!(rig.run("读一大段").await, StopReason::EndTurn);

        let (id, is_error, preview) = rig.sink.tool_results().into_iter().next().unwrap();
        assert_eq!((id.as_str(), is_error), ("r0c0", false));
        assert_eq!(preview.chars().count(), 201, "200 个字符 + 一个省略号");
        assert!(preview.ends_with('…'), "截断必须留标记：{preview}");
        assert!(
            !preview.contains(&long),
            "整块输出被搬进事件流了（完整内容只该留在 history）"
        );
        assert_eq!(
            tool_results_of(rig.session.history())[0].2,
            long,
            "history 里必须是完整输出——那才是下一轮模型的输入"
        );
        assert_every_request_paired(&rig);
    }

    // ---- Task 13 的形状：session 与它的 run future 都得是 Send ----

    #[tokio::test]
    async fn a_turn_runs_inside_a_spawned_task() {
        // `tokio::spawn` 要求 future 是 `Send + 'static`：这是个编译期事实，
        // 一旦有人往 SessionDeps 里塞 `Rc`，红的就是这里，而不是到装配那天才报
        // （gate.rs / registry.rs 各自也有一条同类的 Send 测试）。
        let rig = Rig::new(vec![deltas(&["在别的任务里回答"])]);
        let Rig {
            mut session,
            sink,
            recorder,
            ..
        } = rig;
        let out = tokio::spawn(async move {
            let r = session.run(user("spawn")).await;
            (r, session.history().to_vec())
        })
        .await
        .unwrap();
        assert_eq!(out.0, StopReason::EndTurn);
        assert_eq!(out.1.len(), 2);
        assert_eq!(sink.texts(), "在别的任务里回答");
        assert_eq!(recorder.recorded().len(), 1);
    }
}
