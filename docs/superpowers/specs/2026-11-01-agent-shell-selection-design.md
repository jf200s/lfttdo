# Agent Shell 执行与 Shell 后端选择设计

- 日期：2026-11-01
- 状态：设计已确认，待生成实施计划（接口契约桩 `src-tauri/src/tools/shell.rs` 已随本设计交付）
- 范围：给 agent 新增一把执行 shell 命令的工具，shell 后端可选——内置预设 profile（bash、sh、zsh、fish、git-bash、pwsh、powershell、cmd）+ 用户自定义 profile（任意可执行文件路径）。默认 shell 存在设置里，工具调用时可临时指定。流式输出、定时任务、后台常驻进程均不在本次范围。

---

## 1. 背景与问题

现状：

- `src-tauri/src/tools/` 下的生产工具只有 `fetch_url`、`read_file`、`use_skill`、`memory_search`、`write_file`、`edit_file`，没有任何执行命令的工具。
- README「安全说明」里的「前端无 shell / 文件系统权限，进程执行全部在 Rust 侧」仍然成立；MCP 侧的进程执行只服务 MCP server 本身，模型无法用它跑一条用户想跑的命令。
- 权限门的 `Risk::Exec` 档、`rule_key` 的「整条命令文本即作用域」（`gate.rs:203-221`）、进程树回收（`mcp/process.rs`）与 `CREATE_NO_WINDOW`（`mcp/bridge.rs:33`）都已建好并有测试，但还没有第一个内置的 Exec 工具消费者。

用户要求：agent 可以执行 shell 命令；shell 后端可选——预设 + 自定义（含「自己编译的 shell」这种任意可执行文件路径）；默认 shell 存在设置里；单次调用可临时指定。

## 2. 决策清单

| 项 | 决定 | 理由 |
|---|---|---|
| 工具名 | `run_command`，`Risk::Exec` | 门的 `rule_key` 对 `Risk::Exec` 已按 `input.command` 取作用域（`gate.rs:212-218`），入参名与既有口径天然对齐；名字描述动作而不是后端 |
| 入参 | `{ command, shell_id?, cwd?, timeout_ms? }` | 需求原文的形状；`command` 必填，其余可选。**写死于本 spec §5.1**，前端与接线线都以这里为准 |
| Profile 模型 | 数据驱动：`ShellProfile { id, label, program, args }`，命令文本永远作为 argv 的**最后一个元素**整体传入 | 预设与自定义用同一个类型，不需要 per-shell 的枚举分发；「argv 逐段、不拼命令行」（`bridge.rs:88-92` 同规）靠结构保证 |
| 预设表 | 8 个，常量住在 `tools/shell.rs`（§5.2） | 单一真相；前端经 IPC 拿只读副本，不抄第二份 |
| 默认 shell | `Settings.default_shell`（存 profile id）+ `Settings.shells`（自定义表）；normalize 时解析失败回落 `bash` | 复用既有 settings.json 全表通道（`settings_get`/`settings_set`），不开第二份配置文件 |
| 生效时机 | 改设置**下一条消息生效**（每轮 `send_message` 现刷 catalog） | 与 roots / 温度同口径（`ipc.rs:1659-1666` 的既有裁定），无缓存可过期 |
| catalog 注入 | `ShellTool` 持 `Arc<RwLock<ShellCatalog>>`，`AppState` 存同一个 Arc | 注册表是进程级单例、`ToolCtx` 是字面量构造并被十余个测试文件照抄——往 `ToolCtx` 加字段会炸穿三条线所有权之外的所有测试文件；共享句柄让配置仍然「装配层注入、工具不自取」（既有取向） |
| 程序存在性校验 | 一处实现 `ensure_executable`（存在 + 可执行判定，PATH 解析可注入）；**UI 校验命令**与**执行前强制**都调它；**保存时不校验** | 需求硬约束落在使用点；保存时校验会复制 R284 教训（U 盘拔了 → 设置再也存不进去）。settings `normalize()` 维持「不碰文件系统」纪律 |
| 超时与输出上限 | `timeout_ms` 默认 30 000、钳 `1..=600_000`；stdout+stderr 合计上限 512 KiB，超限继续读取并丢弃（防管道堵塞），交出文本截断 + 挂标记 | 需求硬约束；512 KiB 与 `read_file::MAX_BYTES` 同一量级，是「一页上下文」的既有口径 |
| 非 UTF-8 输出 | lossy 转换 + 在输出尾部**注明**，不拒绝 | 与 `read_file` 的「拒绝」刻意不同：中文 Windows 的 GBK 输出若按拒绝处理，`dir` 这类命令永远读不到结果；区别与理由记录在案（§8.5） |
| 退出码 | 非零退出**不是**工具错误：`Ok`，输出尾部带「退出码 N」 | 命令失败是模型该看到并处理的**结果**；spawn 失败（程序不存在等）才是 `Err` |
| 进程树回收 | 复用 `mcp::process::create_kill_job` + Unix `process_group(0)` + Windows `CREATE_NO_WINDOW`；job 随本次调用结束 Drop，**整棵树回收**，不留后台进程 | T34 的既有设施；agent 跑的命令不允许留下孤儿进程（R323 同口径：拿不到 job 不 spawn） |
| cwd | 缺省 = 当前会话输出目录（`ToolCtx.write_root`）；两者都没有 → 明确报错；显式 cwd 必须是存在的绝对路径目录 | 生产装配里 `output_dir` 恒有值（`ipc.rs:1637`）；回落到应用进程 cwd 是不可复现的行为，不做 |
| 权限 | 不改门。`Risk::Exec` 每次弹窗；会话级放行的粒度是**整条 trim 后的命令文本**（门里既有行为） | 闸门已经是想要的形状；「隐藏不是授权」等纪律照旧 |
| 声明过滤 | 不进 `filtered_tool_defs` 的 retain 名单，**恒声明** | R286 的过滤判据是「这一轮注定不可用」（空 roots、无技能）；shell 没有这种形态，最多执行时报错 |
| capabilities/default.json | **不改** | 不新增任何 Tauri 插件权限；前端能力面零变化（前端 `McpPanel.test.tsx` 里那条能力守卫测试不受影响，避免跨线耦合） |
| 不做 | 流式输出、多命令脚本、环境变量编辑、白名单命令表、后台任务、定时执行 | 最小闭环；命令级白名单与「每次弹窗」是两种威胁模型，后者已由门承担 |

## 3. 用户流程

### 3.1 设置页「Shell」区块

设置面板（`SettingsPanel.tsx`）在「文件访问」之后新增一个 `fieldset.panel-group`，`<legend>Shell</legend>`：

1. **预设（只读）**：`shell_presets()` 返回的 8 条，逐条展示 id、显示名、program、args。没有任何可编辑控件。
2. **默认 shell**：一个 `<select>`，选项 = 预设 id + 自定义 id，绑定 `form.default_shell`。保存随整表走 `settings_set`。
3. **自定义 profile**：列表（当前 `form.shells`）+ 表单（名字 id、显示名 label（可空）、程序路径 program、参数 args——**每行一个参数**的 textarea）。动作：添加 / 修改（选中回填）/ 删除 / 校验。
   - 「校验」调用 `shell_profile_validate`，失败原因原样显示在错误行；成功提示「程序存在且可执行」。
   - id 与预设同名的自定义 profile **合法且生效时覆盖预设**（§6.3）；表单提示这句话。
   - 程序路径为空时「添加 / 保存」按钮禁用（前端只做按钮使能；真校验只有 Rust 一处，同 `memory_update_body` 的既有口径）。
4. 保存后下一条消息生效；面板提示文案与「文件访问」区块同款（「保存后下一条消息生效。」）。

### 3.2 模型侧

- 系统提示不变；工具声明（name/description/schema）由注册表自动带出，description 写明：命令文本会被交给选定的 shell 解释、`shell_id` 如何选后端、超时与输出上限、输出按文本返回。
- 每次调用 `run_command` 都触发权限弹窗（`Risk::Exec`）；用户选「本会话都允许」只对**逐字相同**的命令文本生效（门既有行为）。

## 4. 架构与模块边界

```text
src-tauri/src/tools/shell.rs     ←「shell 核心」线独占
    ShellProfile / ShellCatalog（类型 + 预设表 + 解析/校验/归一化）
    ensure_executable / build_command / 超时与输出捕获 / ShellTool
src-tauri/src/settings.rs        ←「Rust 接线」线
    Settings.shells / Settings.default_shell + normalize 规则
src-tauri/src/tools/registry.rs  ←「Rust 接线」线（build_production_registry 注册）
src-tauri/src/ipc.rs             ←「Rust 接线」线（AppState.shells、每轮刷新、两条 IPC 命令、登记表）
src/components/SettingsPanel.tsx ←「前端 UI」线
src/types.ts / src/ipc/commands.ts ←「前端 UI」线
```

- `tools/shell.rs` 与 `tools/` 其余文件同一条纪律：**不得 `use tauri`**。它只依赖 `tokio`（已在依赖树，`features=["full"]`）、`crate::agent::types`、`crate::tools::registry`、`crate::mcp::bridge::CREATE_NO_WINDOW` 与 `crate::mcp::process`（复用进程树回收；`mcp/bridge.rs` 本就反向引用 `tools::registry::ToolError`，这个方向的依赖是既有事实）。
- `settings.rs` → `tools::shell` 的引用方向是允许的（settings 只取 `ShellProfile` 类型与 `DEFAULT_PRESET_ID` 常量）；反向不存在——shell.rs 不认识 `Settings`。
- `tools/mod.rs` 的 `pub mod shell;` 已随契约桩交付时注册，三条线都**不需要也不允许**再改它。

## 5. 接口契约

### 5.1 工具入参（写死，前端与接线线以此为准）

```jsonc
{
  "type": "object",
  "properties": {
    "command":    { "type": "string",  "description": "要执行的命令文本，交给选定的 shell 解释" },
    "shell_id":   { "type": "string",  "description": "可选：shell profile 的 id（预设或自定义）；缺省用设置里的默认 shell" },
    "cwd":        { "type": "string",  "description": "可选：工作目录，绝对路径且必须已存在；缺省用当前会话的输出目录" },
    "timeout_ms": { "type": "integer", "minimum": 1, "description": "可选：超时毫秒数，默认 30000，上限 600000" }
  },
  "required": ["command"]
}
```

形状错误一律 `ToolError::BadInput`（缺 `command`、空/全空白 `command`、非字符串的 `shell_id`/`cwd`、非正整数的 `timeout_ms`）——沿用 `read_file::optional_line` 的「形状不对必须报、不许静默按默认跑」口径。`timeout_ms` 数值超出上限按钳位处理（夸大时限不是形状错误）。

### 5.2 `ShellProfile` 与预设表

```rust
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ShellProfile {
    pub id: String,        // 稳定键；预设 = "bash" 等 8 个；自定义 = 用户命名
    pub label: String,     // UI 显示名；预设给中文；自定义可空（空则 UI 回显 id）
    pub program: String,   // 可执行文件：绝对/相对路径，或按 PATH 解析的名字
    pub args: Vec<String>, // 固定参数模板；命令文本永远追加为最后一个 argv 元素
}
```

预设表（`program` 为裸名的按 PATH 解析；命令文本 = `<command>`）：

| id | label | program | args | 说明 |
|---|---|---|---|---|
| `bash` | Bash | `bash` | `["-lc"]` | 登录档保住 PATH 等 env；需求示例原样 |
| `sh` | sh | `sh` | `["-c"]` | POSIX sh 无标准 `-l` |
| `zsh` | Zsh | `zsh` | `["-lc"]` | 与 bash 同取登录档 |
| `fish` | Fish | `fish` | `["-c"]` | fish 登录档语义不同，agent 场景取最简 |
| `git-bash` | Git Bash | `C:\Program Files\Git\bin\bash.exe` | `["-lc"]` | Git for Windows 标准安装位置；装在别处的用户建自定义 profile 指过去 |
| `pwsh` | PowerShell (pwsh) | `pwsh` | `["-NoLogo", "-NoProfile", "-Command"]` | 关 profile：agent 场景要确定性；要 profile 就建自定义 |
| `powershell` | Windows PowerShell | `powershell` | `["-NoLogo", "-NoProfile", "-Command"]` | 同上 |
| `cmd` | CMD | `cmd` | `["/d", "/s", "/c"]` | `/d` 关 AutoRun；`cmd.exe` 的确切引号行为标「Windows 真机验收」 |

默认预设 id 常量：`DEFAULT_PRESET_ID = "bash"`（settings normalize 的回落目标、装配期空 catalog 的缺省）。

### 5.3 `ShellCatalog`

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShellCatalog {
    pub customs: Vec<ShellProfile>,   // 自定义表（来自 Settings.shells）
    pub default_id: String,           // 来自 Settings.default_shell；空 = DEFAULT_PRESET_ID
}
```

- `ShellCatalog::new()`：空自定义 + `DEFAULT_PRESET_ID`（装配期占位；第一条消息前 `send_message` 会现刷）。
- `ShellCatalog::from_parts(customs, default_id)`：接线线从 `Settings` 构造的唯一入口（不依赖 settings.rs 类型）。
- `resolve(&self, id: Option<&str>) -> Result<ShellProfile, String>`：
  - `Some(id)`：先查 `customs`（**自定义可覆盖预设**，这是有意语义：用户明确加同名 profile 就是要换掉预设），再查预设表；都查不到 → `Err`，错误里列出全部可用 id。
  - `None`：按 `default_id` 走同一条查找；`default_id` 为空 → `DEFAULT_PRESET_ID`；仍解析不到 → `Err`（settings normalize 保证不走到，但 resolve 自身 fail-closed）。

### 5.4 校验与程序存在性

- `validate_custom(profile) -> Result<ShellProfile, String>`：trim `id`/`label`/`program` 与 `args` 各项；`id` 为空、`program` 为空、`args` 任一项为空/含空白都是 `Err`。**不做存在性检查**（那一步可能贵、可能随环境漂，单列）。
- `ensure_executable(program, search_dirs) -> Result<PathBuf, String>`：需求硬约束的唯一实现。
  - program 含路径分隔符 → 直接判：存在、是普通文件；Unix 加可执行位（`PermissionsExt::mode() & 0o111`），Windows 加扩展名启发式（`exe|com|bat|cmd`，NTFS 无执行位，账面写明这是启发式）。
  - program 是裸名 → 逐个 `search_dirs` 目录按同判据找（生产传 `PATH`（Windows 再叠 `PATHEXT`）；测试传假表，同 R305 的「解析器可注入，单测不依赖本机装了什么」）。
- 消费点两个：IPC `shell_profile_validate`（UI 校验按钮）与 `ShellTool::execute` spawn 之前（强制）。**保存时不校验**（§2 决策清单）。

### 5.5 IPC 契约（接线线）

新增两条命令（其余全部沿用既有 `settings_get`/`settings_set` 通道）：

```ts
shellPresets(): Promise<ShellProfile[]>            // 命令 shell_presets：只读预设表
shellProfileValidate(profile: ShellProfile): Promise<void>
                                                   // 命令 shell_profile_validate：
                                                   // validate_custom + ensure_executable；
                                                   // 失败 Err(中文原因)，成功 Ok
```

`AppState` 增 `shells: Arc<RwLock<ShellCatalog>>`（`crate::lock` 的中毒容忍访问）；`build_production_registry` 增收该 Arc 并注册 `ShellTool::new(shells)`；`send_message` 在 `store.load()` 之后用 `ShellCatalog::from_parts(&settings.shells, &settings.default_shell)` 现刷（下一分支的具体写法见计划）。`invoke_handler` 登记两条新命令，并同步补 ipc.rs 里既有的「登记表守卫测试」。

### 5.6 前端类型（UI 线）

```ts
export interface ShellProfile {
  id: string;
  label: string;
  program: string;
  args: string[];
}
// Settings 增两个键（snake_case，与整表同规）：
//   shells: ShellProfile[];
//   default_shell: string;
```

`commands.ts` 增 `shellPresets` / `shellProfileValidate` 两个包装；`SettingsPanelProps` 增 `onLoadShellPresets` / `onValidateShellProfile` 两个可注入回调（同面板既有注入风格）。

**前端夹具的连带义务（B1，2026-11-01 评审 blocker）**：`Settings` 是必填键接口，而仓库里有四份 `(): SettingsView => ({…全键字面量…})` 的测试夹具被**有意**用作编译期绊线（注释自证「少写就编译不过」）：`src/App.test.tsx`、`src/AppSettings.test.tsx`、`src/AppImages.test.tsx`、`src/components/SettingsPanel.test.tsx`。新增这两个键时四份夹具必须同步补键，否则 `tsconfig.json`（`strict` + `"include": ["src"]`）下直接 TS2739。裁决：键维持必填（Rust serde 恒序列化这两个字段，线上没有「可选」形态），补键义务归前端线；`App.tsx` 本体零改动（新面板 props 走可选 + 生产默认）。

## 6. Settings 与归一化（接线线）

```rust
/// 自定义 shell profile 表。空 = 只有预设可用。
pub shells: Vec<ShellProfile>,
/// 默认 shell 的 profile id。normalize 解析失败回落 DEFAULT_PRESET_ID。
pub default_shell: String,
```

`Settings::normalize()`（读写两侧各一次的既有纪律）对这两个键只做**词法**整理，不碰文件系统（R284 同规）：

1. `shells`：逐条做词法整理（trim）。注意与 `validate_custom` 的分工——normalize **不调它也不拒绝**：空 id/空 program/空 args 项的条目**原样保留**（执行时 `validate_custom` / `ensure_executable` fail-closed 报错，不静默删用户数据）；normalize 只额外做按 id 去重（保留首条，`dedupe_roots` 同纪律）。
2. `default_shell`：trim；为空、或既不是任何预设 id 也不是 `shells` 里的 id → 回落 `DEFAULT_PRESET_ID`（同 accent「认不出来回落预设」的取向，不报错）。

## 7. 执行语义（核心线）

1. **取消**：`ctx.cancel.is_cancelled()` 在解析入参**之前**查一次（工具侧那一次，同 `read_file`）；等待子进程期间 `tokio::select!`（`biased`，取消臂优先）监听取消 → 杀整棵树 → `ToolError::Cancelled`。
2. **spawn 前**：`ensure_executable` 失败 → `ToolError::Failed`（点名 program 与判过哪些位置）；`create_kill_job` 失败 → 不 spawn 直接 `Failed`（R323 同口径：不静默退化）。
3. **命令构造**（`build_command`，纯函数、可测）：`Command::new(program)` + `args` 逐段 `.arg()` + 命令文本作**最后一个元素**；`current_dir(cwd)`；Windows `creation_flags(CREATE_NO_WINDOW)`（复用 `mcp::bridge::CREATE_NO_WINDOW` 常量）+ Unix `process_group(0)`；`kill_on_drop(true)` 兜底；env 继承应用环境（shell 没有配置层 env 可增补）。
4. **超时**：`tokio::time::timeout` 包住「读输出 + wait」。超时 → 杀整棵树 → `ToolError::Failed("命令在 N ms 超时被终止…")`。
5. **输出捕获**：stdout/stderr 分别 pipe 读取；**合计**字节上限 `MAX_OUTPUT_BYTES = 512 * 1024`。超限后**继续读取并丢弃**（管道不塞死、子进程能跑完），交出的文本截到上限并在尾部挂标记（含实测丢弃字节数，风格同 `read_file` 的页尾标记）。
6. **输出编排**（都是 `ToolOutput::Text`）：stdout 在前；stderr 非空则跟 `[stderr]` 段；退出码非零追加一行 `[退出码 N]`（仍是 `Ok`）；stdout/stderr 全空且退出 0 → `[命令成功，没有输出]`。
7. **收尾**：无论正常退出 / 超时 / 取消，本次调用的 `KillOnCloseJob` Drop 即回收整棵树——agent 跑的命令**不留下后台进程**（想跑常驻服务不是本工具的用途，记录为已知限制）。

## 8. 安全红线与账面诚实

- **风险真相源**：`ShellTool::meta().risk = Risk::Exec`，注册表 `risk_of` 是唯一出口；`ToolDef` 无 risk 字段，模型改不了自己的权限级别（`registry.rs` 既有结构，不新增通路）。
- **不拼命令行**：命令文本是 argv 的一个元素，永远不与 program/args 拼成字符串（`bridge.rs:88-92` 同规）。命令文本内部写什么由模型负责、由用户在弹窗里看到——`rule_key` 的作用域就是**整条命令文本**。
- **Windows 控制台**：`CREATE_NO_WINDOW`；「孙进程按控制台继承规则共用」沿用 `bridge.rs:30-32` 的判读口径，不写成实测。
- **非 UTF-8 输出**：lossy + 注明。与 `read_file` 拒绝非 UTF-8 的区别是**有意的**：read_file 的对象是「用户根目录里的文件」，拒读是安全取向；shell 输出的对象是「命令跑出来的东西」，GBK 输出按拒绝处理等于中文 Windows 上这把工具残废。U+FFFD 的风险（模型把替换字符当内容）在这里被「尾部注明」补偿——它知道这段是被转换过的。
- **残余风险（记录在案）**：弹窗里用户只能看到命令文本，看不到它将被哪个 profile 执行——`shell_id` 也应展示在权限卡片里吗？**不做**：卡片渲染归门与前端既有结构，改它超出本轮；弹窗粒度是整条命令文本，后端差异不改变「这条命令会被执行」的事实。已知限制收进 §10。

## 9. 失败处理

| 场景 | 行为 |
|---|---|
| `command` 缺失/非字符串/空白 | `BadInput`，不 spawn |
| `shell_id` 未知 | `BadInput`，错误里列出全部可用 id |
| `cwd` 相对路径 / 不存在 / 不是目录 | `BadInput`（execute 内判定，报具体原因） |
| 没给 `cwd` 且 `write_root` 为 `None` | `BadInput`：要求显式给 cwd（生产装配恒有输出目录，这只在测试/异常装配出现） |
| program 找不到 / 不可执行 | `Failed`，点名 program 与查找范围 |
| 拿不到 kill job | `Failed`，不 spawn（R323） |
| 超时 | 杀整棵树；`Failed`，含超时值 |
| 取消（等待中） | 杀整棵树；`Cancelled` |
| 非零退出码 | `Ok`，输出尾带 `[退出码 N]` |
| 输出超 512 KiB | 截断 + 尾部标记（实测字节数），命令照常跑完 |
| spawn 本身失败（权限/格式） | `Failed`，透传 io 错误要点 |

## 10. 非目标与已知限制

- 不做流式输出：命令跑完才回文本（预览截断由 `ToolOutput::preview` 既有机制承担）。
- 不做命令白名单 / 危险命令识别：授权粒度就是「每次弹窗 + 整条命令文本」，识别器造不出安全增量。
- 不做环境变量编辑、不隐藏应用环境：子进程继承 app 环境（凭据本就不在 env 里，见 README 安全说明）。
- 命令结束后整棵进程树被回收：想在 agent 一轮里留一个后台服务，不是本工具支持的用法。
- `cmd.exe` 在 `/C` + 独立 argv 元素下的引号细节、`powershell.exe` 5.1 的编码行为：**Windows 真机验收项**，本设计在 Linux 开发环境无法实测，如实标注。
- 权限卡片不展示 `shell_id`（§8 残余风险）。
- 默认 shell 改动后**正在流式的那一轮**不换后端（catalog 每轮开头刷新一次，轮内不变）。

## 11. 测试

### 11.1 契约桩（2026-11-01 已交付，`cargo test --lib` 内）

类型级：预设表内容与形状、`resolve` 的「custom 覆盖 preset / 缺省回落 / 未知报错」、`validate_custom` 的归一化与拒绝、`build_command` 的 argv 形状（命令文本是最后一个元素、cwd 落进 Command）、超时钳位、`ensure_executable`（注入 search_dirs 的命中/未命中/不可执行三态）、桩 `execute` 对合法入参返回**明确的未实现错误**（不是 panic、不是死循环）、对坏入参返回 `BadInput`、`meta()` 的名字/风险/schema 钉死。

### 11.2 Rust（核心线 / 接线线交付）

- 核心线：超时、输出上限（截断标记）、取消（spawn 前与等待中）、非零退出码、stderr 编排、空输出句、kill job 失败不 spawn。可跑真进程的路径在 Unix 用 `/bin/sh`；Windows 真机路径按 §10 标注验收，不造假通过。
- 接线线：settings normalize 的 shells 去重/保留坏条目/default 回落；`build_production_registry` 注册后 `risk_of("run_command") == Exec` 且 `defs()` 含该工具；两条 IPC 命令的单测（validate 命中/未命中；presets 返回 8 条）；ipc.rs 既有登记表守卫测试补两条命令与 `ShellTool` 注册断言。

### 11.3 前端（UI 线，Vitest + Testing Library，注入假 props）

预设只读列表渲染、默认 shell 下拉与 `form.default_shell` 联动、自定义增/删/改的表单流转、程序为空禁用添加、校验失败原因上屏/成功提示、保存载荷包含 `shells` 与 `default_shell`、既有面板测试不回归。

### 11.4 命令

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib
npx vitest run src/components/SettingsPanel.test.tsx   # UI 线自证用单文件
npm test / npm run build / npm run lint:css            # baseline 已知失败（2026-11-01），只要求不新增失败
```

注意：`npm test`、`npm run build`、`npm run lint:css` 在本计划启动时就是失败态（基线记录），不属于本功能，任何线都**不修**它们——各线自证用单文件 vitest 与 `cargo test --lib`。

## 12. 验收标准

- 模型工具表里有 `run_command`；调用时弹 Exec 窗，拒绝则红卡、放行则拿到 stdout/stderr/退出码。
- 设置页能看到 8 个预设、能改默认 shell、能增删改自定义 profile 并校验「程序存在且可执行」；改完下一条消息生效。
- 自己编译的 shell（任意可执行文件路径）经自定义 profile 接入后可被 `shell_id` 选中执行。
- 超时命令被杀且整棵树不留孤儿；输出超 512 KiB 被截断并说明；取消立即生效。
- `cargo test --lib` 全绿（基线 749 passed / 1 ignored 之上只增不减）。
- 前端能力面零变化（`capabilities/default.json` 未动）。

## 13. 发布与回滚

- 无迁移：`settings.json` 新增两键走结构体级 `#[serde(default)]`（R91 的读侧向前兼容），旧文件读出新默认；回滚代码后旧程序忽略这两个未知字段，无残留。
- 预设表是编译期常量，不落盘；自定义 profile 与默认 shell 都是普通 JSON 字段。
