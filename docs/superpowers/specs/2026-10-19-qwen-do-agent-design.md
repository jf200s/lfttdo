# Qwen Do — 轻量日常 Agent 设计文档

- 日期：2026-10-19
- 状态：设计已确认，待生成实施计划
- 范围：MVP。第二阶段内容在第 10 节明确排除。

---

## 1. 概述与目标

一个跑在 Windows 桌面上的轻量 AI 助手，主场景是**日常聊天、联网查询、看图问答**，通过 MCP 客户端接入外部工具生态。文件与编程类操作不是主场景。

三条硬性目标：

| 目标 | 指标 |
|---|---|
| 运行时轻 | 安装包 <15MB，冷启动 <1.5s，空闲常驻内存 <150MB |
| 交互轻 | 流式逐字上屏不掉帧；日常使用全程零弹窗；中断在 1s 内生效 |
| 可自定义 | 主题级：预设主题、亮/暗、强调色、字号 |

「轻」在这里指运行时占用与交互摩擦，**不**指代码简单——用户明确接受代码复杂度换取运行时轻量。

## 2. 决策清单

### 2.1 生效中的决策

| 项 | 决定 | 主要理由 |
|---|---|---|
| 定位 | 日常助手：聊天、联网、看图 | 用户主场景；编程/文件为次要 |
| 技术栈 | Tauri v2 + **Rust 全内核** + React/TS 前端 | 满足运行时轻；且消灭了两个技术风险（见 2.3） |
| 模型接入 | OpenAI 兼容协议，`async-openai` | 一套 base_url+key 覆盖百炼 Qwen / DeepSeek / OpenRouter / Ollama |
| 视觉能力 | 用户发图给模型（vision 输入） | 不含 agent 自主截屏、不含图片生成 |
| MCP | 仅客户端，`rmcp`，stdio + Streamable HTTP | 95% 的需求；工具从外部生态获取 |
| 内置工具 | MVP 只有 `fetch_url`、`read_file`，均为只读 | 搜索能力交给 MCP，顺带验证 MCP 链路 |
| 权限 | 分级确认；MVP 内置工具全为 `Read` → 零弹窗 | 日常场景弹窗即毁体验 |
| 界面自定义 | 主题级（CSS 变量 + 预设） | 收益直观、工作量小 |
| 并发模型 | 同一时刻只跑一轮；切换会话即中断当前轮 | 多会话并发要处理事件路由/挂起归属/取消语义，日常不需要 |

### 2.2 过程中被推翻的决策（保留以免重蹈）

| 曾决定 | 推翻原因 |
|---|---|
| TypeScript + Electron | Electron 约 200MB、内存 150–300MB、启动 1–3s，与「运行时轻」直接冲突 |
| 定位为编程助手（读写文件、执行命令） | 用户实际主场景是日常聊天与联网；文件工具从 MVP 移除 |
| 内置文件六件套 + 分级弹窗 | 同上；且日常场景下每次搜索都弹窗不可接受 |
| 内核在 TS（WebView 内）+ 薄 Rust 桥 | 用户选择全 Rust 内核；该方案需自写 SSE 桥与 MCP stdio Transport，风险更高 |

### 2.3 选择全 Rust 内核的两个附带收益

1. **SSE 流式不再走 `tauri-plugin-http`。** 该插件的流式行为曾有已知问题（plugins-workspace#2415），社区有「不支持流传输」的记录，也有项目成功用它绕过 WebView CORS——即结论依赖版本，属于必须验证的风险。Rust 侧直接用 `reqwest`，此风险消失。
2. **MCP 不需要自写 Transport。** 官方 TS SDK 的 stdio 传输依赖 `process.stdin/stdout`，WebView 中不存在，本需基于 `plugin-shell` 手写约 100–150 行行分帧 JSON-RPC。`rmcp` 是官方 Rust SDK，客户端与 stdio 传输都是原生支持路径。

## 3. 架构与模块边界

```
qwen-do/
├─ src-tauri/src/
│  ├─ main.rs               # Tauri 入口：注册 commands、装配依赖
│  ├─ agent/
│  │  ├─ session.rs         # AgentSession：循环编排（唯一编排者）
│  │  ├─ events.rs          # AgentEvent 枚举 + serde(tag="t")
│  │  └─ context.rs         # system prompt + 历史 + token 预算 + 压缩
│  ├─ model/
│  │  └─ openai_compat.rs   # async-openai 封装：流式、tool_call 分片拼装、base_url 规范化
│  ├─ tools/
│  │  ├─ registry.rs        # Tool trait + Risk 元数据
│  │  ├─ fetch_url.rs
│  │  └─ read_file.rs
│  ├─ permission/gate.rs    # 规则表 + 待决请求（oneshot channel 挂起等 UI）
│  ├─ mcp/bridge.rs         # rmcp 客户端：连接、发现、命名空间前缀、信任级别
│  ├─ store/
│  │  ├─ db.rs              # rusqlite：sessions / messages（WAL 模式）
│  │  └─ secrets.rs         # keyring：API key 进系统凭据库，不落明文
│  └─ ipc.rs                # Tauri commands + 事件发射（唯一依赖 tauri 的文件）
├─ src-tauri/Cargo.toml
├─ src-tauri/tauri.conf.json
└─ src/                     # React + TS 前端
   ├─ state/reducer.ts      # 纯函数：AgentEvent[] → UiState
   ├─ components/           # 气泡、工具卡片、权限卡片、输入区、设置面板
   └─ theme/                # tokens.css + presets.css
```

### 3.1 单元职责

| 单元 | 做什么 | 依赖谁 | 对外接口 |
|---|---|---|---|
| `AgentSession` | 跑循环：发请求 → 收流 → 判停 → 执行工具 → 回填 | 其余全部 | `run(input)` |
| `openai_compat` | 统一消息格式 ↔ 供应商格式；SSE 增量 → 完整 `tool_call` | `ModelProvider` | `stream(req) -> Stream<ModelDelta>` |
| `ToolRegistry` | 持有工具，校验入参，执行 | `Tool` trait | `tool_defs()` / `execute()` |
| `PermissionGate` | 判定 Auto/Ask/Block；Ask 时挂起等 UI | `RuleTable` | `check(call) -> Decision` |
| `McpBridge` | 把远程 MCP 工具变成 registry 里的普通工具 | `rmcp` | `connect/save/disconnect/test` |
| `ContextBuilder` | 组装待发消息数组；超预算则压缩 | 历史 | `build(history) -> Vec<Message>` |
| `reducer.ts` | 事件序列 → 界面状态 | `UiState` 类型 | `reduce(events, state)` |

### 3.2 四条边界约束

1. **UI 只消费事件，不参与决策。** 前端不判断工具风险级别——只渲染 `permission_request` 带来的卡片，点击后把 `respond` 打回 Rust。
2. **MCP 工具与内置工具在 registry 里同质。** MCP 工具注册时加服务器名前缀并携带信任级别；循环、权限门、UI 都不为 MCP 写特例。信任级别就是 `RuleTable.overrides` 中的一条记录。
3. **只有 `ipc.rs` 允许 `use tauri`。** `agent/`、`model/`、`tools/`、`permission/`、`mcp/`、`store/` 全部与 Tauri 无关。这是 Rust 版的可测性命门——否则 `session.rs` 的循环逻辑必须有 `AppHandle` 才能测，等于测不了。
4. **依赖以 trait 注入。** `trait EventSink` 与 `trait ModelProvider`；生产实现为 `TauriEmitter` / `OpenAiCompat`，测试实现为 `VecSink` / `ScriptedModel`。由此整个 agent 循环可用 `#[tokio::test]` 端到端测试，不需要 Tauri、不需要网络、不需要真 API key。

约束 3、4 是本次栈变更后新增的坚持项：Rust 中「核心逻辑依赖框架句柄」是极易犯且后期极难拆的错误。

## 4. 接口与事件契约

### 4.1 消息与内容块

```rust
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { data: String, mime: ImageMime },              // base64，已归一化
    ToolCall { id: String, name: String, input: Value },
    ToolResult { call_id: String, is_error: bool, output: ToolOutput },
}

pub struct Message { pub role: Role, pub blocks: Vec<ContentBlock> }
```

`Image` 只接受**已归一化的 base64**。归一化在 Rust 侧单一函数完成：`image` crate 解码 → 最长边 >1568px 等比缩放 → 重编码；webp/gif 统一转 jpeg（gif 取首帧）；>5MB 逐级降质量，仍超则返回明确错误而非静默截断。前端仅负责取得文件字节。该设计使「用户贴图」与将来「agent 自主读图」共用同一路径，且可单测。

### 4.2 工具契约

```rust
pub enum Risk { Read, Write, Exec }

pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: Risk,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;
    async fn execute(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError>;
}

pub struct ToolCtx {
    pub http: reqwest::Client,        // 全局共享连接池，勿每工具新建
    pub cancel: CancellationToken,    // tokio-util：中断传播
    pub roots: Vec<PathBuf>,          // read_file 的允许目录
}
```

`roots` 是路径穿越防线：解析为绝对路径后必须落在某个 root 之内；`..`、符号链接、跨盘符绝对路径一律拒绝。

### 4.3 事件流——Rust 与前端之间的唯一通道

```rust
#[derive(Serialize, Clone)]
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
    Error { #[serde(rename = "where")] at: ErrorWhere, message: String, fatal: bool },
    Done { stop_reason: StopReason },
}

pub enum Decision {
    Allow { scope: Scope },          // Once | Session
    Deny { reason: String },
}
```

`where` 是 Rust 关键字，故字段名用 `at` 并以 `serde(rename)` 输出为 `where`。

`preview` 是供 UI 显示的截断摘要（前 200 字符）。**完整工具输出不进事件流**——一次 `fetch_url` 拉回 200KB 正文会同时拖垮 IPC 与前端渲染。完整内容只进 history；前端需要看全文时单独取。

上述枚举引用的辅助类型一并定死，避免实现时各自发明：

```rust
pub enum ErrorWhere { Model, Tool, Mcp, Loop, Store }   // fatal 由发出方决定，不由 where 推断
pub enum StopReason { EndTurn, MaxTurns, Interrupted, Error }
pub enum Scope { Once, Session }

pub enum ImageMime { Png, Jpeg }        // 归一化后只可能是这两种，见 4.1
pub enum Role { System, User, Assistant }

pub enum ToolOutput {
    Text { text: String },
    Image { data: String, mime: ImageMime },
}
impl ToolOutput { pub fn error(msg: impl Into<String>) -> Self { /* Text + is_error 标记 */ } }
```

`ImageMime` 只有两个变体是刻意收窄：4.1 的归一化规定 webp/gif 一律转 jpeg，因此内部不再需要处理其他格式。若将来支持「原样透传给支持 webp 的端点」，再扩这个枚举。

### 4.4 两个可替换依赖

```rust
pub trait EventSink: Send + Sync { fn emit(&self, e: AgentEvent); }

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(&self, req: ChatRequest)
        -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>;
}
```

`ModelDelta` 至少包含：`Text(String)`、`ToolCallStart{index,id,name}`、`ToolCallArgs{index,fragment}`、`Usage{..}`、`Done`。

### 4.5 权限门：挂起等 UI，但绝不永久挂死

```rust
pub async fn check(&self, call: &ToolCallView) -> Decision {
    if let Some(d) = self.rules.lookup(call) { return d; }   // 命中规则：直接放行，不发事件
    let (tx, rx) = oneshot::channel();
    let id = Uuid::new_v4().to_string();
    self.pending.lock().insert(id.clone(), tx);
    self.sink.emit(AgentEvent::PermissionRequest {
        id: id.clone(), call: call.clone(), risk: call.risk,
    });
    rx.await.unwrap_or(Decision::Deny { reason: "确认窗口已关闭".into() })
}
```

`rx.await` 挂起的是 async task，不阻塞线程。`unwrap_or` 是必需兜底：用户关窗口、前端崩溃、会话被中断时 `tx` 被 drop——没有兜底则整个循环永久卡死且无任何报错。「静默挂死」是权限门设计中最容易漏的一处。

### 4.6 循环骨架

`max_turns` 默认 25。日常对话通常 1 轮结束，带工具的多为 2–5 轮；25 是防模型自我循环的保险丝，触顶时发 `Done{MaxTurns}` 并在 UI 明确提示，而不是静默停止。

```rust
for turn in 1..=max_turns {
    if cancel.is_cancelled() { return finish(Interrupted); }
    sink.emit(TurnStart { turn });

    let msgs = context.build(&history)?;                  // token 预算 + 压缩
    let mut stream = model.stream(msgs, registry.tool_defs()).await?;
    let mut acc = AssistantAcc::default();
    while let Some(item) = stream.next().await {
        tokio::select! {
            _ = cancel.cancelled() => return finish(Interrupted),
            d = item => acc.apply(d?, &sink)?,            // TextDelta 边收边 emit
        }
    }

    let assistant = acc.into_message();
    history.push(assistant.clone());
    if assistant.tool_calls().is_empty() { return finish(EndTurn); }

    for call in assistant.tool_calls() {                  // 顺序执行，不并行
        let result = match gate.check(call).await {
            Decision::Deny { reason } =>
                ToolOutput::error(format!("用户拒绝执行：{reason}")),
            Decision::Allow { .. } => registry.execute(call, &ctx).await,
        };
        history.push(Message::tool(result));
    }
}
finish(MaxTurns)
```

**拒绝不是错误。** 用户拒绝后写入一条 `is_error: true` 的 `ToolResult`，内容为拒绝理由，循环继续。模型据此自行换路，比中断整轮对话好用得多。

**工具顺序执行，不并行。** 并行虽快，但难以追踪「哪个写操作改了哪个文件」，且与分级确认的交互顺序冲突。这是刻意取舍。

### 4.7 流式 tool_call 分片拼装（头号 bug 源）

流式 `tool_calls` **按 `index` 分片到达**：首个分片带 `id` 与 `name`，后续分片只带 `arguments` 的字符串片段。累加器必须按 `index` 归组、拼接 `arguments` 字符串、在流结束后才对完整字符串做一次 `serde_json::from_str`。提前解析或忽略 `index` 会得到空参数或 panic。

`AssistantAcc` 单测必须覆盖：分片乱序到达、JSON 在 chunk 中间被切断、`arguments` 为空、一次返回多个 `tool_call`。

### 4.8 IPC 契约

**Commands（前端 → Rust）**

```
send_message(session_id, blocks)      respond(request_id, decision)
interrupt(session_id)                 normalize_image(bytes) -> ImageBlock
session_list / session_create / session_delete / session_rename
session_messages(session_id)          settings_get / settings_set
mcp_list / mcp_save / mcp_connect / mcp_disconnect / mcp_test
```

**Events（Rust → 前端）**：单一通道 `agent`，payload `{ sessionId, event }`。

## 5. 核心逻辑

### 5.1 权限规则表

```rust
pub enum Verdict { Auto, Ask, Block }

pub struct RuleTable {
    by_risk: HashMap<Risk, Verdict>,     // 按风险级别的默认值
    overrides: Vec<Rule>,                // 按工具名 / server 名的精确覆盖
}
```

默认策略：

| Risk | 默认 Verdict | MVP 成员 |
|---|---|---|
| `Read` | **Auto** | `fetch_url`、`read_file`、多数 MCP 只读工具 |
| `Write` | Ask | MVP 无 |
| `Exec` | Ask | MVP 无 |

MVP 内置工具全为 `Read`，因此**全程零弹窗**。这是「交互轻」的直接兑现。第二阶段加入 `write_file` 时，弹窗链路已就位并已被测试。

**session 级「以后都允许」的作用域令牌（安全要点）。** 只记裸工具名会出事：模型请求写 `~/.ssh/authorized_keys`，用户点了一次「本会话都允许」，此后写任何路径都不再询问。因此 session 规则的 key 必须带作用域令牌：

```
工具名 + (文件类 → 允许的根目录) / (命令类 → 可执行文件名) / (MCP → server 名)
```

即「本会话都允许」= 允许**在该目录内**写文件，而非无限写。

### 5.2 MCP 桥接

配置格式**兼容 `mcpServers` 结构**（Claude Desktop / ChatWise 那套），使用户可直接抄用现成配置。

连接流程：

```
读 mcp.json
  → 各 server 并行连接（互不阻塞）
      stdio: TokioChildProcess spawn 子进程
      http:  StreamableHttpClientTransport
  → initialize() 握手 → list_tools()
  → 每个工具包成 McpTool{ server, original_name, trust }
  → 注册进 registry，重命名为 mcp__{server}__{tool}
```

四个必须处理的点：

1. **信任级别落在 server 粒度，且它就是一条 override。** 添加 server 时选 `Auto` / `Ask` / `Block`，默认 `Ask`。UI 首次连接时明示「该服务器提供 N 个工具」，一键切 `Auto` 并持久化。因它是 `RuleTable` 的普通记录，第 3.2 节约束 2（同质）得以守住。
2. **MCP 返回的图片必须映射为 `ContentBlock::Image` 并走同一归一化路径。** MCP 的 `content` 是 text/image/resource 混合数组。`browser-use` 类 server 的 `take_screenshot` 返回图片；若不映射，vision 模型看不见它，只会把 base64 字符串当文本塞进上下文。这条通路是「看图」能力在 MCP 侧的延伸。
3. **工具名 64 字符上限。** `mcp__{server}__{tool}` 遇长命名 server 会超 OpenAI 兼容端限制。兜底：超限时对 server 段取短哈希，冲突时追加序号。
4. **子进程不能泄漏。** 应用退出、server 被删除、连接失败时均需显式 kill。桌面应用最常见的 bug 是关窗后 `npx` 子进程留在后台不断累积；Windows 上 tokio `Child` 的 kill 行为与 Unix 不同，需单独验证。

失败处理：spawn 失败或握手超时 → 该 server 标 `Disconnected`，UI 显示状态点，**不影响其他 server 与主对话**；工具调用失败 → 转成 `ToolOutput::error`，模型可见并自行换路。

### 5.3 上下文管理

日常聊天必然越聊越长，「失忆」会比任何功能缺失更早毁掉体验。

```
预算 = min(模型上下文上限 - 输出预留, 配置上限)
```

具体默认值（均可在设置面板调整）：

| 参数 | 默认 | 含义 |
|---|---|---|
| 输出预留 | 4096 tokens | 从模型上下文上限中扣掉，留给回复本身 |
| 配置上限 | 32000 tokens | 用户可下调以省钱 |
| 压缩阈值 | 预算的 85% | 超过即触发压缩，留出余量避免刚好卡线 |
| 保留轮数 K | 最近 10 轮 | 压缩时这部分保留原文 |
| 压缩批量 N | 最老的一半（至少 4 轮） | 一次压掉多少，避免每轮都压一次 |
| 滑窗兜底 | 最近 40 轮 | 切片 1 的临时保护，摘要压缩上线后仍保留为最后防线 |

- **Token 估算**：粗估起步（CJK 约 1 字 = 1 token，英文约 4 字符 = 1 token），每次 API 返回真实 `usage` 后校准系数。仅用粗估会在中文长文上严重低估。
- **滚动摘要压缩**：超阈值时把最老的 N 轮交给模型摘要成一条备注消息（`早期对话摘要：…`），保留最近 K 轮原文，并发 `ContextCompacted` 事件让 UI 提示「已压缩早期对话」。属 MVP 范围——砍掉它等于砍掉主场景。
- **图片降级**：压缩时历史中的 `Image` 块**必须**降级为文本占位（`[图片已移除以节省上下文]`）。图片 token 极贵，且不少兼容端点不接受出现在非最近消息位置的历史图片。留着的后果是「聊到第 20 轮突然 400」，极难排查。
- **兜底**：摘要后仍超预算 → 从最老的非 system 消息整条丢弃，保证不报错。摘要调用自身失败时同样降级为直接丢弃，并附一条 `[早期对话已省略]` 占位，使用户知道发生了信息丢失而非模型无故失忆。
- **滑窗兜底在切片 1 就实现**（保留最近 40 轮），使长对话在摘要压缩完成前也不会撑爆。

### 5.4 `fetch_url`

**SSRF 防护是本工具唯一的安全红线。** 模型可被一句「看看 http://169.254.169.254/latest/meta-data/」或内网地址诱导并把内容读回。必须硬性拒绝：`file://`、`localhost`、环回与私网 IP 段（127/8、10/8、172.16/12、192.168/16、169.254/16）、云元数据 IP，**以及重定向后的每一跳目标**（不能只校验首次 URL）。此项须有单测覆盖，不是可选项。

其余实现：

- 共享 `ToolCtx.http`（明确 UA、20s 超时、重定向 ≤5 跳）
- 响应体上限 2MB；只接受 `text/html`、`text/plain`、`application/json`，其余返回明确错误
- HTML → Markdown：**选定 `htmd`**（见 5.5）
- 输出截断默认 8000 字符，保留标题层级；超限附 `[已截断，原文 N 字符]`，并提供 `offset` 参数让模型分页读
- 302 跳登录页/反爬页时返回明确提示，不把验证码页面喂给模型

### 5.5 两处已拍板的小决策

| 决策 | 选定 | 理由 | 可推翻时机 |
|---|---|---|---|
| HTML → Markdown 选型 | `htmd` | 定位非编程场景，正文质量够用即可；比 `readability` + `html2md` 组合少两个依赖 | 切片 3 内实测抽取质量，不达标即换 |
| 摘要压缩用哪个模型 | 与主对话同一模型 | MVP 不引入第二套模型配置；配置项二期加 | 切片 5 若压缩成本明显，加专用模型配置 |

## 6. 前端交互与 Tauri 绑定

### 6.1 界面结构

```
┌─ 侧栏（会话列表 + 新建 + 设置入口）─┐ ┌─ 主区 ──────────────┐
│                                    │ │ 消息流（虚拟列表）    │
│                                    │ ├─ 输入区 ──────────────┤
│                                    │ │ 缩略图条 / 输入框     │
│                                    │ │ 附件 · 模型名 · 发送/停止 │
└────────────────────────────────────┘ └──────────────────────┘
```

四块：侧栏、消息流、输入区、设置面板（模态）。**没有文件树、编辑器、diff 视图。**

### 6.2 事件 → 状态：一个纯 reducer

```ts
reduce(events: AgentEvent[], state: UiState): UiState
```

| 事件 | reducer 动作 |
|---|---|
| `turn_start` | 新建 assistant turn |
| `text_delta` | 追加/合并到该 turn 末尾的 text block |
| `tool_call` | push `status: running` 的工具卡片 |
| `permission_request` | push 权限卡片（pending），按 `id` 入索引 |
| `permission_resolved` | 按 `id` 更新卡片为 已允许/已拒绝 |
| `tool_result` | 按 `call_id` 找回工具卡片，填 `preview`、耗时、错误标记 |
| `usage` | 更新状态栏 token 计数 |
| `context_compacted` | push 分隔提示「已压缩早期对话」 |
| `error` | `fatal` → 顶部红条；否则 → turn 内警示条 |
| `done` | `streaming = false`，恢复输入 |

**关键决定：界面状态不落盘，持久化的 messages 才是唯一真相源。** 重启后把 messages 映射成等价事件序列，重放同一 reducer 得到界面。收益：没有两套渲染路径、无需为 UI 状态设计存储格式、reducer 的测试天然就是「喂事件、断言状态」。代价：`usage`、压缩提示等瞬时事件重开即消失——可接受，它们本就不该假装是历史。

`tool_result` 与 `permission_resolved` 需按 id 回找先前 block，故 state 内维护 `Map<string, index>`。这是该 reducer 唯一的状态复杂度。

### 6.3 流式渲染性能

模型逐 token 输出时 `TextDelta` 可达 50–100 次/秒。每次 IPC → 每次 `setState` → 每次重解析整段 Markdown，长回答必然掉帧。**两层合帧，第一层在 Rust：**

1. **Rust 侧合帧**：`TextDelta` 累积到 30ms 或 512 字符才 `emit` 一次。IPC 穿越次数降一个数量级，比在前端省渲染有效得多。
2. **前端合帧**：reducer 收到的文本先进缓冲区，由 `requestAnimationFrame` 冲刷。
3. **只对已完成的块跑 Markdown**：流式中的最后一个块按纯文本渲染（保留换行），块结束或 `done` 后再做完整 Markdown 与代码高亮。避免半截语法反复重解析，也避免闪烁。

### 6.4 主题系统

```
theme/tokens.css    # --bg-surface --bg-raised --text-primary --text-muted
                    # --accent --border --code-bg --radius --font-size --bubble-max-w
theme/presets.css   # [data-theme="noir|paper|ocean"] + [data-mode="light|dark"] 覆盖 tokens
```

**一条纪律撑着整节：组件内禁止硬编码颜色，只能用 token 变量。** 缺此纪律，「主题可自定义」会在第四个组件处崩掉，且每次都要回炉旧组件。需在 review 中当真执行。

设置项：预设主题 3 套 + 亮/暗 + 强调色取色器（写 `--accent`）+ 字号。存设置表，通过 `<html>` 上的 `data-theme` / `data-mode` 属性生效。

### 6.5 图片输入

三个入口：拖拽进窗口、`Ctrl+V` 粘贴剪贴板图、点附件按钮选文件。

通路：前端取 `File.arrayBuffer()` → 以 `Vec<u8>` 传给 command（Tauri v2 支持二进制 IPC，不走 JSON base64）→ Rust 跑 4.1 的归一化函数 → 返回 `ImageBlock` → 界面显示缩略图 → 发送时随消息带上。

- 单条消息最多 4 张，超出直接提示。
- 归一化失败必须指名是哪张、为什么（如「GIF 过大且降质后仍超 5MB」），不得静默丢图——否则模型答非所问时无法判断是否图未送达。
- 二进制 IPC 若不如预期，兜底方案为 base64（见第 9 节风险）。

### 6.6 Capabilities 配置

**白捡的安全收益：** MCP 子进程由 Rust 侧 `tokio::process` 直接 spawn，**不经 shell plugin**，因此前端不申请任何 `shell:allow-spawn` 权限——**前端完全没有执行进程的能力**。这是全 Rust 内核路线的额外好处，须写入设计而非偶然获得。

前端仅申请：自定义 commands、`dialog:allow-open`、窗口状态恢复。文件系统不给全局权限；`read_file` 的目录白名单由 Rust 侧 `ToolCtx.roots` 管理，不依赖 Tauri fs scope。

窗口配置：单窗口、记住尺寸位置、release 关 devtools。开发机为 Windows 11 24H2，WebView2 运行时系统自带；若打包给 Win10 用户，安装说明需附 WebView2 bootstrapper。

### 6.7 设置面板

五组：

1. **模型**：base_url、model、api_key、输出上限、温度
2. **上下文**：预算上限、保留轮数 K、压缩阈值
3. **MCP**：server 列表（名称 / 命令或 URL / 环境变量 / 信任级别 / 启停）+ 连接状态点 + 测试按钮
4. **工具**：`read_file` 允许目录 `roots`
5. **界面**：主题 / 亮暗 / 强调色 / 字号

`api_key` 只写进 `keyring`（Windows 凭据管理器），**不写 settings 表、不进导出、不写日志**；设置读取接口返回掩码值而非明文。

**停止按钮**：streaming 时发送键变「停止」，点击后立即禁用并显示「停止中…」，等 Rust 的 `Done{interrupted}` 再恢复输入。**不做乐观更新**——中断是真异步的，乐观显示会让人以为已停止却继续收到文字。

## 7. 实施切片

每片都是端到端可用的，不是分层堆叠。

| 切片 | 交付的可用能力 | 触及模块 |
|---|---|---|
| **0 风险验证** | 可丢弃骨架：真 key 流式收一次；`rmcp` 连一次真 server；二进制 IPC 传一次图；打包体积基线 | main.rs, openai_compat, mcp/bridge |
| **1 纯聊天** | 多轮对话 + 流式 + 停止 + SQLite 会话持久化 + 侧栏/消息流/输入区 + token 变量骨架 + 滑窗兜底 | agent/session, store/db, 前端全部 |
| **2 看图** | 拖拽/粘贴/选择三入口 + `normalize_image` + 缩略图 + vision block + 历史图片降级规则 | agent/types, ipc, 前端输入区 |
| **3 工具与联网** | `Tool` trait + registry + `fetch_url`（含 SSRF 防护）+ `AssistantAcc` 流式拼装 + 循环回填 + 工具卡片 + 权限门机制（默认全 Auto，不弹但代码就位） | tools/\*, permission/gate |
| **4 MCP** | `mcp.json` 兼容配置 + stdio/Streamable HTTP 双传输 + 命名空间与 64 字符兜底 + 信任级别 override + MCP 图片结果映射 + server 管理 UI + 子进程清理 | mcp/bridge |
| **5 上下文** | usage 校准 + 预算 + 滚动摘要压缩 + `ContextCompacted` 事件 + 长会话压测 | agent/context |
| **6 设置与交付** | 五组设置 + `keyring` + 三套预设主题/亮暗/强调色/字号 + Windows NSIS 打包 + 性能达标 | store/secrets, theme |

切片 1 结束即得到一个**能天天用的最小聊天客户端**——刻意如此，不要等到第 6 片才第一次用上。切片 3 先建权限门但不启用弹窗，让机制在真正需要它的切片之前就经过测试。

## 8. 测试策略

**Rust 单测**（依赖 3.2 约束 3、4，无需 Tauri、无需网络）

- `session.rs` × `VecSink` × `ScriptedModel`：`end_turn` / 带工具轮 / 用户拒绝后继续 / 中断 / `max_turns` 触顶 / 压缩触发 —— 六条路径
- `AssistantAcc`：分片乱序、JSON 跨 chunk 切断、空 `arguments`、多 `tool_call` 的 index 归组
- `openai_compat`：base_url 规范化（`/v1` 重复、末尾斜杠、缺 `/v1`）
- `gate`：命中规则时**不发** `permission_request`；sender 被 drop 时兜底 `Deny` 而非挂死
- `fetch_url`：SSRF 用例表（`file://`、`127.0.0.1`、`10/172.16/192.168`、`169.254.169.254`、**302 跳转到内网**）
- `read_file`：`..` 穿越、符号链接、跨盘符越界
- `context`：预算计算、图片降级、丢弃兜底不报错、摘要失败降级
- `normalize_image`：各格式、超限降质、gif 首帧

**集成测试**：本地起假的 OpenAI 兼容 server（吐预录 SSE 字节流）验证真流式管线；`rmcp` 连官方 `server-everything` 跑 `list_tools` + `call_tool` + 图片结果映射。

**前端**：reducer 纯函数测试（喂事件序列断言 `UiState`）；组件禁止硬编码颜色的规则在 review 中执行。

**手动验收**：真 key 全流程、真 MCP server、体积/内存/启动时间基线实测。

## 9. MVP 验收标准

每条都给出测量方式——无法测量的标准等于没有标准。

```
□ 安装包 <15MB
    测法：NSIS 产物文件体积
□ 冷启动 <1.5s
    测法：双击图标 → 输入框可获得焦点并响应键盘（非窗口出现即算）；取 5 次中位数
□ 空闲常驻内存 <150MB
    测法：无对话进行中、窗口打开，任务管理器中该应用全部进程（含 WebView2 子进程）之和
□ 流式输出不掉帧
    测法：DevTools Performance 录制一次长回答，主线程单帧耗时 <16ms；
          且合帧后 React 渲染频率 ≤10 次/秒
□ 中断在 1s 内生效
    测法：点击停止 → 前端不再收到新的 text_delta 的耗时 <1s
□ 长对话不失忆、不 400
    测法（确定性部分）：连续 100 轮后，history 中存在覆盖被丢弃区间的摘要消息，
          且第 100 轮请求的 input tokens ≤ 配置预算
    测法（人工部分）：第 1 轮告知一个具体事实（人名+数字），第 100 轮询问，模型答对
□ 全程零弹窗
    测法：完成上述 100 轮 + 若干次联网查询，permission_request 事件数为 0
□ 看图可用
    测法：一次贴 4 张图，模型正确回答图中内容；单张超限时 UI 指名是哪张、为什么
□ MCP 可用
    测法：接入一个真实搜索 server，模型调用其工具并在回答中引用来源
□ 无孤儿子进程
    测法：连接 ≥1 个 stdio MCP server 后关闭窗口，进程树中无残留 npx/node
□ 主题生效
    测法：3 套预设 × 亮暗 × 强调色 × 字号，切换后即时生效且无需重启
□ api_key 不落明文
    测法：key 位于 Windows 凭据管理器；对配置目录与日志文件全文搜索该 key 字符串，0 命中
```

## 10. 明确不做

写文件 / 改文件 / 执行命令、git 检查点回滚、多模型路由、会话导出分享、快捷键体系、系统托盘、语音输入、组件级插件系统、agent 自主截屏、图片生成、多会话并发。

前两项不是「做不到」——权限门与工具注册表的结构正是为它们准备的。新增 `write_file` 只需实现一个 `Risk::Write` 的工具，弹窗链路在切片 3 已建好并测试过。**这是刻意留的门，不是遗漏。**

## 11. 风险与技术验证清单

| # | 风险 | 影响 | 处理 |
|---|---|---|---|
| 1 | Tauri v2 二进制 IPC（`ArrayBuffer` → `Vec<u8>`）不如预期 | 图片输入通路 | 切片 0 验证；兜底改 base64 传输 |
| 2 | **Windows 上 spawn `npx` 失败** —— `npx` 实为 `npx.cmd`，tokio `Command` 无法直接 spawn `.cmd`/`.bat` | MCP stdio 在 Windows 上整体不可用（开发机正是 Windows） | 切片 0 验证；兜底为解析 `where npx` 取真实路径，或经 `cmd /C` 包装并严格校验参数 |
| 3 | 流式 `tool_call` 分片拼装 | 工具调用参数错乱或 panic | 4.7 的 `AssistantAcc` 单测四类用例 |
| 4 | 反向代理吃掉流式（响应被缓冲） | 用户自架代理时逐字上屏失效、表现为「卡到最后一次性出现」 | 文档化排查步骤；提供「关闭流式」开关作为诊断手段 |
| 5 | base_url 拼接（`/v1` 重复、末尾斜杠、鉴权头形式） | 连不上模型，报错难懂 | 规范化函数 + 单测；错误信息中回显最终 URL |
| 6 | WebView2 在 Win10 缺失 | 目标机器打不开 | 安装说明附 bootstrapper |
| 7 | `htmd` 抽取质量不达标 | 网页正文噪声大 | 切片 3 实测；不达标换 `readability` + `html2md` |
| 8 | 内存基线超标（Rust core + WebView2 实测 60–120MB） | 「运行时轻」不成立 | 切片 0 建立基线，超标即早期调整而非末期返工 |
| 9 | SQLite 并发写 | 多会话写入冲突 | 启用 WAL；MVP 单轮并发本身已规避大部分场景 |
| 10 | MCP 工具名超 64 字符 | 模型端报错 | 5.2 短哈希兜底 |
| 11 | 子进程泄漏 | 后台 npx/node 堆积 | 退出与删除路径显式 kill；验收标准含孤儿进程检查 |

## 12. 附录：外部参考

- `rmcp` v2.1.0 为官方 Rust MCP SDK（Tokio 构建），feature 分 `client` / `transport-io`（stdio）/ `transport-streamable-http-client`
- `async-openai` 支持自定义 `base_url`、流式、`tool_calls`、`image_url` vision 内容块
- Qwen-VL 走阿里云百炼 OpenAI 兼容端点；兼容模式另有 `enable_search` 原生联网参数（本项目不采用，搜索改由 MCP 提供，以便工具调用过程对用户可见）
- OpenAI 兼容协议避坑要点：base_url 的 `/v1` 重复或缺失、末尾斜杠、鉴权头不止 `Authorization: Bearer` 一种、反向代理吃掉流式
- `tauri-plugin-http` 流式行为问题（plugins-workspace#2415）——本设计已不使用该插件，记录于此以说明栈选择的依据
