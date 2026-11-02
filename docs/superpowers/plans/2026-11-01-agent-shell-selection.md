# Agent Shell Selection (run_command) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the agent a `run_command` tool that executes a command through a selectable shell backend — 8 built-in preset profiles plus user-defined profiles (any executable path), with the default shell persisted in settings and overridable per call.

**Architecture:** `tools/shell.rs` owns the whole shell domain (profiles, presets, catalog, validation, command construction, timeout/output capture, the `Tool` itself); `settings.rs` persists `shells` + `default_shell`; `ipc.rs` registers the tool with a shared catalog handle, refreshes it per turn from the turn's `Settings`, and exposes two read/validate commands; the settings panel gains a read-only preset list, a default-shell select, and a custom-profile form. The permission gate is untouched — the tool is `Risk::Exec` and the gate already scopes Exec to the whole command text.

**Tech Stack:** Tauri v2, Rust (`tokio` full features already in the tree — zero new dependencies), React 19, TypeScript, Vitest + Testing Library.

**Spec:** `docs/superpowers/specs/2026-11-01-agent-shell-selection-design.md` — the tool input schema (§5.1), the preset table (§5.2), and the two IPC commands (§5.5) are **frozen there**; every line implements against the spec, never against another line's in-progress code.

## Global Constraints

- **No new dependencies.** Runtime or dev, Rust or npm. `tokio` is already `features = ["full"]`; everything needed (process, timeout, pipes) is in the tree. `Cargo.toml` and `package.json` are owned by no line and must not change.
- **Risk truth source:** `run_command` is declared `Risk::Exec` in `ToolMeta` and nothing else may influence its risk. `ToolDef` has no risk field; do not add one.
- **The tool input schema is spec §5.1 verbatim.** `{ command, shell_id?, cwd?, timeout_ms? }`, `command` required. If a line believes the schema must change, it escalates instead of editing.
- **No filesystem access in `Settings::normalize()`** (R284 discipline): shell entries are normalized lexically; existence/executability checks live only in `ensure_executable`, called from the IPC validate command and from `execute` — never from normalize or save.
- **`tools/mod.rs` is frozen.** The contract stub delivery already added `pub mod shell;`; no line edits this file.
- **`capabilities/default.json` is frozen.** No Tauri permission changes; the frontend capability surface is unchanged, and the `McpPanel.test.tsx` capability guard must keep passing untouched.
- **`src/App.tsx` is owned by no line** (its body must end this plan with a zero diff — new `SettingsPanel` props MUST be optional with production defaults that call `commands.ts` directly, same pattern as `McpPanel`'s `loadConfigStatus = mcpConfigStatus`). The **App-level test fixtures** that mirror `SettingsView` as full literals, however, MUST move with the type change: `src/App.test.tsx`, `src/AppSettings.test.tsx`, and `src/AppImages.test.tsx` are owned by the UI line (they are deliberate compile-time tripwires — their own comments say a missing key must fail the build). **B1 (review blocker, 2026-11-01):** adding the required `Settings` keys without updating those three fixtures breaks them with TS2739 under `tsconfig.json` `strict` + `"include": ["src"]`, and no `npm`-based gate is trustworthy in this environment — so the fixture edits are a mandatory, self-contained step of Task 3.1, not something `tsc` is expected to discover.
- **Baseline failures are not ours to fix.** As of 2026-11-01: `npm test`, `npm run build`, and `npm run lint:css` fail on the untouched tree; `cargo test --manifest-path src-tauri/Cargo.toml --lib` passes with **749 passed / 0 failed / 1 ignored**. Architect-environment fact: `node_modules` is **absent** there — `npx tsc` resolves to the placeholder package, vitest/stylelint cannot start (`ERR_MODULE_NOT_FOUND`), so no npm-based check was runnable at plan time. A frontend line that lands in such an environment runs `npm ci` first (never edits `package.json` / `package-lock.json` — that would violate the no-new-dependencies rule), then self-verifies with single-file `npx vitest run <file>`. Nobody "fixes" the failing baseline commands, and no line may add new failures to them.
- **Worktree seeding:** the contract stub `src-tauri/src/tools/shell.rs`, the `tools/mod.rs` registration line, both docs, and the `.gitignore` line exist in the main working tree (uncommitted at plan time). Create each worktree only after that baseline is committed, and branch the three feature branches from it.
- **Merge order:** `feat/agent-shell-core` → `feat/agent-shell-wiring` → `feat/agent-shell-ui`. The wiring line compiles against the stub's contract (unchanged by core's completion); the UI line compiles against the wiring commands. All three lines develop in parallel; only merging serializes.
- **Commits:** each line commits locally on its own feature branch inside its own worktree; never push; never commit in the main working tree.
- All UI copy is Chinese and reuses existing `panel-*` classes; no native dialogs (`window.alert/confirm/prompt` throw in the test setup).

---

# 线 1 · feat/agent-shell-core「shell 核心」

**Worktree:** `.worktrees/shell-core` · **Owns:** `src-tauri/src/tools/shell.rs`（独占）

**任务书：** 把契约桩 `src-tauri/src/tools/shell.rs` 里「未实现」的执行路径做成完整实现。文件的类型与签名以桩为准（它是与接线线/UI 线的契约），本线只允许：填充实现、新增私有辅助、增补测试；**不改任何 `pub` 签名、不改预设表内容、不改入参 schema**。所有逻辑纯 Rust + tokio，禁止 `use tauri`。

### Task 1.1: 执行路径 —— spawn、超时、取消、进程树回收

**Files:**
- Modify: `src-tauri/src/tools/shell.rs`（桩的 `execute` 里「返回未实现错误」的分支；文件头注释）
- Test: `src-tauri/src/tools/shell.rs`（inline `#[cfg(test)]` 扩充）

**Interfaces:**
- Consumes: 桩内 `ShellProfile` / `ShellCatalog::resolve` / `ensure_executable` / `build_command` / `DEFAULT_TIMEOUT_MS` / `MAX_TIMEOUT_MS`；`crate::mcp::bridge::CREATE_NO_WINDOW`；`crate::mcp::process::{create_kill_job, KillOnCloseJob}`。
- Produces: `ShellTool::execute` 全路径；内部可测缝 `async fn run_with_job_factory(job_factory: &dyn Fn() -> Result<KillOnCloseJob, String>, …) -> Result<ToolOutput, ToolError>`（`execute` 以 `create_kill_job` 调它，测试注入假工厂）。

- [ ] **Step 1: 写失败测试**（都放 `#[cfg(unix)]`，用 `/bin/sh`；Windows 真机项按 spec §10 记录，不造假通过）
  1. `echo` 经 `{"command":"echo hi"}`（默认 catalog = bash 预设不可用时的兜底：测试用自定义 profile `program:"/bin/sh", args:["-c"]`）→ `Ok`，文本含 `hi`。
  2. 非零退出码 → `Ok` 且尾部含 `[退出码 3]`。
  3. stdout+stderr 都有 → `[stderr]` 段编排符合 spec §7.6。
  4. 全空输出退出 0 → `[命令成功，没有输出]`。
  5. `timeout_ms: 50` 跑 `sleep 5` → `Failed` 且错误含超时值；随后 `ps` 断言无残留 sh（进程树被回收）。
  6. spawn 前取消（`ctx.cancel.cancel()`）→ `Cancelled`，且没有进程被启动（用注入的假 job 工厂 + 不存在程序时的错误次序佐证）。
  7. 等待中取消：入参用一条 `sleep 5`，在测试里先拿到 child 句柄不可行——改为对 `run_with_job_factory` 注入一个「等待中被 cancel」的令牌 → `Cancelled`。
  8. `job_factory` 返回 `Err` → `Failed`，且没有 spawn（R323：不静默退化）。
  9. program 不存在（绝对路径指向缺失文件）→ `Failed` 点名 program。
  10. 无 `cwd` 且 `write_root: None` → `BadInput`；`cwd` 相对路径/不存在 → `BadInput`。
- [ ] **Step 2:** `cargo test --manifest-path src-tauri/Cargo.toml --lib shell::` → 新测试红。
- [ ] **Step 3: 实现**：替换桩的未实现分支。顺序：取消检查 → 解析入参（桩已实现）→ `resolve` → cwd 决定（显式 cwd 校验绝对+存在+是目录；缺省取 `ctx.write_root`，无则 `BadInput`）→ `ensure_executable` → `create_kill_job`（经工厂）→ `build_command` + `kill_on_drop(true)` → spawn → 两个 pipe 各起一个「读到上限后继续读并丢弃」的 reader → `tokio::select!{ biased; cancel => …, timeout(...) => …, join!(readers) + child.wait() }` → 按 spec §7.6 编排文本。超时与取消两臂先 `child.kill().await` 再让 job Drop 兜树。
- [ ] **Step 4:** `cargo test --manifest-path src-tauri/Cargo.toml --lib shell::` 全绿，且桩既有类型测试不回归。
- [ ] **Step 5:** `git add src-tauri/src/tools/shell.rs && git commit -m "feat(shell): execute run_command with timeout, output cap and tree reaping"`

### Task 1.2: 输出上限与截断标记

**Files:** Modify + Test: `src-tauri/src/tools/shell.rs`

**Interfaces:** Consumes `MAX_OUTPUT_BYTES = 512 * 1024`（桩内常量）。Produces：超限后**继续读取并丢弃**的 reader（防管道堵塞），交出文本截到上限 + 尾部标记（实测丢弃字节数，风格同 `read_file` 的页尾标记）。

- [ ] **Step 1: 失败测试**（`#[cfg(unix)]`）：`/bin/sh -c` 产出 > 512 KiB 的 stdout → `Ok`，交出文本 ≤ 上限、含「512 KiB」字样的标记与实测字节数；命令进程正常退出（不因 reader 停读而卡死）。
- [ ] **Step 2:** 红。**Step 3:** 实现 bounded reader。**Step 4:** `cargo test --manifest-path src-tauri/Cargo.toml --lib shell::` 全绿。
- [ ] **Step 5:** `git commit -m "feat(shell): cap captured output at 512 KiB with an honest truncation marker"`

### Task 1.3: 非 UTF-8 输出的 lossy 处理

**Files:** Modify + Test: `src-tauri/src/tools/shell.rs`

**Interfaces:** Produces：字节→文本的单一实现（优先 `from_utf8`；失败则 lossy + 尾部注明「输出含非 UTF-8 字节，已按替换字符转换」）。与 `read_file` 的拒绝语义**刻意不同**，理由见 spec §8.5，写进实现注释。

- [ ] **Step 1: 失败测试**（`#[cfg(unix)]`）：`printf '\xff\xfe'` → `Ok`，文本含 U+FFFD 与「非 UTF-8」注明。**Step 2:** 红。**Step 3:** 实现。**Step 4:** 模块测试全绿。
- [ ] **Step 5:** `git commit -m "feat(shell): lossily convert non-UTF-8 output with an explicit notice"`

---

# 线 2 · feat/agent-shell-wiring「Rust 接线」

**Worktree:** `.worktrees/shell-wiring` · **Owns:** `src-tauri/src/settings.rs`、`src-tauri/src/tools/registry.rs`、`src-tauri/src/ipc.rs`、`src-tauri/src/agent/session.rs`、`src-tauri/src/lib.rs`（预期零改动，留作兜底）

**任务书：** 把桩（或核心线完成后的同签名实现）接进生产：设置持久化与归一化、注册表装配、每轮 catalog 刷新、两条 IPC 命令。对 `shell.rs` 只调 `pub` 接口（以桩 + spec §5 为契约），绝不改它。

### Task 2.1: `Settings` 增 `shells` 与 `default_shell`

**Files:**
- Modify: `src-tauri/src/settings.rs`（`Settings` 字段区、`Default`、`normalize()`）
- Test: `src-tauri/src/settings.rs`（inline `#[cfg(test)]`）

**Interfaces:**
- Consumes: `crate::tools::shell::{ShellProfile, DEFAULT_PRESET_ID}`（settings → tools 的单向引用，spec §4 已裁定方向合法）。
- Produces: `Settings.shells: Vec<ShellProfile>`、`Settings.default_shell: String`（`Default`：`Vec::new()` / `DEFAULT_PRESET_ID.into()`）；normalize 规则见 spec §6。

- [ ] **Step 1: 写失败测试**
  1. `shells` 里空 id / 空 program / args 含空项的条目 **normalize 后原样保留**（不静默删数据，执行时 fail-closed）。
  2. 同 id 重复 → 保留首条（与 `dedupe_roots` 同纪律）。
  3. `default_shell` 为空、或指向不存在的 id → 回落 `"bash"`；指向预设 id 或某个自定义 id → 原样。
  4. 字段序列化成 `{"shells":[…],"default_shell":"…"}` 的 snake_case 线格式（沿 `roots_serializes_as_an_array_of_strings_for_the_frontend_mirror` 的形状测试风格）。
  5. 旧 `settings.json`（无这两个键）`load` 后得到默认值（结构体级 `#[serde(default)]` 的回归测试，仿 R91 既有测试）。
- [ ] **Step 2:** `cargo test --manifest-path src-tauri/Cargo.toml --lib settings::` 红。
- [ ] **Step 3: 实现**：字段 + Default + normalize（词法 trim、逐条校验空形状**保留**、去重、default 回落）。normalize 不做任何存在性检查。
- [ ] **Step 4:** `cargo test --manifest-path src-tauri/Cargo.toml --lib settings::` 全绿。
- [ ] **Step 5:** `git add src-tauri/src/settings.rs && git commit -m "feat(settings): persist custom shell profiles and the default shell"`

### Task 2.2: 注册 `ShellTool` + 每轮刷新 catalog

**Files:**
- Modify: `src-tauri/src/ipc.rs`（imports、`AppState`、`build_production_registry`、`send_message`）
- Modify: `src-tauri/src/tools/registry.rs`（仅当装配需要时；预期零改动）
- Test: `src-tauri/src/ipc.rs`（inline tests，含装配守卫测试）

**Interfaces:**
- Consumes: `crate::tools::shell::{ShellCatalog, ShellTool}`。
- Produces: `AppState.shells: Arc<RwLock<ShellCatalog>>`；`build_production_registry(db: Arc<Db>, shells: Arc<RwLock<ShellCatalog>>)`（签名变更，调用点全在本文件）；`shell_catalog_of(settings: &Settings) -> ShellCatalog`（纯函数：`ShellCatalog::from_parts(settings.shells.clone(), settings.default_shell.trim())`）。

- [ ] **Step 1: 写失败测试**
  1. `build_production_registry(db, shells)` 后 `defs()` 名字含 `run_command`，且 `risk_of("run_command") == Risk::Exec`（仿既有 `production_registry_ships_fetch_url_and_read_file`）。
  2. `shell_catalog_of`：settings 的 shells/default_shell 原样进 catalog；default 空串 → `DEFAULT_PRESET_ID`。
  3. 装配守卫：现有 `the_assembly_wires_the_production_registry` 断言按新签名自然存活（它按前缀匹配 `registry: build_production_registry(`）；补一条断言 `AppState` 装配段含 `shells:` 字段接线（照同一测试的取法）。
  4. ipc.rs 内既有调用 `build_production_registry(db)` 的测试全部改为传 `Arc::new(RwLock::new(ShellCatalog::new()))`。
- [ ] **Step 2:** 红。**Step 3: 实现**：`send_message` 在 `store.load()`（`ipc.rs:1601`）之后、`run_turn` 之前插一行 `*crate::lock::write(&state.shells) = shell_catalog_of(&settings);`——与 roots/temperature 同一条「本轮现取」线，注释引 spec §2「生效时机」行。
- [ ] **Step 4:** `cargo test --manifest-path src-tauri/Cargo.toml --lib ipc::` 全绿 + 全库一次。
- [ ] **Step 5:** `git add src-tauri/src/ipc.rs src-tauri/src/tools/registry.rs && git commit -m "feat(ipc): register the run_command tool and refresh its shell catalog per turn"`

### Task 2.3: 两条 IPC 命令

**Files:**
- Modify: `src-tauri/src/ipc.rs`（`shell_presets`、`shell_profile_validate`、`invoke_handler`）
- Test: `src-tauri/src/ipc.rs`（inline tests + 登记表守卫测试）

**Interfaces:**
- Produces（spec §5.5 写死）:
  - `#[tauri::command] pub fn shell_presets() -> Vec<ShellProfile>`（返回 8 条预设的克隆；纯函数无状态）。
  - `#[tauri::command] pub fn shell_profile_validate(profile: ShellProfile) -> Result<(), String>`：`validate_custom(profile)?` 后按 program 形状走 `ensure_executable`（裸名传 `PATH` 目录表，Windows 叠 `PATHEXT`；生产解析器与 MCP 的「解析器可注入」同款，测试注入假目录）。
- 错误是给人读的中文串（`Err` 原样上屏）；不新增 view 结构体（`ShellProfile` 本身就是线格式）。

- [ ] **Step 1: 写失败测试**
  1. `shell_presets` 返回 8 条、id 集合与 spec §5.2 的表逐项一致、全部 `Serialize`（`serde_json::to_value` 含 `id/label/program/args` 四键）。
  2. `shell_profile_validate`：合法路径（注入假 search_dirs，unix 下用 `tempfile` + `chmod +x`，Windows 分支用 `std::env::current_exe()` 兜底真值）→ `Ok(())`；裸名不在假 PATH → `Err` 且含 program；`validate_custom` 的形状错误（空 id）→ `Err` 且不做存在性检查。
  3. 登记表守卫：仿 `the_normalization_command_is_registered_in_the_handler_list` 的取法，断言 `shell_presets` 与 `shell_profile_validate` 都出现在 `generate_handler![` 清单里。
- [ ] **Step 2:** 红。**Step 3: 实现 + `invoke_handler` 登记。**
- [ ] **Step 4:** `cargo test --manifest-path src-tauri/Cargo.toml --lib ipc::` 全绿；整库 `cargo test --manifest-path src-tauri/Cargo.toml --lib` 不低于基线 749 passed 且全绿。
- [ ] **Step 5:** `git add src-tauri/src/ipc.rs && git commit -m "feat(ipc): expose shell presets and profile validation"`

---

# 线 3 · feat/agent-shell-ui「前端 UI」

**Worktree:** `.worktrees/shell-ui` · **Owns:** `src/components/SettingsPanel.tsx`、`src/components/SettingsPanel.test.tsx`、`src/types.ts`、`src/ipc/commands.ts`、`src/App.test.tsx`、`src/AppSettings.test.tsx`、`src/AppImages.test.tsx`（B1：三份 App 级 `SettingsView` 全键夹具随类型改动补键）、`README.md`（收尾文档任务）

**任务书：** 设置面板新增「Shell」区块（spec §3.1 的交互全部落地），类型与命令包装镜像 Rust 线格式。**不改 `App.tsx`**（新 props 可选 + 生产默认）；**不改任何 Rust 文件**。真校验只有 Rust 一处，前端只做按钮使能级别的表单使能。

### Task 3.1: 类型镜像 + IPC 包装

**Files:**
- Modify: `src/types.ts`（`ShellProfile`；`Settings` 增 `shells: ShellProfile[]`、`default_shell: string`）
- Modify: `src/ipc/commands.ts`（`shellPresets()`、`shellProfileValidate(profile)`）
- Modify（B1，与上面的类型改动**同一个 commit**）：`src/App.test.tsx:66`、`src/AppSettings.test.tsx:82`、`src/AppImages.test.tsx:52` —— 三份 `(): SettingsView => ({…全键字面量…})` 夹具各补 `shells` 与 `default_shell` 两键（它们自己注释写明「少写就编译不过」；`tsconfig.json` 是 `strict` + `"include": ["src"]`，缺键 = TS2739，而本计划的 npm 闸门不可信，见 Global Constraints 的 B1 条）。

**Interfaces:** 逐字照 spec §5.6。注释写明：预设表的真相在 Rust（`tools/shell.rs`），前端只有只读副本。

- [ ] **Step 1:** 类型与包装落位（先于组件，让 fixture 编译得过）。
- [ ] **Step 2（B1）:** 三份 App 级夹具补键，取值按各文件既有的语义分工：
  - `App.test.tsx` / `AppImages.test.tsx`：生产默认值 `shells: []`、`default_shell: "bash"`（与它们给 `roots: []` 的同一条注释逻辑——这两份不断言保存载荷，键只承担「少一键就编译不过」）。
  - `AppSettings.test.tsx`：**非默认值**（`default_shell: "pwsh"`、`shells: [{ id: "mine", label: "", program: "/opt/mine", args: ["-c"] }]`），与它给 `roots` 非默认值的既有取向一致——它的载荷断言要能区分「App 原样转交」与「App 重拼默认表」。
  - `src/components/SettingsPanel.test.tsx:28` 的夹具在本线名下，随 Task 3.2 一起补。
- [ ] **Step 3:** 若本机有 TypeScript（`node_modules/.bin/tsc` 存在）跑 `npx tsc --noEmit`：只允许出现**由本线改动引入**的报错并修掉，基线报错原样保留。架构师环境（2026-11-01）`node_modules` 缺失、该检查不可执行——先 `npm ci` 再跑；仍然不可执行时如实记录「未跑」，不许拿 `vite build` 冒充类型检查。**B1 的兜底即 Step 2**：夹具补键是本任务的必做编辑项，不依赖 tsc 才发现。
- [ ] **Step 4:** `git add src/types.ts src/ipc/commands.ts src/App.test.tsx src/AppSettings.test.tsx src/AppImages.test.tsx && git commit -m "feat(types): mirror ShellProfile and the two shell IPC commands"`

### Task 3.2: SettingsPanel「Shell」区块

**Files:**
- Modify: `src/components/SettingsPanel.tsx`（新 `fieldset.panel-group`、props、草稿状态）
- Modify: `src/components/SettingsPanel.test.tsx`（fixture `view()` 补 `shells`/`default_shell` 两键 + 新用例）

**Interfaces:**
- `SettingsPanelProps` 增两个**可选**回调（生产默认直连 `commands.ts`，`App.tsx` 零改动）：
  - `onLoadShellPresets?: () => Promise<ShellProfile[]>`（默认 `shellPresets`）
  - `onValidateShellProfile?: (p: ShellProfile) => Promise<void>`（默认 `shellProfileValidate`）
- 区块结构（spec §3.1）：`<fieldset className="panel-group"><legend>Shell</legend>` 内依次为——预设只读列表（`panel-readonly` 行：`{label}（{id}）· {program} {args.join(" ")}`）；默认 shell `<select>`（选项 = 预设 ∪ 自定义 id，绑定 `form.default_shell`）；自定义 profile 列表 + 表单（id、label、program 三个 `panel-row` 输入，args 用**每行一个参数**的 textarea）与「添加 / 修改 / 删除 / 校验」按钮（`panel-actions`）。
- 表单草稿是面板局部 state；「添加/保存」在 program 为空时 `disabled`；id 与预设同名时表单下挂 `panel-hint`：「同名自定义 profile 会在执行时覆盖同名预设。」；校验失败进面板既有 `error` 通道，成功就地显示「程序存在且可执行。」；区块尾 `panel-hint`：「保存后下一条消息生效。」

- [ ] **Step 1: 写失败测试**（注入假 props，不碰 IPC；沿用文件头既定的 `view()` 夹具与 `act/waitFor` 口径）
  1. 挂载后渲染 `onLoadShellPresets` 返回的 8 条预设（只读：无任何 input 绑定它们的 program/args）。
  2. 默认 shell 下拉含预设 id 与自定义 id；切换后点保存，载荷 `default_shell` 为新值。
  3. 「添加」把表单草稿 push 进载荷 `shells`（`onSave` 断言整表）；program 为空时按钮禁用。
  4. 「校验」失败 → 错误通道显示后端原因，草稿不丢；成功 → 「程序存在且可执行。」。
  5. 「删除」从载荷 `shells` 里移除对应条目。
  6. 「修改」：选中一条回填表单 → 改 program → 保存载荷里该条被替换且位置不变。
  7. 既有测试全绿（fixture 补两键后）。
- [ ] **Step 2:**（先 `npm ci`，若环境还没有 node_modules）`npx vitest run src/components/SettingsPanel.test.tsx` 红。**Step 3: 实现**。**Step 4:** 同命令全绿；再跑 `npx vitest run src/components/McpPanel.test.tsx src/components/SettingsPanel.test.tsx` 确认相邻面板无回归。
- [ ] **Step 5:** `git add src/components/SettingsPanel.tsx src/components/SettingsPanel.test.tsx && git commit -m "feat(ui): shell profiles section in the settings panel"`

### Task 3.3: README 收尾（最后合并的一条线做）

**Files:** Modify: `README.md`

- [ ] **Step 1:** 「功能」列表加一条 Shell 执行 bullet（`run_command`、预设 + 自定义 profile、默认 shell 存设置、每次调用弹 Exec 确认）；「安全说明」加一条「`run_command` 是 `Risk::Exec`：每次弹窗、会话级放行的粒度是整条命令文本；命令结束整棵进程树被回收」；「数据位置」的 `settings.json` 行补「自定义 shell profile 与默认 shell」。
- [ ] **Step 2:** `git add README.md && git commit -m "docs: document the run_command tool and shell profiles"`

---

## Self-Review Notes

- Spec coverage: 工具与 schema（线 1 / spec §5.1）、预设表与 catalog（线 1 桩 + spec §5.2–5.4）、settings 与 normalize（线 2 Task 2.1 / spec §6）、装配与刷新（线 2 Task 2.2 / spec §2 生效时机、§4 边界）、IPC（线 2 Task 2.3 / spec §5.5）、前端区块（线 3 Task 3.2 / spec §3.1）、安全红线与失败处理（线 1 执行路径测试逐条对 spec §8–9）。
- Ownership（B1 修订后全量盘点）：线 1 = `src-tauri/src/tools/shell.rs`；线 2 = `settings.rs`、`tools/registry.rs`、`ipc.rs`、`agent/session.rs`、`lib.rs`；线 3 = `types.ts`、`ipc/commands.ts`、`SettingsPanel.tsx`、`SettingsPanel.test.tsx`、`App.test.tsx`、`AppSettings.test.tsx`、`AppImages.test.tsx`、`README.md`——三个清单两两不相交。无主且**禁改**：`tools/mod.rs`（桩交付时已注册 `pub mod shell;`）、`capabilities/default.json`、`src/App.tsx`（本体；其三份测试夹具归线 3）、`Cargo.toml`、`package.json`/`package-lock.json`。
- 类型改动波及面已穷举（B1）：全仓库 `(): SettingsView` 全键字面量夹具恰四份——`App.test.tsx:66`、`AppSettings.test.tsx:82`、`AppImages.test.tsx:52`（线 3 Task 3.1 补键）、`SettingsPanel.test.tsx:28`（线 3 Task 3.2 补键）；`SettingsPanel.tsx:411` 的保存载荷是 `{...form, roots, accent}` 的 spread 形状，新键自动携带，不破。
- No placeholders: 每个任务给出确切接口、测试清单与验收命令；模板级的代码本体由实现线按桩与 spec 写出（桩内已带可运行的类型层参考实现）。
- Baseline honesty: 749 passed / 0 failed / 1 ignored（`cargo test --lib`，2026-11-01 实测）；`npm test` / `npm run build` / `npm run lint:css` 基线失败，只要求不新增失败；架构师环境无 `node_modules`，npm 系检查（含 vitest）在计划期一条都没跑过。

---

## Addendum（2026-11-01，feat/shell-detect-presets：候选 + 检测）

合并后的一处设计修订（spec §5.2 的带日期修订为准）：**8 个内置预设从「写死恒在」改为「运行时检测」。** §5.2 的表冻结为候选表（id/label/program/args 与展示顺序不变），落点：

- `tools/shell.rs`：新增 `detected_presets()`（生产检测单一真相）与 `is_preset_candidate_id()`（纯词法候选判定）；`ShellCatalog` 增 `presets` 字段，构造点（`new`/`from_parts`，测试注入口 `from_parts_with_probe`）对每条候选用 `ensure_executable` + 候选专属搜索目录判定，通过的才进预设集；检测只过滤、不回写解析路径，执行期 `ensure_executable` 兜底不变。Windows 候选目录表 `cfg(windows)` 才编译（真机项）。
- `settings.rs`：normalize 的 `default_shell` 判定改为「候选 id 静态集合 ∪ 自定义 id」，R284（不碰文件系统）不破；Global Constraints 第 4 条的「shell 条目词法整理」语义不变。
- `ipc.rs`：`shell_presets()` 返回检测子集（自然变少，可能为空）；Task 2.3 里「presets 返回 8 条」的测试钉法相应改为「spec §5.2 顺序的子序列 + 与候选表逐字段一致」。`shell_profile_validate` 与自定义覆盖预设的优先级不变。
- 前端零改动：SettingsPanel 纯动态渲染预设列表与默认 shell 下拉（无写死数量），既有 UI 测试的 8 条 mock 测的是渲染不是后端。
