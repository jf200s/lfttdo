# Plan C — 上下文压缩、设置面与交付 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax and are one action each.

**Goal:** 让长对话真的不失忆（usage 校准 + 预算 + 滚动摘要压缩 + `ContextCompacted`），把「设置」从**代码里存在、界面上不存在**变成用户能自己改的五组设置，并用实测数字关掉 spec §9 至今从未测过的三条验收线（安装包 <15MB、冷启动 <1.5s、空闲常驻 <150MB）。

**Architecture:** 全部复用 Plan A 铺好的接缝，**不新建通道**。压缩是**两次对话之间的存储动作**，不是在请求构造中途改写内存历史（理由见「前置事实」第 2 条，那是本计划最硬的一条约束）；摘要调用直接走 `ModelProvider::stream` 而**不经 `sink`**，否则摘要文本会变成一条用户看得见的气泡；设置面沿用 `Settings`（单文件 `settings.json`、整表覆盖）+ keyring 掩码出口，前端只有 `SettingsPanel` 一个新组件；`--accent` 是 `applyTheme` 写的**第二个** inline 变量，配一道纯函数把单一取色展开成可读的三件套。

**Tech Stack:** Rust（无新依赖——本计划**不允许**新增任何直接依赖，理由见 Global Constraints）、Tauri v2（仍只有 `ipc.rs` 依赖它）、React + TypeScript + Vitest 前端、Windows PowerShell（体积/内存/启动实测）。

**Spec:** `docs/superpowers/specs/2026-10-19-qwen-do-agent-design.md`（本计划覆盖切片 5、6；切片 0–2 见 Plan A，切片 3–4 见 Plan B）

**依赖 Plan A / Plan B 的产出（派发前逐条验证符号存在，符号名比行号可靠）：**

| 来自 | 消费的符号 | 用于 |
|---|---|---|
| Plan A T8 | `ContextConfig{reserve_output,budget_tokens,window_messages}`、`Built{messages,dropped,est_input_tokens}`、`ContextBuilder::{new,set_calibration,build}`、`estimate_tokens` | T38–T41 |
| Plan A T10 | `SessionDeps{model,registry,gate,sink,context}`、`AgentSession::{restore,history,run,cancel_handle}`、`SessionConfig{max_turns}`、`Message::{user,system_note,tool,text_of}`、`Role` | T38、T41 |
| Plan A T11 | `Db::{messages,append_message}`、`events_from_messages`（`Role::System => {}` 整行跳过，R69） | T41 |
| Plan A T12 | `Settings`（**恰好 8 字段**）、`Settings::{normalize,normalized,to_context_config,to_model_config}`、`mask_key`、`SettingsStore::{load,save,set_api_key,api_key,api_key_masked,clear_api_key,take_corrupt_backup}` | T39、T43 |
| Plan A T13 | `ipc.rs` 的 `run_turn`（读历史→`prior`→`restore`→`run`→`append_rows(&history[prior..])`）、`SettingsView{#[serde(flatten)] settings, api_key_masked, corrupt_backup}`、`settings_get/settings_set/settings_set_api_key` | T41、T45 |
| Plan A T14 | `reducer.ts` 的 `case "context_compacted"`（已能出「已压缩早期对话：省略 N 条，摘要 M 字」）、`types.ts` 的 `SendBlock` | T41 |
| Plan A T15 | `theme.ts` 的 `THEME_NAMES/THEME_TOKENS(15 项)/USER_TOKENS(["--font-size"])/applyTheme/resolveTheme` | T44 |
| Plan B T26 | `Settings.roots` | T43、T45（工具组） |
| Plan B T29/T35 | `mcp.json` 配置层 + `mcp_*` 命令 + server 管理 UI | T45（MCP 组是否已存在要有判断，见该任务 Step 0） |

**前置事实（2026-10-21 对 HEAD 实测，全部给 `file:line`；与你的假设冲突时以文件为准）：**

1. **`set_calibration` 是一个没有调用方的真接口。** 定义在 `context.rs:73-78`（语义 = 真实 ÷ 粗估，夹 `0.2..=3.0`，非有限值忽略），全仓 grep 排除 `context.rs` 自身 = **0 命中**。`Built.dropped` 与 `Built.est_input_tokens` 同样无消费者：`session.rs:296-298` 的注释自己写着「Plan A 没有消费者：`ContextCompacted` 属切片 5」。⇒ 切片 5 不是新建能力，是**给三条已经躺着的路接上电**。
2. **`ipc.rs:320` 的持久化公式假设历史只增不减**：`let fresh = session.history()[prior..].to_vec();`（`prior` 在 :302 于 `restore` 之前捕获）。⇒ **任何在 `run()` 里就地裁剪 `self.history` 的做法都会把这一行变成越界 panic 或静默错附**。本计划的压缩因此被**规定为存储侧事务**（T41），并在 `restore` 之前完成，`prior` 必须在压缩后重新取。这条是 T41 的第一验收项，不是可选风格。
3. **`temperature` 的管线已经通了，只差一个字段。** `ChatRequest.temperature: Option<f32>`（`provider.rs:44`）、发送侧 `if let Some(t) = req.temperature { b.temperature(t) }`（`openai_compat.rs:281-282`）、且有测试钉它进请求体（`openai_compat.rs:917-936`）。而 `session.rs:301` 硬写 `temperature: None`。⇒ T39 的活是「加 `Settings.temperature` 并把 `None` 换成它」，**不是**从零做温度支持。
4. **`settings.rs` 里有两条测试会合谋掩盖 T43。** 一条是 `serialized_field_names_are_exactly_the_eight_settings_fields`（:1260），它既断 8 个键名（:1265-1279），又断序列化结果**不含** `"api_key"`/`"temperature"`/`"accent"`/`"mcp"`/`"roots"`（:1282-1288）；另一条是 `unknown_fields_are_ignoreged_and_the_file_is_not_backed_up`（约 :1318，夹具在 :1326），它**正是拿 `temperature` 与 `accent` 当「不认识的字段**」。加字段时第一条会红（必须同步改），**第二条会原样绿但从此不再测它声称的事**——那才是真正的坑。T43 把夹具换成永远不认识的键名。
5. **`ContextCompacted` 事件与 reducer 分支已经存在**：`events.rs:59-62` 变体、`types.ts:160`、`reducer.ts:267-270`（已能生成「已压缩早期对话：省略 N 条，摘要 M 字」），缺的只有 Rust 侧 emit。⇒ T41 只补 emit，**不许**动 reducer 的投影文案。
6. **摘要/压缩逻辑今天完全不存在。** `src-tauri/src` 里 grep `compact|summary|摘要|threshold|压缩` 只命中注释（`context.rs:45/84/199/594`、`db.rs:344`）与 `image_prep.rs:35 TooLargeAfterCompression`（那是图片降质，不是上下文压缩）。`早期对话摘要：` 只作为测试夹具出现在 `context.rs:596/603`。
7. **历史读取没有 LIMIT**：`Db::messages`（`db.rs:206-218`）是 `SELECT role,blocks_json … ORDER BY id`，调用点 `ipc.rs:293`（`run_turn`）与 `ipc.rs:479`（`session_messages` 全量重放）。唯一上限是 `window_messages`（`context.rs:99`），它只约束**发给模型**的那份，不约束**读进内存**的那份。⇒ T42 必须量 100 轮时的真实条数与字节，而不是猜。
8. **`Settings` 落盘是整表覆盖，无 schema 版本、无迁移**：`settings_set`（`ipc.rs:523`）收整个 `Settings` 直交 `save`（`settings.rs:478-490`，temp→`sync_all`→rename，实例锁内）。读侧靠 `#[serde(default)]`（:242）前向兼容，解不开则备份 `settings.json.corrupt-<ms>` 后回落默认（:439-447）。⇒ T45 的保存语义必须是「读整表→改本地副本→写整表」，不许发明增量 patch。
9. **设置界面今天不存在**：`setSettings`（`commands.ts:62`）与 `setApiKey`（:65）在非测试代码**零消费者**；`src/components/` 只有 `Composer/MessageBubble/MessageList/Sidebar`。`getSettings` 的唯一消费者是 `App.tsx:89`（驱动主题）。⇒ T45 是**第一个**把这两条命令接到界面上的任务，也是 `Settings` 扩容能被用户看见的唯一路径。
10. **交付基线三项从未实测**：`docs/findings/plan-a-risk-verification.md:31` 那个 12,822,528 字节是 **debug** exe，文档 :38 自己写着「debug 体积不能用于验收」；:49 的「工作集 30 MB」是 dev、**单进程**，不是 spec §9 要求的「含 WebView2 子进程之和」。NSIS 专项配置不存在（`tauri.conf.json:27` 只有 `bundle.targets:"all"`）。⇒ T46/T47 关掉的是**至今为零**的证据，不是重测已知数字。
11. **`--accent` 只由预设钉值**：`tokens.css:7-9` 定义 `--accent/--accent-hover/--accent-text`，`presets.css` 六个块（3 主题 × 2 模式，选择器在 :10/28/46/64/82/100）各写具体色；`applyTheme`（`theme.ts:32-47`）只写两个 data 属性 + `--font-size` 一个变量。⇒ 「强调色自定义」是**新功能**，不是配置漏接，并且必须处理「用户挑的色与预设的 hover/文字色不成一套」这个真实冲突（T44）。
12. **冷启动有一小段主题真空**：`index.html` 的 `<html>` 没有 `data-theme`/`data-mode` 初值，属性要等 `settings_get` 回来才由 `App.tsx:94` 写，之前靠 `tokens.css` 的 `:root` 兜底。⇒ T44 顺带关掉这道闪，且不许为此让 `index.html` 里出现内联脚本之外的第二份主题真相。

## Global Constraints

每个任务都隐式包含本节。

- **本计划不新增任何直接依赖**（Rust 与 npm 都不加）。切片 5/6 的每一项都能用已有设施做完；若某个任务认为必须加依赖，那是**停下来报告**的信号，不是自己 `cargo add` 的许可。已有体积红线：安装包 <15MB，`Cargo.lock` 每加一个包都要在 #29 重算预算。
- 只有 `ipc.rs` 允许 `use tauri`。`agent/`、`store/`、`settings.rs`、`context.rs` 必须能在 `cargo test` 里无 Tauri、无网络地跑。
- **单测不许碰网络，也不许创建或修改真实 Windows 凭据**（R81/R156 继续生效）。摘要调用的测试一律用 `ScriptedModel`；真 key 的判据写进 `docs/findings/`，没跑过就标 `未实测`。真实 key 不许出现在日志、findings、对话或本计划的任何代码块里。
- **压缩不许丢用户内容而不留痕迹**：每次压缩必须在库里留下一条 `Role::System` 的摘要备注（`events_from_messages` 已按 R69 整行跳过它，不会变成气泡），且必须 emit `ContextCompacted`。摘要调用失败时降级为「整条丢弃 + `[早期对话已省略]` 占位」（spec :365），**不许**把「压不动」变成报错。
- **摘要文本不进 `sink`**：它不是模型对用户的回答。走 `ModelProvider::stream` 自行收 `ModelDelta::Text`，忽略 `ToolCall*` 与 `Usage`（工具声明传空 `vec![]`，模型才不会被诱导去调工具）。
- 零弹窗：前端禁 `window.prompt`/`alert`/`confirm`；`capabilities/default.json` 在本计划**不新增权限**。T45 的 api_key 输入框必须是 `type="password"` + `autoComplete="off"`，且提交后立刻清空本地值。
- 组件内禁止硬编码颜色，只能用 token 变量；`npm run lint:css` 必须 rc=0（豁免只有 `src/theme/tokens.css` 与 `src/theme/presets.css`）。
- `api_key` 只写 keyring：不写 `settings.json`、不进 `SettingsView` 明文字段（只有 `api_key_masked: Option<String>`）、不写日志、不进 T47 的 findings（findings 只准出现字段名，不准出现 key 或 `cmdkey /list` 全文，R164）。
- 本机执行规矩（每次 Bash 调用都带上）：`export PATH="$HOME/bin:$PATH"`；**永不 `cargo fmt`**，格式化用 `rustfmt --edition 2024 [--check] <file>`；cargo 走 `CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=5 CARGO_HTTP_TIMEOUT=60`；PowerShell 输出接 `| Out-String -Width 200`，`cmdkey` 输出是 GBK 要 `iconv -f GBK -t UTF-8`。
- **字节同一性证明的正确写法**（本仓库 `core.autocrlf=true` 且无 `.gitattributes`）：`cmp <(git cat-file blob HEAD:<path>) <(tr -d '\r' <<path>)` **加上** `git diff --exit-code -- <path>`。直接拿 blob 和工作区文件 `cmp` 对干净文件**也会红**（blob 是 LF、工作区是 CRLF），这是一条已经骗过一次假红的坑，别再照抄。
- 行号以派发时实际文件为准；引用一律「符号名 + 当前行号」，实施者先验证符号存在再动手。

## 共享测试辅助（引用即须有定义，禁止「想象中的 API」）

- `ScriptedModel`（Plan A T10 建，`agent/session.rs` 的 `mod tests`）：**跨任务复用要抽到 `src-tauri/tests/common/mod.rs` 或 `#[cfg(test)]` 的共享模块**，T41 Step 0 先确认它今天在哪；若只有 session.rs 内部能用，就在 T41 里加一份 `summarizer` 自己的假模型（**不许**为了复用去改可见性然后再动 session.rs 的测试）。
- `VecSink`（同上）：断言「摘要调用的文本没有进 sink」的唯一手段。T41 的验收判据是 `sink` 里**没有** `TextDelta`，而有 `ContextCompacted`。
- `tempfile::tempdir()`（dev-dependency 已有）：`Db` 与 `SettingsStore` 的路径夹具，一律用它，不碰用户真实 `%APPDATA%`。
- `Settings::default()`（`settings.rs`）：新增字段必须进 `Default`（:258-269），否则 `..Default::default()` 的既有测试夹具会静默拿到 0 值——那是 T39/T43 最容易漏的一条。
- 前端：`src/test/` 下已有的 render/fireEvent 辅助与 `vi.mock("@tauri-apps/api/core")` 模式（`App.test.tsx:21-22` 那套 mock 工厂）照抄形状，不自造第二套。
- PowerShell 侧（T46/T47）：每条测量命令写全，输出**原样**贴进 findings；测量脚本不落盘成 `.ps1`（一次性），除非某条要跑 5 次取中位数——那种情况写成 `docs/findings/scripts/cold-start.ps1` 并在报告里给绝对路径。

## 文件结构（Plan C 建立/修改的文件）

| 文件 | 责任 | 任务 |
|---|---|---|
| Modify `src-tauri/src/agent/session.rs` | usage → 校准的第一次接线（`request()` 交出 `est_input_tokens`，`Usage` 臂回灌） | T38 |
| Modify `src-tauri/src/agent/context.rs` | `ContextConfig` 增 `compact_threshold`/`keep_turns`；新增纯函数 `plan_compaction` | T39、T40 |
| Create `src-tauri/src/agent/summarize.rs` | 摘要提示词、`collect_summary`（流→String）、失败降级文本 | T41 |
| Modify `src-tauri/src/store/db.rs` | `Db::compact_session`（一个事务：删最老区间 + 插摘要备注） | T41 |
| Modify `src-tauri/src/ipc.rs` | `run_turn` 里压缩段插在 `restore` 之前 + `prior` 重取 + emit `ContextCompacted` | T41 |
| Modify `src-tauri/src/settings.rs` | `temperature`/`max_output_tokens`/`compact_threshold`/`keep_turns`/`accent` 字段 + 钳位 + 两条钉子同改 | T39、T43 |
| Create `src/components/SettingsPanel.tsx`（+ test） | 五组设置的唯一入口，`setSettings`/`setApiKey` 的第一个消费者 | T45 |
| Modify `src/theme/theme.ts`（+ `src/theme/tokens.test.ts`） | `USER_TOKENS` 扩容 + `accentTriplet` + `applyTheme` 写 accent 三件套 | T44 |
| Create `src/theme/accent.ts`（+ test） | 纯函数：单个 hex → 可读三件套；hex 校验 | T44 |
| Modify `src/main.tsx` | 冷启动同步读缓存，消掉主题真空 | T44 |
| Modify `src/styles.css` | 面板样式（全 token，无 hex） | T45 |
| Modify `src-tauri/tauri.conf.json` | `bundle.targets:["nsis"]` + `bundle.windows.nsis` | T46 |
| Create `src-tauri/src/window_state.rs`（+ 内联测试） | `WindowGeom`/`load`/`save`/`sane`/`should_persist`：spec §6.6 的「记住尺寸位置」，不加插件 | T46b |
| Modify `src-tauri/src/ipc.rs` + `src-tauri/src/lib.rs` | 挂 `on_window_event`、启动时按存档几何建窗（越屏则回落）、模块声明 | T46b |
| Create `docs/findings/plan-c-delivery-baseline.md` | 体积/冷启动/内存三项实测与验收判定，外加 T46b 的「窗口状态」一节 | T46、T46b、T47 |
| Create `docs/findings/plan-c-long-session.md` | 100 轮压测的真实数字（条数、字节、token、压缩次数） | T42 |

---

## 切片 5 — 上下文：让长对话真的不失忆

### Task 38: usage 校准闭环——`set_calibration` 拿到它的第一个调用方

**Files:**
- Modify: `src-tauri/src/agent/session.rs`（`request()` 约 :291-310、`Usage` 臂约 :152-155、`AgentSession` 结构体 :36-49）
- Test: `src-tauri/src/agent/session.rs`（内联 `mod tests`）

**Interfaces:**
- Consumes: `ContextBuilder::set_calibration(&mut self, factor: f64)`（`context.rs:73`）、`Built.est_input_tokens: u32`、`ModelDelta::Usage{input_tokens: u32, output_tokens: u32}`（`provider.rs:26`）、`SessionDeps.context: ContextBuilder`（**不是 `Arc`**，所以 `&mut` 可用）
- Produces: `AgentSession` 多一个私有字段 `last_est_input: u32`；`fn request(&mut self) -> ChatRequest`（**签名从 `&self` 变 `&mut self`**——这是本任务唯一的连带改动，见 Step 3 的第 4 点）

- [ ] **Step 0: 先验证四个符号，别按本计划的行号下注**

```bash
export PATH="$HOME/bin:$PATH"
cd src-tauri && grep -n "fn request\|est_input_tokens\|ModelDelta::Usage\|context: ContextBuilder" src/agent/session.rs
```

Expected: 四条都命中，且 `fn request(&self)` 目前是 `&self`。若 `request()` 已经是 `&mut self` 或 `est_input_tokens` 已被消费，停下来报告——那说明 Plan B 在飞期间有人接过了这根线。

- [ ] **Step 1: 写失败测试——真实 usage 必须回灌成系数**

```rust
#[tokio::test]
async fn real_usage_calibrates_the_estimator_in_the_right_direction() {
    // 粗估 1000，端点真实回报 500 ⇒ factor = 真实 ÷ 粗估 = 0.5（context.rs:65 的语义）。
    // 接反成 粗估 ÷ 真实 = 2.0 不会报错，但估算会一直朝危险方向偏 —— 这条测试存在的唯一理由。
    let (est, actual) = (1000u32, 500u32);
    let mut s = session_with_estimator_est(est, vec![ModelDelta::Text("好".into()),
        ModelDelta::Usage { input_tokens: actual, output_tokens: 7 }]).await_run().await;
    // 校准只能从 ContextBuilder 侧观测：给它同一份历史再 build 一次，看 est 落下来了。
    let again = s.deps.context.build(&s.history).est_input_tokens;
    assert!(again < est, "真实比粗估省 ⇒ 之后的估算必须跟着降：{again} vs {est}");
    assert_eq!(again, (est as f64 * 0.5) as u32, "系数必须是真实÷粗估");
}
```

**这条测试的辅助是新的**：`session_with_estimator_est` 需要一个「按注入的 `est` 回报」的 `ContextBuilder`。做法**不许**是给 `ContextBuilder` 加测试专用的 setter（R110：没有消费者的接口）；而是用真实的 `ContextConfig`，把历史灌到 `estimate_tokens` 落在已知值上，再从 `built.est_input_tokens` **读出来**当分母。上面那段伪代码里的 `est` 因此必须是运行时取的，不是常量：

```rust
let built = ContextBuilder::new(cfg.clone()).build(&history);
let est = built.est_input_tokens;
// …跑完带 Usage{input_tokens: est / 2} 的一轮…
let after = s.deps.context.build(&history).est_input_tokens;
assert_eq!(after, est / 2, "回报减半 ⇒ 系数 0.5");
```

- [ ] **Step 2: 写第二条失败测试——坏 usage 不许把预算清零**

```rust
#[tokio::test]
async fn a_zero_or_absurd_usage_leaves_the_budget_gate_functioning() {
    // 两种坏法都要守：(a) input_tokens=0 → factor 0 → 预算形同虚设；
    // (b) est_input_tokens=0 → 除零 → inf。二者都已被 set_calibration 的 clamp/finite 闸挡住，
    // 这里的职责是钉住「session 侧不要自己造第三道闸、也不要绕过那两道」。
    for actual in [0u32, u32::MAX] {
        let s = run_with_usage(actual).await;
        let f = s.deps.context.calibration_for_test();  // ← 见 Step 3：这个入口不存在，本步骤不新增它
    }
}
```

**裁定**：Step 2 **不新增** `calibration_for_test`。可观测面只有 `build()` 的返回，所以断言改成行为式：跑完坏 usage 的一轮后，`build(&history).est_input_tokens` 必须**仍然 > 0**，且与坏之前的值之比落在 `0.2..=3.0` 内（即 clamp 生效）。判据一句话：**坏 usage 之后预算闸还在工作**。

```rust
let before = s.deps.context.build(&history).est_input_tokens;
assert!(before > 0);
let after = /* 喂 input_tokens = 0 跑一轮后再 build */;
assert!(after > 0, "估算被清零 ⇒ 预算永不生效，正是 context.rs:69 禁止的后果");
assert!(after <= before * 3 && after >= before / 5 || after == before,
    "系数必须落在 0.2..=3.0（除零得 inf 时 set_calibration 应忽略它，保留上一个系数）");
```

- [ ] **Step 3: 跑测试确认红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml calibration`
Expected: FAIL——两条都失败在「校准后 est 与校准前相同」，因为 `set_calibration` 还没人调。红必须是断言失败，不是编译错误。

- [ ] **Step 4: 最小实现**

`AgentSession` 加字段 `last_est_input: u32`（`new()` 里初始化 0）。`request()` 改成 `&mut self`，把 `built.est_input_tokens` 存进 `self.last_est_input`。`Usage` 臂改成：

```rust
Some(Ok(ModelDelta::Usage { input_tokens, output_tokens })) => {
    // R60 的取走顺序不变（assistant_acc.rs:37-39 会静默丢掉 Usage）。
    // 切片 5 在这里补上 set_calibration 的第一个调用方（前置事实 1）。
    // 分母用**发出这次请求时**的 est，不是现在的：一轮里可能有多次 request（工具循环），
    // 每 turn 的 usage 只能和它自己那次 build 对齐。
    if input_tokens > 0 && self.last_est_input > 0 {
        self.deps.context
            .set_calibration(input_tokens as f64 / self.last_est_input as f64);
    }
    self.deps.sink.emit(AgentEvent::Usage { input_tokens, output_tokens });
}
```

`&self`→`&mut self` 的连带面**只有** `request()` 的调用点；`run()` 已经是 `&mut self`，`fn request` 今天只被 `run()` 调（自己 grep 确认）。**不许**为绕开 `&mut` 而把 `last_est_input` 塞进 `Cell`/`Mutex`——那是给一个不存在的问题付代价。

- [ ] **Step 5: 跑测试绿 + 不回归**

Run: `cargo test --manifest-path src-tauri/Cargo.toml session`
Expected: PASS，且 Plan A 的 session 全套不回归（把改前/改后的 `passed` 数原样记进报告）。

- [ ] **Step 6: 变异判据（两条都要实测）**

1. 把 `set_calibration(input_tokens / last_est)` 换成 `last_est / input_tokens`（接反）⇒ Step 1 必须红（系数方向是这条测试的唯一存在理由）。
2. 删掉 `input_tokens > 0 && self.last_est_input > 0` 那道闸 ⇒ Step 2 必须红（0/0 = NaN、x/0 = inf）。
还原后按 Global Constraints 的字节同一性写法复验。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/agent/session.rs
git commit -m "feat: calibrate the token estimator from real usage so budgets stop drifting" -- src-tauri/src/agent/session.rs
git show --stat  # 必须只含这一个文件
```

---

### Task 39: 压缩预算的三个参数进 `ContextConfig` 与 `Settings`，并把 `temperature` 接上

**Files:**
- Modify: `src-tauri/src/agent/context.rs`（`ContextConfig` :20-38）
- Modify: `src-tauri/src/settings.rs`（`Settings` :243-269、`normalize` :276-、`to_context_config` :305、`to_model_config` :315、钉子测试 :1260）
- Test: 两处内联

**Interfaces:**
- Consumes: `ContextConfig::default()`（:30-38）、`Settings::to_context_config`（`settings.rs:305`）、`ChatRequest.temperature: Option<f32>`（`provider.rs:44`）、`openai_compat.rs:281-282` 已发送
- Produces:
  - `ContextConfig { reserve_output: u32, budget_tokens: u32, window_messages: usize, compact_threshold: f32, keep_turns: usize }`
  - `Settings` 新增 `temperature: f32`（默认 0.7）、`compact_threshold: f32`（默认 0.85）、`keep_turns: u32`（默认 10）
  - `SessionConfig` **不变**（`max_turns: 25` 与压缩无关，别混）

- [ ] **Step 0: 先读那两条会合谋的测试（前置事实 4）**

`serialized_field_names_are_exactly_the_eight_settings_fields`（:1260）里有**两个**断言面：键名数组（:1273-1279 的 8 项）与 `for banned in ["api_key","temperature","accent","mcp","roots"]`（:1282）。加 `temperature` 后第二个断言必红。`unknown_fields_are_ignored_and_the_file_is_not_backed_up`（约 :1318）**不会红**——它的夹具（:1326）里的 `temperature` 从此变成认识的字段，测试继续绿但不再测「不认识字段」。本任务把它的夹具换掉。

- [ ] **Step 1: 写失败测试——参数有默认值且被钳位**

```rust
#[test]
fn compaction_defaults_match_the_spec_table() {
    let cfg = ContextConfig::default();
    assert_eq!((cfg.reserve_output, cfg.budget_tokens, cfg.window_messages), (4096, 32_000, 40));
    assert_eq!(cfg.compact_threshold, 0.85, "spec 5.3：预算的 85% 触发，留余量别卡线");
    assert_eq!(cfg.keep_turns, 10, "spec 5.3：压缩时最近 10 轮留原文");
}

#[test]
fn absurd_compaction_settings_are_clamped_not_obeyed() {
    // 1.5 的阈值意味着「永不压缩」，0 的阈值意味着「每轮都压」。两者都不该照原样进内核，
    // 也不该报错——钳位是 normalize 的职责（settings.rs:276 已有那道闸的兄弟字段 font_size 12..=24）。
    let s = Settings { compact_threshold: 1.5, keep_turns: 0, temperature: 9.0, ..Default::default() };
    let n = s.normalized();
    assert!(n.compact_threshold <= 0.95, "阈值上界：留至少 5% 余量");
    assert!(n.keep_turns >= 1, "K=0 会让压缩把最近一轮也压掉 ⇒ 当场失忆");
    assert!((0.0..=2.0).contains(&n.temperature), "兼容端点对 >2 普遍报错，别把用户的锅变成 400");
}
```

- [ ] **Step 2: 写失败测试——`to_context_config` 必须把新参数带过去**

```rust
#[test]
fn settings_reach_the_context_builder_end_to_end() {
    let s = Settings { compact_threshold: 0.5, keep_turns: 3, ..Default::default() };
    let c = s.to_context_config();
    assert_eq!(c.compact_threshold, 0.5);
    assert_eq!(c.keep_turns, 3);
}

#[test]
fn settings_temperature_reaches_the_request_body() {
    // 前置事实 3：管线早就通了（openai_compat.rs:281 且 :917 钉着），缺的只有 session.rs:301 那句
    // `temperature: None`。这条测试钉的是**最后一段线**，不是重测 openai_compat。
    let s = Settings { temperature: 0.3, ..Default::default() };
    let mc = s.to_model_config("k");
    assert_eq!(mc.temperature, Some(0.3));
}
```

`ModelConfig` 是否已有 `temperature` 字段——**自己 grep 确认**；若没有，本步骤把它加进 `ModelConfig` 并在 `to_model_config`（:315）填上，`openai_compat` 那侧不用动。

- [ ] **Step 3: 跑红**

Run: `cargo test --manifest-path src-tauri/Cargo.toml context settings`
Expected: FAIL（新字段不存在 ⇒ 编译错，这是允许的第一种红；加完字段后必须再跑一次，确认红的是断言而不是缺字段）。

- [ ] **Step 4: 实现——字段、`Default`、钳位、映射四处在同一提交里**

1. `ContextConfig` 加两字段（`#[derive(Debug, Clone)]` 已有，`Default` :30-38 必须同步给值——**漏 `Default` 是这类任务最常见的静默回归**，因为所有 `..Default::default()` 夹具会拿到 0）。
2. `Settings` 加三字段，serde 键名 snake_case 与字段同名（沿用现有风格，无 `rename_all`）。
3. `normalize`（:276）里钳位：`compact_threshold` `0.05..=0.95`、`keep_turns` `1..=50`、`temperature` `0.0..=2.0`。**非有限值**（NaN/inf）一律回落默认——照 `set_calibration` 的先例（`context.rs:73-76`），因为 `f32::clamp` 对 NaN 返回 NaN，而 NaN 阈值会让「该不该压」的判定永远为假且静默。
4. `to_context_config`（:305）带上两项；`to_model_config`（:315）带上 `temperature`。
5. `session.rs` 的 `request()` 里 `temperature: None` → 来自 `ModelConfig` 的值（这里要确认 `AgentSession` 拿得到 `ModelConfig`；拿不到就是 `ChatRequest` 构造点缺一个入参，**别在 session 里重新读 Settings**——那会造出第二份配置真相，Settings 的读取归装配点 `ipc.rs`）。

- [ ] **Step 5: 同改两条钉子（不许只改第一条）**

- `serialized_field_names_are_exactly_the_eight_settings_fields`：键名数组补到实际项数、排序仍是 `sort_unstable` 后的字典序；`banned` 列表**只留 `"api_key"`**（它是唯一永远不该序列化的键）。函数名里的 `eight` 是承诺，改完数字不对就**改名**，别留一个说谎的名字。
- `unknown_fields_are_ignoreged_and_the_file_is_not_backed_up`：把夹具里的 `"temperature":0.7,"accent":"#ffffff","mcp":{"n":1}` 换成**永远不会被认识**的键（例如 `"quantum_mode":true,"legacy_engine":"x"`），保留它原本要测的东西：不认识字段时其余设置照原样保住、且不生成备份。

- [ ] **Step 6: 跑绿 + 变异判据**

Run: `cargo test --manifest-path src-tauri/Cargo.toml settings context`
Expected: PASS。判据两条实测：(a) 把 `normalize` 的钳位删掉 ⇒ `absurd_compaction_settings_are_clamped_not_obeyed` 红；(b) 把 `to_context_config` 里 `compact_threshold` 传成常量 ⇒ `settings_reach_the_context_builder_end_to_end` 红。另附一条**证明钉子换对了**：把夹具换回 `temperature` 且保留新字段 ⇒ 该测试仍绿（说明它没在测「不认识」），所以必须用 `quantum_mode`。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/agent/context.rs src-tauri/src/settings.rs src-tauri/src/agent/session.rs src-tauri/src/model/
git commit -m "feat: add compaction threshold, keep-turns and temperature to settings and context" -- src-tauri/src/
```

---

### Task 40: `plan_compaction` —— 纯函数决定「压哪一段」，且与 T20 的图片降级划清边界

**Files:**
- Modify: `src-tauri/src/agent/context.rs`（新增纯函数 + 内联测试）
- Test: `src-tauri/src/agent/context.rs`

**Interfaces:**
- Consumes: `ContextConfig`（T39 后含 `compact_threshold`/`keep_turns`）、`Message`/`Role`（`types.rs:82`）、`estimate_tokens`（`context.rs:146`）
- Produces:
  - `pub struct CompactionPlan { pub drop_from: usize, pub drop_to: usize, pub dropped: usize, pub source: Vec<Message> }`（下标是**对话序列**（非 system 消息）里的位置，不是 `history` 的下标——理由见 Step 3 第 1 点）
  - `pub fn plan_compaction(history: &[Message], cfg: &ContextConfig, calibration: f64) -> Option<CompactionPlan>`
  - `const MIN_COMPACT_TURNS: usize = 4;`（spec :359「最老的一半，至少 4 轮」）

- [ ] **Step 0: 边界声明（写进代码注释，别只写在报告里）**

Plan A **T20** 拥有「历史图片降级为 `[图片已移除以节省上下文]`」，落在 `ContextBuilder::build` 的返回前，函数名 `degrade_old_images`、常量 `KEEP_IMAGE_RECENT = 3`（见 `task-20-brief.md`）。本任务**不实现、不重命名、不调用**它：`plan_compaction` 只回答「哪一段该被摘要掉」。两者的唯一交集是**都读同一份 history**，而 T41 落库时摘要的是「即将被删的那些行」，其中图片行的 `blocks` 原样躺在库里（`db.rs` 不做降级）。派发前若发现 T20 还没落地，本步骤照常执行——这条边界不依赖它。

- [ ] **Step 1: 写失败测试——阈值以下绝不压**

```rust
#[test]
fn no_compaction_while_inside_the_threshold() {
    let cfg = ContextConfig { budget_tokens: 4096, reserve_output: 512, ..Default::default() };
    let hist = user_assistant_pairs(3); // 短历史
    assert!(plan_compaction(&hist, &cfg, 1.0).is_none(),
        "预算 3584 × 0.85 = 3046，远没到 ⇒ 一次都不该压，压了就是在无谓烧一次模型调用");
}

#[test]
fn the_threshold_leaves_headroom_so_we_never_land_exactly_on_the_wire() {
    // 断言 est 落在 (0.85, 1.0] × 预算时**触发**，而 ≤ 0.85 时不触发。
    // 这条测的是 spec :357「超过即压缩，留出余量避免刚好卡线」这个理由本身。
}
```

- [ ] **Step 2: 写失败测试——批量与保留的算术**

```rust
#[test]
fn compaction_takes_the_oldest_half_but_at_least_four_turns_and_keeps_k_verbatim() {
    let cfg = ContextConfig { budget_tokens: 1024, reserve_output: 0, keep_turns: 10, ..Default::default() };
    let hist = user_assistant_pairs(40);
    let p = plan_compaction(&hist, &cfg, 1.0).expect("远超阈值");
    assert_eq!(p.dropped, 20, "最老的一半：40 轮压掉 20 轮，不是「压到刚好卡线」——否则每轮都要重压一次");
    assert!(p.drop_to <= 40 - 10, "最近 K=10 轮必须留在原文里");
    assert_eq!(p.source.len(), p.dropped, "source 就是要交给模型摘要的那些消息，不多不少");
}

#[test]
fn a_two_turn_session_is_not_worth_compacting_even_over_budget() {
    // MIN_COMPACT_TURNS=4 的正面：只有 2 轮却超预算时，压它只会把「唯一的历史」变成一句摘要，
    // 而下一轮照样超预算（超的是本轮那条长输入）。此时该让 build() 的滑窗去兜底。
    let hist = user_assistant_pairs(2);
    assert!(plan_compaction(&hist, &long_budget_cfg(), 1.0).is_none());
}

#[test]
fn the_cut_never_lands_between_an_assistant_tool_call_and_its_result() {
    // context.rs:118 的 advance 在滑窗侧守的就是这条；摘要侧同样不能把
    // assistant(ToolCall) 收进 source 而把它的 ToolResult 留在 tail —— 那正是 spec :364 的 400。
    let hist = pairs_with_a_tool_round_in_the_middle();
    let p = plan_compaction(&hist, &cfg_at_threshold(), 1.0).unwrap();
    let conv: Vec<&Message> = hist.iter().filter(|m| m.role != Role::System).collect();
    assert!(!has_tool_result(conv[p.drop_to]),
        "切口后第一条不许是孤儿工具结果（has_tool_result 已在 context.rs:192，别新造一个）");
}
```

**夹具命名说明**：`user_assistant_pairs(n)`、`pairs_with_a_tool_round_in_the_middle()`、`long_budget_cfg()`、`cfg_at_threshold()` 都是**本任务在 `mod tests` 里自己写的局部夹具**，不是已有设施。`cfg_at_threshold()` 的存在理由：`the_cut_never…` 要在「刚好会触发压缩」的那一档取数，写成常量就会随 `ContextConfig::default()` 漂移。判定孤儿工具结果一律用已有的 `has_tool_result`（`context.rs:192`），**不许**为测试再造第二个谓词。

- [ ] **Step 3: 实现——`plan_compaction` 是纯函数，不碰模型、不碰 IO**

1. **先把 `history` 分成 `notes`（`Role::System`）与 `conv`（其余）**，与 `build()`（`context.rs:89-97`）同一套划分；返回的下标一律相对 `conv`，因为 `db.rs` 侧要按「第几条对话消息」删（T41 给 `Db::compact_session` 的入参就是这个下标区间 + 事务里按 id 定位）。
2. 触发判定：用 `(estimate_tokens(&[system 一条 + notes + conv 全部]) as f64 * calibration) as usize` 与 `((budget_tokens - reserve_output) as usize)` 的 `compact_threshold` 倍比较。**`saturating_sub`** 照 `context.rs:100-103` 的写法（`budget < reserve` 时预算是 0，不是下溢）。
3. 批量：`dropped = (conv 轮数 / 2).max(MIN_COMPACT_TURNS)`，再 `min(轮数 - keep_turns)`（不许为负——`saturating_sub`）。轮数的定义与 `SessionConfig.max_turns` 无关，**自己选定并在注释里写明**：一次发送 = 一条 user 到下一条 user 之前，含它自己的 assistant/工具行。选定错了的症状是 K=10 实际只保 5 轮，测试 `keeps_k_verbatim` 会红。
4. `source = conv[drop_from..drop_to]`，`drop_from` 恒为 0（Plan C 只压最老一段，不做「中段摘要」——那是二期，spec 表里没它）。
5. **`calibration` 从哪来**：`ContextBuilder` 私有持有它（`context.rs:54`）。本任务给 `ContextBuilder` 加一个**有真消费者**的读口 `pub fn calibration(&self) -> f64`（T41 的 `run_turn` 要调 `plan_compaction`，那是第二个消费者；若 T41 之后仍只有它一个，就在 T41 里改为把 plan 逻辑挪成 `ContextBuilder` 的方法，别留双份）。

- [ ] **Step 4: 跑绿 + 变异判据**

Run: `cargo test --manifest-path src-tauri/Cargo.toml compaction`
Expected: PASS。判据：(a) 把 `.max(MIN_COMPACT_TURNS)` 删掉 ⇒ 两条批量测试至少一条红；(b) 把阈值比较从 `>` 改成 `>=` 并把阈值写成 1.0 ⇒ `the_threshold_leaves_headroom…` 红（证明「留余量」被测的是余量本身，不是巧合）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/agent/context.rs
git commit -m "feat: plan which turns to compact with threshold, batch and keep-turn arithmetic" -- src-tauri/src/agent/context.rs
```

---

### Task 41: 摘要压缩落地——存储侧事务 + `ContextCompacted` emit，且 `prior` 必须重取

**Files:**
- Create: `src-tauri/src/agent/summarize.rs`
- Modify: `src-tauri/src/store/db.rs`（新增 `compact_session`）
- Modify: `src-tauri/src/ipc.rs`（`run_turn` :275-330 的压缩段）
- Modify: `src-tauri/src/lib.rs`（若模块声明需要）
- Test: `src-tauri/src/agent/summarize.rs`、`src-tauri/src/store/db.rs`、`src-tauri/src/ipc.rs` 内联

**Interfaces:**
- Consumes: `CompactionPlan`（T40）、`ModelProvider::stream(&self, req: ChatRequest) -> Pin<Box<dyn Stream<Item = Result<ModelDelta, ModelError>> + Send + '_>>`（`provider.rs:87-92`）、`ChatRequest{messages, tools: Vec<ToolDef>, temperature: Option<f32>}`（`provider.rs:40-45`）、`Message::system_note(text)`（`types.rs:101`）、`Db::{open, messages, append_message}`、`AgentEvent::ContextCompacted{dropped, summary_chars}`（`events.rs:59-62`）、`events_from_messages` 的 `Role::System => {}`（`db.rs:343`，R69）
- Produces:
  - `pub const SUMMARY_PREFIX: &str = "早期对话摘要：";`
  - `pub const SUMMARY_OMITTED: &str = "[早期对话已省略]";`
  - `pub async fn collect_summary(model: &dyn ModelProvider, req: ChatRequest, cancel: &CancellationToken) -> Result<String, String>`
  - `pub fn summary_request(src: &[Message], temp: Option<f32>) -> ChatRequest`
  - `pub fn compact_session(&self, session_id: &str, plan: &CompactionPlan, note_text: &str) -> Result<(), rusqlite::Error>`

- [ ] **Step 0: 先钉住那条会炸的持久化公式（前置事实 2）**

```bash
cd src-tauri && grep -n "let prior = history.len()\|let fresh = session.history()" src/ipc.rs
```

把这两行的**当前行号与内容抄进报告**。本任务的实现必须让它们保持同一件事：`fresh` 只包含本轮新增的行。若你选择在 `run()` 里裁 `self.history`，`session.history()[prior..]` 会当场越界（`prior > len`）——**那就是本任务的红线**，也是判据 `prior_is_recounted_after_compaction` 存在的原因。

- [ ] **Step 0b: 压缩段必须抽成「不依赖 `State` 的函数」，否则它测不到（这是硬要求，不是风格）**

Plan A T13 已经实测过：`#[tauri::command]` **在单测里调不动**——`tauri::ipc::Request`/`State` 的填充点是 `CommandArg::from_command`，它要一个活的 `AppManager`。所以「在 `run_turn` 里加一段压缩逻辑」这句话的字面执行结果，是一段**没有测试的、会删用户数据的代码**。本任务规定的形状：

```rust
/// 压缩段的全部真相：读到的历史进来，压缩后的历史 + 要不要 emit 出去。
/// 入参刻意不含 `State`/`AppManager` ⇒ `cargo test` 里能用 `Db::open(tempdir)` +
/// `VecSink` + 假模型直接调它（Step 1–3 的测试全都调这里，不调 `run_turn`）。
pub(crate) async fn compact_before_turn(
    db: &Db,
    model: &dyn ModelProvider,
    cfg: &ContextConfig,
    calibration: f64,
    temperature: Option<f32>,
    history: Vec<Message>,
    cancel: &CancellationToken,
) -> Result<(Vec<Message>, Option<AgentEvent>), String>;
```

`run_turn` 只做三件事：`messages()` → `compact_before_turn(...)` → 把返回的 `Option<AgentEvent>` `emit` 出去、用返回的历史继续（`prior` 从**返回的**历史取）。**判据**：`grep -c "compact_before_turn" src/ipc.rs` ≥ 3（定义 1、命令里调 1、测试里调 ≥1），且 Step 1–3 的测试文件里**没有**一处直接调 `run_turn`。

- [ ] **Step 1: 写失败测试——摘要调用不进气泡**

```rust
#[tokio::test]
async fn the_summary_call_never_leaks_into_the_event_stream() {
    let sink = VecSink::default();
    // 关键：这一轮的历史必须远超阈值，触发压缩。
    run_turn_with(sink.clone(), long_history(), model_that_returns("早期要点：用户叫林，卡号尾 4021")).await;
    let evs = sink.events();
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::ContextCompacted { .. })), "压了就要说");
    assert!(!evs.iter().any(|e| matches!(e, AgentEvent::TextDelta { .. } if e_text(e).contains("早期要点"))),
        "摘要文本是给模型读的，不是回答；进了 TextDelta 用户就会看到一句莫名其妙的小结");
}
```

- [ ] **Step 2: 写失败测试——库里真的留下覆盖被丢区间的摘要，且不重复**

```rust
#[tokio::test]
async fn the_summary_row_is_written_once_and_covers_the_dropped_range() {
    let db = Db::open(&tempfile::tempdir().unwrap().path().join("s.db")).unwrap();
    let sid = db.create_session("t").unwrap();
    for m in long_history() { db.append_message(&sid, &m).unwrap(); }
    let before = db.messages(&sid).unwrap().len();
    compact_once(&db, &sid).await;
    let after = db.messages(&sid).unwrap();
    let notes: Vec<&Message> = after.iter().filter(|m| m.role == Role::System).collect();
    assert_eq!(notes.len(), 1, "每次压缩一条备注；两条备注意味着同一段历史被摘要了两遍");
    assert!(notes[0].text_of().starts_with(SUMMARY_PREFIX));
    assert!(after.len() < before, "被压掉的行必须真的没了，否则下一轮还是全量读");
    // §9 那句「存在覆盖被丢弃区间的摘要消息」的可测形式：
    assert!(db.messages(&sid).unwrap().iter()
        .any(|m| m.role == Role::User && m.text_of().contains("轮 1")),
        "第 1 轮的话要么在原文里、要么必须在摘要覆盖范围内 —— 这里断被保留侧，另一侧由下一条测试管");
}
```

- [ ] **Step 3: 写失败测试——摘要失败降级成占位，而不是报错**

```rust
#[tokio::test]
async fn a_failing_summary_still_frees_space_and_says_so() {
    // 模型调用报错（429、断流、取消都算）。spec :365：降级为直接丢弃 + [早期对话已省略]。
    let got = collect_summary(&FailingModel, summary_request(&src, None), &never_cancelled()).await;
    assert!(got.is_err());
    // 落库侧由 compact_session(.., SUMMARY_OMITTED) 走同一条事务 —— 不许有第二条「丢弃」路径。
    let after = run_turn_with(ErroringModel, long_history()).await.then_load_db();
    assert!(after.iter().any(|m| m.role == Role::System && m.text_of().contains(SUMMARY_OMITTED)));
    assert!(after.len() < long_history().len(), "摘要失败也不能把该删的留着撑爆下一轮");
}

#[tokio::test]
async fn cancelling_mid_compaction_leaves_a_consistent_store() {
    // 取消发生在「模型还在写摘要」时：事务还没开始，所以库必须一字未改（原子性由 T41 的事务给）。
    let untouched = run_with_cancelled_token().await.then_load_db();
    assert_eq!(untouched.len(), 原始条数, "取消不许留下半条摘要或已经删掉的行");
}
```

- [ ] **Step 4: 实现 `summarize.rs`**

```rust
/// 摘要调用用的提示词。**独立于 SYSTEM_PROMPT**（`context.rs:11`）：那条教模型「别编造、可用工具」，
/// 而这里要的是相反的行为——只压缩已给内容，不补充、不调工具。
pub const SUMMARY_INSTRUCTION: &str = "把下面的对话历史压缩成不超过 400 字的要点备忘。只保留事实（人名、数字、结论、未完成的请求），\
不要评价、不要补充对话里没有的内容。用中文。";

pub fn summary_request(src: &[Message], temperature: Option<f32>) -> ChatRequest {
    let mut msgs = vec![Message::system_note(SUMMARY_INSTRUCTION)];
    msgs.extend(src.iter().cloned());
    ChatRequest {
        messages: msgs,
        // 空工具声明 = 模型不会去调工具（R43 的教训反过来用：未声明的工具它不可能调用）。
        tools: vec![],
        temperature,
    }
}

/// 收流。只认 `ModelDelta::Text`；`ToolCall*` 与 `Usage` 一律忽略（摘要不需要工具，
/// 也不该拿它的 usage 去校准主对话的系数——那不是同一批 token）。
pub async fn collect_summary(
    model: &dyn ModelProvider,
    req: ChatRequest,
    cancel: &CancellationToken,
) -> Result<String, String> {
    // tokio::select! { biased; _ = cancel.cancelled() => Err("压缩已取消"), … }
    // 取消语义照 session.rs:144-150 那套：biased 让取消优先，且取消时**不**返回半截文本当成功。
    // 实现细节由 TDD 的红驱动，这里规定的只有合同：
    //   1) Ok(空串) 是不可能状态——空摘要必须转成 Err，否则会落一条零信息备注并把原文删掉。
    //   2) Err 的字符串是给用户看的（进 notice），要能判断「该重试还是该改设置」。
}
```

（`collect_summary` 的循环体照 `session.rs:144-170` 已验证的形状写：`while` + `select!` + `deltas.next()`。**不许**为它引 `AssistantAcc`——那个累加器的职责是 tool_call 分片，用它只会让摘要文本的来源多一条路径。）

- [ ] **Step 5: 实现 `Db::compact_session`（一个事务，三件事）**

```rust
/// 删除 `conv` 下标区间 `[from, to)` 对应的行，并在**最前**插入一条摘要备注。
/// 一个事务：删除 + 插入要么都成、要么都不做（Step 3 的取消测试靠这条成立）。
/// `conv` 下标到 `id` 的映射：`SELECT id FROM messages WHERE session_id=? AND role<>'system' ORDER BY id`
/// —— 顺序即 `Db::messages` 的顺序（db.rs:206-218 的 `ORDER BY id` 是这条映射的唯一根据，
/// 别用 LIMIT/OFFSET：SQLite 的 OFFSET 在 role 过滤下与 `messages()` 的切片不对齐）。
pub fn compact_session(&self, session_id: &str, plan: &CompactionPlan, note_text: &str) -> Result<(), rusqlite::Error>
```

实现约束：`rusqlite::Transaction`（`self.conn` 已有 `&self` 方法，沿用 `open` 的连接；若今天连接是 `Mutex<Connection>` 就取锁后开事务——**自己读 `db.rs:54-70` 确认**，别假设）。插入走已有 `append_message` 的 SQL，不新写一条 `INSERT`（两条 INSERT 语句就是两份「一行消息长什么样」的真相，`blocks_json` 编码必须只有一个来源）。

- [ ] **Step 6: 实现 `ipc.rs` 的压缩段——顺序是验收项**

`run_turn` 里，在 `let history = reader.messages(&sid)` 之后、`session.restore(history)` 之前插：

```
1. plan = plan_compaction(&history, &cfg, calibration)      // 纯函数，不碰模型
2. if let Some(p) = plan:
     a. summary = collect_summary(...).await                 // 可取消
     b. text = summary.unwrap_or(SUMMARY_OMITTED.to_string()) 加/不加 SUMMARY_PREFIX（见 Step 7）
     c. db.compact_session(&sid, &p, &text)                  // 事务
     d. history = reader.messages(&sid)                      // **重读**
3. let prior = history.len();                                // 压缩之后才取
4. session.restore(history); session.run(blocks).await
5. append_rows(&history[prior..]) 前，先 sink.emit(ContextCompacted{dropped: p.dropped, summary_chars: text.chars().count()})
```

第 3 点必须**在压缩后**取 `prior`。判据测试 `prior_is_recounted_after_compaction`：构造「压缩删了 20 行、本轮新增 2 行」，断言结束后**总行数 = 压缩后行数 + 2**，且最后两行是本轮的（不是被删过的旧行）。少了这条，`prior` 停在压缩前会让 `fresh` 少附甚至负向切片 panic。

- [ ] **Step 7: 两条边界，写死在实现里**

1. **`SUMMARY_PREFIX` 只加一次**：备注文本落库时是 `早期对话摘要：<摘要>`；`build()` 侧（`context.rs:89-94`）把 System 行当 `notes` 原样带上，所以**不许**在 `build` 里再拼一次前缀（那会双重前缀）。降级路径的 `[早期对话已省略]` **也带前缀**——因为「有一条被压过的痕迹」是 spec :365 的意图，而带前缀让第二次压缩能把它一起并进新摘要。
2. **`summary_chars` 是字符数不是字节数**：`text.chars().count()`。前端 `reducer.ts:267-270` 的文案是「摘要 {n} 字」，用 `len()` 会让中文显示成 3 倍字数（这条已有前车之鉴，别重演）。

- [ ] **Step 8: 跑红 → 实现 → 绿 + 变异判据**

Run: `cargo test --manifest-path src-tauri/Cargo.toml compact summar`
Expected: 新增四到六条 PASS；Plan A 的 session/db/ipc 全套不回归。
判据（至少实测三条）：(a) 把 `prior` 放回压缩前 ⇒ `prior_is_recounted_after_compaction` 红；(b) 把 `Role::System` 的跳过（`db.rs:343`）改成产出事件 ⇒ 用户会看到摘要气泡，`the_summary_call_never_leaks_into_the_event_stream` 红；(c) 把 `compact_session` 拆成两个非事务调用 ⇒ 取消测试红。

- [ ] **Step 9: 提交**

```bash
git add src-tauri/src/agent/summarize.rs src-tauri/src/store/db.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat: compact the oldest turns into a stored summary between turns" -- src-tauri/src/
```

---

### Task 42: 长会话压测（§9「长对话不失忆、不 400」的确定性那一半）

**Files:**
- Test: `src-tauri/tests/long_session.rs`（新建集成测试，无 Tauri、无网络）
- Create: `docs/findings/plan-c-long-session.md`

**Interfaces:**
- Consumes: `Db`、`ContextBuilder`、`plan_compaction`、`compact_session`、`estimate_tokens`（全部真实实现）+ 一个**假模型**（吐固定文本 + 一条 `Usage{input_tokens: est, output_tokens: 1}`）

**不做**：本任务不测「第 1 轮告知一个事实、第 100 轮问出来」——那是人工那一半，归 T48。也不做历史读取分页（`messages()` 今天无 LIMIT，前置事实 7；分页在 spec 里没有要求，加它就是范围蔓延，本任务只**量**它）。

- [ ] **Step 1: 写这个测试（它今天一定过不了，因为压缩尚未存在时它应当以「超预算」失败）**

```rust
// tests/long_session.rs
#[tokio::test]
async fn one_hundred_rounds_stay_inside_the_budget_and_keep_a_summary() {
    let cfg = ContextConfig { budget_tokens: 4096, reserve_output: 512, ..Default::default() };
    let db = seed_100_rounds();          // 每轮 user 60 字 + assistant 200 字，含 3 张图
    let mut compactions = 0usize;
    for round in 1..=100 {
        let hist = db.messages(&sid).unwrap();
        if let Some(p) = plan_compaction(&hist, &cfg, 1.0) {
            db.compact_session(&sid, &p, &format!("{SUMMARY_PREFIX}第 {round} 轮前的要点")).unwrap();
            compactions += 1;
        }
        let built = ContextBuilder::new(cfg.clone()).build(&db.messages(&sid).unwrap());
        assert!(built.est_input_tokens <= cfg.budget_tokens - cfg.reserve_output,
            "第 {round} 轮请求就会顶到预算外 ⇒ 端点 400，正是 spec :364 点名的难排查故障");
        assert!(built.messages.iter().any(|m| m.role == Role::System && m.text_of().starts_with(SUMMARY_PREFIX))
            || built.dropped == 0,
            "丢了消息却没有覆盖被丢区间的摘要 = 静默失忆，§9 那一条判的就是这个");
    }
    assert!(compactions >= 1, "100 轮一次都没压，说明阈值或批量算错了");
    record_baselines(&db, compactions);  // 见 Step 2：把真实条数/字节写进 findings
}
```

- [ ] **Step 2: 跑测试并把真实数字落盘**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test long_session`
Expected: PASS。然后在 `docs/findings/plan-c-long-session.md` 记：**压缩次数、每次删掉的行数、库内行数峰值、重放一次读出的条数与字节**（前置事实 7 要的「100 轮时全量读到底多大」）、`est_input_tokens` 曲线首尾值。这份 findings 是 T48 人工验收的对照表，数字没跑出来就标 `未实测`。

- [ ] **Step 3: 一条关于 400 的额外守卫测试**

```rust
#[test]
fn no_orphan_tool_result_at_either_end_of_the_request() {
    // 压缩切口 + 滑窗切口**都**生效后，最终 messages 里每条 role=tool 语义的行
    // 前面必须有一条带同 id 的 assistant(ToolCall)。T41 的切口约束与 context.rs:118 的 advance
    // 各守一半，两条一起踩才安全 —— 这条测的是「两条都在时」。
}
```

- [ ] **Step 4: 提交**

```bash
git add src-tauri/tests/long_session.rs docs/findings/plan-c-long-session.md
git commit -m "test: prove a 100-round chat stays inside the budget with a covering summary" -- src-tauri/tests/long_session.rs docs/findings/plan-c-long-session.md
```

---

## 切片 6 — 设置与交付：把已经存在的后端接到用户手上，并用数字关掉三条从未测过的线

### Task 43: `Settings.accent` —— 字段、校验与那两条钉子（再一次）

**Files:**
- Modify: `src-tauri/src/settings.rs`（`Settings` :243-269、`normalize` :276、钉子 :1260）
- Modify: `src/types.ts`（`Settings` :109-118、`SettingsView` :128-131）
- Modify: `src/ipc/commands.test.ts`（:57 与 :69 两处 `Settings` 夹具）
- Test: `src-tauri/src/settings.rs` 内联

**Interfaces:**
- Consumes: T39 改过的 `serialized_field_names_are_exactly_the_eight_settings_fields`（键数已不是 8，函数名在 T39 已改名——**先读当前名字**）
- Produces: `Settings.accent: String`（`""` = 跟随预设），`pub fn normalize_accent(s: &str) -> String`（只放行 `#rgb`/`#rrggbb`，输出小写 6 位；其余返回 `""`）

- [ ] **Step 1: 写失败测试——只放行两种 hex，且规范化**

```rust
#[test]
fn accent_accepts_only_the_two_hex_forms_and_lowercases_them() {
    assert_eq!(normalize_accent("#0aF"), "#00aaff",
        "3 位必须展开成 6 位：CSS 自己会展开，但前端要做亮度判定，得拿同一个形状");
    assert_eq!(normalize_accent("#AA00FF"), "#aa00ff");
    for bad in ["", "red", "#ff00", "not-a-color", "#aaaaaaa", "aa00ff"] {
        assert_eq!(normalize_accent(bad), "", "{bad} 不是可信的强调色 ⇒ 回落预设，而不是原样写进 CSS 变量");
    }
}

#[test]
fn an_unparseable_accent_on_disk_falls_back_without_marking_the_file_corrupt() {
    // settings.json 是用户可编辑的文件。写坏了 accent 不该让整个设置被备份成 corrupt-<ms> 并回落默认
    // （那会把用户别的设置一起丢掉）—— 这是 R84/R91 边界的一条延伸，必须单独钉。
    let d = tempfile::tempdir().unwrap();
    let store = SettingsStore::new(d.path());
    std::fs::write(store.path(), r#"{"model":"qwen-turbo","accent":"tomato"}"#).unwrap();
    let got = store.load().unwrap();
    assert_eq!(got.model, "qwen-turbo", "其余设置照原样保住");
    assert_eq!(got.accent, "", "认不出的 accent 回落『跟随预设』");
    assert!(corrupt_backups(d.path()).is_empty(), "单个字段可修复不是文件损坏");
}
```

- [ ] **Step 2: 跑红 → 实现**

`accent: String` 进 `Settings` 与 `Default`（默认 `String::new()`）；`normalize` 里过 `normalize_accent`；`load()` 侧同样过一遍——**磁盘值不可信**：如果规范化只在 `normalized()`/`save` 前跑，读侧就会带脏值出去。自己读 `load`（:432）确认它今天是否调 `normalize`，没调就在这里调，并在报告里写明为什么。

- [ ] **Step 3: 同改钉子的两个面**

`banned` 列表此刻含 `"accent"`（:1282）——删掉它（它现在是被支持的键），只留 `"api_key"`；键名数组加一项。TS 侧 `Settings` 接口加 `accent: string`，`commands.test.ts` 两处夹具加 `accent: ""`（少一处就是 `npx tsc --noEmit` 红，而那会打断 `npm run build`）。

- [ ] **Step 4: 变异判据**

(a) `normalize_accent` 改成「非空就原样返回」⇒ 第一条测试红（`"tomato"` 会被当颜色）。(b) 把 `load()` 里的规范化去掉 ⇒ `an_unparseable_accent_on_disk…` 红。(c) `banned` 留着 `"accent"` 不改 ⇒ 钉子红（**这条红是有用的**：它证明钉子真的在看序列化结果，不是摆设）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/settings.rs src/types.ts src/ipc/commands.test.ts
git commit -m "feat: add a validated accent setting that falls back to the preset" -- src-tauri/src/settings.rs src/types.ts src/ipc/commands.test.ts
```

---

### Task 44: 强调色落地——`accentTriplet` 纯函数、`applyTheme` 写三件套、冷启动不再有主题真空

**Files:**
- Create: `src/theme/accent.ts`、`src/theme/accent.test.ts`
- Create: `src/theme/cache.ts`（+ 内联测试）、Modify: `src/main.tsx`（首帧前同步读缓存）、Modify: `src/App.tsx`（`:94` 的 `applyTheme` 调用点传 accent）
- Modify: `src/theme/theme.ts`（`USER_TOKENS` :30、`applyTheme` :32-47）、`src/theme/tokens.test.ts`
- Test: `src/App.test.tsx` 追加一条（十三条原样绿）

**Interfaces:**
- Consumes: `THEME_TOKENS`（15 项，含 `--accent`/`--accent-hover`/`--accent-text`，`theme.ts:10-26`）、`USER_TOKENS = ["--font-size"]`（:30）、`applyTheme(el, theme, mode, fontSize)`、`resolveTheme`（:52-60）、`Settings.accent`（T43）
- Produces:
  - `normalizeHex = (raw: string): string | null`（`null` = 跟随预设）
  - `accentTriplet = (hex: string): { accent: string; hover: string; text: string }`
  - `relativeLuminance = (hex: string): number`（`accentTriplet` 的内部判据，测试要读它 ⇒ 必须导出；不导出就是「测试只能靠猜实现」的形状）
  - `applyTheme(el, theme, mode, fontSize, accent: string)`（**第 5 个入参**，`""` = 清掉三个 inline 变量）
  - `USER_TOKENS = ["--font-size", "--accent", "--accent-hover", "--accent-text"]`

**两条连带面，同一次提交里一起做完，否则 `npx tsc --noEmit` 直接红：**
`grep -rn "applyTheme(" src/` 会命中 `App.tsx:94` 与既有测试里的调用——**每一个调用点都要补第 5 个实参**（App 传 `settings.accent`，测试传 `""`）。别把第 5 个入参做成可选：可选意味着漏传是合法的，而漏传的症状正是「强调色设了没反应」。

**与 T43 的 `normalize_accent` 是两份校验，这是决定不是重复**：Rust 那份是权威（`settings.json` 用户可手改，内核不能信前端），TS 那份是因为前端**本来就要解析 hex** 才能算亮度——不解析就没法产出三件套。两份的规则必须一致（只放行 `#rgb`/`#rrggbb`、输出小写 6 位），T44 的 `rejects anything that is not #rgb or #rrggbb` 与 T43 的 `accent_accepts_only_the_two_hex_forms_and_lowercases_them` 就是「规则一致」这件事的两个钉子；两边用例表要同款（`"tomato"`、`"#ff00"`、`"#gggggg"`、`"  #aabbcc "`、`"#0aF"→"#00aaff"`）。

- [ ] **Step 0: 先读 `tokens.test.ts` 对 `USER_TOKENS` 的断言**

它今天断「每个预设覆盖 `THEME_TOKENS` 的 15 项、一个不漏」（R124），另有规则说 `USER_TOKENS` 只在 `tokens.css` 的 `:root` 兜底、**不进预设块**（`theme.ts:28-29` 的注释）。把 `--accent*` 加进 `USER_TOKENS` 会同时碰这两条：**预设块仍然必须定义它们**（那是默认值来源），`USER_TOKENS` 的含义变成「`applyTheme` 可能在 inline 上覆盖的项」。测试怎么改由这条新语义决定——先读，再改，别照抄本任务的描述。

- [ ] **Step 1: 写失败测试——单一取色必须展开成可读的三件套**

```ts
import { accentTriplet, normalizeHex } from "./accent";

it("keeps text legible on the picked accent instead of trusting the preset", () => {
  const { accent, hover, text } = accentTriplet("#aa00ff");
  expect(accent).toBe("#aa00ff");
  expect(hover).not.toBe(accent);               // hover 与静止同色 = 看不出可点（R168 治过的同一个病）
  expect(text).toBe("#ffffff");                 // 深底必须白字
  expect(accentTriplet("#ffff00").text).toBe("#000000");  // 亮底必须黑字
});

it("moves hover toward the press, not toward a random shade", () => {
  // 判据由实现给一个常量 MIN_LUMA_DELTA（建议 0.06），方向由底色亮度决定：
  // 亮蓝 #2080ff 的 hover 往深走（「按下 = 更深」的习惯），深色底的 hover 往浅走。
  const { accent, hover } = accentTriplet("#2080ff");
  expect(relativeLuminance(hover)).toBeLessThan(relativeLuminance(accent));
});

it("rejects anything that is not #rgb or #rrggbb", () => {
  for (const bad of ["tomato", "", "#ff00", "  #aabbcc ", "#gggggg"]) {
    expect(normalizeHex(bad)).toBeNull();
  }
  expect(normalizeHex("#ABC")).toBe("#aabbcc");
});
```

**为什么必须有这条，而不是让 `<input type="color">` 直接写 `--accent`：** 预设里 `--accent-hover`（`tokens.css:8`）与 `--accent-text`（:9）是**跟预设的 accent 配套**的色。用户换掉 `--accent` 而另两项留在旧值上，症状是「按钮底色变亮黄、文字还是白」——不可读，而 `lint:css` 抓不到它（那不是硬编码色）。三件套由一个纯函数产出，就是把这条不可读挡在唯一出口上。

- [ ] **Step 2: 写失败测试——`applyTheme` 拥有三个 inline 变量，清除能回到预设**

```ts
it("writes the triplet inline and removes all three when the setting is empty", () => {
  const el = document.createElement("div");
  applyTheme(el, "noir", "dark", 15, "#aa00ff");
  expect(el.style.getPropertyValue("--accent")).toBe("#aa00ff");
  expect(el.style.getPropertyValue("--accent-hover")).toBeTruthy();
  expect(el.style.getPropertyValue("--accent-text")).toBe("#ffffff");
  applyTheme(el, "noir", "dark", 15, "");
  for (const t of ["--accent", "--accent-hover", "--accent-text"]) {
    expect(el.style.getPropertyValue(t)).toBe("",
      `${t} 必须被 removeProperty：残留会让「跟随预设」变成「沿用上次挑的色」`);
  }
});
```

inline 样式优先于 `[data-theme]` 块，这是 CSS 的既有保证，**不许**用 `!important`、也不许把动态值写进 `presets.css`（那是预设的唯一定居地）。

- [ ] **Step 3: 写失败测试——冷启动不再闪（前置事实 12）**

```ts
import { seedFromCache } from "./theme/cache";

it("seeds the theme synchronously from a cache so the first paint is not the default", () => {
  localStorage.setItem("qwen-do.theme", JSON.stringify({ theme: "ocean", mode: "light", fontSize: 17, accent: "" }));
  const el = document.createElement("html");
  seedFromCache(el);
  expect(el.getAttribute("data-theme")).toBe("ocean");
  expect(el.style.getPropertyValue("--font-size")).toBe("17px");
});

it("ignores a corrupt cache instead of throwing during startup", () => {
  localStorage.setItem("qwen-do.theme", "{not json");
  expect(() => seedFromCache(document.createElement("html"))).not.toThrow();
});

it("refuses to seed an unknown theme name from the cache", () => {
  localStorage.setItem("qwen-do.theme", JSON.stringify({ theme: "matrix", mode: "light", fontSize: 15, accent: "" }));
  const el = document.createElement("html");
  seedFromCache(el);
  expect(el.getAttribute("data-theme")).toBeNull(), "名单校验只有 resolveTheme 这一份（R125），缓存侧不许再造一份"
});
```

裁定：这是**缓存**不是第二份真相——`writeThemeCache` 只在 `applyTheme` 成功后写；`settings_get` 回来后无条件覆盖。写失败（隐私模式、配额）不许影响启动，所以吞异常，并在注释里写明「吞掉它是设计而不是偷懒：首帧优化的失败模式应该是『看到默认主题』，不是白屏」。第三条测试守的是「缓存里躺着未来版本/手改的主题名」时不要把 `data-theme="matrix"` 写上去（那会让所有预设块都不匹配，比默认更糟）。

- [ ] **Step 4: 跑测试 + 变异判据**

`npx vitest run`（先跑再改，基线以实际输出为准）、`npx tsc --noEmit`、`npm run lint:css`。判据至少实测三条：(a) `accentTriplet` 让 `hover === accent` ⇒ Step 1 第一条红；(b) `applyTheme` 里把 `removeProperty` 改成什么都不做 ⇒ Step 2 红；(c) 亮度阈值方向反转 ⇒ `#ffffff` 那条红。

- [ ] **Step 5: 提交**

```bash
git add src/theme/ src/main.tsx src/App.tsx
git commit -m "feat: derive a legible accent triplet and seed the theme before first paint" -- src/theme/ src/main.tsx src/App.tsx
```

---

### Task 45: 设置面板——`setSettings` 与 `setApiKey` 的第一个消费者

**Files:**
- Create: `src/components/SettingsPanel.tsx`、`src/components/SettingsPanel.test.tsx`
- Modify: `src/App.tsx`（面板开关）、`src/styles.css`（面板样式，全 token）、`src/components/Sidebar.tsx`（开关按钮放这里还是放 App 顶部由你选定并说明）
- **不动**：`src-tauri/**`（命令面已够：`settings_get` / `settings_set` / `settings_set_api_key` 三条在 `ipc.rs:505/523/531` 且已登记）

**Interfaces:**
- Consumes: `SettingsView`（`types.ts:128-131`：`#[serde(flatten)]` 后是**扁平一层** + `api_key_masked: string | null` + `corrupt_backup: string | null`）、`commands.ts` 的平铺导出 `getSettings`(:60) / `setSettings`(:62) / `setApiKey`(:65)（**没有 `api` 对象**，R206；`App.tsx` 是 namespace import，所以新导出自动可见）、`resolveTheme`/`applyTheme`（T44 后含 accent 入参）
- Produces: `SettingsPanel` props `{ initial: SettingsView; onSave: (s: Settings) => Promise<void>; onSaveKey: (k: string) => Promise<void>; onClearKey: () => Promise<void>; onClose: () => void }`

- [ ] **Step 0: 先确定五组里哪些字段真的存在（别渲染存不进去的设置）**

```bash
cd src-tauri && grep -n "pub struct Settings" -A 22 src/settings.rs
cd .. && grep -n "export interface Settings" -A 18 src/types.ts
grep -rn "mcp" src-tauri/src/ipc.rs | head
grep -n "pub roots" src-tauri/src/settings.rs
```

**组与来源的对照（本任务唯一的合法字段表）：**

| 组 | 字段 | 来源 |
|---|---|---|
| 模型 | `base_url`、`model`、`reserve_output`（文案「输出预留（上限）」，见「刻意偏离」1）、`temperature` | Plan A + T39 |
| api_key | 只有掩码显示 + 写入框，**没有读回** | `api_key_masked`（`ipc.rs:496`） |
| 上下文 | `budget_tokens`、`window_messages`、`compact_threshold`、`keep_turns` | Plan A + T39 |
| MCP | 以 Step 0 的 grep 为准 | Plan B T29/T35 |
| 工具 | `roots` | Plan B T26 |
| 界面 | `theme`、`mode`、`accent`、`font_size` | Plan A + T43 |

**MCP 组与 roots 组在 Plan B 未落地时整组不渲染**——先渲染一个存不进去的输入框会造出「改了但没生效」的用户可见 bug，比缺一个组更糟。报告里写明你 grep 到的实际状态。

- [ ] **Step 1: 写失败测试——保存是整表覆盖，未编辑的字段不许变**

测试里用到的 `view(partial)` 是**本任务的测试文件里自己写的局部夹具**（不是已有设施，也不许放进生产代码）：

```tsx
// 一份完整的 SettingsView，按派发时 `types.ts` 的真实字段填默认值，partial 只覆盖被这条测试关心的那几项。
// 「完整」是重点：写成 `as SettingsView` 的断言转换会让面板少读一个字段也照样绿。
const view = (p: Partial<SettingsView> = {}): SettingsView => ({
  base_url: "", model: "qwen-plus", theme: "noir", mode: "dark", font_size: 15, accent: "",
  reserve_output: 4096, budget_tokens: 32_000, window_messages: 40, compact_threshold: 0.85, keep_turns: 10,
  temperature: 0.7, api_key_masked: null, corrupt_backup: null, ...p,
});
```

字段清单以**派发时**的 `src/types.ts` 为准（T39/T43 之后是 10+ 项，Plan B 若已落 `roots` 还要多一项）；缺项会让 TS 直接报错——那是本夹具唯一有用的报错方式，别用 `Partial`/`as` 绕过。

```tsx
it("saves the whole settings table with only the edited field changed", async () => {
  const onSave = vi.fn(async () => {});
  render(<SettingsPanel initial={view({ model: "qwen-plus", budget_tokens: 32000, temperature: 0.7 })}
    onSave={onSave} onSaveKey={vi.fn()} onClearKey={vi.fn()} onClose={vi.fn()} />);
  fireEvent.change(screen.getByLabelText(/模型名/), { target: { value: "qwen-turbo" } });
  fireEvent.click(screen.getByRole("button", { name: /保存/ }));
  expect(onSave).toHaveBeenCalledWith(expect.objectContaining({
    model: "qwen-turbo", budget_tokens: 32000, temperature: 0.7,  // 没动的两项必须原样带上
  }));
});

it("never puts the key into the settings payload", async () => {
  // spec:473 + R83 的前端一侧：面板里存在过明文 key 的那一秒，也不能让它进 setSettings 的 body。
  const onSave = vi.fn(), onSaveKey = vi.fn(async () => {});
  render(<SettingsPanel initial={view({})} onSave={onSave} onSaveKey={onSaveKey}
    onClearKey={vi.fn()} onClose={vi.fn()} />);
  const FAKE = "sk-THIS-IS-NOT-A-REAL-KEY";
  fireEvent.change(screen.getByLabelText(/API key/i), { target: { value: FAKE } });
  fireEvent.click(screen.getByRole("button", { name: /保存/ }));
  expect(JSON.stringify(onSave.mock.calls)).not.toMatch(/NOT-A-REAL-KEY/);
  expect(onSaveKey).toHaveBeenCalledWith(FAKE);
  expect((screen.getByLabelText(/API key/i) as HTMLInputElement).value).toBe("");
});
```

夹具里的 key **必须是明显假的字符串**（R164）。这条同时钉三件事：不进 settings 载荷、走专用命令、提交后本地清空。

- [ ] **Step 2: 写失败测试——掩码不等于值，留空不等于清除**

```tsx
it("shows the mask, and an untouched key field does not clear the credential", async () => {
  const onSaveKey = vi.fn(async () => {});
  render(<SettingsPanel initial={view({ api_key_masked: "sk-a…9z24" })} onSave={vi.fn()}
    onSaveKey={onSaveKey} onClearKey={vi.fn()} onClose={vi.fn()} />);
  expect(screen.getByText(/sk-a…9z24/)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /保存/ }));
  expect(onSaveKey).not.toHaveBeenCalled();
  // 输入框留空 = 不动凭据。把它当「清除」会让一次普通保存悄悄删掉 key。
});

it("clears the credential only through an explicit, confirmed action", async () => {
  // 命令侧的空串语义是清除（settings.rs R88），所以前端**不许**把空串当「没改」转发过去，
  // 清除必须是另一个按钮 + 行内二次确认（R133：不许用 window.confirm）。
  const onClearKey = vi.fn(async () => {});
  render(<SettingsPanel initial={view({ api_key_masked: "sk-a…9z24" })} onSave={vi.fn()}
    onSaveKey={vi.fn()} onClearKey={onClearKey} onClose={vi.fn()} />);
  fireEvent.click(screen.getByRole("button", { name: /清除/ }));
  expect(onClearKey).not.toHaveBeenCalled();   // 第一次点只是询问
  fireEvent.click(screen.getByRole("button", { name: /确认清除/ }));
  expect(onClearKey).toHaveBeenCalledTimes(1);
});
```

- [ ] **Step 3: 写失败测试——保存失败看得见，面板不关**

断言 `onSave` reject 时界面出现那条错误的**原文**（同 R195 口径：前端不解释错误文案），面板保持打开、已输入的值不丢。`corrupt_backup` 非 null 时必须显示「设置文件曾损坏，已备份为 …」——那是 R102 造 `take_corrupt_backup` 之后**至今没有的前端消费者**（先 grep `corrupt_backup` 在 `src/` 的实际使用情况确认；若 `App.tsx` 已在显示，本步骤改成「不许在面板里出现第二条」）。

- [ ] **Step 4: 实现**

- 表单状态：`useState<Settings>`（从 `initial` 拷一份），全受控；数字输入 `type="number"`，提交前 `Number.isFinite` 校验——NaN 进 `Settings` 会被 serde 拒收，症状是「点了保存什么也没发生」。
- 强调色：`<input type="color">` + 一个「跟随预设」复选框（勾选时提交 `""`）。`type="color"` 的 value 恒为合法 6 位 hex，所以 `normalizeHex` 只会收到合法值——但**不许**因此省掉 Rust 侧校验：磁盘文件可编辑（T43 的 `an_unparseable_accent_on_disk…` 就是为这个存在）。
- 零弹窗：禁 `window.confirm`/`prompt`/`alert`；破坏性动作用行内二次确认（按钮文案自己变成「确认清除？」）。
- 样式：全部 `var(--…)`，无 hex（R207，`npm run lint:css` rc=0）。面板打开不许引入新的 effect 依赖环——R208 那个坑在 `App.tsx`，`App.test.tsx` 十三条就是你的探测器。
- 面板打开时**不许**停掉 `App` 的事件订阅或替换 `ui` 状态：它只是一个视图，保存成功才 `applyTheme` 一次。

- [ ] **Step 5: 跑测试 + 变异判据**

`npx vitest run`、`npx tsc --noEmit`、`npm run lint:css`。判据至少实测三条：(a) 把保存改成「只发送编辑过的字段」⇒ Step 1 第一条红；(b) 把空串转发给 `onSaveKey` ⇒ Step 2 第一条红（悄悄删 key 那条路径）；(c) 保存失败时关掉面板 ⇒ Step 3 红。

- [ ] **Step 6: 提交**

```bash
git add src/components/SettingsPanel.tsx src/components/SettingsPanel.test.tsx src/App.tsx src/components/Sidebar.tsx src/styles.css
git commit -m "feat: add the settings panel as the first consumer of settings_set and settings_set_api_key" -- src/components/ src/App.tsx src/styles.css
```

---

### Task 46: NSIS 打包配置与安装包体积（15MB 那条线第一次被真的量）

**Files:**
- Modify: `src-tauri/tauri.conf.json`（`bundle` :25-34）
- Create: `docs/findings/plan-c-delivery-baseline.md`
- Modify: `docs/findings/plan-a-risk-verification.md`（**只加一行勘误**，指向新 findings：那份文档 :31 的 12,822,528 字节是 debug，:38 自己也这么写）

**Interfaces:**
- Consumes: `cargo tauri build`（`@tauri-apps/cli` 需在 devDependencies——先 `grep -n "tauri-apps/cli" package.json` 确认；没装就停下报告，本计划不加依赖）
- Produces: `bundle.targets: ["nsis"]`、`bundle.windows.nsis.installMode`、安装包真实字节数进 findings

- [ ] **Step 1: 先把 `targets: "all"` 换掉并说明为什么**

`all` 在 Windows 上会同时产 NSIS 与 MSI 两套（MSI 要下载 WiX，还会让一次构建变成两件事）。验收线只认 **NSIS 产物文件体积**（spec :518），所以只留 `["nsis"]`。改完跑一次构建，记录：产物绝对路径、`ls -l` 字节数、构建耗时。

- [ ] **Step 2: 装哪一档？写清决策与代价**

| 选项 | 代价 | 何时选 |
|---|---|---|
| `installMode: "currentUser"`（推荐） | 不需要管理员；装到 `%LOCALAPPDATA%` | 个人桌面应用，讨厌 UAC |
| `installMode: "perMachine"` | 每次安装弹 UAC；占 `Program Files` | 明确要给别人装到机器范围时 |

写进 `bundle.windows.nsis.installMode`（**键名自己对照 Tauri v2 的 schema 验证**：`src-tauri/tauri_conf_schema.json` 若不在，就用 `cargo tauri build` 的报错反推，别照抄本计划的拼写）。findings 里留一句装完的 exe 绝对路径，供 T47 用。

- [ ] **Step 3: 体积判定 + 超线的处置**

```bash
export PATH="$HOME/bin:$PATH"
cargo tauri build 2>&1 | tail -20
ls -l src-tauri/target/release/bundle/nsis/*.exe
```

把**真实字节数**写进 `docs/findings/plan-c-delivery-baseline.md` 第一节，并给判定：`<15MB` 达标 / 未达标。**未达标时本任务不许靠调压缩开关掩盖问题**，而是按顺序报：当前字节、`cargo tree` 里最大的几个直接依赖、以及「需要砍哪个能力」的判断（候选只有：`image` 的 features 里 `gif` 是否值得留（`Cargo.toml` R139 关默认 features 的那次决定）、`reqwest` 的 `default-tls` 换 rustls 是否更小、Plan B 可能带来的 `htmd`/`url`/`ipnet`）。那是一次**停下来讨论**，不是自己下的重构决定。

- [ ] **Step 4: 顺带关掉两条 Plan A 遗留的配置缺口（本任务范围内）**

1. `withGlobalTauri` 今天不存在 ⇒ 默认关闭，符合红线（前端拿不到 `window.__TAURI__`）。想验证就只能临时打开跑一次（见 T47 Step 3），做不到就标 `未实测`，**不许**写「已确认」。
2. `security.csp: null`（:22）意味着不注入 CSP。个人离线应用可接受，但要在 findings 里**写明这是决定而不是遗漏**。
3. release 是否带 devtools：`grep -rn "devtools" src-tauri/` 确认没有显式开，然后标「按框架默认 release 无 devtools（未实测）」。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/tauri.conf.json docs/findings/plan-c-delivery-baseline.md docs/findings/plan-a-risk-verification.md
git commit -m "build: target nsis only and record the first real installer size measurement" -- src-tauri/tauri.conf.json docs/findings/
```

`git show --stat` 不许含 `src/`、`src-tauri/src/`。

---

### Task 46b: 记住窗口尺寸与位置——spec §6.6 那条一直没人做的要求（不加依赖的那条路）

**编号带 `b`** 是因为它必须插在 T46 与 T47 之间（和 T46 共用构建产物与 findings 文件），而 T47/T48 的编号在本计划别处被引用了十几处，不重排。

**为什么切片 5/6 的盘点差点漏了它**：spec :461 写的是「单窗口、记住尺寸位置、release 关 devtools」，而 Plan A 只做了「单窗口」。今天 `tauri.conf.json:15-18` 把 `width: 800 / height: 600` 写死，没有 `tauri-plugin-window-state`，也没有任何自存的窗口几何——`grep -rn "window_state|set_size|outer_position" src-tauri/` 零命中。这是**已被 spec 要求、未被任何切片认领**的一条。

**Files:**
- Create: `src-tauri/src/window_state.rs`（+ 内联测试）
- Modify: `src-tauri/src/ipc.rs`（`run()` 的 `Builder` 链接与 `setup`）、`src-tauri/src/lib.rs`（模块声明）
- Modify: `docs/findings/plan-c-delivery-baseline.md`（多一节「窗口状态」）
- **不动** `tauri.conf.json`（理由见 Step 3）、**不动** `capabilities/default.json`（Step 4）

**Interfaces:**
- Consumes: 已有的应用目录（`settings.rs:415` 那个 `dir` 的同源路径，`ipc.rs` 已经拿它放 `settings.json` 与 logs）
- Produces:
  - `pub struct WindowGeom { pub x: i32, pub y: i32, pub width: u32, pub height: u32 }`（`Serialize + Deserialize, PartialEq`）
  - `pub fn load(path: &Path) -> Option<WindowGeom>`（读不到/解不开 ⇒ `None`，**不许**报错）
  - `pub fn save(path: &Path, g: WindowGeom) -> Result<(), String>`（temp→rename，照 `settings.rs:478-490` 的原子写法）
  - `pub fn sane(g: WindowGeom, work: (i32, i32, u32, u32)) -> Option<WindowGeom>`（钳位判定）
  - `pub enum Ev { Resized, Moved, CloseRequested, Destroyed }` + `pub fn should_persist(ev: Ev) -> bool`

- [ ] **Step 0: 先确认「为什么不用现成插件」，并把三个 API 形状验出来**

`tauri-plugin-window-state` 存在，但本计划的 Global Constraints **禁止新增直接依赖**（每条依赖都要在 T46/#29 重算安装包体积，而 T46 正要第一次量它）。自己实现约 80 行，且 `sane()` 的越屏策略本来就必须是我们的代码——插件的恢复策略不可控。

派发前实测确认这三个符号在 Tauri v2 里的真实形状（**别照抄本计划的写法**）：

```bash
export PATH="$HOME/bin:$PATH"
grep -rn "on_window_event\|WebviewWindow\|get_webview_window" src-tauri/src/ipc.rs | head
cargo doc --manifest-path src-tauri/Cargo.toml -p tauri --no-deps 2>&1 | tail -3
```

要找的三样：`tauri::WindowEvent::Resized(PhysicalSize<u32>)`、`tauri::WindowEvent::Moved(PhysicalPosition<i32>)`、`WebviewWindow::{outer_position, outer_size, set_position, set_size, current_monitor}`。若 v2 的事件名或 `current_monitor` 的返回形状与这里不同，按实际的写，并在报告里贴出你查到的签名。

- [ ] **Step 1: 写失败测试——恢复到看不见的屏幕外，是这条任务的存在理由**

```rust
#[test]
fn a_geometry_fully_off_the_current_work_area_is_discarded() {
    // 症状：拔掉副屏后重启，窗口落在 x=3000 上——用户看到的是「程序打不开」，
    // 唯一的线索是任务栏图标点不出来也关不掉。这不是边角，是双屏用户的日常。
    let work = (0, 0, 1920, 1040);
    assert_eq!(sane(WindowGeom { x: 3000, y: 0, width: 800, height: 600 }, work), None);
    // 只要**有一个角**还在工作区内就保留（拖到边缘只露一条是合法用法）：
    assert!(sane(WindowGeom { x: 1919, y: 0, width: 800, height: 600 }, work).is_some());
    assert!(sane(WindowGeom { x: -799, y: 0, width: 800, height: 600 }, work).is_some());
    assert!(sane(WindowGeom { x: -800, y: 0, width: 800, height: 600 }, work).is_none());
}

#[test]
fn tiny_or_absurd_geometry_is_clamped_not_obeyed() {
    let work = (0, 0, 1920, 1040);
    let g = sane(WindowGeom { x: 0, y: 0, width: 1, height: 1 }, work).unwrap();
    assert!(g.width >= 320 && g.height >= 240, "被拖成 0 宽就再也抓不回来");
    // 副屏比主屏大、或最大化写回的尺寸：必须缩回工作区内，而不是钳到某个上限后照样跑出屏幕。
    let big = sane(WindowGeom { x: 0, y: 0, width: 99999, height: 99999 }, work).unwrap();
    assert!(big.width <= 1920 && big.height <= 1040);
}

#[test]
fn a_corrupt_or_missing_state_file_is_absent_not_an_error() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("window.json");
    assert_eq!(load(&p), None, "第一次运行没有这个文件，是常态");
    std::fs::write(&p, "{ not json").unwrap();
    assert_eq!(load(&p), None,
        "解不开就按『没有』处理——启动失败的用户看不到任何解释，只会以为程序坏了");
}
```

- [ ] **Step 2: 写失败测试——落盘时机不许放在每个 resize 事件上**

`Resized`/`Moved` 在一次拖拽里会触发几百次。判据是**可观测的写入次数**，不是注释：把持久化决定抽成纯函数 `should_persist`，断言只有 `CloseRequested`/`Destroyed` 为 `true`，`Resized`/`Moved` 为 `false`。

**裁定：本任务不做定时器去抖。** `Coalescer`（`ipc.rs`）那套是事件合帧，这里是「写文件」；为几百次写入引入一个 timer 就是引入一类新竞态（关窗时定时器还在写）。**只在窗口关闭时持久化。** 代价写进报告与 findings：崩溃/强杀会丢窗口位置，症状是「极少见的下一次回到默认尺寸」，不是数据丢失——这是自觉取舍。

- [ ] **Step 3: 接线，并说明 `tauri.conf.json` 那两行为什么保留**

- `Builder` 的 `setup` 里：`load(...).and_then(|g| sane(g, work_area_of_current_monitor))` → `set_position` + `set_size`；`on_window_event` 里在关闭事件时取 `outer_position()`/`outer_size()` 组装 `WindowGeom` 并 `save`。
- `tauri.conf.json` 的 `"width": 800, "height": 600`（:15-18）**保留**——它是「没有存档时的大小」，删掉就换成 Tauri 的默认尺寸，那是第三份真相。这条理由写在 `ipc.rs` 的 `generate_context!` 附近（F2 已实测该文件既不接受 `//` 注释也不接受自由字段，`cargo check` 会 `rc=101`）。
- 恢复必须发生在 `setup` 里、窗口已创建之后：`app.get_webview_window("main")` 拿的是 label，Plan A 那个 `"label": "main"` 到这里才**第一次**有了真正的消费者（F2 补的理由不是空话）。拿不到（`None`）就按「没有存档」走，**不许** panic。

- [ ] **Step 4: 顺带把 #29 那条旧账数清楚（但不关它）**

`capabilities/default.json` 里的 `core:window:allow-set-size` 与 `allow-start-dragging` 是**前端**权限，而本任务的恢复全在 Rust 侧（Rust 不受 capabilities 约束）。做完本任务后这两条**仍然**没有前端消费者——连同 `dialog:allow-open` 一起，在 #29 的「无消费者权限」清单里数成**三条**，一次提交一起摘、一份证据。本任务**不动** capabilities（R108 的教训：权限的一进一出都要有自己的证据）。

- [ ] **Step 5: 跑测试 + 变异判据 + 真跑一次**

Run: `cargo test --manifest-path src-tauri/Cargo.toml window_state`
Expected: 新增 4 条 PASS；全量数对齐派发时基线（T20 之后应 ≥253）后把真实输出原样贴进报告。
判据（三条都实测）：(a) 把 `sane` 的「一个角在内即保留」改成「左上角在内才保留」⇒ `x:1919` 那条红；(b) 删掉 `load` 的解析失败分支 ⇒ `a_corrupt_or_missing_state_file…` 红；(c) `should_persist(Resized)` 返回 `true` ⇒ Step 2 红。
**人工确认一次**（做不到就标 `未实测`）：拖到非默认位置 → 关闭 → 再启动，位置尺寸保持；然后把存档里的 `x` 手改成 `3000` → 启动 → 必须回到可见位置。这条自加验收存在的原因：spec §9 没给「记住尺寸位置」任何测法。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/window_state.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat: remember the window geometry and clamp it back onto the visible work area" -- src-tauri/src/window_state.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
```

---

### Task 47: 冷启动与常驻内存实测（§9 另两条从未有数字的线）

**Files:**
- Create: `docs/findings/scripts/cold-start.ps1`
- Modify: `docs/findings/plan-c-delivery-baseline.md`（第二、三节）

**Interfaces:** 无代码接口。本任务的产物**只有测量方法与真实数字**——spec §9 的口号是「无法测量的标准等于没有标准」。

- [ ] **Step 1: 冷启动（中位数，各 5 次）**

测法按 spec :519-520：**双击图标 → 输入框可获得焦点并响应键盘**，不是「窗口出现即算」。写进 `cold-start.ps1` 的步骤：

1. `Start-Process` 记录 `Get-Date` 起点；
2. 轮询 `Get-Process -Name qwen-do` 直到主窗口句柄出现，并读 `Responding` 属性；
3. **键盘可达这一步 PowerShell 判不了** ⇒ 由人工在场时「窗口一出现就按一个字母键，看字符是否进输入框」判定。findings 里把两个数字**分开记**，并明确写「到可获得焦点并响应键盘 = 人工判定，脚本值是它的上界」。这句诚实标注是本任务不可省的一条。
4. 关闭再测，共 5 次；**首次启动（冷文件系统缓存）与后续启动各量 5 次**，各报「每次原始值 + 中位数」，判定 `<1.5s`。

- [ ] **Step 2: 空闲常驻内存（全部进程之和）**

spec :521-522 要的是「含 WebView2 子进程」的**和**；Plan A 那个 30 MB 是单进程（前置事实 10）。**不要**按进程名求和：

```powershell
# 错法（别这样）：msedgewebview2 是所有 WebView2 应用共享的进程名，按名字过滤会把别人的内核进程一起算进来。
# Get-Process | Where-Object { $_.ProcessName -in @('qwen-do','msedgewebview2') } | Measure-Object WorkingSet64 -Sum
```

正确做法：以 `qwen-do` 的 PID 为根，用 `Get-CimInstance Win32_Process` 的 `ParentProcessId` **递归收集子进程**，连同根进程一起 `WorkingSet64` 求和。这条搞错会让内存数字虚高数倍，是 T47 最容易踩的坑。空闲 = 「无对话进行中、窗口打开 60 秒后」，取 3 次，判定 `<150MB`。

- [ ] **Step 3: 流式不掉帧 + 中断 1s（§9 里同样没有数字的两条）**

`withGlobalTauri` 关闭 ⇒ DevTools 控制台拿不到 `window.__TAURI__`，但 Performance 面板仍可用。两种做法，选其一并写明：

1. 临时把 `withGlobalTauri` 设 `true` 跑一次量完**再改回**，并在 findings 写明「那次改动的配置未提交」；
2. 或者引 Plan A 已有的 IPC 实测（`plan-a-risk-verification.md:209` 起，raw 中位 7.5ms）作为下限参考，把「合帧后 React 渲染频率 ≤10 次/秒」换成可测的替代判据：T16 的 rAF 合帧测试 + 一次 Performance 录制的主线程最长单帧。

做不到就标 `未实测`。**不许**用「理论上 ≤10 次/秒」充当证据。

- [ ] **Step 4: 把 §9 十二条做成一张判定表**

在 `plan-c-delivery-baseline.md` 末尾列一张表：每条判据 / 测法 / 实测值 / 判定（✅ / ❌ / 未实测 / 待人工）。其中「全程零弹窗」在 Plan B 之前等价于「`permission_request` 事件数 = 0」，而今天生产 registry 是空的（Plan B 前置事实），所以那一格填「Plan A 结构性为 0，但未在真 key 全流程下数过 ⇒ 待 T48」。**这张表是 #29 与 T48 的唯一输入**，写完原样引用，不要另建第二份清单。

---

### Task 48: 切片 6 端到端人工验收（需用户在场，且 key 已轮换）

**Files:** 无代码。产物是 `docs/findings/plan-c-acceptance.md`（把 T47 的判定表逐条打上人工结果）。

- [ ] **Step 0: 先确认安全前提**

`cmdkey /list` 经 `iconv -f GBK -t UTF-8` 后**只数** `qwen-do` 条目是否存在（`grep -c`），**不打印条目内容、不打印任何 key**（R81/R156/R164）。写凭据是用户自己的动作，agent 永不创建/修改真实凭据。此前那把 key 曾被贴进对话、因此欠一次轮换——若尚未轮换，本任务**不开始**，改为报告「欠轮换」并停。

- [ ] **Step 1: 100 轮不失忆的人工那一半**

第 1 轮告知一个具体事实（**内容必须由用户提供**，不许编人名数字，也不许用「示例」冒充真实），第 100 轮问它。记录：是否答对、实际压缩次数（用 T42 的方法在同一份库上读）、有无 400/报错气泡。

- [ ] **Step 2: 设置面的人工那一半**

改主题 / 亮暗 / 强调色（含一个 `#ffff00` 这种专门用来撞文字色的值）/ 字号，每次切换**不重启**看是否即时生效；accent 改回「跟随预设」后是否回到预设值；用**假 key** 走一遍保存，确认 `settings.json` 与日志、`%APPDATA%\qwen-do\` 全文搜索那串 0 命中（R164：记录字段名与命中数，不记录值）。

- [ ] **Step 3: 安装包与卸载**

用 T46 的 NSIS 产物装一次、启动一次（`currentUser` 时确认没有 UAC 弹窗——那本身就是「零弹窗」的一部分）、卸载，再确认 `%APPDATA%` 里该删的删了、凭据**该留的留着**（卸载器不删凭据是决定，需在 findings 写明并给用户一条手动清除路径：`cmdkey /delete:api_key.qwen-do`，由用户自己执行）。

- [ ] **Step 4: 把结果写进判定表并交回**

每条给「实测 / 人工观察 / 未做」。任何 ❌ 都停下报告，**不要在本任务里顺手改代码**——验收失败的正确产物是「哪条、什么现象、归哪个后续任务」。

---

## 与 spec 的四处刻意偏离（写下来，别让后来人以为是遗漏）

1. **「输出上限」不加新字段**（spec §6.7 组 1 列了「输出上限」）。今天 `reserve_output` 同时当预算扣减项与 `max_output_tokens` 用（`settings.rs:305-321`，T39 Step 0 复核）。加第二个字段 = 两份「回复能写多长」的真相，而它们会漂移。UI 文案统一成「输出预留（上限）」，语义在提示文字里说清。**推翻时机**：某个端点确实需要 `reserve ≠ max_tokens` 时，那是一次带证据的加字段。
2. **压缩是存储侧事务，不在 `run()` 里改内存历史**（spec 未规定实现位置，但 `ipc.rs:320` 的 `history()[prior..]` 规定了它必须如此——前置事实 2）。副作用：压缩发生在「本轮第一次请求之前」，本轮的 est 因此是压缩后的值。这是好事，但意味着 §9 那条「第 100 轮请求的 input tokens ≤ 配置预算」由 T42 在**压缩之后**的那份历史上测。
3. **强调色不做透明度/渐变**，只做 `#rgb`/`#rrggbb` → 三件套（T44）。spec 只说「取色器写 `--accent`」，但只写一项会让 `--accent-hover`/`--accent-text` 留在旧预设值上、造出不可读按钮。用「一个纯函数产出三项配套」满足意图，而不是照抄字段名。
4. **不做历史读取分页**（`Db::messages` 无 LIMIT，前置事实 7）。spec §9 没有它，而 100 轮的真实条数/字节要等 T42 才量得出来。**先量再决定**——这是本计划里唯一「已知缺口但刻意不补」的项，必须在 #29 复审时重看。

## 执行顺序与人工节点

```
T38 → T39 → T40 → T41 → T42      （切片 5，纯 Rust，全自动）
T43 → T44 → T45                   （切片 6 设置面，依赖 T39/T43 的字段）
T46 → T46b → T47 → T48             （交付：构建 → 记住窗口 → 测量 → 人工验收）
```

- **T46 的体积数字只在 Plan A + Plan B + Plan C 全部落地后才有意义**：每加一个直接依赖都会挪它。所以 T46 可以先跑通配置与流程，但 findings 里必须标「这是第 N 次快照，依赖集在 Plan B T24 之后会变」。
- **T46b 排在 T46 之后**是因为它要在打包产物上人工验证「关掉再打开还记不记得」，而 T47 的冷启动/内存测量必须在**最终窗口行为**上测——测量一个还会跳位置的窗口没有意义。它同时是 T48 人工验收的一条（「改尺寸 → 退出 → 重开 → 尺寸位置不变」）。
- **T46b 与 Plan B 的 `ipc.rs` 交叉是三个文件里最紧的一处**：T41 改 `run_turn`、T46b 改 `run()`/`setup`，同一个文件的不同区段。并发时必须用 pathspec commit 且各自 `git diff --exit-code` 复核另一方的区段没被带走。
- **人工节点只有 T48**（外加 T47 Step 1/Step 3 里明确标注为人工的判定项）。T38–T45 全部可自动跑。
- 与 Plan B 的交叉面只有两个文件：`ipc.rs`（Plan B T27/T35 ↔ 本计划 T41/T45）与 `settings.rs`（Plan B T26 ↔ 本计划 T39/T43）。**同一条线不要并发**，否则共享 index 上即便用了 pathspec commit，两边的复审也无法归因。
