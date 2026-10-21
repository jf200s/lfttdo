# Plan B — 工具、联网与 MCP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax and are one action each.

**Goal:** 在 Plan A 已交付的聊天+看图内核上，接通内置只读工具（`fetch_url` 带 SSRF 硬防护、`read_file` 带 roots 越界防护）与 MCP 客户端（`rmcp`，stdio + Streamable HTTP，兼容 `mcpServers` 配置），并保持「全程零弹窗」和「安装包 <15MB」两条验收线不被悄悄牺牲。

**Architecture:** 复用 Plan A 建好的四个接缝，不新建通道：`Tool` trait + `ToolRegistry`（`tools/registry.rs:103`/`:118`）承载内置与 MCP 两套工具且**二者同质**；`PermissionGate::check`（`permission/gate.rs:121`）继续是唯一的是非判断点；`AgentSession::run_turn`（`agent/session.rs:204-257`）已有的 `risk_of → tool_call 事件 → gate.check → registry.execute → tool_result 回填` 序列一行不改；MCP 子进程只从 Rust `tokio::process` 起，前端 capabilities 一个字节都不加。新增的只有 `tools/fetch_guard.rs`（纯函数安全层）、`mcp/`（配置、桥、命名、清理）与四到五条 `ipc.rs` 命令。

**Tech Stack:** Rust（`rmcp` 3.5.1 已有，新增 `url`、`ipnet`、`htmd`、`windows` 的 JobObject 面）、Tauri v2（仍只有 `ipc.rs` 依赖它）、React + TypeScript + Vitest 前端（工具卡片与 server 管理面板）。

**Spec:** `docs/superpowers/specs/2026-10-19-qwen-do-agent-design.md`（本计划覆盖切片 3、4；切片 0–2 见 Plan A，切片 5–6 见 Plan C）

**前置事实（Plan A 收尾时的实测，派发每个任务前要用 `file:line` 复核，符号名比行号可靠）：**

- 生产 registry 是空的：`ipc.rs:654` 建 `ToolRegistry` 后没有 `register` 调用；`tools/echo.rs` 的 `EchoTool`/`RiskOverrideTool` 从未注册。⇒ 切片 3 之前模型看不到任何工具。
- `AgentSession::with_roots` 从未被 `run_turn` 调用，`roots` 恒空 ⇒ `resolve_within_roots` 对任何路径都 fail-closed。`Settings`（`settings.rs:243`）没有 `roots` 字段，且 `settings.rs:1283` 那条测试断言未知的 `roots`/`mcp` 键会被丢弃。
- 权限门的 Read 分支在查规则表**之前**就自动放行（`gate.rs:124`），且 `rule_key` 对 `Risk::Read` 只产出 `"{name}|read"`（`gate.rs:92-109`）——没有 per-domain / per-server 作用域。⇒ MCP 的「默认 Ask」在本计划必须改掉这个顺序，否则信任级别形同废纸。
- `ToolOutput` 只有 `Text`/`Image{data,mime}`（`agent/types.rs:33`），`ImageMime` 只有 `"png"|"jpeg"`（`types.rs:26`）。MCP 会返回 gif/webp/resource/audio。
- `ToolRegistry` 只有 `register`/`get`/`risk_of`/`defs`/`execute`，**没有注销**接口。MCP server 被删除或改信任级别时没有卸载工具的办法。
- Windows spawn 实测结论（Plan A 切片 0，`docs/findings/plan-a-risk-verification.md:412-620`）：`Command::new("npx")` **直接失败** `program not found`；`cmd /C npx` 与 `where npx` 取绝对 `.cmd` 路径后 spawn 都能握手成功（`tools=16`）。**两种写法的直接子进程都是 `cmd.exe`，真正的 node server 是孙进程**；`taskkill /T /F` 在四次运行里只杀掉 3 个 PID 中的 1 个；rmcp 的 `service.cancel()` 只作用于直接子进程。`npx` 必须带 `-y`。
- 依赖现状：`rmcp = { version="3.5.1", features=["client","transport-io","transport-child-process"] }`；`reqwest` 关默认 features 只留 `json`/`default-tls`；`htmd` 不在 `Cargo.lock`；`url`/`ipnet` 只是别人的传递依赖，**本 crate 用不了**；`tempfile` 仅 dev。

## Global Constraints

每个任务都隐式包含本节。

- 只有 `ipc.rs` 允许 `use tauri`。`tools/`、`permission/`、`agent/`、`mcp/` 全部与 Tauri 无关，必须能在 `cargo test` 里无 Tauri、无网络地跑。
- **SSRF 是本计划唯一的硬安全红线**：必须拒 `file://` 及一切非 http/https 方案、`localhost`、环回与私网段（127/8、10/8、172.16/12、192.168/16、169.254/16）、云元数据 IP，**以及重定向后的每一跳目标**——只校验首次 URL 不算实现。此项必须有单测，不可选。
- 内置工具全为 `Risk::Read`；除「用户显式把 MCP server 信任级别设为 Ask」外，**正常聊天全程 `permission_request` 事件数 = 0**。
- 单测不许碰网络。需要真网的判据一律写进 `docs/findings/`，并且没有实测过就标 `未实测`。
- 任何测试与探针**不得创建或修改真实 Windows 凭据**，MCP 配置的 `env` 值一律用假值；真实 key 不许出现在日志、findings 或对话里。
- 前端不得申请 `shell:allow-spawn`，也不得申请通配 `fs:allow-*`；MCP 子进程只能由 Rust 起。`capabilities/default.json` 在本计划**不应有任何新增**，如果某个任务想加权限，那是要停下来讨论的信号。
- 零弹窗：前端禁 `window.prompt`/`alert`/`confirm`；Rust 侧 spawn 子进程必须 `CREATE_NO_WINDOW`（`0x0800_0000`），桌面弹一个黑窗等同于弹窗。
- 安装包 <15MB、空闲内存 <150MB、冷启动 <1.5s。每加一个直接依赖，任务内必须给 `Cargo.lock` 的行级增减数与 `cargo tree` 的新增包清单作为证据；锁文件行数不等于体积，最终由 #29 那次体积实测收口。
- 组件内禁止硬编码颜色，只能用 token 变量；`lint:css` 必须过。
- 本机执行规矩（每次 Bash 调用都要带上）：`export PATH="$HOME/bin:$PATH"`；**永不 `cargo fmt`**，要格式化就 `rustfmt --edition 2024 <file>`；cargo 走 `CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=5 CARGO_HTTP_TIMEOUT=60`；`cmdkey` 输出是 GBK，管道接 `iconv -f GBK -t UTF-8`。
- 行号以派发时实际文件为准；引用一律写「符号名 + 当前行号」，且实施者要先验证符号存在。

## 共享测试辅助（引用即须有定义，禁止「想象中的 API」）

下面这些函数名在本计划的测试代码里被直接调用。**每个都由点名它的那个任务在自己的 `#[cfg(test)] mod tests` 里实现**，跨任务复用时要把它复制过去而不是 `pub` 到生产模块里（Plan A 的先例：测试替身留在测试模块）。签名写死在这里，实施者不必猜。

| 辅助函数 | 签名 | 定义于 |
|---|---|---|
| `ok200` | `fn ok200(url: &str, ct: &str, body: &[u8]) -> Step` | T23 |
| `ok302` | `fn ok302(from: &str, location: &str) -> Step` | T23 |
| `redirect_with_raw_location` | `fn redirect_with_raw_location(from: &str, raw_location: &str) -> Step` | T23 |
| `chunked_ok200` | `fn chunked_ok200(url: &str, ct: &str, total_bytes: usize) -> Step`（分块吐，用于测「边收边判容量」） | T23 |
| `Step` | `struct Step { status: u16, content_type: String, chunks: Vec<Vec<u8>>, location: Option<String> }` | T23 |
| `ScriptedTransport` | `impl ScriptedTransport { fn new(Vec<Step>) -> Self }` + `impl FetchTransport`（按调用次序吐 `Step`，用完即 `panic!("吐完了还请求")`） | T23 |
| `PublicResolver` | `impl Resolver`，任何主机一律返回 `93.184.216.34` | T23 |
| `fetch_with_transport` | `async fn fetch_with_transport(Vec<Step>) -> Result<String, GuardError>`（走 T24 的 `FetchTool::execute`，注入替身） | T24 |
| `temp_root` / `temp_root_with` | `fn temp_root() -> PathBuf`（`tempfile::tempdir` 保活包装，见下方告警）/ `fn temp_root_with(name: &str, body: &[u8]) -> PathBuf` | T25 |
| `write` / `write_bytes` | `fn write(root: &Path, name: &str, body: &[u8])`（两个同义，任选一名，**不得两名并存**） | T25 |
| `p` | `fn p(root: &Path, name: &str) -> String`（`to_string_lossy`） | T25 |
| `ctx` / `ctx_with_roots` | `fn ctx(root: &Path) -> ToolCtx` / `fn ctx_with_roots(Vec<PathBuf>) -> ToolCtx`（`http` 用 `reqwest::Client::new()`，`cancel` 用未触发 token） | T25 |
| `view` | `fn view(name: &str, risk: Risk, input: Value) -> ToolCallView`（`id` 恒 `"c1"`） | T33，沿用 Plan A `gate.rs` 测试里的同名函数并加 `input` 形参 |
| `never_cancelled` | `fn never_cancelled() -> CancellationToken` | T33 |
| `pending_key` | `async fn pending_key(&PermissionGate) -> String`（读 `pending` map 的唯一键，**只测键形状，不测内部结构**） | T33 |
| `stdio` / `http` | `fn stdio(name: &str, command: &str, args: &[&str]) -> ServerDef` / `fn http(name: &str, url: &str) -> ServerDef` | T29 |
| `load_json` / `load_raw` | `fn load_json(&str) -> McpConfig` / `fn load_raw(&str) -> McpConfig`（写进临时目录后调 `load`，两个名字两种入口是为了读测试时一眼看出测的是哪层） | T29 |
| `ExitPath` / `start_tree` / `wait_gone` | `enum ExitPath { AppDrop, ServerRemoved, ConnectFailed }`；`async fn start_tree(ExitPath) -> (KillOnCloseJob, Vec<u32>)`；`async fn wait_gone(&[u32], Duration) -> bool`（轮询 `tasklist`，GBK 输出走本机 iconv 规矩） | T34 |
| `mcp_servers_for` | `async fn mcp_servers_for(Arc<McpState>) -> Vec<McpServerView>`（`ipc.rs` 命令体的可测内核，命令本身只做一层转发） | T35 |

⚠️ **`tempfile` 的保活陷阱**（T25/T29/T34 都会踩）：`tempfile::TempDir` 一 drop 目录就没了，而 `ToolCtx` 里只存 `PathBuf`。所有用临时目录的测试**必须**把 `TempDir` 变量留在同一个作用域活到最后一个 `assert`，否则失败信息会指向「路径不存在」而真正原因是目录被回收 —— 这类红极易被误读成实现的错。辅助函数返回裸 `PathBuf` 时要显式 `std::mem::forget(dir)` 并在 `Drop` 前 `temp_dir_cleanup()`，或者一律让测试自己持有 `TempDir` 并把 `dir.path()` 传进去。**裁定选后者**（不泄漏、不玄学），上表的 `temp_root()` 因此返回 `(TempDir, PathBuf)` 二元组，测试写成 `let (_d, root) = temp_root();`。


## 文件结构（Plan B 建立/修改的文件）

```
src-tauri/src/
├─ tools/
│  ├─ mod.rs            改：声明 fetch_guard / fetch / read_file
│  ├─ registry.rs       改：+ remove_prefix、+ scope_of（Read 也有作用域令牌）
│  ├─ fetch_guard.rs    T22 新建：纯函数安全层（方案/主机/地址分类）
│  ├─ fetch.rs          T23-T24：逐跳重定向 + FetchTransport 注入 + FetchTool
│  └─ read_file.rs      T25：roots 内的只读文件工具
├─ permission/gate.rs   T33：override 先于 Read 自动放行；per-server 信任
├─ agent/session.rs     T26：run_turn 用 ctx.roots 与 registry.scope_of
├─ mcp/
│  ├─ mod.rs            T29 新建
│  ├─ config.rs         T29：mcp.json（兼容 mcpServers）解析 + Debug 脱敏
│  ├─ naming.rs         T30：mcp__{server}__{tool} 与 64 字符兜底（FNV-1a）
│  ├─ bridge.rs         T31/T32：TokioChildProcess 连接、list_tools、McpTool::execute
│  ├─ process.rs        T34：JobObject KILL_ON_JOB_CLOSE
│  └─ http.rs           T36：Streamable HTTP 传输（体积闸门后可选）
├─ settings.rs          T26：+ roots: Vec<PathBuf>
└─ ipc.rs               T27/T35：注册生产工具；mcp_* 四条命令
src/
├─ components/ToolCard.tsx      T27：工具卡片（名称 + 一行 preview + 截断标记）
├─ components/McpPanel.tsx      T35：server 列表、状态点、信任级别切换
├─ ipc/commands.ts              T35：mcp_* 包装
└─ types.ts                     T32/T35：镜像新事件与 McpServerView
docs/findings/
├─ plan-b-fetch-quality.md      T24：htmd 抽取质量实测（风险 7 的判定门）
└─ plan-b-mcp-checks.md         T31/T34/T37：spawn 选型、孤儿进程、真 server 引用来源
```

---

### Task 22: `fetch_guard` —— 纯函数 SSRF 判定层

SSRF 判定必须是**不依赖网络、不依赖 reqwest** 的纯函数，否则「每一跳都复验」和「DNS rebinding」两类用例根本无法测。本任务只做这一层。

**Files:**
- Create: `src-tauri/src/tools/fetch_guard.rs`
- Modify: `src-tauri/src/tools/mod.rs`（`pub mod fetch_guard;`）
- Modify: `src-tauri/Cargo.toml`（`url = "2"`、`ipnet = "2"` 提为直接依赖）
- Test: `src-tauri/src/tools/fetch_guard.rs`（内联 `mod tests`）

**Interfaces:**
- Consumes: 无（叶子模块）
- Produces:
  - `pub enum GuardError { SchemeDenied(String), NoHost, Denied { addr: IpAddr, reason: &'static str }, PrivateInDnsSet { addr: IpAddr }, Resolution(String, String), TooManyHops(String) }`，`Display` 文案必须是能直接给用户看的人话，且**回显最终 URL**（spec 风险 5）。
  - `pub struct Target { pub url: Url, pub host: String, pub addrs: Vec<IpAddr> }`
  - `pub fn deny_reason(addr: IpAddr) -> Option<&'static str>` —— 唯一分类处，任何地方要判断「这个 IP 能不能连」都调它。
  - `pub fn check_url(raw: &str) -> Result<Url, GuardError>` —— 方案 + 主机存在性 + **字面 IP 直接分类**。
  - `pub fn assert_all_public(addrs: &[IpAddr]) -> Result<(), GuardError>` —— 解析结果整集复验。
  - `pub trait Resolver { fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, GuardError>; }`（生产实现 `SystemResolver` 用 `tokio::net::lookup_host`，测试用脚本）。

- [ ] **Step 1: 写失败测试 —— 分类表必须逐条点名**

```rust
#[test]
fn deny_reason_covers_every_private_class() {
    let cases: &[(&str, &str)] = &[
        ("127.0.0.1", "环回"), ("127.2.3.4", "环回"),
        ("10.0.0.7", "私网"), ("172.16.5.5", "私网"), ("192.168.1.1", "私网"),
        ("169.254.169.254", "链路本地/云元数据"),
        ("100.64.0.1", "运营商级 NAT"),
        ("0.0.0.0", "本网络"),
        ("224.0.0.1", "组播"), ("255.255.255.255", "保留"),
        ("::1", "IPv6 环回"), ("fe80::1", "IPv6 链路本地"),
        ("fd00::1234", "IPv6 唯一本地地址"), ("ff02::1", "IPv6 组播"),
        ("::", "IPv6 未指定"),
        ("::ffff:127.0.0.1", "IPv4 映射环回"),
        ("::ffff:169.254.169.254", "IPv4 映射链路本地"),
    ];
    for (s, want) in cases {
        let a: IpAddr = s.parse().unwrap_or_else(|_| panic!("{s} 不是合法 IP，用例写错了"));
        let got = deny_reason(a).unwrap_or_else(|| panic!("{a} 必须被拒，理由应为「{want}」"));
        assert!(got.contains(want), "{a} 的理由不对：{got}");
    }
    for ok in ["93.184.216.34", "1.1.1.1", "8.8.8.8", "2606:4700::1111"] {
        let a: IpAddr = ok.parse().unwrap();
        assert_eq!(deny_reason(a), None, "{a} 是公网，不许误杀（误杀会让联网功能整体不可用）");
    }
}
```

- [ ] **Step 2: 写失败测试 —— URL 形态层的绕过**

```rust
#[test]
fn check_url_denies_non_http_and_literal_private_hosts() {
    for bad in ["file:///C:/secrets.txt", "gopher://127.0.0.1:6379/", "ftp://example.com/x"] {
        assert!(matches!(check_url(bad), Err(GuardError::SchemeDenied(_))), "{bad} 必须按方案拒绝");
    }
    // WHATWG URL 会把十进制/十六进制/八进制简写解析成 IPv4，这些是经典绕过面
    assert!(matches!(check_url("http://2130706433/"), Err(GuardError::Denied { .. })));
    assert!(matches!(check_url("http://0x7f.0.0.1/"), Err(GuardError::Denied { .. })));
    assert!(matches!(check_url("http://[::1]/"), Err(GuardError::Denied { .. })));
    assert!(matches!(check_url("http://169.254.169.254/latest/meta-data/"), Err(GuardError::Denied { .. })));
    // 用户没写方案时 Url::parse 会失败；不许替它猜 http://，要报明确错误
    assert!(check_url("example.com").is_err());
    assert!(check_url("https://example.com/a?b=1").is_ok());
}
```

若 `Url` crate 实测**不**把 `2130706433` 解析成 `Host::Ipv4(127.0.0.1)`，就**不许**放宽这条测试：改成在 `check_url` 里补一段数字/十六进制主机名的显式识别，并把它记进 findings。理由：这条测试的红与绿直接对应一个真实存在的绕过面。

- [ ] **Step 3: 写失败测试 —— DNS 返回混合地址集时必须整集拒绝**

```rust
#[test]
fn a_dns_answer_containing_one_private_address_is_denied() {
    struct Scripted(Vec<IpAddr>);
    impl Resolver for Scripted {
        fn lookup(&self, _: &str) -> Result<Vec<IpAddr>, GuardError> { Ok(self.0.clone()) }
    }
    let mix = Scripted(vec!["93.184.216.34".parse().unwrap(), "127.0.0.1".parse().unwrap()]);
    let url = check_url("https://evil.example/").unwrap();
    match resolve_target(&url, &mix) {
        Err(GuardError::PrivateInDnsSet { .. }) => {}
        other => panic!("A 记录里混一个环回就必须整集拒绝，实得 {other:?}"),
    }
    // 空解析结果不能当成「没有私网地址」放行
    assert!(matches!(resolve_target(&url, &Scripted(vec![])), Err(GuardError::Resolution(..))));
}
```

`resolve_target(&Url, &dyn Resolver) -> Result<Target, GuardError>`：`Host::Ipv4/Ipv6` 直接分类且**不做二次解析**；`Host::Domain` 调 `Resolver` 后用 `assert_all_public`。

- [ ] **Step 4: 跑测试确认红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml fetch_guard`（带惯用 env）
Expected: FAIL，三条测试各自因函数缺失或分类表不全而红。**先看失败信息是否指向预期的分支**，不确定的失败不许进 Step 5。

- [ ] **Step 5: 写最小实现**

`deny_reason` 用 `IpAddr` match + `Ipv4Addr::is_loopback/is_private/is_link_local/is_unspecified/is_multicast`，再补 `ipnet` 的 `Ipv4Network::from_masked("100.64.0.0/10")` / `"198.18.0.0/15"` / `"192.0.2.0/24"` 等显式段；IPv6 侧把 `::ffff:0:0/96` 拆出内嵌 v4 **递归**回 `deny_reason`（这一步是整个表的要点，不递归就等于留了个洞）。`check_url` 里先 `Url::parse`，`scheme` 不在 `["http","https"]` 即 `SchemeDenied`，`url.host()` 为 `None` 即 `NoHost`。

- [ ] **Step 6: 跑测试确认绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml fetch_guard`
Expected: PASS。并跑 `cargo tree -e normal --manifest-path src-tauri/Cargo.toml` 记下 `url`/`ipnet` 提为直接依赖后**新增**的包名（应为 0 新增包，只是归属变化），写进本任务报告。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/tools/fetch_guard.rs src-tauri/src/tools/mod.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: add the pure ssrf decision layer for fetched urls"
```

---

### Task 23: 逐跳重定向 —— 每一跳复验，且关掉 DNS rebinding

`reqwest` 自己跟重定向时，**每一跳都会重新解析 DNS**，本任务要把它变成「我们手动走跳」：既不把重定向交给库，也才能让 `resolve_to_addrs` 把「校验过的 IP」和「实际连接的 IP」锁死成同一个，这才是 DNS rebinding 的真防线。

**Files:**
- Create: `src-tauri/src/tools/fetch.rs`
- Modify: `src-tauri/src/tools/mod.rs`
- Test: `src-tauri/src/tools/fetch.rs`（内联）

**Interfaces:**
- Consumes: T22 `check_url`/`resolve_target`/`Target`/`Resolver`/`GuardError`
- Produces:
  - `pub struct Fetched { pub status: u16, pub content_type: String, pub body: Vec<u8>, pub final_url: String, pub hops: usize, pub cross_host: bool }`
  - `#[async_trait] pub trait FetchTransport: Send + Sync { async fn get(&self, url: &Url, addrs: &[SocketAddr]) -> Result<Fetched, GuardError>; }`
  - `pub struct RedirectWalker<R: Resolver, T: FetchTransport> { resolver: R, transport: T, max_hops: usize, max_bytes: usize }`
  - `async fn fetch(&self, raw: &str) -> Result<Fetched, GuardError>` —— 只负责「走跳 + 每跳复验 + 体积上限」，**不做** HTML 转换（T24）。

- [ ] **Step 1: 写失败测试 —— 第一跳公网、第二跳内网必须被拒**

```rust
#[tokio::test]
async fn a_redirect_into_loopback_is_denied() {
    // 302 链：public → 127.0.0.1。老写法（让 reqwest 自己跟）会直接把内网内容取回来。
    let t = ScriptedTransport::new(vec![
        ok302("https://ok.example/", "http://127.0.0.1/admin"),
    ]);
    let w = RedirectWalker::new(PublicResolver, t).with_max_hops(5).with_max_bytes(2 * 1024 * 1024);
    let e = w.fetch("https://ok.example/").await.unwrap_err();
    assert!(matches!(e, GuardError::Denied { addr, .. } if addr.to_string() == "127.0.0.1"),
        "重定向后的内网目标必须拒绝并点名，实得 {e:?}");
}

#[tokio::test]
async fn hop_budget_is_enforced_and_names_the_last_url() {
    let mut chain = vec![ok302("https://a.test/1", "https://b.test/2"); 6];
    chain.extend(vec![ok200("https://b.test/2", "text/html", b"never")]);
    let w = RedirectWalker::new(PublicResolver, ScriptedTransport::new(chain))
        .with_max_hops(5).with_max_bytes(1024);
    let e = w.fetch("https://a.test/1").await.unwrap_err();
    assert!(matches!(&e, GuardError::TooManyHops(u) if u == "https://b.test/2"), "{e:?}");
}

#[tokio::test]
async fn oversize_body_is_stopped_at_the_cap_not_buffered_then_checked() {
    // 先声明超长再分块给：实现若是「全收完再判长度」，这条会 OOM 而不是报容量
    let w = RedirectWalker::new(PublicResolver, ScriptedTransport::new(vec![
        chunked_ok200("https://big.test/", "text/plain", 3 * 1024 * 1024)]))
        .with_max_hops(5).with_max_bytes(2 * 1024 * 1024);
    let e = w.fetch("https://big.test/").await.unwrap_err();
    assert!(format!("{e:?}").contains("2097152"), "容量错误要报出上限，实得 {e:?}");
}
```

- [ ] **Step 2: 写失败测试 —— Location 的畸形与非 http 方案**

```rust
#[tokio::test]
async fn weird_locations_are_rejected_not_fetched() {
    for (loc, why) in [
        ("file:///C:/windows/win.ini", "跨方案"),
        ("gopher://x/", "跨方案"),
        ("//127.0.0.1/x", "协议相对也要补出方案再判"),
        ("http://127.0.0.1\\@ok.example/", "user-info 伪装主机"),
    ] {
        let w = RedirectWalker::new(PublicResolver,
            ScriptedTransport::new(vec![redirect_with_raw_location("https://ok.example/", loc)]))
            .with_max_hops(5).with_max_bytes(1024);
        let e = w.fetch("https://ok.example/").await.unwrap_err();
        assert!(!matches!(e, GuardError::Resolution(..)), "{why}：不许因为解析不了就退化成放行，实得 {e:?}");
    }
}
```

绝对/相对 `Location` 一律用 `Url::join` 归一后再走 `check_url`；`url.username()/password()` 非空 ⇒ `Denied`（`@` 伪装主机是实测存在的绕过面）。

- [ ] **Step 3: `cross_host` 判据的失败测试**

`cross_host` 供 T24 生成「已跳转至 <host>」提示，**不做**验证码页识别。判据钉死为 registrable host 不等：`https://a.example/x` → `https://www.a.example/y` **不算**跨主（同一注册域），→ `https://login.b.example/` 算。本任务先只实现 `final_url` 主机的取法（`Target.host()`，`Host::Domain` 时去尾点、小写），并测：

```rust
#[test]
fn cross_host_compares_registrable_suffix_not_whole_host() {
    assert!(!cross_host("www.a.example", "a.example"));
    assert!(cross_host("a.example", "b.example"));
    assert!(cross_host("a.example.com", "a.example.org")); // 后缀不同必须算跨主
}
```

`fn registrable(host: &str) -> String` = 末两段（`a.co.uk` 这类公共后缀**不处理**，写进 findings 说明这是有意的精度取舍：宁可多报一次跨主，也不引入 `psl` 依赖）。

- [ ] **Step 4: 跑测试确认红 → 实现 → 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml fetch`
Expected: 先 FAIL（模块/方法缺失），实现后 PASS。
实现要点：`RedirectWalker::fetch` 循环 `loop { check_url → resolve_target → transport.get → if 3xx && Location → 归一 join → 复验 → continue }`；`max_hops` 默认 **5**（spec 5.4 的重定向 ≤5 跳）；`ScriptedTransport`/`PublicResolver` 都是 `#[cfg(test)]` 内的测试替身，**不许**为测试在生产类型上开 `pub fn`。生产 `ReqwestTransport` 用 `ClientBuilder::new().redirect(Policy::none()).timeout(Duration::from_secs(20)).user_agent(UA).resolve_to_addrs(host, addrs)`，UA 常量写死并测一次（spec 5.4「明确 UA」）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tools/fetch.rs src-tauri/src/tools/mod.rs
git commit -m "feat: walk redirects hop by hop and pin resolved addresses"
```

---

### Task 24: `FetchTool` —— 方案类型闸门、`htmd` 抽取、截断分页，以及风险 7 的判定门

**Files:**
- Create: `src-tauri/src/tools/fetch.rs`（续：`FetchTool` 实现 `Tool`）
- Modify: `src-tauri/Cargo.toml`（`htmd`）、`src-tauri/src/tools/mod.rs`
- Create: `src-tauri/examples/fetch_probe.rs`
- Test: `src-tauri/src/tools/fetch.rs`（内联）
- Create: `docs/findings/plan-b-fetch-quality.md`

**Interfaces:**
- Consumes: T23 `RedirectWalker`、T22 `GuardError`、Plan A `Tool`/`ToolMeta`/`ToolCtx`/`ToolOutput`/`ToolError`/`Risk`
- Produces:
  - `pub struct FetchTool`，`meta().name == "fetch_url"`，`risk == Risk::Read`
  - `input_schema`：`{ url: string(必填), offset?: integer, max_chars?: integer }`，`required == ["url"]`
  - `pub fn render_markdown(content_type: &str, body: &[u8]) -> Result<String, ToolError>`
  - `pub fn slice_page(text: &str, offset: usize, max_chars: usize, total_note: bool) -> String`
  - `ToolCtx.http` 在本任务**不改**：`FetchTool` 自带 `RedirectWalker`（`ctx` 的 client 留给别的用途），并在 `fetch.rs` 顶部注释写明为什么两个 client 是故意的。

- [ ] **Step 1: 写失败测试 —— 内容类型白名单**

```rust
#[tokio::test]
async fn only_html_text_and_json_are_accepted() {
    for ct in ["text/html; charset=utf-8", "text/plain", "application/json"] {
        assert!(render_markdown(ct, b"hello").is_ok(), "{ct} 必须接受");
    }
    for ct in ["image/png", "application/pdf", "application/octet-stream", "text/css"] {
        let e = render_markdown(ct, b"x").unwrap_err();
        assert!(e.to_string().contains("content-type"), "{ct} 必须给明确错误而不是空文本，实得 {e}");
    }
}
```

- [ ] **Step 2: 写失败测试 —— 截断必须自报原文长度且可分页**

```rust
#[test]
fn truncation_is_explicit_and_offset_pages_forward() {
    let long = "字".repeat(9_000);           // 每字 3 字节，同时测「按字符不按字节」
    let note = "[已截断，原文 9000 字符]";
    let first = slice_page(&long, 0, 8_000, true);
    assert!(first.ends_with(note), "尾部标记不对：{first}");
    assert_eq!(first.chars().count(), 8_000 + note.chars().count(), "正文必须正好 8000 字符，标记另计");
    let second = slice_page(&long, 8_000, 8_000, true);
    assert_eq!(second.chars().count(), 1_000, "第二段只剩 1000 字");
    assert!(!second.contains("已截断"), "取到末尾时不许再挂截断标记，否则模型会以为还有内容");
    assert!(slice_page(&long, 99_000, 100, true).contains("超出范围"), "offset 越界要说明，不能返回空串让模型猜");
}
```

- [ ] **Step 3: 写失败测试 —— 跨主提示**

```rust
#[tokio::test]
async fn a_cross_host_landing_is_announced_in_the_output_text() {
    // 反爬/登录页场景：spec 要求「不把验证码页喂给模型」。判据实现为「明示最终主机」，
    // 而不是假装能识别验证码。
    let out = fetch_with_transport(ScriptedTransport::new(vec![
        ok302("https://news.example/", "https://passport.other.example/login"),
        ok200("https://passport.other.example/login", "text/html", b"<h1>登录</h1>"),
    ])).await.unwrap();
    assert!(out.starts_with("[已跳转至 passport.other.example]"), "实得 {out}");
}
```

同域跳转**不许**加这一行（否则正文前多噪声）。403/429 同样前置一行 `[上游返回 403，内容可能受限]`，并把 `status` 一并写进 preview，让模型自己换路。

- [ ] **Step 4: `htmd` 抽取质量实测（风险 7 的判定门，本任务唯一的真网步骤）**

```bash
cargo run --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live
```

六个固定页面（新闻正文 / 博客 / 带大量导航的门户 / 纯文本文件 / JSON 接口 / 一个 SPA 空壳），每个把「抽取后前 400 字 + 总字符数 + 耗时」写进 `docs/findings/plan-b-fetch-quality.md`。**判定门（逐字来自 spec 5.5）**：正文里导航/页脚噪声占比明显、或正文被截成碎片 ⇒ `htmd` 不达标，换成 `readability` + `html2md` 并新开一个任务，不许「凑合用」。findings 里必须明确写：哪几个页面人眼判定为可用、哪个不可用、以及依据 `未实测` 的部分。

- [ ] **Step 5: 体积账（每个新依赖都要还）**

```bash
cargo tree -e normal --manifest-path src-tauri/Cargo.toml | grep -c ''
git diff --numstat src-tauri/Cargo.lock
cargo tree -e normal --manifest-path src-tauri/Cargo.toml --invert htmd
```

Expected: 报告里写清 `htmd` 带来哪些**新增**包。若引入 `scraper`/`html5ever` 之外的重依赖（例如 `serde_yaml`、`image` 之类不相关的），记为债务交 #29 决策，不许在此任务里偷偷 `default-features = false` 之外的裁剪实验。

- [ ] **Step 6: 跑测试确认绿并提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::fetch`
Expected: PASS（Step 1–3 全绿；无网络访问）。

```bash
git add src-tauri/src/tools/fetch.rs src-tauri/src/tools/mod.rs src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/examples/fetch_probe.rs docs/findings/plan-b-fetch-quality.md
git commit -m "feat: add the fetch_url tool with content-type gate, paging and measured extraction"
```

---

### Task 25: `read_file` —— roots 内的只读文件工具

`resolve_within_roots`（`registry.rs:57`）已有，但符号链接/junction 分支**从未被测**（`:55` 的注释自己承认了）。本任务补这条洞，并给出用户真正会撞上的两个失败：路径在 root 外、文件是二进制/超大。

**Files:**
- Create: `src-tauri/src/tools/read_file.rs`
- Modify: `src-tauri/src/tools/mod.rs`、`src-tauri/src/tools/registry.rs`（`resolve_within_roots` 改为对 root 也 canonicalize）
- Test: `src-tauri/src/tools/read_file.rs` + `registry.rs` 内联新增

**Interfaces:**
- Consumes: `Tool`/`ToolMeta`/`ToolCtx`/`ToolError`/`ToolOutput`/`Risk`
- Produces:
  - `pub struct ReadFileTool`，`name == "read_file"`，`risk == Risk::Read`
  - `input_schema`：`{ path: string(必填), offset?: integer, limit?: integer }`
  - `const MAX_BYTES: usize = 512 * 1024;`
  - `pub fn looks_binary(head: &[u8]) -> bool`

- [ ] **Step 1: 写失败测试 —— 符号链接越界（Windows junction 同路径）**

```rust
#[cfg(windows)]
#[tokio::test]
async fn a_symlink_pointing_outside_the_root_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root"); std::fs::create_dir(&root).unwrap();
    let outside = dir.path().join("secret.txt");
    std::fs::write(&outside, b"do-not-read").unwrap();
    std::os::windows::fs::symlink_file(&outside, root.join("link.txt")).unwrap();
    let ctx = ctx_with_roots(vec![root]);
    let e = ReadFileTool.execute(json!({"path": ctx.roots[0].join("link.txt").to_string_lossy()}), &ctx)
        .await.unwrap_err().to_string();
    assert!(e.contains("root") || e.contains("越界"), "符号链接逃逸必须被拒，实得 {e}");
}
```

判据：**canonicalize 之后的候选路径**必须 `starts_with(canonicalize 之后的 root)`。root 自己也要 canonicalize——否则「root 本身是符号链接」时整棵树都被拒（过度收紧同样是缺陷，要在测试里两头都钉住：一条拒逃逸，一条放正常）。

若本机权限不允许建符号链接（非开发者模式），**不许**把 `#[ignore]` 当成通过：在 findings 里写明「本机未能建链接」并改用 junction（`mklink /J`）重测，两者都失败才降级为源码审查并标 `未实测`。

- [ ] **Step 2: 写失败测试 —— 跨盘符与 `..`**

```rust
#[tokio::test]
async fn absolute_paths_on_other_drives_and_dotdot_are_denied() {
    let root = temp_root_with("a.txt", b"ok");
    let ctx = ctx_with_roots(vec![root.clone()]);
    for p in ["C:\\Windows\\win.ini", "\\\\nas\\share\\x", "D:\\other\\a.txt"] {
        assert!(ReadFileTool.execute(json!({"path": p}), &ctx).await.is_err(), "{p} 必须被拒");
    }
    let tricky = root.join("sub").join("..").join("..").join("escape.txt");
    assert!(ReadFileTool.execute(json!({"path": tricky.to_string_lossy()}), &ctx).await.is_err());
}
```

（`..` 那条依赖 canonicalize 而不是字符串检查——`a/../../b` 会跳出 root；写死这个断言。）

- [ ] **Step 3: 写失败测试 —— 二进制/超大/编码**

```rust
#[tokio::test]
async fn binary_oversize_and_non_utf8_all_say_why() {
    let root = temp_root();
    write(&root, "blob.bin", &[0u8, 1, 2, 255, 0]);
    write(&root, "big.txt", &vec![b'a'; MAX_BYTES + 1]);
    write_bytes(&root, "gbk.txt", &[0xD6, 0xD0, 0xCE, 0xC4]); // 「中文」的 GBK 字节，写死而不依赖本机 codepage
    let ctx = ctx(&root);
    let e1 = ReadFileTool.execute(json!({"path": p(&root, "blob.bin")}), &ctx).await.unwrap_err().to_string();
    assert!(e1.contains("二进制"), "{e1}");
    let e2 = ReadFileTool.execute(json!({"path": p(&root, "big.txt")}), &ctx).await.unwrap_err().to_string();
    assert!(e2.contains("524288") && e2.contains("offset"), "超大必须报上限并指出可用 offset 分页，实得 {e2}");
    let e3 = ReadFileTool.execute(json!({"path": p(&root, "gbk.txt")}), &ctx).await.unwrap_err().to_string();
    assert!(e3.contains("UTF-8"), "非 UTF-8 必须明说而不是静默替换成 U+FFFD（模型会把 replacement 字符当内容解读）：{e3}");
}
```

- [ ] **Step 4: 跑测试确认红 → 最小实现 → 跑绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tools::read_file`
Expected: 先 FAIL，实现后 PASS。`offset`/`limit` 按**行**计（日常读日志/文本的直觉），与 `fetch_url` 的按字符分页刻意不同——两处各自在 `description` 里写清单位，防止模型混用。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/tools/read_file.rs src-tauri/src/tools/mod.rs src-tauri/src/tools/registry.rs
git commit -m "feat: add read_file with canonicalized root confinement"
```

---

### Task 26: 把 `roots` 真的接上 —— `Settings.roots` + `run_turn` 注入

前置事实：`AgentSession::with_roots` 从未被调用，`roots` 恒空 ⇒ T25 的 `read_file` 在生产里对**任何**路径都会 fail-closed。这个缺陷不补，切片 3 交付的第二个工具就是死的。

**Files:**
- Modify: `src-tauri/src/settings.rs`（`Settings` 加 `pub roots: Vec<PathBuf>`）
- Modify: `src-tauri/src/agent/session.rs`（`run_turn` 构造 `ToolCtx` 处填 `roots`）
- Modify: `src-tauri/src/ipc.rs`（`settings_get` 的 `SettingsView` 带上 `roots`）
- Test: `settings.rs`、`agent/session.rs` 内联新增

**Interfaces:**
- Consumes: `Settings::normalize`（`settings.rs:276`，唯一纠正处）、`ToolCtx.roots`
- Produces: `Settings.roots: Vec<PathBuf>`（默认空）、`Settings::normalize` 对 roots 的处理（去重、丢弃不存在的目录、`canonicalize` 失败即丢）

- [ ] **Step 1: 写失败测试 —— 默认空 ⇒ 一律拒绝，且这是设计不是缺陷**

```rust
#[test]
fn roots_default_empty_and_read_file_therefore_refuses_everything() {
    let s = Settings::default();
    assert!(s.roots.is_empty(), "不许替用户决定允许读哪些目录（与 R90 的空 base_url 同一取向）");
    assert!(s.normalize_roots_with(&|_p| false).is_empty(), "不存在的目录必须被丢弃");
}
```

- [ ] **Step 2: 写失败测试 —— `normalize` 的 roots 语义**

`["C:\\notes", "C:\\notes\\", "C:\\notes", "C:\\ghost"]` 经 `normalize`（`exists` 闭包让前三条为真、第四条假）⇒ 结果 `["C:\\notes"]`：去重、去尾斜杠、丢不存在项。断言写成 `assert_eq!(out.len(), 1)` 再断言内容，**不许**只断言长度（防止顺序与内容同时错掉）。

- [ ] **Step 3: 改掉那条会被我们抢走语义的旧测试**

`settings.rs:1283` 一带的测试断言「未知键 `mcp`/`roots` 会被丢弃」以证明读侧向前兼容（R91）。`roots` 现在是**已知字段**，那条测试必须改为用一个仍然未知的键（`quantum_mode`）来证明同一件事，并把断言拆成两半：`roots` **被读回**、`quantum_mode` **被忽略**。

⚠️ 不许把整条测试删掉来「让它通过」——那是把 R91 的哨兵拆了。`mcp` 键仍然必须是「被忽略」的那一半（MCP 配置在 T29 走独立文件，不进 `settings.json`）。

- [ ] **Step 4: 写失败测试 —— 循环真的把 roots 传到了执行点**

在 `session.rs` 的测试模块里，用 `VecSink` + `ScriptedModel` 跑一个「模型调 `read_file`」的剧本，注册一个假工具 `AssertRootsTool`（`execute` 里断言 `ctx.roots == 期望值`），证明 `run_turn` 注入的 ctx 带着非空 roots。

```rust
#[tokio::test]
async fn run_turn_passes_configured_roots_into_tool_ctx() {
    let (swept, _) = run_with_model_returning_read_file(
        SessionDeps { roots: vec![PathBuf::from(r"C:\notes") }, ..Default::default() }).await;
    assert_eq!(swept.roots_seen(), vec![PathBuf::from(r"C:\notes")],
        "roots 没传到执行点 ⇒ read_file 永远 fail-closed，这是本任务存在的唯一理由");
}
```

- [ ] **Step 5: 跑测试红 → 实现 → 绿**

Run: `cargo test --manifest-path src-tauri/Cargo.toml -- roots` 与 `cargo test --manifest-path src-tauri/Cargo.toml settings`
Expected: PASS。实现要点：`SessionDeps` 加 `roots: Vec<PathBuf>`（`ipc.rs` 每轮从 `settings` 现取，**不缓存**——用户改了设置下一轮就该生效）；`SettingsView`（TS 镜像会用到）加 `roots: Vec<String>`。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/settings.rs src-tauri/src/agent/session.rs src-tauri/src/ipc.rs
git commit -m "fix: wire configured roots into the tool context so read_file can work"
```

---

### Task 27: 生产 registry 装配 + 工具卡片

**Files:**
- Modify: `src-tauri/src/ipc.rs`（`ipc.rs:654` 那个空 registry 注册 `FetchTool`/`ReadFileTool`）
- Create: `src/components/ToolCard.tsx` + `src/components/ToolCard.test.tsx`
- Modify: `src/types.ts`（若 T14 镜像缺 `ToolOutput` 的 kind 判别，补齐）
- Test: `src/ipc/commands.test.ts`（若需要）、`ToolCard.test.tsx`

**Interfaces:**
- Consumes: `ToolRegistry::register`、`AgentEvent::tool_call`/`tool_result`（前端 reducer 已投影，Plan A T14）
- Produces: `ToolCard({ call, result }: { call: ToolCallUi; result?: ToolResultUi })`

- [ ] **Step 1: 写 Rust 侧失败测试 —— 生产 registry 不再为空**

```rust
#[test]
fn production_registry_ships_fetch_url_and_read_file() {
    let r = crate::ipc::build_production_registry(/* ... */);
    let names: Vec<String> = r.defs().into_iter().map(|d| d.name).collect();
    assert_eq!(names, vec!["fetch_url", "read_file"],
        "模型看到的工具集必须是这份名单；echo/guarded 这类测试工具绝不能进生产（它们只活在 #[cfg(test)]）");
    assert_eq!(r.risk_of("fetch_url"), Risk::Read, "内置工具全为 Read ⇒ 才谈得上零弹窗");
    assert_eq!(r.risk_of("delete_everything"), Risk::Exec, "未知工具按最严一档（R158）");
}
```

把 `ipc.rs` 里内联的 registry 构造抽成 `pub fn build_production_registry() -> Arc<ToolRegistry>`，本任务唯一允许的 `ipc.rs` 重构。

- [ ] **Step 2: 写前端失败测试 —— 卡片渲染真内容，不硬编码颜色**

```tsx
it("shows the tool name, a one-line preview and the truncation marker", () => {
  render(<ToolCard call={{ id: "c1", name: "fetch_url", risk: "read", input: { url: "https://a.test" } }}
                   result={{ callId: "c1", kind: "text", preview: "正文前两行…", isError: false, ms: 812 }} />);
  expect(screen.getByText("fetch_url")).toBeInTheDocument();
  expect(screen.getByText(/正文前两行/)).toBeInTheDocument();
  expect(screen.getByText(/0\.8s/)).toBeInTheDocument();
});
it("marks an errored result without hiding the message", () => {
  render(<ToolCard call={call} result={{ ...result, isError: true, preview: "上游返回 403，内容可能受限" }} />);
  expect(screen.getByRole("status", { name: /工具失败/ })).toHaveTextContent("403");
});
```

样式只用 `var(--*)` token；`npm run lint:css` 必须过（Plan A 的 no-hardcoded-color 规则会抓）。`ms` 为重放来源时恒 0（Plan A R161），**0 显示为「—」而不是「0.0s」**——这条也要一个 `it`。

- [ ] **Step 3: 跑双测试器**

Run: `cargo test --manifest-path src-tauri/Cargo.toml ipc` / `npx vitest run src/components/ToolCard.test.tsx` / `npx tsc --noEmit`
Expected: 全绿。若 `App.tsx`/`MessageBubble.tsx` 里挂卡片的挂载点与 T16 实产结构不符，按 T16 的实际文件改，**不要**为了测试去重构 T16 的组件树。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/ipc.rs src/components/ToolCard.tsx src/components/ToolCard.test.tsx src/types.ts
git commit -m "feat: ship the two built-in read-only tools and render their cards in the stream"
```

---

### Task 28: 切片 3 端到端验证（需人工在场）

**Files:**
- Create: `docs/findings/plan-b-slice3-checks.md`

- [ ] **Step 0: 派发规则（控制器先读）**

`R153` 的拆法在这里同样适用：Step 1–4 要真窗口 + 真 key，子 agent **不得**声称「联网已验证」，除非 findings 里贴了原始帧/原文摘录；做不到就标 `未执行（需人工在场）` 并停下回报。真网判据不许用源码推断代替。

- [ ] **Step 1: 零弹窗判据（本切片最重要的一条测量）**

`npm run tauri dev`，问五条需要联网才能答的问题（今天的天气、某个仓库的 star 数、某条新闻的标题……）。
Expected: `permission_request` 事件数 **= 0**（内置全 Read）；`tool_call` 卡片出现且 preview 是正文而非 base64。取帧方式沿用 Plan A R159：在 `subscribeAgent` 回调里临时 `console.log(JSON.stringify(payload))`，**收尾前删掉，不许提交**。

- [ ] **Step 2: 每条 SSRF 红线的真机抽样**

让模型「访问」下列地址（模型会照做，这正是风险所在）：
`http://127.0.0.1:8080`、`http://169.254.169.254/latest/meta-data/`、`file:///C:/Windows/win.ini`，以及一个真实会 302 到内网的公网域名（若无现成的，用本机临时脚本 + hosts 记录为 `未实测` 并说明原因）。
Expected: 卡片显示工具失败 + 文案点名是哪一个地址、为什么；主对话继续，不卡死、不弹窗。

- [ ] **Step 3: `read_file` 的两条路径**

先在 `settings.json` 里**不配** `roots` 问「读一下 C:\notes\a.txt」⇒ 必须拒绝并说明未配置允许目录；再配 `roots: ["C:\\notes"]`、**不重启**（`run_turn` 每轮现取，T26 Step 5 的设计）问同一句 ⇒ 必须读出内容。

- [ ] **Step 4: 截断分页真的被模型用上**

抓一个长文页面问「第 3000 字之后在讲什么」。Expected: 出现两次 `fetch_url` 调用且 `offset` 非 0 —— 这是模型真的读到了分页参数，而不是我们替它高兴。

- [ ] **Step 5: 记录并提交**

findings 写：五条问题的实际往返耗时、卡片文案原文（去掉任何账号信息）、SSRF 三条的拒绝文案、`htmd` 在真页面上的表现与 T24 判定门的一致性。

```bash
git add docs/findings/plan-b-slice3-checks.md
git commit -m "docs: record slice 3 end-to-end checks for tools and web lookup"
```

---

### Task 29: `mcp.json` —— 兼容 `mcpServers` 的配置层，且秘密不外泄

**Files:**
- Create: `src-tauri/src/mcp/mod.rs`、`src-tauri/src/mcp/config.rs`
- Test: `mcp/config.rs` 内联

**Interfaces:**
- Consumes: 无（叶子）
- Produces:
  - `pub enum ServerDef { Stdio { name, command, args, env }, Http { name, url, headers } }`
  - `pub struct McpConfig { pub servers: Vec<ServerDef> }`
  - `pub fn load(dir: &Path) -> McpConfig`（文件不存在 ⇒ 空列表，**不报错**）
  - `pub const MCP_FILE: &str = "mcp.json";`

- [ ] **Step 1: 写失败测试 —— 直接抄来的 Claude Desktop 配置必须能解析**

```rust
#[test]
fn parses_a_claude_desktop_style_config() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("mcp.json"), r#"{
      "mcpServers": {
        "memory": { "command": "npx", "args": ["-y","@modelcontextprotocol/server-memory"],
                    "env": { "TOKEN": "shh" } },
        "remote": { "url": "https://mcp.example/sse", "headers": { "Authorization": "Bearer shh" } }
      }}"#).unwrap();
    let c = load(&dir);
    assert_eq!(c.servers.len(), 2);
    let s = c.servers.iter().find(|s| s.name() == "memory").unwrap();
    assert_eq!(s.command(), Some("npx"));
    assert_eq!(s.args(), vec!["-y", "@modelcontextprotocol/server-memory"]);
    assert!(matches!(c.servers.iter().find(|s| s.name() == "remote").unwrap(), ServerDef::Http { .. }));
}
```

- [ ] **Step 2: 写失败测试 —— 一个坏 server 不许拖垮其余（spec 5.2 失败处理）**

```rust
#[test]
fn a_broken_entry_is_skipped_with_a_reason_not_a_crash() {
    // 无 command 也无 url / JSON 整体坏掉 / 同名重复，三种都要单独条目级失败
    let c = load_json(r#"{ "mcpServers": { "ok": {"command":"x"}, "bad": {}, "also": {"command":"y"} } }"#);
    assert_eq!(c.servers.len(), 2, "坏的那条跳过，好的两条必须留下来");
    assert_eq!(c.skipped[0].name, "bad");
    let garbage = load_raw("{ not json");
    assert!(garbage.servers.is_empty() && !garbage.parse_error.is_none(), "整体坏掉要有明确错误，但不 panic");
}
```

- [ ] **Step 3: 写失败测试 —— 脱敏（这是本任务的红线，不是可选项）**

```rust
#[test]
fn debug_never_prints_env_or_headers() {
    let s = ServerDef::Stdio { name: "x".into(), command: "npx".into(), args: vec![],
        env: HashMap::from([("TOKEN".to_string(), "sk-REAL-SECRET".to_string())]) };
    let d = format!("{s:?}");
    assert!(!d.contains("sk-REAL-SECRET"), "`{:?}` 泄密：{d}");
    assert!(d.contains("TOKEN") && d.contains("***"), "键名要留着排查，值必须打码：{d}");
    assert!(!load_raw(r#"{"mcpServers":{"x":{"command":"npx","env":{"T":"sk-REAL-SECRET"}}}}"#).to_string().contains("sk-REAL-SECRET"));
}
```

⇒ 手写 `Debug`（不用 `#[derive(Debug)]`），照 Plan A 里 `Decision` 手写序列化的先例。同一条测试顺手钉住「`mcp.json` 不参与任何导出」：`McpConfig` 不提供 `to_json_string()` 之类会回吐原值的 API（只有 `parse_error` 摘要与脱敏 Debug）。

- [ ] **Step 4: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml mcp::config`
`serde` 侧用 `#[serde(flatten)]` + 手工判别 `command`/`url` 二选一；未知字段忽略（向前兼容，与 R91 同取向）。

```bash
git add src-tauri/src/mcp/mod.rs src-tauri/src/mcp/config.rs
git commit -m "feat: load mcp servers from a claude-compatible mcp.json with redacted debug"
```

---

### Task 30: `mcp__{server}__{tool}` 命名与 64 字符兜底

**Files:**
- Create: `src-tauri/src/mcp/naming.rs`
- Modify: `src-tauri/src/mcp/mod.rs`
- Test: `naming.rs` 内联

**Interfaces:**
- Consumes: 无
- Produces: `pub fn namespaced(server: &str, tool: &str) -> String`、`pub fn fnv1a64(s: &str) -> u64`、`pub const NAME_LIMIT: usize = 64;`

- [ ] **Step 1: 写失败测试 —— 长度上限与稳定性**

```rust
#[test]
fn short_names_pass_through_and_long_names_get_a_stable_short_hash() {
    assert_eq!(namespaced("web", "search"), "mcp__web__search");
    let server = "a-very-long-community-server-name-that-goes-on";
    let tool = "get_the_current_weather_forecast_for_a_location";
    let n = namespaced(server, tool);
    assert!(n.len() <= NAME_LIMIT, "超 64 会被兼容端点拒：{n}({})", n.len());
    assert_eq!(n, namespaced(server, tool), "同一输入必须永远得到同一名字（重启后要能认出来）");
    assert_ne!(n, namespaced(server, &format!("{tool}2")), "只换工具名末字符时不许撞名");
}
```

**裁定：不许用 `std::collections::hash_map::DefaultHasher`** —— 官方文档明说其算法不稳定、跨版本可能变；用它做工具名会让「升级 rustc 之后所有历史会话里的 tool_call 名字都对不上」。`fnv1a64` 手写 15 行（`0xcbf2_9ce4_8422_2325` / `0x100_0000_01b3`），测试里钉一个已知向量：`assert_eq!(fnv1a64("hello"), 0xa4_32_d8_43_6a_df_8b_a6)`（FNV-1a 64 标准值；实现者要用另一实现交叉验证后才写死）。

- [ ] **Step 2: 写失败测试 —— 兜底形状与冲突**

```rust
#[test]
fn collisions_are_disambiguated_in_order() {
    // 截断前缀相同的两条长名字，必须拿到不同结果且后缀是 -2、-3
    let a = namespaced("srv", "tool_name_that_is_extremely_long_aaaaaaaaaaaaaaaaaaaaaaaa");
    let b = namespaced("srv", "tool_name_that_is_extremely_long_aaaaaaaaaaaaaaaaaaaaaabbb");
    assert_ne!(a, b);
    let mut taken = HashSet::new();
    let mut out = vec![];
    for raw in [a.clone(), a.clone(), b.clone()] {
        out.push(dedup(&raw, &mut taken));
    }
    assert_eq!(out[1], format!("{a}-2"), "第二次同名要挂序号");
    assert!(out.iter().all(|n| n.len() <= NAME_LIMIT), "挂序号也不许突破上限：{out:?}");
}
```

`namespaced` 的兜底形状（写进实现并在注释里给理由）：`mcp__{server 前 8}__{tool 前 24}#{fnv1a64 全名的前 7 个 hex}` ⇒ 恒长 8+1+8+2+24+1+7 = 51，给 `-N` 序号留 13 位余量。

- [ ] **Step 3: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml mcp::naming`

```bash
git add src-tauri/src/mcp/naming.rs src-tauri/src/mcp/mod.rs
git commit -m "feat: namespace mcp tool names with a collision-safe 64 char budget"
```

---

### Task 31: stdio 桥 —— Windows spawn、`-y`、`CREATE_NO_WINDOW`、并行连接

**Files:**
- Create: `src-tauri/src/mcp/bridge.rs`
- Modify: `src-tauri/Cargo.toml`（`rmcp` 加 `which-command` feature，若走 crate 内置的解析路径）
- Create: `src-tauri/examples/mcp_bridge_probe.rs`
- Test: `bridge.rs` 内联 + `docs/findings/plan-b-mcp-checks.md`

**Interfaces:**
- Consumes: T29 `ServerDef`、T30 `namespaced`、Plan A `ToolRegistry`/`Tool`/`EventSink`
- Produces:
  - `pub enum ServerStatus { Connecting, Ready { tools: usize }, Disconnected { reason: String } }`
  - `pub struct McpBridge { ... }`、`async fn connect_all(&self, servers: &[ServerDef]) -> Vec<(String, Result<usize, String>)>`
  - `pub fn spawn_command(def: &ServerDef) -> Result<tokio::process::Command, String>` —— **纯函数**，把选型逻辑与真 spawn 分开，选型才可测。

- [ ] **Step 1: 写失败测试 —— 选型逻辑（复刻切片 0 的实测结论）**

```rust
#[test]
fn npx_is_resolved_to_its_absolute_cmd_path_and_always_carries_y() {
    let c = spawn_command(&stdio("x", "npx", &["-y", "@modelcontextprotocol/server-memory"])).unwrap();
    let prog = c.get_program().to_string_lossy().to_string();
    assert!(prog.ends_with(".cmd") || prog.contains("npx.exe"), "裸 `npx` 在本机实测直接 program not found：{prog}");
    assert!(c.get_args().any(|a| a == "-y"), "`npx` 缺 -y 会弹安装确认，桌面端等于弹窗（findings:555 实测）");
    assert_eq!(c.get_creation_flags() & 0x0800_0000, 0x0800_0000, "CREATE_NO_WINDOW 必须显式打开");
}
```

`get_creation_flags()` 不是 `tokio::process::Command` 的公开方法 ⇒ 实现里把 flags 存进 `ServerDef` 解析出的小结构体并暴露 `pub fn creation_flags(&self) -> u32`，测试断言那个结构而不是 `Command`。若 `where npx` 在本机返回多条（实测确实返回 `\npx` 与 `\npx.cmd` 两条），**裁定取 `.cmd` 那条**并写进注释：`.cmd` 才是 `cmd.exe` 能执行的批处理，`.ps1` 之类会被当成非法。

- [ ] **Step 2: 写失败测试 —— 一个 server 连不上不影响其余**

```rust
#[tokio::test]
async fn one_dead_server_does_not_take_down_the_others() {
    let out = connect_all_for_test(&[stdio("dead", "definitely-not-a-real-binary-xyz", &[]),
                                     fake_ok_server()]).await;
    assert!(out[0].1.is_err(), "起不来的必须报 Disconnected");
    assert!(out[1].1.is_ok(), "别的 server 不许被连累（spec 5.2 失败处理）");
    assert!(out[0].1.as_ref().unwrap_err().len() < 400, "错误文案要能上 UI，不能把整个 stderr 尾巴糊上去");
}
```

`fake_ok_server()` 用 `#[cfg(test)]` 起一个**同 crate 内**的假 stdio MCP server 二进制不现实 ⇒ 裁定：用 `tower`-less 的 `rmcp::transport::IO`（`transport-io` feature 已在）在内存管道上 `().serve(...)` 跑通握手，`list_tools` 返回两条。真子进程路径留给 Step 3 的探针，两者不互相冒充。

- [ ] **Step 3: 真 npx 探针（人工在场或本机已装 node 时自动跑）**

```bash
cargo run --manifest-path src-tauri/Cargo.toml --example mcp_bridge_probe -- --server server-memory
```

Expected: 打印 `tools=N`、握手耗时、**直接子进程的 PID 与映像名**（必须是 `cmd.exe`，据此坐实「真 node 是孙进程」这一 T34 的前提）。结果写进 `docs/findings/plan-b-mcp-checks.md`，没跑就标 `未实测`。

- [ ] **Step 4: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml mcp::bridge`

```bash
git add src-tauri/src/mcp/bridge.rs src-tauri/Cargo.toml src-tauri/src/mcp/mod.rs src-tauri/examples/mcp_bridge_probe.rs docs/findings/plan-b-mcp-checks.md
git commit -m "feat: connect stdio mcp servers in parallel with a windows-safe spawn path"
```

---

### Task 32: MCP 结果映射 —— 混合 content 数组，图片走同一条归一化路径

**Files:**
- Modify: `src-tauri/src/mcp/bridge.rs`（`McpTool` 实现 `Tool::execute`）
- Modify: `src-tauri/src/tools/registry.rs`（若需要 `defs()` 的输入 schema 直传）
- Test: `bridge.rs` 内联

**Interfaces:**
- Consumes: `rmcp::CallToolResult`、T18 `image_prep::normalize_image`（**Plan A 交付，签名以其提交后的实际代码为准**）、`ToolOutput`
- Produces: `pub fn map_result(r: rmcp::model::CallToolResult) -> Result<ToolOutput, ToolError>`、`McpTool { server, original_name, meta }`

- [ ] **Step 1: 写失败测试 —— 文本 + 图片混合数组不能只留文本**

```rust
#[test]
fn mixed_text_and_image_content_keeps_both_and_renames_nothing() {
    let out = map_result(call_result(vec![text("看这张"), image("image/png", base64_of_tiny_png())])).unwrap();
    match out { Multi { parts } => {
        assert_eq!(parts.len(), 2, "只取 text 会让 vision 模型看不见图（spec 5.2 第 2 点）");
        assert!(matches!(parts[0], Part::Text(_)));
        assert!(matches!(parts[1], Part::Image { .. }));
    } _ => panic!("混合结果需要一个新的 ToolOutput 形状，见 Step 3 的裁定") }
}
```

- [ ] **Step 2: 写失败测试 —— gif/webp 必须转成端点收得下的形状**

`ToolOutput::Image` 的 `mime` 只有 `"png"|"jpeg"`（`agent/types.rs:26`），而 MCP server 会回 `image/gif`。
**裁定：扩 `ImageMime` 而不是加一条「非 png/jpeg 就丢成文本」的降级** —— 丢成文本等于把 spec 5.2 第 2 点写的那条通路废掉；扩枚举则同时改 Rust 枚举、TS 镜像 `types.ts`、`reducer.ts` 与 Plan A T14 那条跨语言名单守卫测试（`reducer.test.ts` 里读 `events.rs`/`types.rs` 切片比集合的那条）—— 四处一起改，正是那条测试存在的意义。

```rust
#[test]
fn a_gif_result_is_reencoded_to_png_via_the_same_normalizer() {
    let out = map_result(call_result(vec![image("image/gif", base64_of_first_frame_gif())])).unwrap();
    // 复用 T18 的归一化：MCP 侧与贴图侧走同一条路，才叫「同一归一化路径」
    assert!(matches!(out, Multi { parts } if matches!(parts[0], Part::Image { mime: ImageMime::Png, .. })));
}
#[test]
fn an_unreadable_image_becomes_text_that_says_so_instead_of_silently_disappearing() {
    let out = map_result(call_result(vec![image("image/png", "not-base64-at-all")])).unwrap();
    assert!(format!("{out:?}").contains("图片无法读取"), "静默丢掉内容块会让模型说瞎话");
}
```

- [ ] **Step 3: `resource` / `resource_link` / `audio` 的处置**

`resource` 的文本 ⇒ 追加为 `Part::Text`（带 `uri` 前缀一行）；`resource_link` ⇒ 只渲染 `[链接] {name} {uri}`，不自动抓取（那是 `fetch_url` 的活，且会自动抓取等于替模型做 SSRF 决定）；`audio` ⇒ 明确「本工具返回了音频，当前不播放」的文本占位。三条各一个 `assert`，写进同一条测试 `non_text_content_is_rendered_as_its_own_part`。

- [ ] **Step 4: `isError` 走 `ToolError`，不要塞成正常文本**

```rust
#[test]
fn tool_error_flag_becomes_is_error_true_with_the_servers_message() {
    let e = map_result(call_result_err("上游炸了")).unwrap_err();
    assert_eq!(e.to_string(), "上游炸了", "模型看到的必须是 server 给的话，不是我们的包装句");
}
```

- [ ] **Step 5: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml mcp::bridge` 与 `npx vitest run src/state/reducer.test.ts`（跨语言守卫必须仍然绿；改了 `ImageMime` 而没改 TS 镜像会在这里红）

```bash
git add src-tauri/src/mcp/bridge.rs src-tauri/src/agent/types.rs src/types.ts src/state/reducer.ts src/state/reducer.test.ts
git commit -m "feat: map mcp mixed content arrays onto the same image pipeline as pasted images"
```

---

### Task 33: 信任级别 —— override 必须走在 Read 自动放行之前

前置事实（不改就等于没实现）：`gate.rs:124` 在看到 `Risk::Read` 时**立刻**返回 Allow，`RuleTable::lookup` 根本没有机会说话；而 spec 5.2 第 1 点要求「添加 server 时默认 Ask」。这两件事现在直接矛盾，本任务解决它。

**Files:**
- Modify: `src-tauri/src/permission/gate.rs`（查表顺序 + server 前缀匹配）
- Modify: `src-tauri/src/tools/registry.rs`（`remove_prefix`、`scope_of`）
- Modify: `src-tauri/src/agent/session.rs`（`ToolCallView` 带上 scope）
- Test: 三处内联

**Interfaces:**
- Consumes: `Decision`、`RuleTable`、`ToolCallView`
- Produces:
  - `RuleTable::set_server_trust(&self, server: &str, Verdict)`、`Verdict::{Auto, Ask, Block}`
  - `RuleTable::verdict_for(&self, name: &str, scope: &str) -> Verdict` —— **唯一判定入口**，顺序：精确 `name|scope` override → server 前缀 override → 按 Risk 默认（`Read`=Auto、`Write`/`Exec`=Ask）
  - `ToolRegistry::remove_prefix(&self, prefix: &str) -> Vec<String>`
  - `ToolRegistry::scope_of(&self, name: &str, input: &Value) -> String`（默认实现：`Read` ⇒ `"read"`；`fetch_url` ⇒ 主机名；MCP ⇒ server 名）

- [ ] **Step 1: 写失败测试 —— 默认 Ask 的 MCP 工具必须弹窗**

```rust
#[tokio::test]
async fn a_server_defaulted_to_ask_pops_exactly_one_request_per_call() {
    let rules = RuleTable::new();
    rules.set_server_trust("search", Verdict::Ask);
    let gate = PermissionGate::new().with_rules(rules.clone()).with_sink(sink.clone());
    let d = gate.check(&view("mcp__search__web_search", Risk::Read, json!({"q":"x"})), &never_cancelled()).await;
    assert!(d.denied_until_answered(), "server 设了 Ask 却被 Read 自动放行，信任级别就是废纸");
    assert_eq!(sink.events().len(), 1, "一次调用一个 permission_request");
    assert_eq!(pending_key(&gate).await, "mcp__search__web_search|search", "key 必须带 server 作用域");
}
```

- [ ] **Step 2: 写失败测试 —— `Block` 之后模型仍拿到 tool_result**

```rust
#[tokio::test]
async fn a_blocked_server_denies_without_waiting_and_the_loop_still_backfills() {
    rules.set_server_trust("evil", Verdict::Block);
    let d = gate.check(&view("mcp__evil__x", Risk::Read, json!({})), &never_cancelled()).await;
    assert!(matches!(d, Decision::Deny { reason, .. } if reason.contains("evil")));
    // session 侧：Deny ⇒ ToolOutput::text(reason) + is_error=true，history 仍 push（session.rs:228-257 现成路径）
}
```

`Block` **不许**等待、**不许**发 `permission_request`；判据是 sink 里事件数为 0。这一条要跑在 `session.rs` 的集成测试里（`VecSink` + `ScriptedModel` 剧本），不只是 gate 单元测试。

- [ ] **Step 3: 写失败测试 —— 改信任级别要真的换掉已注册工具**

```rust
#[tokio::test]
async fn switching_trust_reregisters_without_duplicating_or_losing_others() {
    let r = registry_with(mcp_tools("search", ["a", "b"]), trust = Verdict::Ask);
    assert_eq!(r.defs().len(), 2);
    r.remove_prefix("mcp__search__");            // T33 新增
    register_mcp_tools(&r, "search", ["a", "b"], Verdict::Auto);
    assert_eq!(r.defs().len(), 2, "重注册后不许变 4 条（重复注册会喂给模型两份同名工具）");
    assert!(r.get("mcp__search__a").is_some());
}
```

- [ ] **Step 4: 写失败测试 —— `fetch_url` 的 per-domain 作用域（为切片 6 的「问一次」预留，现在不启用弹窗）**

`scope_of("fetch_url", {"url":"https://News.Example/a"})` ⇒ `"news.example"`（小写、去端口、非 URL 输入 ⇒ `"read"`）。这条现在不影响判定（默认 Auto），但它是**唯一**让「以后都允许」有作用域的机制；测试只钉 `rule_key` 的形状：`"fetch_url|news.example"`。

- [ ] **Step 5: 不许改坏的既有行为（回归钉子）**

重跑 Plan A 的十三条 gate 测试，全绿。特别是 `read_risk_is_auto_and_emits_no_permission_event`（内置 Read 仍零弹窗）与 `session_allow_is_scoped_by_root_not_bare_tool_name`（R158/Write 作用域仍生效）。`dropped_sender_falls_back_to_deny_instead_of_hanging` 保持：MCP 的 `check` 依然只靠 cancel 与 sender-drop 两条出路，**本任务不加超时**（超时策略归 Plan C 的设置项）。

- [ ] **Step 6: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml permission`
Expected: 新增四条 PASS，Plan A 十三条不回归。

```bash
git add src-tauri/src/permission/gate.rs src-tauri/src/tools/registry.rs src-tauri/src/agent/session.rs
git commit -m "feat: make per-server mcp trust override win over the read auto-allow"
```

---

### Task 34: 子进程清理 —— JobObject，因为实测 `taskkill /T` 不够

切片 0 的实测结论是这整个任务的理由（`docs/findings/plan-a-risk-verification.md:559-620`）：`cmd /C npx` 与 `where npx` 两条路的直接子进程都是 `cmd.exe`，真正的 node server 是**孙进程**；`service.cancel()` 只打直接子进程；`taskkill /T /F` 在一次运行里 3 个 PID 只杀掉 1 个；JobObject 方案当时写了「推荐」但**明确未实测**。spec 验收线里有「无孤儿子进程」这一条，所以本任务必须把它测出来，而不是再推荐一次。

**Files:**
- Create: `src-tauri/src/mcp/process.rs`
- Modify: `src-tauri/Cargo.toml`（`windows` crate 的 `Win32_System_JobObjects` / `Win32_Foundation` features）
- Create: `src-tauri/examples/mcp_kill_probe.rs`
- Modify: `src-tauri/src/mcp/bridge.rs`（spawn 时把子进程 assign 进 job）
- Test: `process.rs` 内联 + `docs/findings/plan-b-mcp-checks.md`

**Interfaces:**
- Consumes: `tokio::process::Child::id()`
- Produces:
  - `pub struct KillOnCloseJob { handle: HWND/Handle }`（`Send`，`Drop` 里 `CloseHandle`）
  - `pub fn create_kill_job() -> Result<KillOnCloseJob, String>` —— `CreateJobObjectW` + `SetInformationJobObject(JobObjectExtendedLimitInformation, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)`
  - `pub fn assign(&self, pid: u32) -> Result<(), String>` —— `OpenProcess(PROCESS_SET_QUOTA|PROCESS_TERMINATE)` + `AssignProcessToJobObject`
  - 一个 job **每进程一个**，所有 MCP 子进程共享 ⇒ 应用退出时一次 `Drop` 带走整棵树

- [ ] **Step 1: 写探针（这是本任务的判据主体，测试只是它的机器化外壳）**

`examples/mcp_kill_probe.rs`：起一个「自己再 spawn 一个长睡子进程」的 stdio server（用 `node -e` 内联脚本，`child_process.spawn("ping", ["-t","127.0.0.1"])`），随后 drop job，每 200ms 轮询 `tasklist`，断言 3 个 PID（cmd / node / ping）全部消失，并打印各自消失时间。

```bash
cargo run --manifest-path src-tauri/Cargo.toml --example mcp_kill_probe
```

Expected: `KILL_ALL_OK in <n> ms`。**若 ping 那条留在世**（`taskkill /T /F` 当年的表现）⇒ JobObject 也没解决问题，此时**不许**在 findings 里写「基本可用」：改成对每条命令包一层「我们自己的 job + 显式 `TerminateJobObject`」并把残余风险写进 spec 验收线的例外清单，交 #29 决策。

- [ ] **Step 2: 写失败测试 —— 三个生命周期都要拆桥**

```rust
#[tokio::test]
async fn the_job_terminates_the_whole_tree_on_each_exit_path() {
    for path in [ExitPath::AppDrop, ExitPath::ServerRemoved, ExitPath::ConnectFailed] {
        let (job, pids) = start_tree(path).await;
        drop(job);
        assert!(wait_gone(&pids, Duration::from_secs(3)).await, "{path:?} 之后仍有残留进程：{pids:?}");
    }
}
```

`ConnectFailed` 那条最容易被漏：spawn 成功但握手超时被丢弃的 `Child`，如果没进 job 就是第一批孤儿。

- [ ] **Step 3: 写失败测试 —— 拿不到 job 时不许静默降级**

`create_kill_job()` 失败（权限、非 Windows）⇒ 返回 `Err`，`connect_all` 把该 server 标 `Disconnected { reason: "无法建立进程回收作业对象" }`，**不许**退化成「不带 job 直接 spawn」。理由：静默退化正是「关窗后 npx 堆积」这个桌面经典 bug 的产生方式。

- [ ] **Step 4: 体积账 + 依赖面**

`windows` crate 走 features 裁剪，报告 `cargo tree --invert windows` 的新增包数。预期近乎免费（Tauri 已在树里带它），但**必须实测**再写进报告。

- [ ] **Step 5: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml mcp::process`

```bash
git add src-tauri/src/mcp/process.rs src-tauri/src/mcp/bridge.rs src-tauri/Cargo.toml src-tauri/examples/mcp_kill_probe.rs docs/findings/plan-b-mcp-checks.md
git commit -m "feat: reclaim the whole mcp process tree with a kill-on-close job object"
```

---

### Task 35: server 管理 UI + 四条 `mcp_*` 命令

**Files:**
- Create: `src/components/McpPanel.tsx` + `McpPanel.test.tsx`
- Modify: `src/ipc/commands.ts`、`src/types.ts`、`src-tauri/src/ipc.rs`
- Test: 前端内联；`ipc.rs` 内联命令层测试

**Interfaces:**
- Consumes: `McpBridge`、`ServerStatus`、`RuleTable::set_server_trust`
- Produces: 命令 `mcp_servers() -> Vec<McpServerView>`、`mcp_set_trust(name: String, trust: String) -> Result<(), String>`、`mcp_reconnect(name: String) -> Result<usize, String>`、`mcp_disconnect(name: String) -> Result<(), String>`
  `McpServerView { name, status: "connecting"|"ready"|"disconnected", tools: usize, trust: "auto"|"ask"|"block", reason: Option<String> }`（camelCase 层沿用 `AgentPayload` 的先例）

- [ ] **Step 1: 写前端失败测试 —— 状态点与「提供 N 个工具」**

```tsx
it("announces the tool count and offers one-click Auto on first connect", () => {
  render(<McpPanel servers={[{ name: "search", status: "ready", tools: 4, trust: "ask", reason: null }]} ... />);
  expect(screen.getByText(/search 提供 4 个工具/)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "本服务器全部允许" }));
  expect(setTrust).toHaveBeenCalledWith("search", "auto");
});
it("shows the failure reason for a disconnected server without a popup", () => {
  render(<McpPanel servers={[{ name: "dead", status: "disconnected", tools: 0, trust: "ask",
    reason: "spawn 失败：program not found" }]} ... />);
  expect(screen.getByText(/program not found/)).toBeInTheDocument();
  expect(windowSpy.alert).not.toHaveBeenCalled();   // R133/R158：禁弹窗
});
```

- [ ] **Step 2: 写 Rust 侧失败测试 —— 命令不许把 env 值带出去**

```rust
#[tokio::test]
async fn mcp_servers_view_never_leaks_env_or_headers() {
    let v = mcp_servers_for(state_with_stdio_server_env_token("sk-REAL-SECRET")).await;
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("sk-REAL-SECRET"), "命令返回值会进 DevTools 与日志：{s}");
    assert!(s.contains("\"tools\":4") || s.contains("\"tools\":0"), "视图只要状态与数量");
}
```

- [ ] **Step 3: 写失败测试 —— `capabilities` 一个字都不许加**

```ts
it("grants the front end no process or filesystem capability", () => {
  const caps = require("../../src-tauri/capabilities/default.json");
  const flat = JSON.stringify(caps.permissions);
  expect(flat).not.toMatch(/shell:allow-spawn|fs:allow-read|fs:allow-write/);
  expect(flat).toMatch(/core:event/);   // 事件通道仍是前端唯一的外部世界
});
```

这条测试是安全红线的机器哨兵：MCP 的进程执行全在 Rust，若有人图省事给前端开 `plugin-shell`，这里立刻红。**同时**要改掉 `capabilities/default.json` 的 `description`（Plan A 遗留：那句「前端只允许事件、对话框与自定义命令」在 #29 删掉 dialog 授权后必须同步改），两处一次改完，别留注释比权限宽的状态。

- [ ] **Step 4: 跑双测试器 + 体积/性能不回归**

Run: `npx tsc --noEmit` / `npx vitest run` / `npm run lint:css` / `cargo test --manifest-path src-tauri/Cargo.toml ipc`
Expected: 全绿。面板的挂载位置按 Plan A T16 实产的组件树（侧栏底部或设置抽屉，以 T16 交付的结构为准，**不重构 T16**）。

- [ ] **Step 5: 提交**

```bash
git add src/components/McpPanel.tsx src/components/McpPanel.test.tsx src/ipc/commands.ts src/types.ts src-tauri/src/ipc.rs src-tauri/capabilities/default.json
git commit -m "feat: add the mcp server panel with status, trust switching and four commands"
```

---

### Task 36: Streamable HTTP 传输（体积闸门后可选）

**Files:**
- Modify: `src-tauri/Cargo.toml`（`rmcp` 加 `transport-streamable-http-client`）
- Modify: `src-tauri/src/mcp/bridge.rs`（`ServerDef::Http` 分支）
- Test: `bridge.rs` 内联

- [ ] **Step 1: 先量成本再决定做不做（这一步的产物是数字，不是代码）**

```bash
cargo tree -e normal --manifest-path src-tauri/Cargo.toml | grep -c ''
git diff --numstat src-tauri/Cargo.lock   # 加 feature 前后各跑一次
cargo build --release --manifest-path src-tauri/Cargo.toml && ls -l src-tauri/target/release/qwen-do.exe
```

Expected: 报告写清新增包数与 exe 体积前后差。**裁定**：若该 feature 引入第二套 HTTP 栈（`reqwest` 之外的 `hyper`+`rustls` 组合或 `aws-lc`）导致 exe 增幅 >5%，就把 HTTP 传输推到 Plan C 之后单独决策，本任务只留 `ServerDef::Http` 的 `Disconnected { reason: "此构建未启用 HTTP 传输" }` 分支与测试——**明确降级比悄悄拖大安装包好**。

- [ ] **Step 2: 写失败测试 —— URL 走同一套 SSRF 判定**

MCP 的 `url` 是**用户自己配的**，但仍要过 T22 的 `check_url`：用户抄来的一份配置指向 `http://169.254.169.254/` 时，错误要指名，而不是让 `rmcp` 抛一个难读的 connect 超时。

- [ ] **Step 3: 写失败测试 —— headers 里的 token 不进日志**

`ServerDef::Http { headers }` 复用 T29 的手写 `Debug` 打码；再加一条：连接失败的 `reason` 字符串**不许**内嵌请求头回显。

- [ ] **Step 4: 跑测试红 → 实现 → 绿 → 提交**

Run: `cargo test --manifest-path src-tauri/Cargo.toml mcp::bridge`

```bash
git add src-tauri/src/mcp/bridge.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: connect streamable http mcp servers behind a size gate"
```

---

### Task 37: 切片 4 端到端验证（需人工在场）

**Files:**
- Create: `docs/findings/plan-b-slice4-checks.md`

- [ ] **Step 0: 同 Plan A R153 的拆法**

Step 1–4 要真 node、真 server、真 key；子 agent 不得以源码推断冒充运行结果，做不到就写 `未执行（需人工在场）`。

- [ ] **Step 1: 接一个真的搜索 server**

`mcp.json` 填 `{"mcpServers":{"fetch":{"command":"npx","args":["-y","@mcpserver/fetch"]}}}`（或本地已验证的 `server-everything`）。问一个必须联网才能答的问题。
Expected: 模型调用 `mcp__fetch__*` 并在回答中**引用来源 URL** —— spec 验收线「MCP 可用」的原话判据。卡片 preview 不许是 base64。

- [ ] **Step 2: 信任级别三档各跑一次**

`ask` ⇒ 出现一张权限卡，允许后本轮继续；拒绝 ⇒ `tool_result is_error=true` 且卡片状态落定（Plan A R131：`respond` 返回 `false` 时卡片不许变化）。`block` ⇒ **没有**卡片、模型直接收到拒绝理由。`auto` ⇒ 无卡。
findings 要记下每档的实际事件数，`permission_request` 在非 ask 档必须为 0。

- [ ] **Step 3: 无孤儿子进程（spec 验收线原话）**

连接 ≥1 个 stdio server 后关窗口，等 3s，`tasklist` 里不得有 `node.exe`/`npx` 残留。**同时**记下一个诚实的边界：本机若跑着与本应用无关的 node 进程（Qoder 自己就有！），先记下基线 PID 再对比，**不许**用「看起来干净」代替差集。

- [ ] **Step 4: MCP 图片结果真的进了 vision 通路**

用一个会返回截图的 server（`server-everything` 有 image 测试工具即可）：Expected: 模型能描述图片内容。失败模式要点名：如果模型说「我看不到图」，先查 `map_result` 的 mime 分支而不是怪模型。

- [ ] **Step 5: 性能三条不回归（每片都要重量，不是只在最后一量）**

安装包体积 / 冷启动 5 次中位数 / 空闲常驻内存，写进同一份 findings，并与 Plan A #29 记下的数字并排 —— 差值要能归因到具体依赖（`windows` features、`htmd`、可选的 rmcp feature）。

- [ ] **Step 6: 记录并提交**

```bash
git add docs/findings/plan-b-slice4-checks.md
git commit -m "docs: record slice 4 end-to-end mcp checks including trust levels and orphan scan"
```

---

## 与 spec 的四处刻意偏离（写下来，别让后来人以为是遗漏）

1. **`Location` 伪装主机的拒绝（T23 Step 2）**：spec 只写了「重定向后的每一跳目标」，本计划额外拒 `@` 伪装与协议相对补方案后再判 —— WHATWG 解析器会让 `http://127.0.0.1\@ok.example/` 的主机变成 `ok.example`，不额外处理等于留洞。
2. **`registrable()` 不处理公共后缀（T23 Step 3）**：为不引入 `psl` 依赖，`a.co.uk` 这类会被算成跨主。方向是安全的（多报一次提示，不会漏报），但会多几行噪声，记在此处。
3. **反爬/验证码页不做识别（T24 Step 3）**：spec 5.4 要求「不把验证码页面喂给模型」，本计划实现为「明示最终主机 + 回显 403/429」，把判断交给模型而不是我们假装能识别。
4. **`ToolOutput` 由两变体扩为支持多部分（T32 Step 1）**：MCP 的 `content` 本来就是混合数组，硬塞成单个 Text 会丢图。这条改动会同时碰 `agent/types.rs`、TS 镜像与 Plan A 的跨语言名单守卫测试 —— 触碰面比「加一个工具」大，是 spec 5.2 第 2 点的必然代价，不是范围蔓延。

## 执行顺序与人工节点

`22 → 23 → 24 → 27 → 28(人工)` 与 `25 → 26 → 27` 两条支链在 T27 汇合；`29 → 30 → 31 → 32 → 33 → 34 → 35 → 37(人工)` 串行，`36` 独立且可被体积闸门否决。T28 与 T37 需要真实窗口与用户本人的凭据（沿用 Plan A R153/R156 的拆法：控制器可以跑只读探针，代填真 key 不允许）。

