// `run_command`：把一条命令文本交给选定的 shell 后端执行。
//
// 2026-11-01：类型、预设表、解析/校验/命令构造由接口契约桩交付并被下面的类型级
// 单测钉住；**spawn/超时/取消/输出捕获/进程树回收**已由 feat/agent-shell-core 交付
// （执行语义见 spec §7）。设计全文：
// `docs/superpowers/specs/2026-11-01-agent-shell-selection-design.md`。
//
// 2026-11-01 修订（feat/shell-detect-presets，spec §5.2 的带日期修订）：预设从
// 「8 条恒在」改为「**候选 + 检测**」—— 下面的静态表降格为候选表（id/label/program/
// args 与展示顺序不变），构造 catalog 时对每条候选用 `ensure_executable` 判可得性，
// 检测通过的才进预设集；一台机器可能一条都没有，此时预设列表为空，其余 shell 一律
// 由用户以自定义路径添加（机制不变）。
//
// 与接线线/UI 线的契约（改任何 `pub` 签名都是破坏契约，需三线同步）：
// * 工具名 `run_command`，`ToolMeta.risk = Risk::Exec`（风险唯一真相源，registry::risk_of 出）。
// * 入参 schema 写死在 spec §5.1：`{ command, shell_id?, cwd?, timeout_ms? }`。
// * `ShellProfile` 同时是 settings.json 与 IPC 的线格式（snake_case 四键，serde 派生）。
// * `ShellCatalog::from_parts` 仍是接线线从 `Settings` 构造 catalog 的唯一入口（构造点
//   跑候选检测；测试注入口是 `from_parts_with_probe`）；`DEFAULT_PRESET_ID` 是 settings
//   normalize 的回落目标（合法性按「候选 id 静态集合 ∪ 自定义 id」判，见
//   `is_preset_candidate_id` —— normalize 全程不碰文件系统，R284 同规）。
// * `ensure_executable` 是「程序存在且可执行」的唯一实现：IPC 校验命令与执行前强制
//   都调它；保存（normalize）**不**调它 —— settings 归一化不碰文件系统（R284 同规）。
//
// 层边界：本文件不得 `use tauri`（tools/ 模块树纪律）。对 `crate::mcp` 只取两样：
// `bridge::CREATE_NO_WINDOW`（Windows 无窗常量，抄第二份会漂）与 `process` 的进程树
// 回收（T34 设施；mcp/bridge.rs 本就反向引用 tools::registry::ToolError，这个方向的
// 依赖是既有事实）。
use crate::agent::types::{Risk, ToolOutput};
use crate::mcp::process::{KillOnCloseJob, create_kill_job};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// 工具注册名。闸门对 `Risk::Exec` 的作用域令牌取 `input.command`（gate.rs rule_key），
/// 入参名与这份既有口径对齐，不改门。
pub const NAME: &str = "run_command";

/// 默认超时（毫秒）。命令级默认值只此一处；模型没给 `timeout_ms` 时用它。
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 超时上限：夸大时限按钳位处理（不是形状错误），10 分钟封顶。
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// stdout + stderr 合计的字节上限。与 `read_file::MAX_BYTES` 同一量级——「一页上下文」
/// 的既有口径。超限后**继续读取并丢弃**（管道不塞死、命令能跑完），交出的文本截断。
pub const MAX_OUTPUT_BYTES: usize = 512 * 1024;
/// 尾部装饰（截断标记、`[退出码 N]`、非 UTF-8 注明）要占的字节预算：交出的**整段**
/// 文本承诺 ≤ `MAX_OUTPUT_BYTES`，内容就得留出装饰的空间（read_file 的 CONTENT_BUDGET
/// 同一口径——512 字节对这几行提示绰绰有余）。
const MARKER_RESERVE: usize = 512;
/// 抓取阶段的合计字节预算 = `MAX_OUTPUT_BYTES - MARKER_RESERVE`。stdout 与 stderr
/// 两根管道共享这一份，原子扣减保证「合计」恰好成立。
const OUTPUT_BUDGET: usize = MAX_OUTPUT_BYTES - MARKER_RESERVE;

/// 默认预设 id：settings normalize 的回落目标、装配期空 catalog 的缺省。
/// spec §5.2 的表里它排第一（需求示例即 `bash -lc`）。它是**候选** id——本机没检测到
/// bash 时，resolve 的缺省路径照旧 fail-closed 报错（不悄悄换一个后端）。
pub const DEFAULT_PRESET_ID: &str = "bash";

/// 一个 shell 后端的描述。**预设与自定义共用这一个类型**：命令文本永远作为 argv 的
/// 最后一个元素整体传入，`args` 是命令文本之前的固定参数模板 ——「argv 逐段、不拼
/// 命令行」（mcp/bridge.rs 同规）靠这个结构保证，不需要 per-shell 的枚举分发。
///
/// serde 形状是 settings.json / IPC 的线格式（snake_case 四键），字段名即线上键名。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ShellProfile {
    /// 稳定键。预设 = 候选表（spec §5.2）的 8 个 id 经本机检测的子集；自定义 = 用户命名。
    /// 自定义与预设同名时**覆盖预设**（resolve 先查 customs）：用户明确加同名 profile
    /// 就是要换掉那个后端。
    pub id: String,
    /// UI 显示名。预设给专有名；自定义允许空串（UI 回显 id）。
    pub label: String,
    /// 可执行文件：绝对/相对路径，或按 PATH 解析的裸名（`ensure_executable` 判定）。
    pub program: String,
    /// 固定参数模板，每项是**单个 argv 元素**（UI 里每行一个）；含空白的项在
    /// `validate_custom` 被拒——那意味着调用方想拼命令行，而这条纪律禁止拼。
    pub args: Vec<String>,
}

/// 内置**候选**表（spec §5.2 冻结的 8 条；2026-11-01 修订：候选 + 检测 —— 表的 id/
/// label/program/args 与展示顺序一概不变，检测通过的子集才是预设集，见
/// [`detected_presets`]）。`program` 为裸名的按 PATH 解析；`git-bash` 用 Git for
/// Windows 的标准安装位置——装在别处的用户请建自定义 profile 指过去，预设不猜第二条路径。
fn presets() -> &'static [ShellProfile] {
    static PRESETS: LazyLock<Vec<ShellProfile>> = LazyLock::new(|| {
        vec![
            ShellProfile {
                id: "bash".into(),
                label: "Bash".into(),
                program: "bash".into(),
                // 登录档（-l）保住 PATH 等环境，再交给 -c 执行——需求示例原样。
                args: vec!["-lc".into()],
            },
            ShellProfile {
                id: "sh".into(),
                label: "sh".into(),
                program: "sh".into(),
                // POSIX sh 没有标准的 -l，取最简。
                args: vec!["-c".into()],
            },
            ShellProfile {
                id: "zsh".into(),
                label: "Zsh".into(),
                program: "zsh".into(),
                // 与 bash 同取登录档。
                args: vec!["-lc".into()],
            },
            ShellProfile {
                id: "fish".into(),
                label: "Fish".into(),
                program: "fish".into(),
                // fish 的登录档语义不同，agent 场景取最简；要登录环境就建自定义。
                args: vec!["-c".into()],
            },
            ShellProfile {
                id: "git-bash".into(),
                label: "Git Bash".into(),
                program: "C:\\Program Files\\Git\\bin\\bash.exe".into(),
                args: vec!["-lc".into()],
            },
            ShellProfile {
                id: "pwsh".into(),
                label: "PowerShell (pwsh)".into(),
                program: "pwsh".into(),
                // 关 profile：agent 场景要确定性；要加载用户 profile 就建自定义。
                args: vec!["-NoLogo".into(), "-NoProfile".into(), "-Command".into()],
            },
            ShellProfile {
                id: "powershell".into(),
                label: "Windows PowerShell".into(),
                program: "powershell".into(),
                args: vec!["-NoLogo".into(), "-NoProfile".into(), "-Command".into()],
            },
            ShellProfile {
                id: "cmd".into(),
                label: "CMD".into(),
                program: "cmd".into(),
                // /d 关 AutoRun；/s + /c 后接整条命令文本。cmd.exe 在「/C + 独立 argv
                // 元素」下的确切引号行为是 Windows 真机验收项（spec §10），本环境不可实测。
                args: vec!["/d".into(), "/s".into(), "/c".into()],
            },
        ]
    });
    &PRESETS
}

/// 候选 id 的**纯词法**判定（settings normalize 对 `default_shell` 的合法性判定用）：
/// 归属「候选 id 静态集合 ∪ 自定义 id」里前一半的判定。R284 同规——normalize 全程
/// 不碰文件系统，这里只查表、绝不触发探测：没检测到的候选 id 在这台机器上仍算合法
/// （换机器的 settings 文件不失效），运行期 `resolve`/`ensure_executable` 给出明确报错。
pub fn is_preset_candidate_id(id: &str) -> bool {
    presets().iter().any(|p| p.id == id)
}

/// 生产检测入口（IPC `shell_presets` 与 catalog 构造共用，单一真相）：候选表逐条过
/// `ensure_executable`（候选专属搜索目录），通过的按 spec §5.2 的展示顺序组成预设集。
/// 检测**只做过滤**：通过的候选原样保留（program 仍是表里的裸名/标准路径，不把解析出
/// 的绝对路径写回 profile），执行期 `ensure_executable` 照旧兜底。
pub fn detected_presets() -> Vec<ShellProfile> {
    detect_presets_with(&|program, dirs| ensure_executable(program, dirs))
}

/// 检测的可注入内核（同 `run_with_job_factory` 的注入口径，R305）：probe 形状即
/// `ensure_executable` 的签名，测试注入假 probe 钉「全在 / 全不在 / 部分在」三态，
/// 单测不依赖本机装了什么。
// probe 的 `&dyn Fn` 签名是刻意的注入口（R305），收拢为 type alias 属于签名重构，超出机械清理范围。
#[allow(clippy::type_complexity)]
fn detect_presets_with(
    probe: &dyn Fn(&str, &[PathBuf]) -> Result<PathBuf, String>,
) -> Vec<ShellProfile> {
    presets()
        .iter()
        .filter(|p| probe(&p.program, &candidate_search_dirs(p)).is_ok())
        .cloned()
        .collect()
}

/// 候选的专属搜索目录（检测用）：基础是 `path_search_dirs()`（与执行期同一份 PATH），
/// Windows 候选再叠各自的常见安装位置（[`windows_candidate_dirs`]，cfg(windows) 才
/// 编译进探测）。检测目录 ⊇ 执行目录（PATH），所以「检测到但执行期才报错」只可能在
/// 「程序只在专属目录、不在 PATH」的极端情形出现——fail-closed，错误点名 program。
/// `git-bash` 的 program 本就是绝对路径，`ensure_executable` 对它直判该路径，这里的
/// 目录只是同一张表的一部分，不参与它的判定。
fn candidate_search_dirs(preset: &ShellProfile) -> Vec<PathBuf> {
    let dirs = path_search_dirs();
    #[cfg(windows)]
    {
        let mut dirs = dirs;
        dirs.extend(windows_candidate_dirs(&preset.id));
        dirs
    }
    #[cfg(not(windows))]
    {
        let _ = preset;
        dirs
    }
}

/// Windows 候选的专属目录表（表即文档，spec §5.2 修订 2026-11-01）：`pwsh` 的 MSI 装
/// 在 `%ProgramFiles%\PowerShell\<主版本>`；Windows PowerShell 与 cmd.exe 是系统组件，
/// 各在自己唯一的系统目录；git-bash 叠 Git for Windows 的常见安装根。环境变量缺失时
/// 跳过对应目录——检测不到就少一条预设（fail-closed），不影响其余候选。
/// **Windows 真机项**：本目录表在 Linux 开发环境无法实测，只有 cfg(windows) 测试与
/// 真机验收覆盖。
#[cfg(windows)]
fn windows_candidate_dirs(id: &str) -> Vec<PathBuf> {
    fn under(var: &str, sub: &str) -> Option<PathBuf> {
        std::env::var_os(var).map(|root| PathBuf::from(root).join(sub))
    }
    match id {
        "pwsh" => vec![
            under("ProgramFiles", r"PowerShell\7"),
            under("ProgramFiles", r"PowerShell\6"),
        ],
        "powershell" => vec![under("SystemRoot", r"System32\WindowsPowerShell\v1.0")],
        "cmd" => vec![under("SystemRoot", r"System32")],
        "git-bash" => vec![
            under("ProgramFiles", r"Git\bin"),
            under("ProgramFiles", r"Git\usr\bin"),
            under("ProgramFiles(x86)", r"Git\bin"),
        ],
        _ => Vec::new(),
    }
    .into_iter()
    .flatten()
    .collect()
}

/// 自定义 profile 的校验与归一化（**词法**层，不做存在性检查——那一步是
/// `ensure_executable`，只在 IPC 校验命令与执行前调用）。
///
/// trim id/label/program 与 args 各项；空 id、空 program、空或含空白的 args 项都是 `Err`。
/// label 允许空串。幂等：归一后的输出再过一遍结果不变。
pub fn validate_custom(profile: ShellProfile) -> Result<ShellProfile, String> {
    let id = profile.id.trim();
    if id.is_empty() {
        return Err("自定义 shell 的 id 不能为空".into());
    }
    let program = profile.program.trim();
    if program.is_empty() {
        return Err(format!("自定义 shell「{id}」的 program 不能为空"));
    }
    let mut args = Vec::with_capacity(profile.args.len());
    for arg in &profile.args {
        let t = arg.trim();
        if t.is_empty() {
            return Err(format!("自定义 shell「{id}」的参数有空项"));
        }
        if t.chars().any(char::is_whitespace) {
            return Err(format!(
                "自定义 shell「{id}」的参数「{t}」含空白：参数按单个 argv 元素传递，请拆开写"
            ));
        }
        args.push(t.to_string());
    }
    Ok(ShellProfile {
        id: id.to_string(),
        label: profile.label.trim().to_string(),
        program: program.to_string(),
        args,
    })
}

/// 判一个候选程序「存在且可执行」。**唯一**实现（IPC 校验命令与 execute 共用，不留第二把尺子）。
///
/// 平台判据（账面诚实，spec §5.4）：
/// * Unix：存在 + 普通文件 + 任一 x 位（`PermissionsExt`，std 自带）。
/// * Windows：存在 + 普通文件 + 扩展名在 {exe, com, bat, cmd}。**这是启发式**——NTFS
///   没有执行位可查；判不出的形状宁可报错（fail-closed），不静默放行。
pub fn ensure_executable(program: &str, search_dirs: &[PathBuf]) -> Result<PathBuf, String> {
    let looks_like_path = program.contains('/') || program.contains('\\');
    if looks_like_path {
        let candidate = PathBuf::from(program);
        check_candidate(&candidate)?;
        return Ok(candidate);
    }
    for dir in search_dirs {
        for name in candidate_names(program) {
            let candidate = dir.join(&name);
            if check_candidate(&candidate).is_ok() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "在 {} 个搜索目录里找不到可执行程序「{program}」",
        search_dirs.len()
    ))
}

/// 生产用的搜索目录：`PATH`（`std::env::split_paths` 自管平台分隔符）。测试注入假表，
/// 单测不依赖本机装了什么（同 mcp 选型 R305 的注入口径）。
pub fn path_search_dirs() -> Vec<PathBuf> {
    std::env::var("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// 裸名的候选形状：先原名（覆盖「名字已带扩展名」），Windows 再叠 `PATHEXT`
/// （缺省按文档那四条，与 mcp/bridge.rs 的 DEFAULT_PATHEXT 同源口径）。
fn candidate_names(program: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut names = Vec::with_capacity(exts.split(';').count() + 1);
        names.push(program.to_string());
        for ext in exts.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            names.push(format!("{program}{ext}"));
        }
        names
    }
    #[cfg(not(windows))]
    {
        vec![program.to_string()]
    }
}

fn check_candidate(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}：{e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} 不是普通文件", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(format!("{} 没有可执行位", path.display()));
        }
    }
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("exe") | Some("com") | Some("bat") | Some("cmd") => {}
            _ => {
                return Err(format!(
                    "{} 没有 Windows 可执行扩展名（exe/com/bat/cmd）——启发式判定，宁可拒",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// 这一轮可用的 shell 全集：自定义（来自 Settings）+ 检测到的预设 + 默认 id。
///
/// 注册表是进程级单例而 `ToolCtx` 是每轮现构的——往 `ToolCtx` 加字段会炸穿十余个
/// 测试文件里的字面量构造（它们不属于任何一条开发线），所以 catalog 以共享句柄
/// （`Arc<RwLock<_>>`）住在 `ShellTool` 里，由装配层每轮现刷（spec §2「生效时机」）。
/// 锁走 `crate::lock` 的中毒容忍入口。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShellCatalog {
    /// 自定义表（来自 `Settings.shells`）。解析顺序在预设**之前**（同名覆盖预设）。
    pub customs: Vec<ShellProfile>,
    /// 检测到的预设集：候选表（spec §5.2 的 8 条）经可用性检测后的**子集**，保持展示
    /// 顺序（检测只过滤、不重排）。在构造点检测（[`ShellCatalog::new`] / `from_parts`，
    /// 测试注入口 `from_parts_with_probe`），resolve 不碰文件系统。`Default` 给的是空表
    /// ——那是「一条候选都没检测到」的诚实形状，生产的构造一律走 `new`/`from_parts`。
    pub presets: Vec<ShellProfile>,
    /// 来自 `Settings.default_shell`；空串回落 `DEFAULT_PRESET_ID`。
    pub default_id: String,
}

impl ShellCatalog {
    /// 装配期占位：空自定义 + 检测到的预设 + 空 default id。第一条消息前 `send_message`
    /// 会现刷。与 `Default`（纯空表）不同：本构造跑检测（装配点的一次性成本）。
    pub fn new() -> Self {
        Self::from_parts(Vec::new(), "")
    }

    /// 接线线从 `Settings` 构造的唯一入口（不依赖 settings.rs 的类型）。预设集在本
    /// 构造点检测——每轮 `send_message` 刷新时重测一次，与 roots/temperature 同一条
    /// 「本轮现取」线；测试要脱钩本机装了什么，改走 [`ShellCatalog::from_parts_with_probe`]。
    pub fn from_parts(customs: Vec<ShellProfile>, default_id: &str) -> Self {
        Self::from_parts_with_probe(customs, default_id, &|program, dirs| {
            ensure_executable(program, dirs)
        })
    }

    /// 构造的可注入口（R305 同款）：probe 即 `ensure_executable` 的签名，测试注入假
    /// probe 钉「全在 / 全不在 / 部分在」三态，不依赖本机装了什么。
    // probe 的 `&dyn Fn` 签名是刻意的注入口（R305），收拢为 type alias 属于签名重构，超出机械清理范围。
    #[allow(clippy::type_complexity)]
    pub fn from_parts_with_probe(
        customs: Vec<ShellProfile>,
        default_id: &str,
        probe: &dyn Fn(&str, &[PathBuf]) -> Result<PathBuf, String>,
    ) -> Self {
        Self {
            customs,
            presets: detect_presets_with(probe),
            default_id: default_id.trim().to_string(),
        }
    }

    /// 解析要用的 profile。显式 `shell_id` → customs → presets（**检测到的**子集）；
    /// 缺省 → `default_id`（空则 `DEFAULT_PRESET_ID`）。查不到 ⇒ `Err`，错误里列出全部
    /// 可用 id（fail-closed，不悄悄换一个后端——模型以为在用 A 实际跑在 B 上，比失败
    /// 更糟）。点了没检测到的候选 id 同样报错并点名可用面。
    pub fn resolve(&self, id: Option<&str>) -> Result<ShellProfile, String> {
        let wanted = match id {
            Some(explicit) => explicit.trim(),
            None => {
                let default = self.default_id.trim();
                if default.is_empty() {
                    DEFAULT_PRESET_ID
                } else {
                    default
                }
            }
        };
        if let Some(custom) = self.customs.iter().find(|p| p.id == wanted) {
            return Ok(custom.clone());
        }
        if let Some(preset) = self.presets.iter().find(|p| p.id == wanted) {
            return Ok(preset.clone());
        }
        Err(self.unknown_shell_error(wanted))
    }

    fn unknown_shell_error(&self, wanted: &str) -> String {
        let custom_ids = if self.customs.is_empty() {
            "无".to_string()
        } else {
            self.customs
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join("、")
        };
        // 空预设集要说明**为什么**空：本机一条候选都没检测到（不是表丢了），出路是
        // 自定义 profile——报错给得出下一步，才算「清晰」。
        let preset_ids = if self.presets.is_empty() {
            "无（本机未检测到任何内置候选 shell，可在设置里以自定义路径添加）".to_string()
        } else {
            self.presets
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join("、")
        };
        format!("未知的 shell profile：{wanted}。可用——预设：{preset_ids}；自定义：{custom_ids}")
    }
}

/// `timeout_ms` 的形状判定：缺省/`null` → 默认值；正整数 → 钳到上限；
/// `0`、负数、小数、字符串等一切非「正整数」形状 → `BadInput`（同 read_file/fetch 的
/// 口径：形状错误必须报，不许静默按默认跑）。
fn timeout_ms_of(input: &Value) -> Result<u64, ToolError> {
    match input.get("timeout_ms") {
        None | Some(Value::Null) => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => match v.as_u64() {
            Some(0) => Err(ToolError::BadInput(
                "timeout_ms 必须是正整数（毫秒），0 不是合法值".into(),
            )),
            Some(n) => Ok(n.min(MAX_TIMEOUT_MS)),
            None => Err(ToolError::BadInput(format!(
                "timeout_ms 必须是正整数（毫秒），实得 {v}"
            ))),
        },
    }
}

/// `command` 的形状判定：字符串且 trim 后非空；返回**原文**（不 trim——命令文本的
/// 首尾空白交给 shell 解释，闸门的作用域令牌自己会 trim）。
fn required_command(input: &Value) -> Result<String, ToolError> {
    match input.get("command") {
        Some(Value::Null) | None => Err(ToolError::BadInput(
            "run_command 需要非空字符串参数 command".into(),
        )),
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
            _ => Err(ToolError::BadInput(
                "run_command 需要非空字符串参数 command".into(),
            )),
        },
    }
}

/// 可选字符串入参（`shell_id` / `cwd`）。缺省与 `null` 都按「没给」；非字符串或
/// trim 后为空 → `BadInput`。
fn optional_str<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => Ok(Some(s)),
            _ => Err(ToolError::BadInput(format!(
                "{key} 必须是非空字符串，实得 {v}"
            ))),
        },
    }
}

/// 命令构造（纯函数，不 spawn）：program + args 逐段 `.arg()`，命令文本永远是
/// **最后一个** argv 元素；`cwd` 落 `current_dir`；`kill_on_drop(true)` 兜底；
/// Windows `CREATE_NO_WINDOW`（复用 mcp 的常量，不抄第二份）；Unix `process_group(0)`
/// ——建组必须在 spawn 时做，进程树回收（T34 的 job/kill(-pgid)）依赖它。
pub fn build_command(profile: &ShellProfile, command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(&profile.program);
    for arg in &profile.args {
        cmd.arg(arg);
    }
    cmd.arg(command);
    cmd.current_dir(cwd);
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(crate::mcp::bridge::CREATE_NO_WINDOW);
    #[cfg(not(windows))]
    cmd.process_group(0);
    cmd
}

/// 单根管道的「有界排空」：预算内的字节攒下来；预算用尽后**继续读但只丢弃并计数**
/// （spec §7.5）——管道被持续排空，子进程不会因为没人收输出而塞死，跑得完。
///
/// 两根管道必须**并发**抽干：任一根塞满都会卡住子进程，而「读完一根再读另一根」
/// 会让先满的那根永远等不到人——所以读与读之间靠 join! 的并发轮询交错。
/// `budget` 与 `dropped` 是共享原子：两根管道合计受 `OUTPUT_BUDGET` 约束，
/// 原子扣减保证不多留一个字节。
async fn drain_pipe(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    budget: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
) -> Vec<u8> {
    let mut kept = Vec::new();
    let Some(mut pipe) = pipe else {
        return kept; // 没拿到管道（理论不该发生，spawn 已声明 piped）——按空输出处理
    };
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                // 预算认领（CAS 重试）：两根管道并发认领同一份预算，
                // 原子交换保证「合计」恰好成立、不多留一个字节。
                let take = loop {
                    let current = budget.load(Ordering::Relaxed);
                    let take = n.min(current);
                    if take == 0 {
                        break 0;
                    }
                    if budget
                        .compare_exchange_weak(
                            current,
                            current - take,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        break take;
                    }
                };
                if take > 0 {
                    kept.extend_from_slice(&buf[..take]);
                }
                if n > take {
                    dropped.fetch_add(n - take, Ordering::Relaxed);
                }
            }
            Err(_) => break, // 这根管道读挂了就到此为止，不影响另一根
        }
    }
    kept
}

/// 退出状态 → 尾部那一行（`None` = 退出 0，不挂行）。非零退出**不是**工具错误
/// （spec §2：命令失败是模型该看到并处理的结果）；被信号终止时如实点名信号，
/// 不冒充「退出码 1」。我们自己的超时/取消不走这里（那两条直接 Err），
/// 能落进这一行的信号都来自命令本身或外部。
fn exit_note(status: &ExitStatus) -> Option<String> {
    if let Some(code) = status.code() {
        return (code != 0).then(|| format!("[退出码 {code}]"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| format!("[被信号 {s} 终止]"))
    }
    #[cfg(windows)]
    {
        Some("[退出码未知]".to_string())
    }
}

/// 字节 → 文本的唯一入口。与 read_file 的「拒绝非 UTF-8」**刻意不同**（spec §8.5）：
/// read_file 的对象是用户根目录里的文件，拒读是安全取向；这里的对象是「命令跑出来的
/// 东西」——中文 Windows 上 GBK 输出若按拒绝处理，`dir` 这类命令永远读不到结果，
/// 这把工具就残废了。所以失败就 lossy 转换，并返回「发生过替换转换」的标记，
/// 由输出尾部的注明补偿 U+FFFD 被模型当成内容的风险。返回 (文本, 是否有替换转换)。
fn to_text(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

/// 往左找最近的字符边界（std 的 `floor_char_boundary` 还没稳，先自己写）。
fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 输出编排（spec §7.6，纯函数可测）：stdout 在前；stderr 非空则跟 `[stderr]` 段；
/// 退出码非零追加一行；stdout/stderr 全空且退出 0 → `[命令成功，没有输出]`；
/// 管道阶段丢弃过字节就在尾部挂截断标记（含实测丢弃字节数，风格同 read_file 的
/// 页尾标记）；出现过替换转换再挂一行注明。段与段之间只补一个缺着的换行，
/// 不修剪命令自己的输出。
fn compose_output(
    stdout: &[u8],
    stderr: &[u8],
    dropped_bytes: usize,
    exit_line: Option<String>,
) -> String {
    let (stdout_text, stdout_lossy) = to_text(stdout);
    let (stderr_text, stderr_lossy) = to_text(stderr);
    let non_utf8 = stdout_lossy || stderr_lossy;

    let mut body = stdout_text;
    if !stderr_text.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(&stderr_text);
    }
    // 内容预算的兜底：预算是按原始字节扣的，lossy 替换会让文本膨胀（1 字节无效字节
    // → 3 字节 U+FFFD），这里按字符截回预算内。走到这一步必然 non_utf8（合法 UTF-8
    // 是保长的），膨胀的事实由下面的注明一并交代。
    let mut trimmed = false;
    if body.len() > OUTPUT_BUDGET {
        let cut = floor_char_boundary(&body, OUTPUT_BUDGET);
        body.truncate(cut);
        trimmed = true;
    }
    if let Some(line) = exit_line.as_deref() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(line);
    }
    if body.is_empty() && exit_line.is_none() {
        body.push_str("[命令成功，没有输出]");
    }
    if dropped_bytes > 0 {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        // 「已交出」量的是这段文本自身的字节数；丢弃量的是管道里实测的原始字节。
        body.push_str(&format!(
            "[输出超过 512 KiB（{MAX_OUTPUT_BYTES} 字节）上限：已交出前 {} 字节，其余 \
             {dropped_bytes} 字节已读取并丢弃（读而不停，命令照常跑完）]",
            body.len()
        ));
    }
    if non_utf8 {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        let mut note = "[输出含非 UTF-8 字节，已按替换字符（U+FFFD）转换".to_string();
        if trimmed {
            note.push_str("；替换使文本膨胀，已再按字符截到上限内");
        }
        note.push(']');
        body.push_str(&note);
    }
    body
}

/// select 的三种收场（spec §7.4/§7.7）：命令跑完（带全部抓到的输出与退出状态）、
/// 超时、取消。三种出口结束时 `KillOnCloseJob` 都会随本函数返回 Drop——整棵树回收。
enum WaitOutcome {
    Done {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        status: std::io::Result<ExitStatus>,
    },
    TimedOut,
    Cancelled,
}

/// spawn 之后的一切：job 上膛、管道抽干、超时/取消、输出编排。
///
/// 可测缝（R305 注入口径）：`job_factory` 是参数——生产 `execute` 传
/// `create_kill_job`，测试注入假工厂，于是「拿不到 job 就不 spawn」（R323）、
/// 「取消先于工厂先于 spawn」这些**次序**都能在不真的起进程的情况下钉住。
/// trait 对象要 `+ Send + Sync`：本函数的 future 必须是 Send（`Tool` 的
/// `#[async_trait]` 默认 Send 约束），而 `&dyn Fn` 本身不是 Send。
///
/// 无论正常退出、超时还是取消，本函数返回时 `KillOnCloseJob` 一律 Drop：
/// job 上挂着整棵进程树（Unix 杀进程组 / Windows 杀 job），agent 跑的命令
/// **不留后台进程**——想在一轮里留一个常驻服务不是本工具的用途（spec §7.7
/// 的已知限制）。
async fn run_with_job_factory(
    job_factory: &(dyn Fn() -> Result<KillOnCloseJob, String> + Send + Sync),
    profile: &ShellProfile,
    command: &str,
    cwd: &Path,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    // spawn 前的取消检查：进程还没起来，此时取消零成本（等待中的取消由下面 select 接管）。
    if cancel.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    // R323 同口径：拿不到进程回收单位就不 spawn，绝不静默退化成「裸跑」——
    // 一棵没人管的命令树比一次失败难收拾得多。
    let job = job_factory().map_err(ToolError::Failed)?;

    let mut cmd = build_command(profile, command, cwd);
    // stdin 接 Null：命令读 stdin 时立刻见 EOF，而不是挂到超时（这里没有可给的输入）。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Failed(format!("无法启动「{}」：{e}", profile.program)))?;
    // 上膛：把子进程记进回收单位（Unix 记 pgid、Windows 进 job）——这是唯一能拿
    // pid 的时刻。失败就亲手杀掉，不留一个没有兜底的孩子（R323 同一失败策略）。
    // pid 为 None 只可能是「spawn 后立刻被收尸」，此时已无事可回收（bridge.rs 同判读）。
    if let Some(pid) = child.id()
        && let Err(e) = job.assign(pid)
    {
        let _ = child.kill().await;
        return Err(ToolError::Failed(e));
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    // 合计预算（spec §7.5）：两根管道共享 OUTPUT_BUDGET；超限继续读但丢弃并计数，
    // 交出文本截断挂标记——管道不塞死，命令跑得完。
    let budget = Arc::new(AtomicUsize::new(OUTPUT_BUDGET));
    let dropped = Arc::new(AtomicUsize::new(0));

    // 读输出 + wait 一起进 select（biased：取消臂最先轮询，spec §7.1）。kill 不放在
    // select 臂内而是出块后再做：wait future 对 child 的可变借用要到块尾才结束，
    // 出块再杀的语义不变——取消/超时先杀、返回时 job Drop 兜整棵树。
    let outcome = {
        let wait_child = child.wait();
        let wait_out = drain_pipe(stdout_pipe, Arc::clone(&budget), Arc::clone(&dropped));
        let wait_err = drain_pipe(stderr_pipe, Arc::clone(&budget), Arc::clone(&dropped));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => WaitOutcome::Cancelled,
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => WaitOutcome::TimedOut,
            done = async move { tokio::join!(wait_out, wait_err, wait_child) } => {
                let (stdout, stderr, status) = done;
                WaitOutcome::Done { stdout, stderr, status }
            }
        }
    };

    match outcome {
        WaitOutcome::Cancelled => {
            // 先亲手杀直接子进程；返回时 job Drop 把整组（含孙进程）一并 SIGKILL。
            let _ = child.kill().await;
            Err(ToolError::Cancelled)
        }
        WaitOutcome::TimedOut => {
            let _ = child.kill().await;
            Err(ToolError::Failed(format!(
                "命令在 {timeout_ms} ms 超时被终止（连同它启动的子进程一起回收）"
            )))
        }
        WaitOutcome::Done {
            stdout,
            stderr,
            status,
        } => {
            let status = status.map_err(|e| ToolError::Failed(format!("等待命令退出失败：{e}")))?;
            let exit_line = exit_note(&status);
            let dropped_bytes = dropped.load(Ordering::Relaxed);
            Ok(ToolOutput::text(compose_output(
                &stdout,
                &stderr,
                dropped_bytes,
                exit_line,
            )))
        }
    }
}

/// `run_command` 工具本体。catalog 经装配层注入（见 [`ShellCatalog`] 的注释）；
/// 风险 `Risk::Exec`：每次调用弹窗，会话级放行的粒度是整条命令文本（门既有行为）。
pub struct ShellTool {
    catalog: Arc<RwLock<ShellCatalog>>,
}

impl ShellTool {
    pub fn new(catalog: Arc<RwLock<ShellCatalog>>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: "在选定的 shell 后端里执行一条命令并返回它的 stdout 与 stderr；命令文本\
                          由该 shell 解释。参数：command 必填；shell_id 可选（内置预设 id 是\
                          bash/sh/zsh/fish/git-bash/pwsh/powershell/cmd 中**本机检测到**的子集，\
                          或设置里的自定义 id；缺省用用户的默认 shell）；cwd 可选（绝对路径且\
                          必须已存在，缺省是本会话的输出目录）；\
                          timeout_ms 可选（毫秒，默认 30000，上限 600000，超时命令连同它启动的\
                          子进程会被终止）。输出合计上限 524288 字节，超出部分截断并注明；命令\
                          退出码非零时输出尾部带「[退出码 N]」（这不是工具错误）；输出含非 UTF-8 \
                          字节时按替换字符转换并注明。每次调用都会向用户弹窗确认。"
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的命令文本，交给选定的 shell 解释"
                    },
                    "shell_id": {
                        "type": "string",
                        "description": "可选：shell profile 的 id（预设或自定义）；缺省用设置里的默认 shell"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "可选：工作目录，绝对路径且必须已存在；缺省用当前会话的输出目录"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "可选：超时毫秒数，默认 30000，上限 600000"
                    }
                },
                "required": ["command"]
            }),
            risk: Risk::Exec,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // 工具侧那一次取消检查：在任何解析与文件系统访问之前（注册表侧也有一次，R53）。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // —— 以下到 build_command 为止是**契约桩已交付**的部分：入参形状、profile
        // 解析、cwd 决定、程序存在性。它们的行为被类型级单测钉住，接线线与 UI 线
        // 依赖的就是这段的语义。 ——
        let command = required_command(&input)?;
        let shell_id = optional_str(&input, "shell_id")?;
        let cwd_raw = optional_str(&input, "cwd")?;
        let timeout_ms = timeout_ms_of(&input)?;

        let catalog = crate::lock::read(&self.catalog).clone();
        let profile = catalog.resolve(shell_id).map_err(ToolError::BadInput)?;

        let cwd: PathBuf = match cwd_raw {
            Some(raw) => {
                let p = PathBuf::from(raw.trim());
                if !p.is_absolute() {
                    return Err(ToolError::BadInput(format!("cwd 必须是绝对路径: {raw}")));
                }
                p
            }
            // 缺省取会话输出目录（生产装配恒有值）；两者都没有就明确报错——
            // 回落到应用进程 cwd 是不可复现的行为，不做。
            None => ctx.write_root.clone().ok_or_else(|| {
                ToolError::BadInput(
                    "没有指定 cwd，且当前会话没有输出目录：请显式给 cwd".to_string(),
                )
            })?,
        };
        let cwd_meta = std::fs::metadata(&cwd)
            .map_err(|e| ToolError::BadInput(format!("cwd 无法访问: {}: {e}", cwd.display())))?;
        if !cwd_meta.is_dir() {
            return Err(ToolError::BadInput(format!(
                "cwd 不是目录: {}",
                cwd.display()
            )));
        }

        // 执行前的强制校验（需求硬约束的第二个消费点；第一个是 IPC 校验命令）。
        ensure_executable(&profile.program, &path_search_dirs()).map_err(ToolError::Failed)?;

        // spawn 之后的一切（job、超时、取消、输出捕获、编排）走可测缝：
        // 生产用 `create_kill_job`，测试注入假工厂。
        run_with_job_factory(
            &create_kill_job,
            &profile,
            &command,
            &cwd,
            timeout_ms,
            &ctx.cancel,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    /// 与 read_file/registry 测试同一口径的自搭夹具：`ToolCtx` 字段全 pub，
    /// registry 的 `ctx_with_roots` 住在那个模块的私有 tests 里，本文件够不着。
    fn test_ctx(write_root: Option<PathBuf>) -> ToolCtx {
        ToolCtx {
            write_root,
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: Vec::new(),
            skills_dir: None,
            skills_disabled: Vec::new(),
        }
    }

    fn catalog_with(customs: Vec<ShellProfile>, default_id: &str) -> Arc<RwLock<ShellCatalog>> {
        Arc::new(RwLock::new(ShellCatalog::from_parts(customs, default_id)))
    }

    // ———————— 候选表（spec §5.2 的钉子；2026-11-01 修订：候选 + 检测） ————————

    /// 原名 `presets_table_has_exactly_the_eight_spec_ids`：表本身仍是 spec §5.2 那 8 条，
    /// 语义从「恒在的预设」改为「候选」——钉的内容不变。
    #[test]
    fn candidate_table_has_exactly_the_eight_spec_ids() {
        let ids: Vec<&str> = presets().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "bash",
                "sh",
                "zsh",
                "fish",
                "git-bash",
                "pwsh",
                "powershell",
                "cmd"
            ],
            "候选表与 spec §5.2 逐项对齐（顺序即展示顺序）"
        );
        // 候选 id 判定与表同源（settings normalize 的 R284 依据）：表内皆真、表外皆假。
        for id in &ids {
            assert!(is_preset_candidate_id(id), "{id} 必须是候选 id");
        }
        assert!(!is_preset_candidate_id("nope"));
        assert!(!is_preset_candidate_id(""));
    }

    #[test]
    fn candidate_entries_are_well_formed() {
        for p in presets() {
            assert!(!p.program.is_empty(), "{} 的 program 为空", p.id);
            assert!(!p.label.is_empty(), "{} 的 label 为空", p.id);
            assert!(!p.args.is_empty(), "{} 的参数模板为空", p.id);
            for a in &p.args {
                assert!(
                    !a.trim().is_empty() && !a.chars().any(char::is_whitespace),
                    "{} 的参数 {a:?} 是空项或含空白（参数是单个 argv 元素）",
                    p.id
                );
            }
        }
        assert_eq!(presets()[0].id, DEFAULT_PRESET_ID);
    }

    // ———————— 候选 + 检测（spec §5.2 修订；假 probe 钉三态，不依赖本机） ————————

    /// 测试注入的假 probe：全在 / 全不在 / 按程序名点名的部分在。
    fn probe_all_present(_: &str, _: &[PathBuf]) -> Result<PathBuf, String> {
        Ok(PathBuf::new())
    }

    fn probe_none_present(program: &str, _: &[PathBuf]) -> Result<PathBuf, String> {
        Err(format!("测试注入：{program} 不存在"))
    }

    #[test]
    fn detection_with_an_all_present_probe_lists_the_whole_candidate_table_in_order() {
        let c = ShellCatalog::from_parts_with_probe(Vec::new(), "", &probe_all_present);
        let ids: Vec<&str> = c.presets.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "bash",
                "sh",
                "zsh",
                "fish",
                "git-bash",
                "pwsh",
                "powershell",
                "cmd"
            ],
            "全部在 ⇒ 8 条按 spec §5.2 展示顺序全出"
        );
        assert_eq!(
            c.resolve(Some("pwsh")).unwrap().program,
            "pwsh",
            "检测到的候选可解析"
        );
    }

    #[test]
    fn detection_with_an_all_missing_probe_yields_an_empty_preset_set_and_a_clear_error() {
        let c = ShellCatalog::from_parts_with_probe(Vec::new(), "", &probe_none_present);
        assert!(c.presets.is_empty(), "全部不在 ⇒ 预设集为空");
        let e = c.resolve(Some("bash")).unwrap_err();
        assert!(
            e.contains("bash") && e.contains("未检测到"),
            "空预设集的报错要说明原因并点名请求的 id：{e}"
        );
        // 自定义不受检测影响；错误里仍列得出自定义 id（可用面只有自定义时同样清晰）。
        let with_custom = ShellCatalog::from_parts_with_probe(
            vec![custom("mine", "/opt/mine")],
            "mine",
            &probe_none_present,
        );
        assert_eq!(
            with_custom.resolve(None).unwrap().id,
            "mine",
            "default 指向自定义时照常解析"
        );
        let e = with_custom.resolve(Some("nope")).unwrap_err();
        assert!(
            e.contains("mine") && e.contains("未检测到"),
            "空预设集 + 有自定义的错误要两边都交代：{e}"
        );
    }

    #[test]
    fn detection_with_a_partial_probe_lists_only_the_hits_in_spec_order() {
        let probe = |program: &str, _: &[PathBuf]| -> Result<PathBuf, String> {
            if program == "zsh" || program == "cmd" {
                Ok(PathBuf::new())
            } else {
                Err(format!("测试注入：{program} 不存在"))
            }
        };
        let c = ShellCatalog::from_parts_with_probe(Vec::new(), "", &probe);
        let ids: Vec<&str> = c.presets.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["zsh", "cmd"],
            "部分在 ⇒ 只出命中的子集，保持 spec 顺序"
        );
        assert!(c.resolve(Some("zsh")).is_ok());
        assert!(
            c.resolve(Some("bash")).is_err(),
            "没检测到的候选不可解析（fail-closed，不悄悄换后端）"
        );
    }

    /// probe 收到的必须是「每条候选各一次、program 与候选专属目录成对」——unix 候选的
    /// 目录就是 `path_search_dirs()`（PATH）这份账面。
    #[cfg(unix)]
    #[test]
    fn detection_feeds_each_candidate_its_program_and_the_path_dirs() {
        let seen = std::sync::Mutex::new(Vec::new());
        let probe = |program: &str, dirs: &[PathBuf]| -> Result<PathBuf, String> {
            seen.lock()
                .unwrap()
                .push((program.to_string(), dirs.to_vec()));
            Err("只记账，不命中".into())
        };
        detect_presets_with(&probe);
        let seen = seen.into_inner().unwrap();
        let expected = path_search_dirs();
        assert_eq!(seen.len(), 8, "每条候选各探一次");
        for (i, (program, dirs)) in seen.iter().enumerate() {
            assert_eq!(
                program.as_str(),
                presets()[i].program,
                "probe 按候选表顺序收到各条的 program"
            );
            assert_eq!(
                dirs, &expected,
                "unix 候选的搜索目录 = PATH（与执行期同一份）"
            );
        }
    }

    /// **Windows 真机项**（本环境无法实测，spec §10 记账口径）：Windows 候选在 PATH 之外
    /// 叠各自的专属目录——表即文档（pwsh → %ProgramFiles%\PowerShell\{7,6}、powershell →
    /// %SystemRoot%\System32\WindowsPowerShell\v1.0、cmd → %SystemRoot%\System32、
    /// git-bash → %ProgramFiles%\Git\bin 等）。
    #[cfg(windows)]
    #[test]
    fn windows_candidates_get_their_documented_specific_dirs() {
        let profile = |id: &str| ShellProfile {
            id: id.into(),
            label: String::new(),
            program: "x".into(),
            args: vec!["-c".into()],
        };
        let contains =
            |id: &str, sub: &str| candidate_search_dirs(&profile(id)).contains(&PathBuf::from(sub));
        let sysroot = std::env::var("SystemRoot").expect("Windows 恒有 SystemRoot");
        let program_files = std::env::var("ProgramFiles").expect("Windows 恒有 ProgramFiles");
        assert!(
            contains("cmd", &format!(r"{sysroot}\System32")),
            "cmd 的专属目录是 %SystemRoot%\\System32"
        );
        assert!(
            contains(
                "powershell",
                &format!(r"{sysroot}\System32\WindowsPowerShell\v1.0")
            ),
            "powershell 的专属目录是系统 WindowsPowerShell 目录"
        );
        assert!(
            contains("pwsh", &format!(r"{program_files}\PowerShell\7"))
                && contains("pwsh", &format!(r"{program_files}\PowerShell\6")),
            "pwsh 叠 MSI 的常见主版本目录"
        );
        assert!(
            contains("git-bash", &format!(r"{program_files}\Git\bin")),
            "git-bash 叠 Git for Windows 的常见安装根"
        );
    }

    // ———————— 工具声明（风险真相源 + schema 钉死） ————————

    #[test]
    fn the_tool_meta_is_run_command_exec_with_the_pinned_schema() {
        let m = ShellTool::new(catalog_with(Vec::new(), "")).meta();
        assert_eq!(m.name, "run_command");
        assert_eq!(m.risk, Risk::Exec, "shell 是最高风险档，Exec 必弹窗");
        let s = m.input_schema;
        assert_eq!(s["type"], json!("object"));
        assert_eq!(s["required"], json!(["command"]), "只有 command 必填");
        let props = s["properties"].as_object().expect("properties 必须是对象");
        // serde_json 的 Map 按键序排列：比较**排序后**的键集 = 「恰好这四键，多一个少一个都算错」。
        let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["command", "cwd", "shell_id", "timeout_ms"],
            "schema 与 spec §5.1 逐键对齐"
        );
        assert_eq!(props["timeout_ms"]["minimum"], json!(1));
        assert!(
            m.description.contains("shell_id") && m.description.contains("524288"),
            "描述要写清后端选择与输出上限（模型只看得到描述）：{}",
            m.description
        );
    }

    #[test]
    fn shell_tool_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ShellTool>();
        assert_send_sync::<ShellCatalog>();
        assert_send_sync::<Arc<RwLock<ShellCatalog>>>();
    }

    // ———————— resolve：custom 覆盖 preset / 缺省回落 / 未知报错 ————————

    fn custom(id: &str, program: &str) -> ShellProfile {
        ShellProfile {
            id: id.into(),
            label: String::new(),
            program: program.into(),
            args: vec!["-c".into()],
        }
    }

    #[test]
    fn resolve_prefers_customs_over_presets_on_name_collision() {
        // customs 先于 presets，检测与否不影响命中 —— 沿用真构造即可。
        let c = ShellCatalog::from_parts(vec![custom("bash", "/opt/my/bash")], "");
        let got = c.resolve(Some("bash")).expect("同名自定义必须可解析");
        assert_eq!(
            got.program, "/opt/my/bash",
            "自定义覆盖同名预设（有意语义）"
        );
    }

    #[test]
    fn resolve_falls_back_to_default_id_then_default_preset() {
        // 2026-11-01 修订后预设侧来自检测：回落断言改用全在 probe，不依赖本机装了什么。
        let c = ShellCatalog::from_parts_with_probe(
            vec![custom("mine", "/opt/mine")],
            "mine",
            &probe_all_present,
        );
        assert_eq!(c.resolve(None).unwrap().program, "/opt/mine");

        let empty_default = ShellCatalog::from_parts_with_probe(Vec::new(), "", &probe_all_present);
        assert_eq!(
            empty_default.resolve(None).unwrap().id,
            DEFAULT_PRESET_ID,
            "default_id 为空 ⇒ 回落默认预设"
        );
    }

    #[test]
    fn resolve_names_the_available_ids_when_unknown() {
        // 全在 probe：错误里列出的预设侧是确定的全集，断言不随本机漂。
        let c = ShellCatalog::from_parts_with_probe(
            vec![custom("mine", "/opt/mine")],
            "",
            &probe_all_present,
        );
        let e = c.resolve(Some("nope")).unwrap_err();
        assert!(e.contains("nope"), "错误要点名请求的那个 id：{e}");
        assert!(
            e.contains("bash") && e.contains("mine"),
            "错误要列出预设与自定义：{e}"
        );
    }

    // ———————— validate_custom：归一化与拒绝 ————————

    #[test]
    fn validate_custom_trims_and_normalizes() {
        let got = validate_custom(ShellProfile {
            id: " my-shell ".into(),
            label: " 我编的 ".into(),
            program: " /opt/my shell ".into(),
            args: vec![" -c ".into(), "--flag".into()],
        })
        .expect("合法形状必须通过");
        assert_eq!(got.id, "my-shell");
        assert_eq!(got.label, "我编的");
        assert_eq!(got.program, "/opt/my shell");
        assert_eq!(got.args, vec!["-c", "--flag"]);

        // 幂等：归一后的输出再过一遍结果不变。
        let again = validate_custom(got.clone()).unwrap();
        assert_eq!(again, got);
    }

    #[test]
    fn validate_custom_rejects_bad_shapes_without_touching_the_filesystem() {
        for (bad, why) in [
            (
                ShellProfile {
                    id: "  ".into(),
                    label: String::new(),
                    program: "x".into(),
                    args: vec![],
                },
                "空 id",
            ),
            (
                ShellProfile {
                    id: "a".into(),
                    label: String::new(),
                    program: "  ".into(),
                    args: vec![],
                },
                "空 program",
            ),
            (
                ShellProfile {
                    id: "a".into(),
                    label: String::new(),
                    program: "x".into(),
                    args: vec!["  ".into()],
                },
                "空参数项",
            ),
            (
                ShellProfile {
                    id: "a".into(),
                    label: String::new(),
                    program: "x".into(),
                    args: vec!["-c extra".into()],
                },
                "含空白的参数项",
            ),
        ] {
            let e = validate_custom(bad).unwrap_err();
            assert!(!e.is_empty(), "{why} 必须给出原因，实得空错误");
        }
    }

    // ———————— ensure_executable（注入 search_dirs，不依赖本机 PATH） ————————

    #[cfg(unix)]
    #[test]
    fn ensure_executable_checks_the_file_itself_on_direct_paths() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let bare = dir.path().join("tool");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();

        std::fs::set_permissions(&bare, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = ensure_executable(bare.to_str().unwrap(), &[]).unwrap_err();
        assert!(e.contains("可执行位"), "无 x 位必须拒：{e}");

        std::fs::set_permissions(&bare, std::fs::Permissions::from_mode(0o755)).unwrap();
        let got = ensure_executable(bare.to_str().unwrap(), &[]).expect("有 x 位必须过");
        assert_eq!(got, bare);
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_executable_searches_the_injected_dirs_for_bare_names() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let hit = dir.path().join("mysh");
        std::fs::write(&hit, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&hit, std::fs::Permissions::from_mode(0o755)).unwrap();
        let empty = tempfile::tempdir().unwrap();

        let found = ensure_executable(
            "mysh",
            &[empty.path().to_path_buf(), dir.path().to_path_buf()],
        )
        .expect("第二个搜索目录里的命中必须被找到");
        assert_eq!(found, hit);
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(empty).ok();
    }

    #[test]
    fn ensure_executable_reports_misses_fail_closed() {
        let empty = tempfile::tempdir().unwrap();
        let e =
            ensure_executable("definitely-not-here", &[empty.path().to_path_buf()]).unwrap_err();
        assert!(
            e.contains("definitely-not-here") && e.contains("搜索目录"),
            "错误要点名 program 与搜索范围：{e}"
        );

        // 直接路径不存在 → 同样报错（执行前强制的失败面）。
        let missing = empty.path().join("nope.bin");
        assert!(ensure_executable(missing.to_str().unwrap(), &[]).is_err());
        std::fs::remove_dir_all(empty).ok();
    }

    #[cfg(windows)]
    #[test]
    fn ensure_executable_on_windows_requires_an_executable_extension() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("tool.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        assert!(ensure_executable(exe.to_str().unwrap(), &[]).is_ok());

        let txt = dir.path().join("tool.txt");
        std::fs::write(&txt, b"hi").unwrap();
        let e = ensure_executable(txt.to_str().unwrap(), &[]).unwrap_err();
        assert!(e.contains("扩展名"), "启发式判定要点明原因：{e}");
        std::fs::remove_dir_all(dir).ok();
    }

    // ———————— build_command：argv 形状与 cwd ————————

    #[test]
    fn build_command_appends_the_command_text_as_the_last_argv_element() {
        let profile = custom("mine", "/opt/mine");
        let cwd = std::env::current_dir().unwrap();
        let cmd = build_command(&profile, "echo hi", &cwd);
        let std_cmd = cmd.as_std();

        assert_eq!(
            std_cmd.get_program(),
            std::ffi::OsStr::new("/opt/mine"),
            "program 原样进 Command"
        );
        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        assert_eq!(args.len(), profile.args.len() + 1, "args 模板 + 命令文本");
        assert_eq!(
            args.last().copied(),
            Some(std::ffi::OsStr::new("echo hi")),
            "命令文本是最后一个元素，且是完整的一个 argv 元素"
        );
        assert_eq!(std_cmd.get_current_dir(), Some(cwd.as_path()));
    }

    // ———————— timeout_ms / command 的形状判定 ————————

    #[test]
    fn timeout_ms_of_defaults_clamps_and_rejects_bad_shapes() {
        let none = json!({});
        assert_eq!(timeout_ms_of(&none).unwrap(), DEFAULT_TIMEOUT_MS);
        let explicit_null = json!({"timeout_ms": null});
        assert_eq!(timeout_ms_of(&explicit_null).unwrap(), DEFAULT_TIMEOUT_MS);

        let big = json!({"timeout_ms": MAX_TIMEOUT_MS * 10});
        assert_eq!(
            timeout_ms_of(&big).unwrap(),
            MAX_TIMEOUT_MS,
            "夸大时限按钳位"
        );

        for bad in [
            json!({"timeout_ms": 0}),
            json!({"timeout_ms": -5}),
            json!({"timeout_ms": 1.5}),
            json!({"timeout_ms": "3000"}),
        ] {
            let e = timeout_ms_of(&bad).unwrap_err();
            assert!(
                matches!(e, ToolError::BadInput(_)),
                "{bad} 必须 BadInput，实得 {e:?}"
            );
        }
    }

    #[test]
    fn required_command_rejects_missing_blank_and_non_string() {
        for bad in [
            json!({}),
            json!({"command": null}),
            json!({"command": ""}),
            json!({"command": "   "}),
            json!({"command": 42}),
        ] {
            let e = required_command(&bad).unwrap_err();
            assert!(
                matches!(e, ToolError::BadInput(_)),
                "{bad} 必须 BadInput，实得 {e:?}"
            );
        }
        assert_eq!(
            required_command(&json!({"command": " ls "})).unwrap(),
            " ls "
        );
    }

    // ———————— 入参/前置检查的失败面（形状错误发生在任何 spawn 之前） ————————

    /// 用「当前测试可执行文件」当 program：它在两个平台上都存在且可执行
    /// （Unix 有 x 位；Windows 是 .exe），免掉平台夹具。下面的测试都只走到
    /// spawn 之前的检查就报错返回，这个 program 永远不会被真的启动。
    fn executable_self_profile(id: &str) -> ShellProfile {
        let exe = std::env::current_exe().expect("测试可执行文件路径");
        ShellProfile {
            id: id.into(),
            label: String::new(),
            program: exe.display().to_string(),
            args: vec!["-c".into()],
        }
    }

    #[tokio::test]
    async fn execute_rejects_bad_shapes_before_spawning() {
        let tool = ShellTool::new(catalog_with(vec![executable_self_profile("self")], "self"));
        // 没有 write_root：默认 cwd 落空必须 BadInput，而不是悄悄用进程 cwd。
        let ctx = test_ctx(None);
        for input in [
            json!({}),                                      // 缺 command
            json!({"command": "x", "shell_id": "nope"}),    // 未知 shell_id
            json!({"command": "x", "timeout_ms": 0}),       // 非法超时
            json!({"command": "x", "cwd": "relative/dir"}), // 相对 cwd
        ] {
            let e = tool.execute(input.clone(), &ctx).await.unwrap_err();
            assert!(
                matches!(e, ToolError::BadInput(_)),
                "{input} 必须 BadInput，实得 {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn execute_rejects_a_missing_cwd_as_bad_input() {
        let tool = ShellTool::new(catalog_with(vec![executable_self_profile("self")], "self"));
        let ctx = test_ctx(Some(std::env::temp_dir().join("qwen-do-shell-no-such-dir")));
        let e = tool
            .execute(json!({"command": "x", "cwd": "/definitely/not/here"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(e, ToolError::BadInput(ref m) if m.contains("cwd")),
            "不存在的 cwd 必须报 cwd 的原因，实得 {e:?}"
        );
    }

    #[tokio::test]
    async fn execute_honours_cancel_before_anything() {
        let tool = ShellTool::new(catalog_with(Vec::new(), ""));
        let ctx = test_ctx(None);
        ctx.cancel.cancel();
        let e = tool.execute(json!({"command": "echo hi"}), &ctx).await;
        assert!(
            matches!(e, Err(ToolError::Cancelled)),
            "取消必须最先查，实得 {e:?}"
        );
    }

    // ———————— 执行路径（spec §7/§9；unix 真进程 /bin/sh，Windows 真机项按 spec §10 记账） ————————

    /// 执行路径测试的夹具：unix 上必有的 `/bin/sh` 走自定义 profile（预设的 bash
    /// 裸名要按 PATH 解析，测试不该依赖本机装了什么——R305 同口径）。
    #[cfg(unix)]
    fn sh_profile() -> ShellProfile {
        ShellProfile {
            id: "sh-test".into(),
            label: "测试 sh".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into()],
        }
    }

    #[cfg(unix)]
    fn sh_tool() -> ShellTool {
        ShellTool::new(catalog_with(vec![sh_profile()], "sh-test"))
    }

    #[cfg(unix)]
    fn sh_ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let ctx = test_ctx(Some(dir.path().to_path_buf()));
        (dir, ctx)
    }

    #[cfg(unix)]
    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("run_command 恒返回文本，实得 {other:?}"),
        }
    }

    #[cfg(unix)]
    fn ps_args_listing() -> String {
        let out = std::process::Command::new("ps")
            .args(["-eo", "args="])
            .output()
            .expect("ps 必须可用");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_runs_a_command_and_returns_its_output() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        let text = text_of(
            tool.execute(json!({"command": "echo hi"}), &ctx)
                .await
                .expect("echo 必须成功"),
        );
        assert!(text.contains("hi"), "输出要含命令产出：{text:?}");
        assert!(!text.contains("[退出码"), "退出 0 不挂退出码行：{text:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_zero_exit_is_ok_with_an_exit_code_line() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        let text = text_of(
            tool.execute(json!({"command": "exit 3"}), &ctx)
                .await
                .expect("非零退出不是工具错误（spec §2）"),
        );
        assert!(text.contains("[退出码 3]"), "尾部要挂退出码行：{text:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_comes_first_then_the_stderr_section() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        let text = text_of(
            tool.execute(json!({"command": "echo out-line; echo err-line >&2"}), &ctx)
                .await
                .expect("必须成功"),
        );
        let out_at = text.find("out-line").expect("stdout 要在：{text:?}");
        let section_at = text
            .find("[stderr]")
            .expect("stderr 非空要有 [stderr] 段：{text:?}");
        let err_at = text.find("err-line").expect("stderr 内容要在：{text:?}");
        assert!(
            out_at < section_at && section_at < err_at,
            "spec §7.6 的编排顺序——stdout、[stderr] 段、stderr 内容：{text:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn empty_output_with_exit_zero_says_so() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        let text = text_of(
            tool.execute(json!({"command": "true"}), &ctx)
                .await
                .expect("必须成功"),
        );
        assert_eq!(text, "[命令成功，没有输出]");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_tree_and_names_the_milliseconds() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        // 标记放进命令文本：它会出现在 `/bin/sh -c …` 的 argv 里，ps 抓得到；
        // `#` 之后是 sh 的注释，不影响命令语义。
        const MARKER: &str = "shell-core-timeout-7f3a-marker";
        let e = tool
            .execute(
                json!({"command": format!("sleep 5 #{MARKER}"), "timeout_ms": 50}),
                &ctx,
            )
            .await
            .unwrap_err();
        match &e {
            ToolError::Failed(msg) => assert!(
                msg.contains("超时") && msg.contains("50"),
                "超时错误要含超时毫秒值：{msg}"
            ),
            other => panic!("超时必须是 Failed，实得 {other:?}"),
        }
        // 进程树被回收：带标记的 sh（连同它启动的 sleep）不许残留。轮询到消失或
        // 超期为止，不把「还没来得及收尸」误判成「泄漏」。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let listing = ps_args_listing();
            if !listing.lines().any(|l| l.contains(MARKER)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "超时后仍见残留进程树：{listing}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_before_spawn_wins_and_never_touches_the_factory() {
        // 直接打可测缝：注入「一被调用就记账」的假工厂。取消先于工厂 ⇒ 更先于
        // spawn——错误次序就是「没有进程被启动」的佐证。
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let factory_flag = Arc::clone(&called);
        let factory = move || -> Result<KillOnCloseJob, String> {
            factory_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Err("工厂不该被调到".to_string())
        };
        let e = run_with_job_factory(
            &factory,
            &sh_profile(),
            "echo hi",
            dir.path(),
            1_000,
            &token,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(e, ToolError::Cancelled),
            "spawn 前取消必须 Cancelled，实得 {e:?}"
        );
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "取消时连 job 工厂都不该被调（进程根本没起）"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_during_the_wait_kills_the_command_and_reports_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });
        let ok_factory = || -> Result<KillOnCloseJob, String> {
            Ok(create_kill_job().expect("unix 支建 job 恒成功"))
        };
        let e = run_with_job_factory(
            &ok_factory,
            &sh_profile(),
            "sleep 5",
            dir.path(),
            30_000,
            &token,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(e, ToolError::Cancelled),
            "等待中的取消必须 Cancelled，实得 {e:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_job_factory_blocks_the_spawn_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-never-be-created");
        let factory = || -> Result<KillOnCloseJob, String> {
            Err("无法建立进程回收作业对象（测试注入的失败）".to_string())
        };
        let e = run_with_job_factory(
            &factory,
            &sh_profile(),
            &format!("touch {}", marker.display()),
            dir.path(),
            1_000,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        match &e {
            ToolError::Failed(msg) => {
                assert!(msg.contains("进程回收"), "错误要透传工厂的失败原因：{msg}")
            }
            other => panic!("工厂失败必须是 Failed，实得 {other:?}"),
        }
        assert!(
            !marker.exists(),
            "R323：拿不到 job 不许 spawn——marker 文件不该出现"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_program_fails_closed_naming_the_program() {
        let (_dir, ctx) = sh_ctx();
        let tool = ShellTool::new(catalog_with(
            vec![ShellProfile {
                id: "ghost".into(),
                label: String::new(),
                program: "/definitely/not/here/sh".into(),
                args: vec!["-c".into()],
            }],
            "ghost",
        ));
        let e = tool
            .execute(json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap_err();
        match &e {
            ToolError::Failed(msg) => assert!(
                msg.contains("/definitely/not/here/sh"),
                "Failed 要点名 program：{msg}"
            ),
            other => panic!("程序缺失必须是 Failed，实得 {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_default_cwd_is_bad_input_not_a_silent_process_cwd() {
        let tool = sh_tool();
        // 没给 cwd 且 write_root 为 None → BadInput（回落进程 cwd 是不可复现行为，不做）。
        let ctx = test_ctx(None);
        let e = tool
            .execute(json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(&e, ToolError::BadInput(m) if m.contains("cwd")),
            "缺省 cwd 落空必须 BadInput 并点名 cwd，实得 {e:?}"
        );
    }

    // ———————— 输出上限与截断标记（spec §7.5） ————————

    #[cfg(unix)]
    #[tokio::test]
    async fn output_over_the_cap_is_truncated_with_an_honest_marker_and_the_command_finishes() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        // 600_000 个 'a'（> 512 KiB）：/dev/zero 管进 tr，输出体积精确可控。
        let text = text_of(
            tool.execute(
                json!({"command": "head -c 600000 /dev/zero | tr '\\000' a"}),
                &ctx,
            )
            .await
            .expect("超限不是工具错误：命令要能跑完并 Ok 返回"),
        );
        assert!(
            text.len() <= MAX_OUTPUT_BYTES,
            "交出文本 ≤ {MAX_OUTPUT_BYTES} 字节：实得 {}",
            text.len()
        );
        assert!(text.starts_with("aaaa"), "交出的应是产出的前缀");
        let tail: String = text
            .chars()
            .rev()
            .take(200)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        assert!(
            text.contains("512 KiB"),
            "标记要写明上限（512 KiB）。文本尾部：…{tail}"
        );
        let expected_dropped = 600_000 - (MAX_OUTPUT_BYTES - MARKER_RESERVE);
        assert!(
            text.contains(&expected_dropped.to_string()),
            "标记要含实测丢弃字节数 {expected_dropped}。文本尾部：…{tail}"
        );
        assert!(
            !text.contains("[退出码"),
            "命令要正常跑完（退出 0，不因 reader 停读而卡死）。文本尾部：…{tail}"
        );
    }

    // ———————— 非 UTF-8 输出的 lossy 处理（spec §8.5） ————————

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_output_is_lossily_converted_with_an_explicit_notice() {
        let (_dir, ctx) = sh_ctx();
        let tool = sh_tool();
        // \377\376 是两个非法 UTF-8 字节（printf 八进制转义，POSIX sh 原生支持）。
        let text = text_of(
            tool.execute(json!({"command": "printf '\\377\\376'"}), &ctx)
                .await
                .expect("非 UTF-8 输出不是错误（spec §8.5，与 read_file 的拒绝刻意不同）"),
        );
        assert!(
            text.contains('\u{FFFD}'),
            "无效字节按替换字符转换：{text:?}"
        );
        assert!(text.contains("非 UTF-8"), "尾部要注明转换：{text:?}");
    }
}
