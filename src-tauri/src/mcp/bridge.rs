// MCP bridge for stdio child processes and streamable-http clients.
//
// 边界（R310，简报 §二）：这一层做到「**握手 + 数工具 + 状态**」为止。
// **不**实现 `tools::registry::Tool`、**不**调 `ToolRegistry::register`、**不**碰 `ipc.rs`：
// 注册是 T32 的活，桥的第一个消费者是 T35 的 `mcp_connect` 命令。`Connection::tools` 之所以
// 把 `rmcp::model::Tool` 的原值存下来，就是为了让 T32 不必为同一批工具重连一次（真 npx 的
// 一次握手是 30s 量级，那是实打实的浪费）。
// 也**不**做进程树回收（T34 的 JobObject）：`shutdown()` 只走 rmcp 自己的
// `close()` → `graceful_shutdown()` 那条链，见那里的注释。
//
// spawn 的两条红线：R311（永不拼命令行、永不包一层 shell）与 R306（`-y` 的注入条件与位置）。

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};

use crate::agent::types::ToolOutput;
use crate::mcp::config::ServerDef;
use crate::tools::registry::ToolError;

/// 一条 stdio server 的连接超时（R308）。真 npx 第一次要下载包，30s 级，所以这条给到 60。
pub const CONNECT_TIMEOUT_SECS: u64 = 60;

/// Windows 的 `CREATE_NO_WINDOW`（R307）。
///
/// **诚实边界**：这个标志给**直接子进程**一个「有控制台但无窗口」的状态，孙进程按
/// 控制台继承规则共用它 —— 这句是**判读不是实测**（实测路径在简报 R307，取不到就标未实测，
/// 不许把判读写成实测）。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// R294 那颗打码常数在本文件的副本（`config.rs` 里的那颗是私有的）：`Debug` 里 env 的值
/// 一律打这个，绝不打「前 4 + … + 后 4」—— 那 8 个字符对第三方令牌是真泄露。
const MASKED: &str = "***";

/// `reason` 的长度上限，按 `chars()` 计（R308：不许按字节切，中文错误消息会让 `&s[..200]`
/// 在字符中间炸 —— T30 的 R301 同款教训）。plan Step 2 那句 `err.len() < 400` 保留为
/// **下限更严**的断言（200 ⇒ 一定 < 400），不反过来放宽。
const REASON_LIMIT: usize = 200;

/// 会被注入 `-y` 的 program **基名**（R306，小写比较）。给 `python`/`uvx` 加 `-y` 会把人家打歪，
/// 所以这张表是白名单而不是「见 args 就加」。
const YES_INJECTING_BINARIES: &[&str] = &["npx", "bunx"];

/// `PATHEXT` 未设时的缺省串（R305，逐字取 Windows 文档那四条）。
#[cfg(windows)]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

// ───────────────────────────── spawn 选型（纯函数，可注入） ─────────────────────────────

/// 选型结果（R304）。**不是** `tokio::process::Command`：plan :864 自己承认
/// `get_creation_flags()` 不是 tokio Command 的公开方法 ⇒ 返回 `Command` 就没法断言。
/// 这个结构是「选型**决定**了什么」的载体，真 spawn 的唯一出口是 `into_command()`。
pub struct SpawnPlan {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    creation_flags: u32,
}

impl SpawnPlan {
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// 返回 `Vec<&str>`，与 T29 `ServerDef::args()` 同一套访问器形状（R291 的理由在这儿同样成立：
    /// 测试写 `assert_eq!(plan.args(), vec!["-y", …])` 靠的是 `Vec<&str> == Vec<&str>`）。
    pub fn args(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// **只交键名，永远不交值**（R304）：env 的值不给外部，`Debug` 里同样只出键名。
    pub fn env_keys(&self) -> Vec<&str> {
        self.env.iter().map(|(k, _)| k.as_str()).collect()
    }

    pub fn creation_flags(&self) -> u32 {
        self.creation_flags
    }

    /// 真 spawn 的唯一出口（R304）。Windows 上在这里、也只有在这里把 `creation_flags`
    /// 交出去 —— 单测断言的是上面那个结构，这一句是它唯一的消费者。
    pub(crate) fn into_command(self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        // R311：argv **逐段** `.arg()`。不拼命令行、不 `join` 成一个字符串、
        // 也不 `cmd` 加斜杠开关包一层 —— T4 findings :635 钉死：绝对路径的 `.cmd`
        // **并不免掉** `cmd.exe`（`CreateProcess` 会为 `.cmd` 另起一个 `cmd.exe` 宿主，
        // 三层树与显式包装同形），C 相对 B 的唯一真实优势就是「整条命令行不是我们手搓拼接的」。
        // 所以本轮连那唯一一点优势也不放弃：任何 shell 包装都算违规。
        for arg in &self.args {
            cmd.arg(arg);
        }
        // env 只**增补**配置里那几条，不清空继承的环境（子进程还要 `PATH` 才能找到自己的运行时）。
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        #[cfg(windows)]
        {
            // 不需要 `CommandExt`：tokio 的 `Command` 自带固有方法 `creation_flags`。
            cmd.creation_flags(self.creation_flags);
        }
        cmd
    }
}

/// 手写 `Debug`（R304，沿用 T29 R294 的教训）：env 的值一律打 `***`、只打键名。
/// `program`/`args` 原样 —— 它们在 T29 就是刻意不脱敏的（R295），遮它们只会让排查变难。
impl std::fmt::Debug for SpawnPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut masked: Vec<String> = Vec::with_capacity(self.env.len());
        for (key, _value) in &self.env {
            masked.push(format!("{key}: \"{MASKED}\""));
        }
        f.debug_struct("SpawnPlan")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &masked)
            .field("creation_flags", &self.creation_flags)
            .finish()
    }
}

/// 把一条 `ServerDef` 选成一份 `SpawnPlan`：**纯函数**，不 spawn、不碰全局状态。
///
/// 第二个参数是解析器的注入点（R305）：生产传 `resolve_program_in_path`（读 `PATH` 与
/// `PATHEXT`、用 `Path::is_file()` 落判），测试传一张假表 ⇒ 选型逻辑可测，且**单测不依赖
/// 本机装没装 node**（与「单测不碰网络」同一条纪律）。不启用 rmcp 的 `which-command`：
/// 它是 feature 门控的（`transport.rs:86`），三条 feature 里没有它，而把它打开等于把
/// 「本机 PATH 里有什么」变成单测的前提。
pub fn spawn_command(
    def: &ServerDef,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<SpawnPlan, String> {
    let Some(command) = def.command() else {
        return Err(format!(
            "{} 是 HTTP 传输：HTTP server 不走子进程（应由 connect_http 处理）",
            def.name()
        ));
    };
    // 所有出口都过 `trim_reason`（R308）：`reason` 会原样进状态台账、再从 T35 的界面出去，
    // 这里尤其要过——错误文案里嵌着用户给的 `command`，它可以任意长。
    let program = select_program(command, resolve).map_err(trim_reason)?;
    let mut args: Vec<String> = def.args().into_iter().map(String::from).collect();
    inject_yes_if_wanted(&program, &mut args);
    // env 按键名排序 ⇒ `SpawnPlan` 的 `Debug` 与 `into_command` 的增补序都是确定的
    // （`HashMap` 的迭代序不定，不排序的话同一条配置两次 `{:?}` 会长得不一样）。
    let mut env: Vec<(String, String)> = match def {
        ServerDef::Stdio { env, .. } => env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        ServerDef::Http { .. } => Vec::new(),
    };
    env.sort();
    Ok(SpawnPlan {
        program,
        args,
        env,
        creation_flags: CREATE_NO_WINDOW,
    })
}

fn http_transport_config(def: &ServerDef) -> Result<StreamableHttpClientTransportConfig, String> {
    let ServerDef::Http { url, headers, .. } = def else {
        return Err("不是 HTTP server".to_string());
    };
    let url = url.trim();
    let parsed =
        url::Url::parse(url).map_err(|_| "URL 无效：需要完整的 http(s) 地址".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("URL 无效：只接受带主机名的 http(s) 地址".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL 不要带用户名/密码；凭据请放在 headers 里".to_string());
    }
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if parsed.scheme() == "http" && !headers.is_empty() && !loopback {
        return Err("明文 http 只允许在回环地址携带 headers；远程 server 请改用 https".to_string());
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    config.allow_stateless = true;
    for (key, value) in headers {
        let name = key
            .parse()
            .map_err(|_| format!("header `{key}` 名称无效"))?;
        let value = value
            .parse()
            .map_err(|_| format!("header `{key}` 值无效"))?;
        config.custom_headers.insert(name, value);
    }
    Ok(config)
}

/// 走查还是直接用（R305）：`def.command` 本身已含路径分隔符、或已带可执行扩展名 ⇒
/// 调用方已经把「起哪个文件」说清楚了，直接用、不走查。
fn select_program(
    command: &str,
    resolve: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    if is_already_addressed(command) {
        return Ok(PathBuf::from(command));
    }
    match resolve(command) {
        Some(program) => Ok(program),
        // 错误里带**程序名**：程序名不是秘密，`command`/`args` 在 T29 就是刻意不脱敏的（R295）。
        None => Err(format!(
            "找不到可执行文件 `{command}`：PATH 走查里没有带扩展名的候选"
        )),
    }
}

/// 「已经说清楚了」的两种形态（R305）。分隔符按 Windows 与 unix 两边都算，
/// 因为配置是用户手抄的，`./x`、`C:\\tools\\x.cmd`、`/usr/local/bin/x` 都真实存在。
fn is_already_addressed(command: &str) -> bool {
    command.contains('/') || command.contains('\\') || has_executable_extension(command)
}

/// 只看「扩展名是不是一个可执行扩展名」，不看有没有路径分隔符（后者由 `is_already_addressed`
/// 自己判）。unix 上恒 `false`：那里的可执行位不在扩展名上，一律交给走查。
fn has_executable_extension(command: &str) -> bool {
    let Some(extension) = Path::new(command).extension() else {
        return false;
    };
    let extension = extension.to_string_lossy().to_ascii_lowercase();
    #[cfg(windows)]
    {
        pathext()
            .iter()
            .any(|candidate| candidate == &format!(".{extension}"))
    }
    #[cfg(not(windows))]
    {
        let _ = extension;
        false
    }
}

/// PATH 走查的**纯内核**（R305）：候选 = `dirs` 每个目录 × `exts` 每个扩展名，
/// 取第一个 `is_file()` 的。生产用 `PATH`/`PATHEXT`（见 `resolve_program_in_path`），
/// 测试传假表 —— 目录表与扩展名表都是参数，不是环境变量。
///
/// **只接受带扩展名的候选**（Windows 支）：T4 实测 `where npx` 给出 `…\npx` 与 `…\npx.cmd`
/// 两条，起得来的是后者；无扩展名那条是个 sh 脚本，`CreateProcess` 不认
/// （A 支原文 `NotFound … program not found`）。这一条判据写在循环**里面**，
/// 所以就算调用方把空扩展名塞进表里也不会退化成 A 支。
///
/// unix 支只走 `PATH` + `is_file()`（`exts` 给一格空串就是「裸名」）：**注释明写**
/// 「本项目发布目标只有 Windows，unix 这支是为了让 `cargo test` 在开发机上不骗人」。
fn walk_path(command: &str, dirs: &[PathBuf], exts: &[String]) -> Option<PathBuf> {
    for dir in dirs {
        for ext in exts {
            if ext.is_empty() && cfg!(windows) {
                continue;
            }
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 生产解析器：读 `PATH`（未设 ⇒ 无候选）与 `PATHEXT`（未设 ⇒ `DEFAULT_PATHEXT`）。
pub fn resolve_program_in_path(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    walk_path(command, &dirs, &pathext())
}

/// 候选扩展名表，全部**小写、带点**，供 `walk_path` 直接拼名字。
/// Windows 读 `PATHEXT`，未设时用 `.COM;.EXE;.BAT;.CMD`（R305）；unix 返回一格空串 = 裸名。
fn pathext() -> Vec<String> {
    #[cfg(windows)]
    {
        let raw = std::env::var_os("PATHEXT")
            .unwrap_or_else(|| std::ffi::OsString::from(DEFAULT_PATHEXT));
        let mut out: Vec<String> = raw
            .to_string_lossy()
            .split(';')
            .map(|e| e.trim())
            .filter(|e| !e.is_empty())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                if e.starts_with('.') {
                    e
                } else {
                    format!(".{e}")
                }
            })
            .collect();
        out.dedup();
        out
    }
    #[cfg(not(windows))]
    {
        vec![String::new()]
    }
}

/// R306：`-y` 的注入条件与位置。
///
/// 条件：只对解析后 **program 的基名**是 `npx`/`bunx` 的条目注入 —— 给 `python`/`uvx`
/// 加 `-y` 会把人家打歪（它们各自的 `-y` 是别的或不存在的意思）。
/// 位置：**插在 args 最前面**，紧贴 program —— `npx -y pkg` 与 `npx pkg -y` **不等价**，
/// 后者那个 `-y` 是传给包自己的。
/// 去重：用户已经写了 `-y` 或 `--yes` ⇒ 不重复注入（也不搬家 —— 他写在哪就是哪，
/// 那是显式意图，桥不替用户改语义）。
///
/// 为什么要注入：不带 `-y` 时 npx 会在 stdin 上弹「要装吗?」的交互确认，而 stdio 传输里
/// 那个 stdin 归 MCP 服务器用 ⇒ **卡死**（T4 findings「`-y` 是必须的」那条实测）。
/// 桌面端弹一个安装确认等同于弹窗，而零弹窗是验收线。
fn inject_yes_if_wanted(program: &Path, args: &mut Vec<String>) {
    if !wants_yes(program) {
        return;
    }
    if args.iter().any(|a| a == "-y" || a == "--yes") {
        return;
    }
    args.insert(0, "-y".to_string());
}

/// 基名（去扩展名）在白名单上吗（R306）。`npx.cmd`/`Npx.EXE` 都算，`npxpy` 不算。
fn wants_yes(program: &Path) -> bool {
    let Some(stem) = program.file_stem() else {
        return false;
    };
    let stem = stem.to_string_lossy().to_ascii_lowercase();
    YES_INJECTING_BINARIES.contains(&stem.as_str())
}

/// `reason` 的唯一出口（R308）：按 `chars()` 截到 [`REASON_LIMIT`]，**不许**按字节切。
/// 截断时留一格给省略号 ⇒ 结果**含省略号**也不超过 200（199 + '…'），这才是「上限 200」的字面意思。
fn trim_reason(raw: String) -> String {
    if raw.chars().count() <= REASON_LIMIT {
        return raw;
    }
    let mut kept: String = raw.chars().take(REASON_LIMIT - 1).collect();
    kept.push('…');
    kept
}

// ───────────────────────────── 结果映射（T32，R314–R319） ─────────────────────────────

/// 把 MCP 的 `CallToolResult` 映射成本项目的 [`ToolOutput`]（T32 / spec 5.2 第 2 点）。
///
/// 三条不变量：
/// 1. **混合数组不摊平**（R317）：≥2 块 ⇒ `ToolOutput::Multi`，1 块 ⇒ 直接是那一块，
///    0 块 ⇒ 一句可读的占位（下一条 tool 消息不能是空的）。
/// 2. **图片走同一条归一化**（R318）：base64 解码后交给 `image_prep::normalize_image`，
///    与贴图侧完全同一个入口；解不开就变成一句「图片无法读取」的文本，**不许静默丢掉**。
///    gif/webp 因此被收窄成 png/jpeg —— 这也是本轮**不扩 `ImageMime`** 的理由（R314）。
/// 3. **server 说 isError ⇒ `ToolError::Server`**（R316）：Display 就是 server 的原话，
///    不套「工具执行失败: 」（R59 的同一个理由：模型该读到上游给的那句话）。
pub fn map_result(result: rmcp::model::CallToolResult) -> Result<ToolOutput, ToolError> {
    let mut parts: Vec<ToolOutput> = Vec::with_capacity(result.content.len());
    for block in result.content {
        parts.push(map_block(block));
    }
    if result.is_error == Some(true) {
        let message = parts
            .iter()
            .map(flatten_text)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(ToolError::Server(if message.trim().is_empty() {
            "（工具报错但没有给出内容）".to_string()
        } else {
            message
        }));
    }
    Ok(match parts.len() {
        0 => ToolOutput::text("（工具返回了空结果）"),
        1 => parts.pop().expect("刚判过 len == 1"),
        _ => ToolOutput::Multi { parts },
    })
}

fn map_block(block: rmcp::model::ContentBlock) -> ToolOutput {
    use rmcp::model::ContentBlock as Mcp;
    match block {
        Mcp::Text(t) => ToolOutput::Text { text: t.text },
        Mcp::Image(img) => image_part(&img.data),
        Mcp::Audio(_) => ToolOutput::text("工具返回了音频，当前不播放（本版本只处理文本与图片）。"),
        Mcp::Resource(res) => ToolOutput::text(resource_text(&res.resource)),
        // resource_link 只渲染，不自动抓取：抓取是 `fetch_url` 的活，自动抓等于替模型做 SSRF 决定。
        Mcp::ResourceLink(link) => ToolOutput::text(format!("[链接] {} {}", link.name, link.uri)),
        // rmcp 的 `ContentBlock` 是 `#[non_exhaustive]`：上游加一种新块时不许 panic，也不许静默吞。
        _ => ToolOutput::text("工具返回了本版本不认识的内容块（已保留一句说明以免静默丢失）。"),
    }
}

/// base64 → 字节 → 与贴图同一条归一化（R318）。每一支失败都给一句能归因的文本。
fn image_part(base64_data: &str) -> ToolOutput {
    use base64::Engine as _;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(base64_data.as_bytes()) {
        Ok(bytes) => bytes,
        Err(e) => return ToolOutput::text(format!("图片无法读取：base64 解码失败（{e}）")),
    };
    match crate::image_prep::normalize_image(&bytes) {
        Ok(crate::agent::types::ContentBlock::Image { data, mime }) => {
            ToolOutput::Image { data, mime }
        }
        Ok(_) => ToolOutput::text("图片无法读取：归一化没有给出图片"),
        Err(e) => ToolOutput::text(format!("图片无法读取：{e}")),
    }
}

fn resource_text(resource: &rmcp::model::ResourceContents) -> String {
    match resource {
        rmcp::model::ResourceContents::TextResourceContents { uri, text, .. } => {
            format!("[{uri}]\n{text}")
        }
        rmcp::model::ResourceContents::BlobResourceContents { uri, mime_type, .. } => format!(
            "[{uri}] 二进制资源（{}），未展开",
            mime_type.as_deref().unwrap_or("未知类型")
        ),
        // `ResourceContents` 同样是 `#[non_exhaustive]`。
        _ => "资源内容（本版本不认识的内容类型），未展开".to_string(),
    }
}

/// 把多块输出摊平成文本：事件流预览之外的**协议层**用途（例如把 isError 的内容变成错误消息）。
pub(crate) fn flatten_text(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Text { text } => text.clone(),
        ToolOutput::Image { mime, .. } => format!("[图片 {mime:?}]"),
        ToolOutput::Multi { parts } => parts
            .iter()
            .map(flatten_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// ───────────────────────────── 工具适配（T32 / R315） ─────────────────────────────

/// 把一条 MCP 工具声明包成注册表认得的 [`Tool`]。
///
/// 三件事只在这里做一次：命名走 T30 的 [`crate::mcp::naming::namespaced`]（模型端看到的是
/// `mcp__{server}__{tool}`）；风险恒为 [`Risk::Read`]（R315——「添加 server 默认 Ask」是
/// `gate` 的信任策略，是 T33 的事，不是工具自己的风险等级）；参数对象原样转交 server。
pub struct McpTool {
    server: String,
    original_name: String,
    name: String,
    description: String,
    input_schema: serde_json::Value,
    bridge: std::sync::Arc<McpBridge>,
}

impl McpTool {
    pub fn from_decl(
        server: &str,
        tool: &rmcp::model::Tool,
        bridge: std::sync::Arc<McpBridge>,
    ) -> Self {
        Self {
            server: server.to_string(),
            original_name: tool.name.to_string(),
            name: crate::mcp::naming::namespaced(server, &tool.name),
            description: tool.description.as_deref().unwrap_or_default().to_string(),
            input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
            bridge,
        }
    }

    /// 模型端看到的注册名（T30 命名层的输出）。gate 的 alias 表拿它当键
    /// （`ipc::mcp_register_tools` 注册期喂入「注册名 → 真名」），别处仍不许
    /// 拿名字反推 (server, tool) —— 兜底形状是有损的。
    pub fn registered_name(&self) -> &str {
        &self.name
    }
}

#[async_trait::async_trait]
impl crate::tools::registry::Tool for McpTool {
    fn meta(&self) -> crate::tools::registry::ToolMeta {
        crate::tools::registry::ToolMeta {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            risk: crate::agent::types::Risk::Read,
        }
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &crate::tools::registry::ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let arguments = match input {
            serde_json::Value::Object(map) => map,
            other => {
                return Err(ToolError::BadInput(format!(
                    "MCP 工具的参数必须是 JSON 对象，收到 {other}"
                )));
            }
        };
        // 取消臂：`call_tool` 可能在等一个慢 server，而用户按了停止（R43 的同一条纪律）。
        let result = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            r = self.bridge.call_tool(&self.server, &self.original_name, arguments) => r?,
        };
        map_result(result)
    }
}

// ───────────────────────────── 状态台账与连接（R308/R309/R310） ─────────────────────────────

/// 一条 server 在桥里的状态（plan :847 的接口）。`Debug` 手写：`reason` 直接打、别的臂只打臂名
/// （R308：`reason` 已经是可上 UI 的成品，包一层没有信息量）。
#[derive(Clone, PartialEq)]
pub enum ServerStatus {
    Connecting,
    Ready { tools: usize },
    Disconnected { reason: String },
}

impl std::fmt::Debug for ServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerStatus::Connecting => f.write_str("Connecting"),
            ServerStatus::Ready { tools } => write!(f, "Ready {{ tools: {tools} }}"),
            ServerStatus::Disconnected { reason } => {
                write!(f, "Disconnected {{ reason: {reason:?} }}")
            }
        }
    }
}

/// 一次成功握手的产物（R310）：pid（内存管道没有）、原始工具声明（T32 直接拿去注册）、
/// 以及**必须被存住**的 service（drop 掉就等于关掉传输）。
pub struct Handshake {
    pub pid: Option<u32>,
    pub tools: Vec<rmcp::model::Tool>,
    pub service: RunningService<RoleClient, ()>,
    /// 这条连接专属的进程回收 job。重连一台 server 只关闭**它自己**的 job，
    /// 其余 server 的进程树不受影响（一个桥共用一个 job 时，换 job 会连带杀掉全部）。
    pub job: Option<std::sync::Arc<crate::mcp::process::KillOnCloseJob>>,
}

struct Connection {
    pid: Option<u32>,
    tools: Vec<rmcp::model::Tool>,
    service: RunningService<RoleClient, ()>,
    job: Option<std::sync::Arc<crate::mcp::process::KillOnCloseJob>>,
}

struct Slot {
    status: ServerStatus,
    conn: Option<Connection>,
}

/// MCP 连接的持有者（R309）。容器是 `BTreeMap`：状态列表与 `pids()` 的迭代序稳定，
/// 与 T29 的字典序、T30 R302 的「稳定喂入序」同一条链。
pub struct McpBridge {
    slots: std::sync::Mutex<std::collections::BTreeMap<String, Slot>>,
}

impl Default for McpBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl McpBridge {
    pub fn new() -> Self {
        Self {
            slots: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    pub(crate) fn set_status(&self, name: &str, status: ServerStatus) {
        let mut slots = crate::lock::guard(&self.slots);
        slots
            .entry(name.to_string())
            .or_insert_with(|| Slot {
                status: ServerStatus::Connecting,
                conn: None,
            })
            .status = status;
    }

    pub fn status_of(&self, server: &str) -> Option<ServerStatus> {
        crate::lock::guard(&self.slots)
            .get(server)
            .map(|slot| slot.status.clone())
    }

    /// 测试用具：某台 server 当前连接持有的 job 身份。
    /// 重连一台不许改变其他 server 的这个值 —— 换了 job 就等于回收了它们的进程树。
    #[cfg(test)]
    pub(crate) fn job_ptr_of(&self, server: &str) -> Option<usize> {
        crate::lock::guard(&self.slots)
            .get(server)
            .and_then(|slot| slot.conn.as_ref())
            .and_then(|conn| conn.job.as_ref())
            .map(|job| std::sync::Arc::as_ptr(job) as usize)
    }

    /// 真子进程的连接才有 pid（R309）。内存管道的腿**不许**造假 pid。
    pub fn pids(&self) -> Vec<(String, u32)> {
        crate::lock::guard(&self.slots)
            .iter()
            .filter_map(|(name, slot)| {
                slot.conn
                    .as_ref()
                    .and_then(|conn| conn.pid)
                    .map(|pid| (name.clone(), pid))
            })
            .collect()
    }

    /// T32 注册用的原始工具名（字典序，`BTreeMap` + `list_tools` 原序之外不再排序——
    /// 这里排一下只是为了让界面与日志可复现）。
    pub fn tool_names(&self, server: &str) -> Vec<String> {
        let mut names: Vec<String> = crate::lock::guard(&self.slots)
            .get(server)
            .and_then(|slot| slot.conn.as_ref())
            .map(|conn| conn.tools.iter().map(|t| t.name.to_string()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// 并行连接（R308）：结果顺序 = 输入顺序；一条失败不影响其余。
    pub async fn connect_all(&self, servers: &[ServerDef]) -> Vec<(String, Result<usize, String>)> {
        let names: Vec<String> = servers.iter().map(|s| s.name().to_string()).collect();

        // `'static` future：闭包按名字 clone 一份配置 ⇒ 返回的 future 不借用任何局部变量
        // （`ServerDef` 因此需要 `Clone`）。job 由 `connect_stdio` 按 server 各建一个：
        // 共享一个 job 时，任何一次「换 job」都会关闭旧句柄并杀掉所有 server 的进程树。
        let defs: std::collections::BTreeMap<String, ServerDef> = servers
            .iter()
            .map(|def| (def.name().to_string(), def.clone()))
            .collect();
        let make = move |name: &str| -> BoxFuture<'static, Result<Handshake, String>> {
            match defs.get(name).cloned() {
                Some(def) => Box::pin(async move { connect_def(&def).await }),
                None => Box::pin(async { Err("配置里没有这条 server".to_string()) }),
            }
        };
        self.connect_all_with(
            &names,
            std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
            &make,
        )
        .await
    }

    /// `connect_all` 的可注入内核（R308/R305）：超时与「怎么建一条连接」都是参数。
    pub async fn connect_all_with(
        &self,
        names: &[String],
        timeout: std::time::Duration,
        make: &(dyn Fn(&str) -> BoxFuture<'static, Result<Handshake, String>> + Sync),
    ) -> Vec<(String, Result<usize, String>)> {
        for name in names {
            self.set_status(name, ServerStatus::Connecting);
        }
        let attempts = futures::future::join_all(names.iter().map(|name| async move {
            match tokio::time::timeout(timeout, make(name)).await {
                Err(_) => Err(trim_reason(format!(
                    "连接 `{name}` 超时：{} 秒内没有完成握手",
                    timeout.as_secs_f32()
                ))),
                Ok(Err(reason)) => Err(trim_reason(reason)),
                Ok(Ok(handshake)) => Ok(handshake),
            }
        }))
        .await;

        let mut out = Vec::with_capacity(names.len());
        for (name, attempt) in names.iter().zip(attempts) {
            match attempt {
                Ok(handshake) => {
                    let tools = handshake.tools.len();
                    let conn = Connection {
                        pid: handshake.pid,
                        tools: handshake.tools,
                        service: handshake.service,
                        job: handshake.job,
                    };
                    // 成功重连 = **只**替换这一台的连接。旧连接在锁外优雅关停，随后
                    // 连同它自己的 job 一起 drop ⇒ 旧进程树被回收，其他 server 不受影响。
                    let old = {
                        let mut slots = crate::lock::guard(&self.slots);
                        let slot = slots.entry(name.clone()).or_insert_with(|| Slot {
                            status: ServerStatus::Ready { tools },
                            conn: None,
                        });
                        slot.status = ServerStatus::Ready { tools };
                        slot.conn.replace(conn)
                    };
                    if let Some(old) = old {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            old.service.cancel(),
                        )
                        .await;
                    }
                    out.push((name.clone(), Ok(tools)));
                }
                Err(reason) => {
                    // 先连、后换：连不上时旧连接与它的工具表都还在，状态必须继续说
                    // Ready。写成 Disconnected 会让界面谎报「这台 server 已经断开」，
                    // 而实际上调用仍然走得通（旧进程还在自己的 job 里）。
                    let kept_tools = crate::lock::guard(&self.slots)
                        .get(name)
                        .and_then(|slot| slot.conn.as_ref())
                        .map(|conn| conn.tools.len());
                    match kept_tools {
                        Some(tools) => self.set_status(name, ServerStatus::Ready { tools }),
                        None => self.set_status(
                            name,
                            ServerStatus::Disconnected {
                                reason: reason.clone(),
                            },
                        ),
                    }
                    out.push((name.clone(), Err(reason)));
                }
            }
        }
        out
    }

    /// 调一次远端工具（T32）。peer 从锁里克隆出来后就放锁：std `Mutex` 的 guard 不许跨 await。
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<rmcp::model::CallToolResult, ToolError> {
        let peer = {
            let slots = crate::lock::guard(&self.slots);
            slots
                .get(server)
                .and_then(|slot| slot.conn.as_ref())
                .map(|conn| conn.service.peer().clone())
        }
        .ok_or_else(|| {
            ToolError::Failed(format!("server `{server}` 当前没有连接，先连接再调用工具"))
        })?;
        let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
        params.arguments = Some(arguments);
        peer.call_tool(params)
            .await
            .map_err(|e| ToolError::Failed(format!("调用 `{tool}` 失败：{e}")))
    }

    /// T32 注册用的原始声明（克隆出来的 `rmcp::model::Tool`，含 description/inputSchema）。
    pub fn decls_of(&self, server: &str) -> Vec<rmcp::model::Tool> {
        crate::lock::guard(&self.slots)
            .get(server)
            .and_then(|slot| slot.conn.as_ref())
            .map(|conn| conn.tools.clone())
            .unwrap_or_default()
    }

    /// 断开一台 server（T35 的 `mcp_disconnect`）：先摘连接再优雅关停，状态落成 `Disconnected`。
    pub async fn disconnect(&self, server: &str) {
        let conn = {
            let mut slots = crate::lock::guard(&self.slots);
            slots.get_mut(server).and_then(|slot| slot.conn.take())
        };
        if let Some(conn) = conn {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.service.cancel())
                .await;
            // 显式关掉这一台 server 自己的 job：kill-on-close 带走整棵旧进程树。
            // 不依赖「conn 在函数末尾自然 drop」的时序（读起来也更像一条生命周期规则）。
            drop(conn.job);
        }
        self.set_status(
            server,
            ServerStatus::Disconnected {
                reason: "已断开".to_string(),
            },
        );
    }

    /// 主动关闭：把 service 从台账里取出来再 `cancel()`（R309：桥是持有者），
    /// 每条给 5 秒上限——`graceful_shutdown` 内部已经会等 stdin EOF，不许在这里无限等。
    /// **本次不做进程树回收**：`cancel()` 只打直接子进程，孙进程靠 stdin EOF 退出，
    /// 彻底回收是 T34 的 JobObject。
    pub async fn shutdown(&self) {
        let drained: Vec<(String, Connection)> = {
            let mut slots = crate::lock::guard(&self.slots);
            let mut out = Vec::new();
            for (name, slot) in slots.iter_mut() {
                if let Some(conn) = slot.conn.take() {
                    out.push((name.clone(), conn));
                }
            }
            out
        };
        for (name, conn) in drained {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.service.cancel())
                .await;
            drop(conn.job);
            self.set_status(
                &name,
                ServerStatus::Disconnected {
                    reason: "已关闭".to_string(),
                },
            );
        }
    }
}

async fn connect_def(def: &ServerDef) -> Result<Handshake, String> {
    match def {
        ServerDef::Stdio { .. } => connect_stdio(def).await,
        ServerDef::Http { .. } => connect_http(def).await,
    }
}

async fn connect_http(def: &ServerDef) -> Result<Handshake, String> {
    let config = http_transport_config(def)?;
    let transport = StreamableHttpClientTransport::<reqwest::Client>::from_config(config);
    let (tools, service) = handshake(transport).await?;
    Ok(Handshake {
        pid: None,
        tools,
        service,
        job: None,
    })
}

/// 真 stdio 连接（R309）：pid 必须在 `.serve()` **消费** transport 之前取出来
/// （`TokioChildProcess::id()`，rmcp `transport/child_process.rs:72`；3.5.1 没有 `process_handle()`）。
async fn connect_stdio(def: &ServerDef) -> Result<Handshake, String> {
    // 每台 server 一个 kill-on-close job：重连时换掉的是它自己的 job，不会连带
    // 关闭其他 server 共享的旧句柄（那会杀掉整片进程树）。建不出来 ⇒ 这条不连，
    // **绝不**退化成「不带 job 直接 spawn」（R323）。
    let job = std::sync::Arc::new(crate::mcp::process::create_kill_job().map_err(trim_reason)?);
    let plan = spawn_command(def, &resolve_program_in_path)?;
    let child = TokioChildProcess::new(plan.into_command())
        .map_err(|e| trim_reason(format!("启动 `{}` 失败：{e}", def.name())))?;
    let pid = child.id();
    // T34：pid 是唯一能把它放进 job 的时刻（`.serve()` 会消费 transport，之后拿不到）。
    // 放不进 job ⇒ 这条不连：一条不受管的长命 node 树比连不上更难收拾（R323）。
    if let Some(pid) = pid {
        job.assign(pid).map_err(trim_reason)?;
    }
    let (tools, service) = handshake(child).await?;
    Ok(Handshake {
        pid,
        tools,
        service,
        job: Some(job),
    })
}

/// 握手 + 列出工具（R310）。泛型是为了让它同时服务两条腿：
/// 真子进程（`TokioChildProcess`）与测试里的内存管道（任意 `AsyncRead + AsyncWrite`，
/// 由 rmcp 的 `IntoTransport` 兜底实现接住）——**真假两条腿走同一段握手代码**，
/// 差别只在 transport 是谁。
async fn handshake<T, E, A>(
    transport: T,
) -> Result<(Vec<rmcp::model::Tool>, RunningService<RoleClient, ()>), String>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let service = ().serve(transport).await.map_err(|e| trim_reason(format!("MCP 握手失败：{e}")))?;
    let listed = service
        .peer()
        .list_tools(Default::default())
        .await
        .map_err(|e| trim_reason(format!("列出工具失败：{e}")))?;
    Ok((listed.tools, service))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    use futures::future::BoxFuture;
    use rmcp::RoleServer;
    use rmcp::model::{
        ClientJsonRpcMessage, ClientRequest, InitializeResult, ListToolsResult, ServerCapabilities,
        ServerJsonRpcMessage, ServerResult, Tool,
    };
    use rmcp::transport::{IntoTransport, Transport};

    use crate::agent::types::ToolOutput;

    /// 1×1 透明 PNG（合法可解码，`normalize_image` 会原样保留 png）。
    const ONE_PX_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    /// 1×1 GIF（走 `normalize_image` 的「gif 取首帧 → 收窄成 jpeg」那一支）。
    const ONE_PX_GIF_B64: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

    fn call_result(content: Vec<rmcp::model::ContentBlock>) -> rmcp::model::CallToolResult {
        // `CallToolResult` 是 `#[non_exhaustive]`：不能结构体字面量，只能先 Default 再赋值。
        let mut result = rmcp::model::CallToolResult::default();
        result.content = content;
        result
    }

    /// 把任意形状的 `ToolOutput` 摊平成文本：只有断言用，不参与生产。
    fn render_for_assert(out: &ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text.clone(),
            ToolOutput::Image { mime, .. } => format!("[图片 {mime:?}]"),
            ToolOutput::Multi { parts } => parts
                .iter()
                .map(render_for_assert)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// 造一条 stdio 的 `ServerDef`（plan :855 的 `stdio(…)` 夹具形状，字段按 T29 R291 的**直接**构造）。
    fn stdio(name: &str, command: &str, args: &[&str]) -> ServerDef {
        ServerDef::Stdio {
            name: name.into(),
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: HashMap::new(),
        }
    }

    /// 在 `dir` 里造一个**真实存在**的空文件并返回其绝对路径：`is_file()` 只认真文件，
    /// 假表里的候选也要能在磁盘上被 `is_file()` 点到（R305 的走查内核不猜路径）。
    fn fixture_exec(dir: &std::path::Path, file_name: &str) -> PathBuf {
        let path = dir.join(file_name);
        std::fs::write(&path, b"@echo off\r\n").unwrap();
        path
    }

    // —— Step 1（plan :853-861 的形状，断言对象按 R304 换成 `SpawnPlan`）——
    #[test]
    fn npx_is_resolved_to_its_absolute_cmd_path_and_always_carries_y() {
        let dir = tempdir().unwrap();
        // 注入的假表（R305）：本机装没装 node 都不影响这条，单测也不碰网络。
        let cmd = fixture_exec(dir.path(), "npx.cmd");
        let table: BTreeMap<String, PathBuf> = BTreeMap::from([("npx".to_string(), cmd.clone())]);
        let resolve = |name: &str| table.get(name).cloned();

        // ① plan :855 逐字那条：用户已经写了 `-y`，位置也在最前面。
        let def = stdio("x", "npx", &["-y", "@modelcontextprotocol/server-memory"]);
        let plan = spawn_command(&def, &resolve).expect("注入表里有 npx.cmd，选型不该失败");
        assert!(
            plan.program().is_absolute(),
            "解析结果必须是绝对路径，裸 `npx` 在本机实测直接 program not found：{:?}",
            plan.program()
        );
        assert!(
            plan.program().to_string_lossy().ends_with(".cmd"),
            "`npx` 要解析到 `.cmd` 那条 shim（T4 实测 `where npx` 给两条，起得来的是 `.cmd`）：{:?}",
            plan.program()
        );
        assert_eq!(
            plan.args(),
            vec!["-y", "@modelcontextprotocol/server-memory"],
            "用户写了 -y ⇒ 不重复注入，且顺序原样"
        );

        // ② 用户**没**写 `-y` ⇒ 注入，且注入在**第一段**（R306：`npx pkg -y` 里那个 `-y`
        // 是传给包自己的，与 `npx -y pkg` 不等价）。
        let def = stdio("x", "npx", &["@modelcontextprotocol/server-memory"]);
        let plan = spawn_command(&def, &resolve).unwrap();
        assert_eq!(
            plan.args().first().copied(),
            Some("-y"),
            "`-y` 必须紧贴 program：{:?}",
            plan.args()
        );
        assert_eq!(
            plan.args(),
            vec!["-y", "@modelcontextprotocol/server-memory"],
            "注入只加一段，不许复制：{:?}",
            plan.args()
        );

        // ③ `--yes` 同义，也算「用户已经写了」。
        let def = stdio("x", "npx", &["--yes", "pkg"]);
        let plan = spawn_command(&def, &resolve).unwrap();
        assert_eq!(
            plan.args(),
            vec!["--yes", "pkg"],
            "--yes 不该再被 -y 顶一遍"
        );

        // ④ 零弹窗的另一半：`CREATE_NO_WINDOW` 恒置位（R307）。
        let def = stdio("x", "npx", &["pkg"]);
        let plan = spawn_command(&def, &resolve).unwrap();
        assert_eq!(
            plan.creation_flags() & CREATE_NO_WINDOW,
            CREATE_NO_WINDOW,
            "CREATE_NO_WINDOW 必须显式打开"
        );
    }

    // —— Step 3（R305/R306/R311）：走查优先级、不注入 `-y` 的条目、argv 逐段 ——
    #[test]
    fn path_walk_prefers_the_cmd_shim_and_never_wraps_a_shell() {
        let dir = tempdir().unwrap();
        // 同目录同时放「无扩展名的 npx」与「npx.cmd」：T4 实测 `where npx` 就是这个形状，
        // 起得来的是 `.cmd`；无扩展名那条是 sh 脚本，`CreateProcess` 不认（A 支 `NotFound`）。
        let bare = fixture_exec(dir.path(), "npx");
        let shim = fixture_exec(dir.path(), "npx.cmd");
        let dirs = vec![dir.path().to_path_buf()];
        let exts: Vec<String> = vec![".exe".into(), ".cmd".into()];
        assert_eq!(
            walk_path("npx", &dirs, &exts),
            Some(shim),
            "必须选 `.cmd` 那条，不许退回无扩展名的 `{bare:?}`"
        );

        // 不在白名单上的 program：`-y` 一个都不许加（给 python 加 `-y` 会把它打歪）。
        let python = fixture_exec(dir.path(), "python.exe");
        let table: BTreeMap<String, PathBuf> = BTreeMap::from([("python".to_string(), python)]);
        let resolve = |name: &str| table.get(name).cloned();
        let def = ServerDef::Stdio {
            name: "py".into(),
            command: "python".into(),
            args: vec!["-m".into(), "a package".into(), "he said \"hi\"".into()],
            env: HashMap::new(),
        };
        let plan = spawn_command(&def, &resolve).expect("假表里有 python.exe");
        assert_eq!(
            plan.args(),
            vec!["-m", "a package", "he said \"hi\""],
            "含空格与引号的 arg 必须依旧是一个 argv 元素（R311：不许 join 成一个字符串）：{:?}",
            plan.args()
        );
        assert_ne!(
            plan.program(),
            Path::new("cmd"),
            "R311：任何 `cmd /C` 包装都算违规"
        );
        assert!(
            !plan.args().iter().any(|a| *a == "/C" || *a == "/c"),
            "argv 里不许出现 shell 开关：{:?}",
            plan.args()
        );

        // 已经是绝对路径 ⇒ 不走查（用户把「起哪个文件」说清楚了）。
        let explicit = fixture_exec(dir.path(), "x.cmd");
        let never = |_: &str| -> Option<PathBuf> { panic!("绝对路径不该再查 PATH") };
        let def = stdio("x", &explicit.to_string_lossy(), &["--flag"]);
        let plan = spawn_command(&def, &never).expect("绝对路径直接用，不该调用解析器");
        assert_eq!(plan.program(), explicit.as_path());
    }

    // —— Step 2（plan :869-876）与 Step 4（R308）：一条腿死/不回，别的照活 ——
    #[tokio::test]
    async fn one_dead_server_does_not_take_down_the_others() {
        let bridge = McpBridge::new();
        let dead = "dead".to_string();
        let ok = "ok".to_string();
        let make = |name: &str| -> BoxFuture<'_, Result<Handshake, String>> {
            if name == "dead" {
                Box::pin(async { Err("起不来的 server".to_string()) })
            } else {
                Box::pin(fake_handshake(2))
            }
        };
        let out = bridge
            .connect_all_with(&[dead.clone(), ok.clone()], Duration::from_secs(5), &make)
            .await;
        assert!(out[0].1.is_err(), "起不来的必须报 Disconnected：{out:?}");
        assert_eq!(out[1].1, Ok(2), "别的 server 不许被连累：{out:?}");
        assert_eq!(
            bridge.status_of("dead"),
            Some(ServerStatus::Disconnected {
                reason: "起不来的 server".into()
            })
        );
        assert_eq!(
            bridge.status_of("ok"),
            Some(ServerStatus::Ready { tools: 2 })
        );
        let reason = out[0].1.as_ref().unwrap_err();
        assert!(
            reason.chars().count() < 400,
            "错误文案要能上 UI，不能把整个 stderr 尾巴糊上去：{reason}"
        );
    }

    #[tokio::test]
    async fn a_server_that_never_answers_times_out_without_harming_its_neighbours() {
        let bridge = McpBridge::new();
        let make = |name: &str| -> BoxFuture<'_, Result<Handshake, String>> {
            if name == "silent" {
                // 对端**收下** initialize 但不回 ⇒ 只有超时能救。
                Box::pin(fake_handshake_never_answers())
            } else {
                Box::pin(fake_handshake(2))
            }
        };
        let started = std::time::Instant::now();
        let out = bridge
            .connect_all_with(
                &["silent".to_string(), "ok".to_string()],
                Duration::from_millis(300),
                &make,
            )
            .await;
        assert!(out[0].1.is_err(), "不回话的必须因为超时变成 Disconnected");
        assert_eq!(out[1].1, Ok(2), "超时不许传染给邻居");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "注入的 300ms 超时必须真的生效，不能等满 CONNECT_TIMEOUT_SECS：{:?}",
            started.elapsed()
        );
        let reason = out[0].1.as_ref().unwrap_err();
        assert!(reason.contains("超时"), "超时文案要认得出来：{reason}");
    }

    // —— Step 5（R308）：脱敏红线 —— 失败文案与 Debug 都不许带 env 的值 ——
    #[test]
    fn a_failure_reason_never_carries_an_env_value() {
        let secret = "sk-REAL-LOOKING-SECRET";
        let mut env = HashMap::new();
        env.insert("TOKEN".to_string(), secret.to_string());
        let def = ServerDef::Stdio {
            name: "leaky".into(),
            command: "definitely-not-a-real-binary-xyz".into(),
            args: vec![],
            env: env.clone(),
        };
        // ① 选型成功时：`SpawnPlan` 的 Debug 只出键名。
        let dir = tempdir().unwrap();
        let exe = fixture_exec(dir.path(), "definitely-not-a-real-binary-xyz.cmd");
        let plan = spawn_command(&def, &|_: &str| Some(exe.clone())).unwrap();
        let debug = format!("{plan:?}");
        assert!(
            debug.contains("TOKEN"),
            "键名要留着（排查时要知道配了哪把）：{debug}"
        );
        assert!(
            !debug.contains(secret),
            "Debug 里出现了 env 的明文值：{debug}"
        );
        assert_eq!(plan.env_keys(), vec!["TOKEN"], "对外只给键名");

        // ② 解析失败时：reason 里不许出现值，且按 char 截到 REASON_LIMIT。
        let reason = spawn_command(&def, &|_: &str| None).expect_err("解析失败必须报错");
        assert!(
            !reason.contains(secret),
            "失败文案里出现了 env 的明文值：{reason}"
        );

        // ③ 超长失败文案也要截断（按 char，不许按字节切）。
        let long = ServerDef::Stdio {
            name: "long".into(),
            command: "x".repeat(500),
            args: vec![],
            env: HashMap::new(),
        };
        let reason = spawn_command(&long, &|_: &str| None).expect_err("解析失败");
        assert!(
            reason.chars().count() <= 200,
            "reason 上限 200 个 char：{}",
            reason.chars().count()
        );
    }

    // —— Step 6（R309/R310）：桥要存住 service、报工具名、如实报 pid ——
    #[tokio::test]
    async fn the_bridge_keeps_the_service_alive_and_reports_the_pid() {
        let bridge = McpBridge::new();
        let seen = Arc::new(AtomicUsize::new(0));
        let peer_done = Arc::new(AtomicBool::new(false));
        let (seen2, done2) = (seen.clone(), peer_done.clone());
        let make = move |_: &str| -> BoxFuture<'_, Result<Handshake, String>> {
            let (seen, done) = (seen2.clone(), done2.clone());
            Box::pin(async move { fake_handshake_counting(2, seen, done).await })
        };
        let out = bridge
            .connect_all_with(&["ok".to_string()], Duration::from_secs(5), &make)
            .await;
        assert_eq!(out[0].1, Ok(2));
        assert_eq!(
            bridge.status_of("ok"),
            Some(ServerStatus::Ready { tools: 2 })
        );
        assert_eq!(
            bridge.tool_names("ok"),
            vec!["tool_0".to_string(), "tool_1".to_string()],
            "T32 要拿这两条去注册，桥必须把它们留住"
        );
        assert!(
            bridge.pids().is_empty(),
            "内存管道没有进程，不许为了过测试给 duplex 腿造假 pid"
        );
        assert!(
            seen.load(Ordering::SeqCst) >= 3,
            "握手 + initialized + tools/list 至少三条消息，实际 {}",
            seen.load(Ordering::SeqCst)
        );

        bridge.shutdown().await;
        assert!(
            matches!(
                bridge.status_of("ok"),
                Some(ServerStatus::Disconnected { .. })
            ),
            "shutdown 之后状态要变成 Disconnected"
        );
        // service 被桥存着的证据：shutdown 关掉传输之后，对端真的走到了 EOF。
        for _ in 0..100 {
            if peer_done.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            peer_done.load(Ordering::SeqCst),
            "shutdown 之后对端没看到 EOF：service 早已被 drop，桥并没有真的持有连接"
        );
    }

    /// 真解析器那条路的失败形态（`connect_all`，不注入）：一条解析不到的 server ⇒
    /// `Disconnected`，且原因被 trim 到上限内。这条测试**不 spawn 任何进程**。
    #[tokio::test]
    async fn a_command_that_cannot_be_resolved_is_a_bounded_reason() {
        let bridge = McpBridge::new();
        let def = ServerDef::Stdio {
            name: "ghost".into(),
            command: "definitely-not-a-real-binary-xyz".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let out = bridge.connect_all(&[def]).await;
        assert_eq!(out.len(), 1);
        let reason = out[0].1.as_ref().expect_err("解析不到就必须 Err");
        assert!(
            reason.chars().count() <= 200,
            "reason 上限 200 char：{reason}"
        );
        assert!(
            matches!(
                bridge.status_of("ghost"),
                Some(ServerStatus::Disconnected { .. })
            ),
            "失败也要落进状态台账"
        );
    }

    // —— T32：`map_result` 的内容映射（R314–R319） ——
    #[test]
    fn mixed_text_and_image_content_keeps_both_parts() {
        let out = map_result(call_result(vec![
            rmcp::model::ContentBlock::text("看这张"),
            rmcp::model::ContentBlock::image(ONE_PX_PNG_B64, "image/png"),
        ]))
        .unwrap();
        let ToolOutput::Multi { parts } = out else {
            panic!(
                "混合结果需要 `ToolOutput::Multi`，否则 vision 模型看不见图（spec 5.2 第 2 点）：{out:?}"
            )
        };
        assert_eq!(parts.len(), 2, "只取 text 会把图丢掉：{parts:?}");
        assert!(matches!(parts[0], ToolOutput::Text { .. }));
        assert!(matches!(parts[1], ToolOutput::Image { .. }));
    }

    #[test]
    fn a_gif_result_is_reencoded_via_the_same_normalizer() {
        // R314：**不扩 `ImageMime`**——`normalize_image` 会把 gif/webp 收窄成 jpeg/png
        // （`image_prep.rs:74-81`，gif 取首帧）。所以端点永远只见到这两种，扩枚举是零收益的跨语言改动。
        let out = map_result(call_result(vec![rmcp::model::ContentBlock::image(
            ONE_PX_GIF_B64,
            "image/gif",
        )]))
        .unwrap();
        match out {
            ToolOutput::Image { mime, data } => {
                assert!(
                    matches!(
                        mime,
                        crate::agent::types::ImageMime::Png | crate::agent::types::ImageMime::Jpeg
                    ),
                    "gif 必须被重编码成端点收得下的形状：{mime:?}"
                );
                assert!(!data.is_empty(), "重编码后要有内容");
            }
            other => panic!("单张图应该原样是一块 Image：{other:?}"),
        }
    }

    #[test]
    fn an_unreadable_image_becomes_text_that_says_so_instead_of_silently_disappearing() {
        let out = map_result(call_result(vec![rmcp::model::ContentBlock::image(
            "not-base64-at-all",
            "image/png",
        )]))
        .unwrap();
        assert!(
            format!("{out:?}").contains("图片无法读取"),
            "静默丢掉内容块会让模型说瞎话：{out:?}"
        );
    }

    #[test]
    fn non_text_content_is_rendered_as_its_own_part() {
        let out = map_result(call_result(vec![
            rmcp::model::ContentBlock::resource(
                rmcp::model::ResourceContents::TextResourceContents {
                    uri: "file:///notes/a.txt".into(),
                    mime_type: Some("text/plain".into()),
                    text: "正文".into(),
                    meta: None,
                },
            ),
            rmcp::model::ContentBlock::ResourceLink(rmcp::model::Resource::new(
                "https://example.test/a",
                "示例",
            )),
            rmcp::model::ContentBlock::audio("AAA", "audio/wav"),
        ]))
        .unwrap();
        let text = render_for_assert(&out);
        assert!(
            text.contains("file:///notes/a.txt"),
            "resource 要带 uri 前缀：{text}"
        );
        assert!(text.contains("正文"), "resource 的文本要留下来：{text}");
        assert!(
            text.contains("https://example.test/a"),
            "resource_link 要渲染成链接：{text}"
        );
        assert!(
            text.contains("音频"),
            "audio 当前不播放，但必须有一句占位而不是静默：{text}"
        );
    }

    #[test]
    fn tool_error_flag_becomes_is_error_true_with_the_servers_message() {
        let mut r = call_result(vec![rmcp::model::ContentBlock::text("上游炸了")]);
        r.is_error = Some(true);
        let e = map_result(r).unwrap_err();
        assert_eq!(
            e.to_string(),
            "上游炸了",
            "模型看到的必须是 server 给的话，不是我们的包装句（R59 的同一个理由）"
        );
    }

    #[test]
    fn an_empty_result_still_produces_something_the_model_can_read() {
        let out = map_result(call_result(vec![])).unwrap();
        let text = render_for_assert(&out);
        assert!(
            !text.trim().is_empty(),
            "空结果也要有一句可读的话，否则下一轮模型收到一条空 tool 消息：{text:?}"
        );
    }

    // —— T32：`McpTool` 适配（R315） ——
    #[test]
    fn mcp_tool_exposes_the_namespaced_name_and_read_risk() {
        use crate::tools::registry::Tool as _;
        let bridge = std::sync::Arc::new(McpBridge::new());
        let decl = Tool::new("search", "查一查", serde_json::Map::new());
        let tool = McpTool::from_decl("web", &decl, bridge);
        let meta = tool.meta();
        assert_eq!(
            meta.name, "mcp__web__search",
            "模型端必须是 T30 的命名（端点字符集那条红线）"
        );
        assert_eq!(
            meta.risk,
            crate::agent::types::Risk::Read,
            "R315：可靠性由 gate 的信任档位管，不靠把 risk 调严"
        );
        assert_eq!(meta.description, "查一查");
        assert!(matches!(meta.input_schema, serde_json::Value::Object(_)));
    }

    #[tokio::test]
    async fn mcp_tool_execute_reaches_the_server_and_maps_the_result() {
        use crate::tools::registry::Tool as _;
        let bridge = std::sync::Arc::new(McpBridge::new());
        let out = bridge
            .connect_all_with(&["web".to_string()], Duration::from_secs(5), &|_| {
                Box::pin(fake_handshake(2))
            })
            .await;
        assert_eq!(out[0].1, Ok(2));
        let decl = Tool::new("search", "查一查", serde_json::Map::new());
        let tool = McpTool::from_decl("web", &decl, bridge);
        let ctx = crate::tools::registry::ToolCtx {
            write_root: None,
            http: reqwest::Client::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            roots: vec![],
            skills_dir: None,
            skills_disabled: Vec::new(),
        };
        let output = tool
            .execute(serde_json::json!({"q": "x"}), &ctx)
            .await
            .expect("假对端会回一条 text");
        assert_eq!(
            render_for_assert(&output),
            "echo:search",
            "server 的原话要原样到模型；同时证明参数走的是原始工具名"
        );
    }

    #[test]
    fn http_entries_never_take_the_child_process_path() {
        let def = ServerDef::Http {
            name: "remote".into(),
            url: "https://example.test/mcp".into(),
            headers: HashMap::new(),
        };
        let err = spawn_command(&def, &|_: &str| None).expect_err("HTTP 条目不能走子进程");
        assert!(err.contains("HTTP"), "原因要认得出来是 HTTP：{err}");
        assert!(
            err.chars().count() <= 200,
            "仍然受 200 char 上限约束：{err}"
        );
    }

    #[tokio::test]
    async fn connect_def_routes_http_entries_to_the_http_leg() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定临时端口失败");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let def = ServerDef::Http {
            name: "remote".into(),
            url: format!("http://127.0.0.1:{port}/mcp"),
            headers: HashMap::new(),
        };
        let err = match tokio::time::timeout(Duration::from_secs(10), connect_def(&def)).await {
            Ok(Err(reason)) => reason,
            Ok(Ok(_)) => panic!("关闭的端口必须连接失败"),
            Err(_) => panic!("HTTP 连接失败必须快速返回，不能挂死"),
        };
        assert!(
            !err.contains("子进程"),
            "HTTP 条目被错误地送进了 stdio 子进程路径：{err}"
        );
        assert!(!err.is_empty(), "失败原因不能是空串");
    }

    #[test]
    fn http_transport_config_parses_url_and_headers() {
        let def = ServerDef::Http {
            name: "remote".into(),
            url: "http://127.0.0.1:9000/mcp".into(),
            headers: HashMap::from([("Authorization".to_string(), "Bearer test".to_string())]),
        };
        let cfg = http_transport_config(&def).expect("合法 HTTP 配置必须能构造");
        assert_eq!(&*cfg.uri, "http://127.0.0.1:9000/mcp");
        assert!(cfg.allow_stateless, "无 session id 的 server 也要能连");
        let (name, value) = cfg.custom_headers.iter().next().expect("header 要保留");
        assert_eq!(name.as_str(), "authorization");
        assert_eq!(value.to_str().unwrap(), "Bearer test");
    }

    #[test]
    fn http_transport_config_rejects_invalid_endpoints() {
        for url in ["", "   ", "ftp://example.test/mcp", "not a url"] {
            let def = ServerDef::Http {
                name: "remote".into(),
                url: url.into(),
                headers: HashMap::new(),
            };
            let err = http_transport_config(&def).expect_err("非法 URL 必须拒绝");
            assert!(err.contains("URL"), "错误要指明 URL：{err}");
        }
    }

    #[test]
    fn http_transport_config_rejects_credentials_in_the_url() {
        let def = ServerDef::Http {
            name: "remote".into(),
            url: "http://user:secret@127.0.0.1:9000/mcp".into(),
            headers: HashMap::new(),
        };
        let err = http_transport_config(&def).expect_err("URL userinfo 必须拒绝");
        assert!(
            err.contains("headers"),
            "要引导用户把凭据放进 headers：{err}"
        );
        assert!(!err.contains("secret"), "错误里不许回显密码：{err}");
    }

    #[test]
    fn http_transport_config_rejects_cleartext_headers_to_non_loopback() {
        let with_auth = |url: &str| {
            http_transport_config(&ServerDef::Http {
                name: "remote".into(),
                url: url.into(),
                headers: HashMap::from([("Authorization".to_string(), "Bearer t".to_string())]),
            })
        };
        let err = with_auth("http://10.0.0.5:9000/mcp").expect_err("远程明文 + headers 必须拒绝");
        assert!(err.contains("https"), "要提示改用 https：{err}");
        assert!(
            with_auth("http://127.0.0.1:9000/mcp").is_ok(),
            "回环明文允许"
        );
        assert!(
            with_auth("http://localhost:9000/mcp").is_ok(),
            "localhost 明文允许"
        );
        let without_auth = http_transport_config(&ServerDef::Http {
            name: "remote".into(),
            url: "http://10.0.0.5:9000/mcp".into(),
            headers: HashMap::new(),
        });
        assert!(without_auth.is_ok(), "无认证头的远程明文仍由用户自行决定");
    }

    #[test]
    fn http_transport_config_rejects_invalid_headers() {
        for (name, value) in [("bad header", "x"), ("x-test", "bad\nvalue")] {
            let def = ServerDef::Http {
                name: "remote".into(),
                url: "http://127.0.0.1:9000/mcp".into(),
                headers: HashMap::from([(name.to_string(), value.to_string())]),
            };
            let err = http_transport_config(&def).expect_err("非法 header 必须拒绝");
            assert!(err.contains("header"), "错误要指明是哪一类问题：{err}");
            assert!(!err.contains(value), "错误里不许回显 header 的值");
        }
    }

    // —— 假对端（R310）：裸 `Transport<RoleServer>` 手搓 JSON-RPC，不实现 Handler ——
    // 配方取自 rmcp 自己的 tests/test_cancelled_response.rs:210-245：
    // duplex 两半 ⇒ `IntoTransport::<RoleServer,_,_>::into_transport(server_half)`
    // ⇒ `receive()` 读请求 ⇒ `ServerJsonRpcMessage::response(..)` 回包。
    async fn fake_handshake(tool_count: usize) -> Result<Handshake, String> {
        fake_handshake_counting(
            tool_count,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        )
        .await
    }

    async fn fake_handshake_counting(
        tool_count: usize,
        seen: Arc<AtomicUsize>,
        done: Arc<AtomicBool>,
    ) -> Result<Handshake, String> {
        let (client_half, server_half) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            // 泛型值不装箱（`Transport` 不是 dyn-safe）：`into_transport` 的返回类型由编译器推出来。
            let mut peer = IntoTransport::<RoleServer, _, _>::into_transport(server_half);
            while let Some(msg) = peer.receive().await {
                seen.fetch_add(1, Ordering::SeqCst);
                let ClientJsonRpcMessage::Request(request) = msg else {
                    continue;
                };
                match request.request {
                    ClientRequest::InitializeRequest(_) => {
                        let info = InitializeResult::new(ServerCapabilities::default());
                        peer.send(ServerJsonRpcMessage::response(
                            ServerResult::InitializeResult(info),
                            request.id,
                        ))
                        .await
                        .unwrap();
                    }
                    ClientRequest::ListToolsRequest(_) => {
                        let tools = (0..tool_count)
                            .map(|i| {
                                Tool::new(
                                    format!("tool_{i}"),
                                    "假对端的工具",
                                    serde_json::Map::new(),
                                )
                            })
                            .collect();
                        peer.send(ServerJsonRpcMessage::response(
                            ServerResult::ListToolsResult(ListToolsResult {
                                tools,
                                ..Default::default()
                            }),
                            request.id,
                        ))
                        .await
                        .unwrap();
                    }
                    ClientRequest::CallToolRequest(call) => {
                        let mut result = rmcp::model::CallToolResult::default();
                        result.content = vec![rmcp::model::ContentBlock::text(format!(
                            "echo:{}",
                            call.params.name
                        ))];
                        peer.send(ServerJsonRpcMessage::response(
                            ServerResult::CallToolResult(result),
                            request.id,
                        ))
                        .await
                        .unwrap();
                    }
                    // 其它请求（ping 等）一律不回：本轮没人会发。
                    _ => {}
                }
            }
            done.store(true, Ordering::SeqCst);
        });
        let (tools, service) = handshake(client_half).await?;
        Ok(Handshake {
            pid: None,
            tools,
            service,
            job: None,
        })
    }

    /// 收下 initialize 但**永远不回**：只用来测超时那条支路。
    async fn fake_handshake_never_answers() -> Result<Handshake, String> {
        let (client_half, server_half) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut peer = IntoTransport::<RoleServer, _, _>::into_transport(server_half);
            while peer.receive().await.is_some() {
                // 收到也不回：超时是唯一出口。
            }
        });
        let (tools, service) = handshake(client_half).await?;
        Ok(Handshake {
            pid: None,
            tools,
            service,
            job: None,
        })
    }

    /// 失败的重连不许把「还活着的旧连接」谎报成 Disconnected，也不许换掉它的 job。
    #[cfg(windows)]
    #[tokio::test]
    async fn a_failed_reconnect_keeps_the_old_connection_and_its_status() {
        let bridge = McpBridge::new();
        let good = |_: &str| -> BoxFuture<'_, Result<Handshake, String>> {
            Box::pin(async {
                let mut h = fake_handshake(2).await?;
                h.job = Some(std::sync::Arc::new(crate::mcp::process::create_kill_job()?));
                Ok(h)
            })
        };
        let bad = |_: &str| -> BoxFuture<'_, Result<Handshake, String>> {
            Box::pin(async { Err("重连握手失败".to_string()) })
        };
        let first = bridge
            .connect_all_with(&["a".to_string()], Duration::from_secs(5), &good)
            .await;
        assert!(first[0].1.is_ok(), "{first:?}");
        let job = bridge.job_ptr_of("a").expect("旧 job");

        let again = bridge
            .connect_all_with(&["a".to_string()], Duration::from_secs(5), &bad)
            .await;
        assert!(again[0].1.is_err(), "重连本身必须报错：{again:?}");
        assert_eq!(
            bridge.status_of("a"),
            Some(ServerStatus::Ready { tools: 2 }),
            "旧连接仍然可用时，状态不许写成 Disconnected"
        );
        assert_eq!(bridge.job_ptr_of("a"), Some(job), "失败重连不许动旧 job");
    }

    /// 回归：重连一台 server 只许换掉它自己的 job。曾经一个桥共用一个 job，
    /// 重连时覆盖 `self.job` ⇒ 旧句柄关闭 ⇒ 其余所有 server 的进程树被连带杀掉。
    #[cfg(windows)]
    #[tokio::test]
    async fn reconnecting_one_server_keeps_the_jobs_of_the_others() {
        let bridge = McpBridge::new();
        let make = |_: &str| -> BoxFuture<'_, Result<Handshake, String>> {
            Box::pin(async {
                let mut h = fake_handshake(1).await?;
                h.job = Some(std::sync::Arc::new(crate::mcp::process::create_kill_job()?));
                Ok(h)
            })
        };
        let names = vec!["a".to_string(), "b".to_string()];
        let first = bridge
            .connect_all_with(&names, Duration::from_secs(5), &make)
            .await;
        assert!(first.iter().all(|(_, r)| r.is_ok()), "{first:?}");
        let (a0, b0) = (
            bridge.job_ptr_of("a").expect("a 必须有 job"),
            bridge.job_ptr_of("b").expect("b 必须有 job"),
        );
        assert_ne!(a0, b0, "每台 server 必须有自己的 job");

        let again = bridge
            .connect_all_with(&["a".to_string()], Duration::from_secs(5), &make)
            .await;
        assert!(again[0].1.is_ok(), "{again:?}");
        assert_ne!(
            bridge.job_ptr_of("a"),
            Some(a0),
            "重连 a 必须换掉它自己的 job，旧进程树才会被回收"
        );
        assert_eq!(
            bridge.job_ptr_of("b"),
            Some(b0),
            "重连 a 不许碰 b 的 job（它的进程树与工具仍然可用）"
        );
    }
}
