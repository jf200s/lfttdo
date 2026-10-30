# 应用内 MCP 配置（JSON 编辑器）设计

- 日期：2026-10-30
- 状态：设计已确认，待生成实施计划
- 范围：只做应用内编辑 `mcp.json`。表单式管理、HTTP 传输、热应用均不在本次范围。

---

## 1. 背景与问题

现状：

- `mcp.json` 位于应用数据目录（`%APPDATA%\com.qwa.do`），只能由用户手工放置或修改。
- MCP 面板（`src/components/McpPanel.tsx`）只能看状态、改信任、重连、断开，不能增删改配置。
- 文件不存在或解析失败时，面板都显示「还没有配置服务器」，用户看不出是「没配」还是「配坏了」。
- 当前构建只启用 stdio 传输；HTTP 条目会保存但连接时明确报「此构建未启用 HTTP 传输」。

用户要求：像主流 agent 一样，在应用内完成 MCP 配置。经确认，采用**应用内原始 JSON 编辑器**，不做表单式增删改。

## 2. 决策清单

| 项 | 决定 | 理由 |
|---|---|---|
| 配置形态 | 仅应用内 JSON 编辑器 | 用户选择；保留手写配置、未知字段与现有文件兼容性 |
| 生效时机 | 保存只写文件，重启应用后生效 | 用户选择；不引入热应用/自动重连的并发与工具表一致性问题 |
| 保存校验 | 严格：全部条目有效才写盘 | 用户选择；避免「保存成功但下次启动少了几台 server」 |
| 实现落法 | 复用现有解析器 + 独立 editor 子模块 + 面板内编辑视图 | 校验与启动加载同源，改动集中 |
| 密钥暴露 | 只有 `mcp_config_read` 回传配置正文；状态/列表命令永远不含 env/headers 值 | 编辑必须看到原文；常规面板加载不应把密钥带进前端 |
| 信任 | 不改 `mcp_trust.json` 的格式与读写路径 | 与本次需求无关，避免迁移 |
| 不做 | 表单式 CRUD、启停开关、HTTP 传输、热应用、配置版本历史、跨 agent 一键导入 | YAGNI，且均超出「应用内配置」的最小闭环 |

## 3. 用户流程

### 3.1 入口

MCP 面板底部新增「编辑配置」按钮。点击后**在同一面板内**切到编辑视图，不叠第二层弹窗。

### 3.2 编辑视图

- 顶部显示完整文件路径，以及一句提示：配置中的 env/headers 会以明文显示；当前构建仅支持 stdio；`disabled` 字段当前不识别。
- 编辑对象是整份文件（`{"mcpServers": {...}}`），等宽字体的多行文本框。
- 文件不存在：预填最小模板

  ```json
  {
    "mcpServers": {}
  }
  ```

  并提示「文件尚不存在，保存后创建」。
- 文件存在但 JSON 损坏：照样加载原文，显示解析错误，用户可直接修。
- 读取失败（权限、路径是目录等）：显示错误与「重试」，不进入可编辑状态。

### 3.3 保存

- 点击「保存」：调用 `mcp_config_save(text)`。
- 校验失败：编辑器保持打开、文本不丢，错误区域列出全部问题。
- 校验通过：原子写入 `mcp.json`，返回服务器列表，并在面板顶部保留提示：

  > 配置已保存，重启应用后生效。当前会话仍按启动时的连接与工具运行。

- 保存成功后重新拉取服务器列表与配置状态（列表此时反映新文件；未重启前新 server 显示「未连接」）。
- 保存期间「保存」「返回」均禁用，避免并发写。

### 3.4 取消、遮罩与 Esc

- 文本有改动：就地显示「放弃修改？」确认，不弹原生对话框。
- 文本无改动：直接返回列表。
- 编辑视图存在期间点击遮罩**不关闭**面板；Esc 走与「返回」相同的脏检查。
- 保存失败后文本原样保留在框里；返回时仍按「相对载入文本是否有改动」判断。
- 保存进行中忽略 Esc 与遮罩，避免与写盘竞争。

### 3.5 面板状态与空态

- 面板挂载时同时调用 `mcp_config_status`（不含密钥）：
  - 有文件级解析错误：列表上方显示「配置文件无法解析：…」，并引导「编辑配置」。
  - 有条目被跳过：列出「名字（原因）」，原因沿用加载器的固定五条措辞。
- 文件不存在或没有 server 时空态文案改为：

  > 还没有配置服务器。点击「编辑配置」直接写 mcp.json。

### 3.6 生效语义

- 保存不触碰运行中的 `McpBridge`、工具注册表与权限门。
- 重启后由现有 `mcp_bootstrap` 按新文件连接并注册工具。
- 现有「重连 / 断开 / 信任」按钮行为不变；「重连」本来就会读取当前文件，等于用户手动让单台 server 应用新配置。这是既有能力，不在本次改动范围。

## 4. 架构与模块边界

### 4.1 `src-tauri/src/mcp/config.rs`

- 把现有 `load` 的解析部分拆成纯函数：

  ```rust
  pub(crate) fn parse_text(content: &str) -> McpConfig
  ```

- `load(dir)` 改为「读 `mcp.json` + `parse_text`」，行为、错误分层与现有测试全部保持不变。
- `config.rs` 继续保持**纯解析、不写盘**；唯一的写入者放到新模块 `editor.rs`。

### 4.2 `src-tauri/src/mcp/editor.rs`（新增）

职责：配置文件的读取、严格校验与原子写入。不碰桥、不碰工具表、不碰信任表。

- `pub const EMPTY_CONFIG_TEXT: &str`：最小模板。
- `pub fn read(dir: &Path) -> Result<Snapshot, String>`
  - `NotFound` → `exists: false`，正文为模板。
  - 其他读失败 → `Err`（编辑器显示错误）。
  - 读成功 → `exists: true`，正文原样；用 `parse_text` 生成 `parse_error` 与 `skipped` 供编辑器即时展示。
- `pub fn status(dir: &Path) -> Status`
  - 只返回路径、是否存在、`parse_error`、`skipped`；**不返回正文**。
  - 非 `NotFound` 的读失败收进 `parse_error`，不 panic、不阻断面板。
- `pub fn save(dir: &Path, text: &str) -> Result<(), String>`
  - `parse_text(text)` 后，只要 `parse_error.is_some()` 或 `skipped` 非空，整份拒绝，**不写盘**。
  - 通过后写同目录临时文件 `mcp.json.tmp`，再 `rename` 覆盖 `mcp.json`；失败时清理临时文件并返回错误。
  - 原样写入用户文本，不重新序列化：未知字段、缩进、键序全部保留。
- `Snapshot` / `Status` 为域内结构；含正文的 `Snapshot` 不实现 `Debug`。

### 4.3 `src-tauri/src/ipc.rs`

新增三条命令与对应的线格式结构（`#[serde(rename_all = "camelCase")]`）：

| 命令 | 返回 | 含正文/密钥 |
|---|---|---|
| `mcp_config_status` | `McpConfigStatusView` | 否 |
| `mcp_config_read` | `McpConfigTextView` | 是（仅此一条） |
| `mcp_config_save` | `()`；失败为错误字符串 | 否（错误只含条目名与固定原因） |

- 三条命令只做 State → 目录 → editor 函数的转发；可测核心直接放在 `editor.rs`。
- 在 `invoke_handler` 中注册。
- 现有 `mcp_servers`、`mcp_set_trust`、`mcp_reconnect`、`mcp_disconnect` 不动。
- 同步更新 `capabilities/default.json` 的说明文字（自定义命令数量变了，仍无 shell/fs 权限；权限列表不变）。

### 4.4 前端

- `src/ipc/commands.ts`：新增三个包装函数。
- `src/types.ts`：新增 `McpConfigStatus`、`McpConfigText`、`McpConfigIssue` 类型。
- `src/components/McpConfigEditor.tsx`（新增）：
  - 挂载时调用注入的 `load`；自己持有正文、原始正文、保存中、错误、脏检查确认等局部状态。
  - 只有这个组件挂载期间持有配置正文；关闭后丢弃，不写入任何 store/持久层。
- `src/components/McpPanel.tsx`：
  - 新增 `view: "list" | "editor"`、`configStatus`、`notice` 状态。
  - 编辑器打开时遮罩点击不关闭面板；编辑器通过 `onDone(saved)` 交回控制权。
  - 保存成功 → 关闭编辑器、设置 notice、刷新列表与状态。
  - 新增 `loadConfigStatus` / `readConfig` / `saveConfig` 可注入 props，生产默认走 `commands.ts`（沿用现有测试注入风格）。
- `src/styles.css`：编辑器布局与等宽文本框，尽量复用现有 `panel-*` 类。

## 5. IPC 契约

```ts
interface McpConfigIssue {
  name: string;
  reason: string; // 加载器的固定原因措辞
}

interface McpConfigStatus {
  path: string;
  exists: boolean;
  parseError: string | null;
  skipped: McpConfigIssue[];
}

interface McpConfigText extends McpConfigStatus {
  text: string;
}
```

- `mcp_config_status(): Promise<McpConfigStatus>`
- `mcp_config_read(): Promise<McpConfigText>`
- `mcp_config_save(text: string): Promise<void>`

保存失败的错误是给人读的多行字符串，例如：

```text
mcp.json 未保存：2 处条目问题
- 「bad」：字段类型不对
- 「dup」：同名重复
```

或：

```text
mcp.json 未保存：JSON 解析失败：第 3 行第 5 列
```

文件级错误只保留行/列位置，不直接透传 serde 的完整文案（完整文案可能把输入里的字符串值带出来）。错误中不出现任何 env/headers 的值。

## 6. 校验规则

严格校验直接复用 `parse_text` 的判定，不新开第二套规则。

**文件级（整份拒绝）**

- JSON 语法错误。
- 根不是对象。
- `mcpServers` 存在但不是对象（含 `null`、数组、数字）。

**条目级（整份拒绝）**

- 名字为空。
- 字段类型不对（如 `"args": "-y"` 少了方括号）。
- `command` 为空字符串。
- 没有 `command` 也没有 `url`。
- 同名重复（加载器保留第一条，编辑器把重复视为错误）。

**兼容与边界**

- 未知字段忽略但原样保留在文本里，保存不会丢。
- HTTP 条目可以通过校验并保存；当前构建连接时会报「未启用 HTTP 传输」。编辑器提示写明这一点。
- `disabled` 字段当前不识别，运行时仍会连接；编辑器提示写明这一点。
- 「url 为空」目前会被加载器接受，编辑器保持一致，不额外开第六种原因。

## 7. 安全红线

- `mcp_config_read` 是唯一把配置正文（含 env/headers 明文）回传前端的命令，只在用户点「编辑配置」时调用。其威胁模型等同于用户自己用记事本打开该文件。
- `mcp_config_status` 与 `mcp_servers` 永远不返回 env/headers 值；新增测试钉住状态视图序列化后不含密钥。
- 保存失败信息只含条目名与固定原因；文件级错误只保留 serde_json 的行/列位置，不回显原文片段（serde 的完整文案可能带出输入里的字符串值，而 `mcp_config_status` 会展示 `parse_error`）。
- 含正文的结构体不实现 `Debug`，不写日志、不发事件、不进 AppState。
- 前端只在编辑器组件挂载期间持有正文；关闭即丢弃。
- 不引入密钥环、不改信任文件、不做配置迁移。
- 残余风险：WebView 内任何能调用自定义命令的代码都能读取配置正文。这是「应用内编辑」不可避免的取舍，记录在案。

## 8. 失败处理

| 场景 | 行为 |
|---|---|
| 读不到（权限/目录） | 编辑器显示错误 + 重试；面板状态把它当日志级 parse_error 展示 |
| 保存校验失败 | 不写盘；编辑器保持打开，文本不丢，列出全部问题 |
| 写临时文件失败 | 返回错误；原文保留；清理临时文件 |
| rename 失败 | 同上；不留下半份配置 |
| 文件被外部同时修改 | 最后写入者获胜；本次不做冲突检测 |
| 保存后未重启 | 运行中的连接与工具不变；面板提示明确说明 |
| 保存时正在流式对话 | 允许；只写文件，不影响当前轮 |

## 9. 测试

### 9.1 Rust

`config.rs`：

- 现有全部测试重构后保持通过（解析行为不变）。

`editor.rs`：

1. 缺文件 → `read` 返回模板、`exists = false`；`status` 同样不报错。
2. 合法配置 `save` → 文件内容与输入逐字节一致；再次 `read` 得到同一文本；`config::load` 能解析出 server。
3. 未知字段、自定义缩进保存后原样保留。
4. JSON 语法坏 / 根不是对象 / `mcpServers` 不是对象 → `Err`，文件保持原状（不存在则仍不存在）。
5. 五种条目级错误逐条覆盖 → `Err` 且点名条目；文件未写。
6. 重复名字 → `Err`。
7. 错误信息（含文件级 `parse_error`，例如根被写成字符串）不含测试夹具里的密钥值。
8. `status` 序列化为 JSON 后不含 env/headers 值。
9. 成功保存后目录里没有 `mcp.json.tmp` 残留。
10. 空文本 → 文件级错误，不写盘。

### 9.2 前端（Vitest + Testing Library）

1. 面板显示「编辑配置」；点击后调用注入的 `readConfig` 并展示正文与路径。
2. 缺文件模板与「保存后创建」提示。
3. 保存失败：编辑器不关闭，正文保留，错误可见。
4. 保存成功：回到列表，显示「重启应用后生效」提示，列表与状态被重新拉取。
5. 脏文本点「返回」/按 Esc → 出现「放弃修改？」；确认后返回；无改动直接返回。
6. 编辑视图打开时点击遮罩不关闭面板。
7. 状态横幅：文件级解析错误、条目被跳过两种都渲染原因；空态文案更新。
8. 既有 4 条 `McpPanel` 测试保持通过（仅空态文案相关断言按需调整）。

### 9.3 命令与手工验证

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm test
npm run build
npm run lint:css
```

UI 改动在 `npm run tauri dev` 中实际点一遍：新建 → 保存 → 重启 → 工具出现；坏配置 → 保存被拒；密钥只在编辑器可见。

便携版已于 2026-10-30 移除（提交 `b28e249`）；本次不重新引入启动器或 `portable.txt`，也不重新打包安装包。

## 10. 非目标与已知限制

- 不做表单式增删改、启停开关、测试连接按钮。
- 不做 HTTP 传输（本构建既有边界）。
- 不做保存后热应用、自动重连、工具表热替换。
- 不做外部修改冲突检测、历史版本、自动备份。
- 不识别 `disabled` 等第三方字段；保留但不执行。
- 不清理已删除 server 在 `mcp_trust.json` 里的旧信任记录（无害，下次同名会沿用）。
- 不改变 MCP 信任、工具命名、权限门与桥的生命周期。

## 11. 验收标准

- 从「没有 mcp.json」开始，用户能在应用内写出一个 stdio server，保存、重启后连接成功并出现在工具列表。
- 任何文件级或条目级错误都会阻止保存，并指出具体是哪一条、为什么。
- 保存不会丢未知字段、缩进与键序。
- 面板常规加载与服务器列表不把 env/headers 值带到前端；只有显式打开编辑器才会。
- 现有 MCP 面板功能（状态、信任、重连、断开）与既有测试不回归。
- 前端构建、CSS lint、Rust 测试、前端测试全部通过。

## 12. 发布与回滚

- 无迁移：已有 `mcp.json` 在用户主动保存前不被改动。
- 外部手工编辑仍完全可用；编辑器只是另一种入口。
- 回滚：撤销代码即可；`mcp.json` 始终是普通 JSON，不依赖新代码才能被旧版本读取。
- 便携版已移除；不重新引入启动器或 `portable.txt`，常规安装包不在本次范围。
