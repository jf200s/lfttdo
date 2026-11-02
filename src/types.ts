// 本文件是 Rust 线格式的**手写镜像**。冲突时以 Rust 为准 —— Rust 侧的序列化测试是权威，
// 这里只是让前端不必跨语言推断形状。核对基线：HEAD 7db0439。
//
//   src-tauri/src/agent/events.rs:18-72   `pub enum AgentEvent`，tag 字段 `t`，`rename_all = "snake_case"`
//   src-tauri/src/agent/events.rs:358-421 `every_agent_event_variant_serializes_its_t_tag`（12 个标签的权威名单）
//   src-tauri/src/agent/events.rs:103-151 `ErrorWhere`/`StopReason`/`Scope`/`Decision`（Decision 是手写序列化，R98）
//   src-tauri/src/agent/types.rs:4-38     `ContentBlock`（tag 字段 `type`）/`ImageMime`/`ToolOutput`（tag 字段 `kind`）
//   src-tauri/src/agent/types.rs:158-173  `ToolCallView`/`Risk`
//   src-tauri/src/ipc.rs:24-32,146        `AgentPayload`（唯一 camelCase 的一层）+ 通道名 `"agent"`
//   src-tauri/src/ipc.rs:630-641          `SettingsView`（`#[serde(flatten)]` ⇒ 线上拍平一层）
//   src-tauri/src/settings.rs:251-255     `Mode`（`rename_all = "lowercase"`）
//   src-tauri/src/settings.rs:261-284     `Settings` 的 22 个键（T43 起含 `accent`，T26 起含 `roots`；
//                                         长期记忆切片起再含 6 个 `memory_*` 键，T12 补了
//                                         `memory_decay_half_life_days` /
//                                         `memory_prospective_window_hours`，见该接口的注释）
//   src-tauri/src/store/db.rs:239-246     `SessionRow`（无 `rename_all` ⇒ snake_case，R128）
//
// 本文件除类型外**只导出一个运行期值**：通道名 `AGENT_CHANNEL`（:115）。
// Rust 侧它是 `const AGENT_CHANNEL: &str = "agent";`（ipc.rs:146），**私有、没有 `pub`**，
// 跨语言根本取不到 ⇒ 前端只能硬编码这一处，不构成第二套真相。之所以留在镜像里而不是挪去
// T16 的 IPC 层：它和线格式同属「Rust 说了算的字符串」，放在这里才会在核对表时被一起看到
// （评审 F7）。除它以外本文件不产生任何运行期代码 —— 长期记忆切片追加的 `MemoryView`
// 也只是 interface，同样零运行期产物。
//
// Task 17 手测时还要对一次真实帧。若 Rust 加了第 12 支，前端这份镜像的缺口会被**机器**喊出来
// （R118；本轮修复后不再只是注释，评审 F1）：
//   1. `reducer.test.ts` 的跨语言用例用 `node:fs` 把 `events.rs` 里 `let tags = [ ... ];`
//      这段**有界切片**的标签原样读出来，和 `WIRE_TAGS` 做集合相等 —— Rust 加一支而镜像没跟上，
//      这条立刻红。
//   2. `WIRE_TAGS` 不再手抄，而是 `Object.keys(probe)`，其中 `probe: Record<AgentEvent["t"], AgentEvent>`
//      —— 镜像自身跟上了一支而 probe 忘了补，`tsc` 直接红（缺键是 TS2741）。
//   3. `reducer.ts` 的 `switch` 有 `never` 穷尽守卫 —— 有支、有键而 reducer 漏写臂，也是 `tsc` 红。

// 唯一一处跨模块引用，而且是 `import type`：编译后不产生代码，所以「本文件除 AGENT_CHANNEL
// 外没有运行期产物」这条仍然成立。目的是让 `Settings.mode` 与 `applyTheme` 收的那一支永远是
// 同一个联合 —— 把 `"light" | "dark"` 在这里再抄一遍就是第五份字面量（R169 刚清掉一份）。
import type { Mode } from "./theme/theme";

/** `Risk`（types.rs:167-173）：`rename_all = "lowercase"`。UI 只拿它画徽章，不参与决策（R114）。 */
export type Risk = "read" | "write" | "exec";

/** `ImageMime`（types.rs:26-31）：线上是小写 `"png"` / `"jpeg"`，不是 Debug 的 `Png`。 */
export type ImageMime = "png" | "jpeg";

/** `Scope`（events.rs:126-131）。 */
export type Scope = "once" | "session";

/** `StopReason`（events.rs:113-120）：`rename_all = "snake_case"`。 */
export type StopReason = "end_turn" | "max_turns" | "interrupted" | "error";

/** `ErrorWhere`（events.rs:103-111）。线上键名是 `where`（Rust 字段 `at`，events.rs:64 手写 rename）。 */
export type ErrorWhere = "model" | "tool" | "mcp" | "loop" | "store";

/** `ToolCallView`（types.rs:158-165）。`input` 是任意 JSON，镜像成 `unknown` 由渲染层收窄。 */
export interface ToolCallView {
  id: string;
  name: string;
  risk: Risk;
  input: unknown;
}

/** `ToolOutput`（types.rs:33-44）：tag 字段是 `kind`。`multi` 是 MCP 的混合内容块（R317）。 */
export type ToolOutput =
  | { kind: "text"; text: string }
  | { kind: "image"; data: string; mime: ImageMime }
  | { kind: "multi"; parts: ToolOutput[] };

/** MCP 信任档位（Rust `permission/gate.rs::Verdict`，命令层映射成这三个词）。 */
export type McpTrust = "auto" | "ask" | "block";

/** MCP server 视图（ipc.rs::McpServerView）：camelCase 层沿用 `AgentPayload` 的先例。
 *  只有状态、数量与脱敏后的失败原因——env/headers 一个字都不带出来。 */
export interface McpServerView {
  name: string;
  status: "connecting" | "ready" | "disconnected";
  tools: number;
  trust: McpTrust;
  reason: string | null;
}

/** 配置加载器的一条跳过记录：原因措辞来自 Rust 的固定五条。 */
export interface McpConfigIssue {
  name: string;
  reason: string;
}

/** `mcp_config_status` 的返回：**不含正文与 env/headers 值**，面板常规加载只调它。 */
export interface McpConfigStatus {
  path: string;
  exists: boolean;
  parseError: string | null;
  skipped: McpConfigIssue[];
}

/** `mcp_config_read` 的返回：**只有这条命令**会把配置原文（含密钥）带到前端。 */
export interface McpConfigText extends McpConfigStatus {
  text: string;
}

/** 技能面板的一行（ipc.rs::SkillView）。选择表按 folder 记，停用的技能仍列出来。 */
export interface SkillView {
  name: string;
  description: string;
  folder: string;
  enabled: boolean;
}

/** `skills_list` 的返回（ipc.rs::SkillsView）：root 是磁盘上的技能目录，面板原样显示。 */
export interface SkillsView {
  root: string;
  skills: SkillView[];
}

/** `ContentBlock`（types.rs:4-24）：tag 字段是 `type`（不是 `t`），字段 snake_case。 */
export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image"; data: string; mime: ImageMime }
  | { type: "tool_call"; id: string; name: string; input: unknown }
  | { type: "tool_result"; call_id: string; is_error: boolean; output: ToolOutput };

/**
 * `Decision`（events.rs:134-168）：手写序列化，恒定两个键、`allow` 在最前。
 * 变体由 `allow` 这个布尔决定，绝不由「哪个键存在」决定（R98）。
 */
export type Decision =
  | { allow: true; scope: Scope }
  | { allow: false; reason: string };

/**
 * 前端**允许发出去**的内容块（R132）。`ContentBlock` 是四支联合、Rust 侧还 derive 了
 * `Deserialize`（types.rs:4-24），拿它当入参类型等于让类型层能往历史里塞一条
 * `type:"tool_result"` —— 那不是工具产生的「工具结果」，会污染下一轮模型上下文。
 * 所以发送侧单独一支联合：只有 `text`（T16）和 `image`（T19）。
 * 镜像的是 `send_message(state, session_id, blocks: Vec<ContentBlock>)` 的**可用子集**，
 * 收窄发生在 TS 侧，Rust 一行没动（裁定就是这么要求的）。
 */
export type SendBlock =
  | { type: "text"; text: string }
  | { type: "image"; data: string; mime: ImageMime };

/**
 * `SessionRow`（db.rs:239-246）：`#[derive(Serialize)]`，**没有 `rename_all`** ⇒ 线上是
 * snake_case（R128）。键名写成 `updatedAt` 的话 TS 不报错、运行时拿到 `undefined`，
 * `sort((a,b)=>b.updatedAt-a.updatedAt)` 全是 `NaN` ⇒ 比较函数恒假、侧栏顺序变成「碰巧」，
 * 而且**一声不响**。所以这一支必须有名字，排序断言必须有（Sidebar.test.tsx）。
 * `updated_at` 是毫秒（R72），前端只拿它排序，不格式化。
 */
export interface SessionRow {
  id: string;
  title: string;
  updated_at: number;
}

export interface SessionOutputView {
  workspace: string | null;
  output_dir: string;
}

/**
 * 一条 shell profile（spec §5.2 的线格式，与 Rust `tools/shell.rs::ShellProfile` 逐键同形）。
 * **预设表的真相在 Rust 的 `tools/shell.rs`**（8 条内置预设是编译期常量）：前端经
 * `shell_presets` 拿到的只是只读副本，不在这里抄第二份；`id`/`label`/`program`/`args`
 * 四键的语义与归一化（trim 等）也都只有 Rust 一处。
 */
export interface ShellProfile {
  /** 稳定键：预设 = `"bash"` 等 8 个；自定义 = 用户命名。同名自定义在执行时覆盖同名预设。 */
  id: string;
  /** UI 显示名；预设由 Rust 给出，自定义可空（空则界面回显 id）。 */
  label: string;
  /** 可执行文件：绝对/相对路径，或按 PATH 解析的名字。存在性只有 Rust 校验。 */
  program: string;
  /** 固定参数模板；命令文本在执行时作为 argv 的最后一个元素追加（Rust 侧保证）。 */
  args: string[];
}

/**
 * `Settings`（settings.rs:261-284）：22 个键（其中 6 个是长期记忆切片的 `memory_*` 键），
 * `serde` 无 `rename_all` ⇒ 全 snake_case；
 * `mode` 是 `rename_all = "lowercase"` 的 `"light"|"dark"`（settings.rs:250-253），
 * 和 `applyTheme` 收的那个 `Mode` 是同一支，所以这里**引用**它而不是再抄一份字面量。
 * 数值一律 `number`：Rust 侧是 `u32`/`usize`/`f32`，钳位只有 `settings.rs` 的
 * `normalize()` 那一处定义（R123），TS 侧不钳第二遍。
 *
 * **加字段的人请注意**：结构体级 `#[serde(default)]`（settings.rs:260）意味着前端**少送一个键
 * 不会报错**，只会静默把那一键抹回 Rust 默认值。`SettingsPanel` 的 `settingsOf` 因此不许再
 * 逐字段列名单——`SettingsPanel.test.tsx` 里 `carries the fields it does not render through a
 * save` 那条钉的就是这件事。
 */
export interface Settings {
  base_url: string;
  model: string;
  /** 自定义系统提示词：`""` = 用内置的 LFTTDO 提示词；非空则整段替换（人设模型靠它）。 */
  system_prompt: string;
  reserve_output: number;
  budget_tokens: number;
  window_messages: number;
  theme: string;
  mode: Mode;
  font_size: number;
  /** T39：采样温度，Rust 侧夹 `0.0..=2.0`，进 `ChatRequest.temperature`。 */
  temperature: number;
  /** T39：**占预算的比例**（不是 token 数），Rust 侧夹 `0.05..=0.95`；消费者在 T41。 */
  compact_threshold: number;
  /** T39：压缩时保留原文的最近轮数 K，Rust 侧夹 `1..=50`。 */
  keep_turns: number;
  /**
   * T43：用户自选的强调色，`""` = **跟随预设**。Rust 侧只有 `normalize_accent()` 一处白名单
   * （`#rgb`/`#rrggbb` ⇒ 小写 6 位，其余回落 `""`），读写两侧各过一次 ⇒ 到这里的一定是
   * `""` 或 `#rrggbb`，TS 侧不校验第二遍（同 `font_size` 的 R123 规矩：两处边界会漂移）。
   * 消费者是 T44 的 `applyTheme`；本切片**没有**控件，所以它也绝不会由界面产生。
   */
  accent: string;
  /**
   * 而**空表意味着一律拒绝** —— 这不是没做完：Rust 侧的 `roots_default_empty_and_read_file_therefore_refuses_everything`
   * 与 `registry.rs::no_roots_denies_everything` 一起把它钉成设计（R90 取向：不替用户决定）。
   *
   * Rust 侧类型是 `Vec<PathBuf>`，序列化后就是字符串数组（那条形状测试钉着）；
   * 每一项过的是一次 `dedupe_roots()`（**只做词法整理**：trim、尾部分隔符归一、按字符串去重，
   * 不 canonicalize、一条都不丢，R284），所以这里可能是相对路径、也可能是当下够不着的那条
   * （U 盘拔了、`\\nas` 没上线）—— 「这一轮能用哪几条」由 Rust 在每轮的使用点
   * `effective_roots()` 现算。TS 侧两样都不做第二遍（同 `font_size` 的 R123 规矩）。
   *
   * `settings.json` 的目录表会在下一次点「保存」时被静默抹回空表 —— 而抹回空表的症状
   */
  roots: string[];
  storage_dir: string;
  /**
   * 长期记忆总开关（设计文档 §7）：`false` = 写入侧不抽取、读取侧不注入。默认 `true`。
   * 与其余三键同走 `settings_set` 全表，面板有控件（复选框），所以它**不是** carry-through 键。
   */
  memory_enabled: boolean;
  /**
   * 长期记忆三个数值旋钮（设计文档 §7）：
   * - `memory_recall_k`：每轮发送前混合召回注入的最大条数，Rust 夹 `1..=20`；
   * - `memory_note_token_cap`：画像 + 检索两条 system note 合计的 token 硬顶，
   *   Rust 夹 `200..=2000` 且 ≤ `budget_tokens` 的 1/5（防记忆挤垮对话预算）；
   * - `memory_profile_char_cap`：常驻画像单行的字符上限，Rust 夹 `100..=2000`（画像永不膨胀）。
   *
   * 钳制与预算约束**只有 Rust `normalize()` 那一处定义**（R123：两处边界会漂移），TS 侧
   * 不钳第二遍、不校验 —— 消费方是 Rust 的 normalize，这里的数只是线上形状的镜像。
   */
  memory_recall_k: number;
  memory_note_token_cap: number;
  memory_profile_char_cap: number;
  /**
   * T12：事实记忆的衰减半衰期（天），Rust 夹 `1..=3650`。召回排序里时间衰减的唯一旋钮：
   * 越短，旧事实沉得越快（`prospective` 待办不衰减，它的价值与 `due_at` 绑定）。
   * 偏好的半衰期按它的 3 倍在 Rust 侧推导，**不是**第二个设置键。
   */
  memory_decay_half_life_days: number;
  /**
   * T12：待办提醒的前瞻窗口（小时），Rust 夹 `1..=168`。到期时刻落在
   * 「现在 + 窗口」内的待办会在本轮被提醒一次；这一项**不受**备注 token 预算约束
   * （「必须说的事」与可选召回不同类）。
   */
  memory_prospective_window_hours: number;
  /**
   * T6：嵌入来源，白名单值 `"endpoint" | "off"`（Rust 默认 `"endpoint"`；normalize 里
   * 不认识的值回落 `"endpoint"`，模仿 accent 的回落纪律 —— 不存在第三态）。
   * `"off"` = 写入侧不建向量、读取侧不嵌查询，召回退化为纯 2-gram 关键词。
   * **面板无控件**：值经 `settingsOf` 结构化展开原样携带（carry-through 既有机制），
   * 手改 `settings.json` 不丢；要改就是手改文件这一条路。
   */
  memory_embedding_source: string;
  /**
   * T6：嵌入请求体里的 model 名（Rust 默认 `"text-embedding-v3"`，与默认 chat 模型
   * 同源的 Qwen 生态常用 embedding 模型）。Rust normalize 只 trim 不校验 —— 端点
   * 支不支持只有端点知道；配错的症状是端点 400 → 嵌入失败降级纯关键词。
   * 同样是 carry-through 键，面板无控件。
   */
  memory_embedding_model: string;
  /**
   * 自定义 shell profile 表（spec §6）。空 = 只有预设可用。**必填**：Rust serde 恒序列化
   * 这个字段，线上没有「可选」形态。Rust normalize 只做词法整理 + 按 id 去重（保留首条），
   * 不删坏条目、不碰文件系统 —— 空 id/空 program 的条目原样保留，执行时 fail-closed。
   * 前端同样不做第二遍归一化；`SettingsPanel` 的增删改直接改这一键，保存随整表走
   * `settings_set`，改完**下一条消息生效**（每轮 send_message 现刷 catalog）。
   */
  shells: ShellProfile[];
  /**
   * 默认 shell 的 profile id（spec §6）。**必填**（同上，serde 恒序列化）。
   * Rust normalize：trim 后为空、或既不是预设 id 也不是 `shells` 里的 id → 回落
   * `"bash"`（`DEFAULT_PRESET_ID`）；前端不回落第二遍，下拉的选项 = 预设 id ∪ 自定义 id。
   */
  default_shell: string;
}

/**
 * `settings_get` 真正的返回形状（第 3 条裁定）：**不是** `Settings`。
 * `ipc.rs:630-641` 是 `struct SettingsView { #[serde(flatten)] settings, api_key_masked, corrupt_backup }`
 * ⇒ `#[serde(flatten)]` 把内层 13 个键**摊平到同一层**，线上是一层扁平对象，外加两个可空键：
 * - `api_key_masked: Option<String>` —— `null` 表示未配置（R99：读不到走 `Err`，不在这个键里编故事）
 * - `corrupt_backup: Option<PathBuf>` —— 字符串路径或 `null`（R102：紧跟那次 `load()` 取走）
 * `setSettings` 反过来只收 `Settings` 那 13 个键，不许把整个 view 原样送回去。
 * T43 的 `accent`、T26 的 `roots` 因此**都不需要动 `ipc.rs`**（R282 明说这一处是空转）：
 * 它们在 `Settings` 里，flatten 自动把它们带上出口、也带上入口。不改成嵌套正是前端的读法
 * （`settings.roots`）成立的前提，这条形状由 `ipc.rs::tests::the_settings_view_carries_roots_at_the_top_level_as_strings`
 * 从 Rust 那一侧钉着 —— 类型镜像看不见运行期的形状，所以那一条才是真防线。
 */
export interface SettingsView extends Settings {
  api_key_masked: string | null;
  corrupt_backup: string | null;
}

/**
 * `memories` 表一行的读视图（设计文档 §3；Rust 侧 `MemoryView`，与 `SessionRow` 同一分工：
 * 形状镜像住 types，`commands.ts` 只放函数）。线上 snake_case。
 *
 * - **失效不删除**（Graphiti，§3）：`invalid_at` 非空 = 已失效，但行还在、历史可追溯。
 *   `valid` 是 Rust 侧按 `invalid_at IS NULL` 现算出的布尔 —— 检索唯一口径（§8.4），
 *   前端**不复算第二遍**（同 `font_size` 的 R123 规矩：两处边界会漂移），设置页只渲染它。
 * - `body` 与三元组并存：注入与设置页展示都用 `body`（人读的完整句子）；`subject`/
 *   `predicate`/`object` 只服务将来的多跳查询（§9 三期），带在视图里只是镜面的诚实。
 * - 可空键在线上是 `null`（不是缺省）：`session_id` 空 = 画像合并出的记忆、无单一来源。
 * - 时间戳一律毫秒数（同 `SessionRow.updated_at` 的 R72 口径），前端只展示不换算。
 */
export interface MemoryView {
  id: number;
  session_id: string | null;
  /** `'fact' | 'preference' | 'entity_rel'` 之一，但镜像成 `string`：名单只有 Rust 说了算。 */
  kind: string;
  subject: string | null;
  predicate: string | null;
  object: string | null;
  body: string;
  confidence: number;
  valid_from: number;
  invalid_at: number | null;
  updated_at: number;
  valid: boolean;
  /**
   * T12：同一事实被再次确认的次数。设置页在 `> 0` 时显示「确认 N 次」——
   * 用户强调过五遍的事与只说过一遍的事，在库里确实不是一个分量。
   * `0` 不显示（从未确认过不带任何标价）。
   */
  reinforcements: number;
  /**
   * T12：待办的到期时刻（ms，`prospective` 才有值）。`null` = 非待办或没解析出时间。
   * 日期格式化在**前端**按本地时区做（Rust 只给毫秒数，前端手工拼 `YYYY-MM-DD`，
   * 不用 `toLocaleDateString`：后者的输出随 ICU/语言环境漂移，测试里会不稳定）。
   */
  due_at: number | null;
  /**
   * T12：`due_at` 非空且 `<= now`。**由 Rust 现算**（`MemoryView::from`），前端直接渲染，
   * 不再与 `Date.now()` 比第二遍 —— 比较口径只有一处，两处会漂移。
   */
  is_due: boolean;
  /**
   * P1：这一行是用户亲手改过/亲手写下的（`memories.user_edited`）。行上显示「· 已编辑」，
   * 让用户认得出哪一条是自己的说法 —— 从这一刻起模型侧的下一次合并只会降级成「并存」，
   * 不再改写它（`update_memory_body_by_user` 的存储层语义）。
   *
   * ⚠️ 核对记录（写进注释而不是悄悄改）：本次前端切片核对 `ipc.rs` 的 `MemoryView` 时，
   * 结构体里**还没有**搬这一个字段（`MemoryRow` 有；`MemoryView::from` 没带出来）。
   * 这里按 P1 的合同先补上镜像，标注占位的行为是「缺席即不显示」：线上取到 `undefined`
   * 时 `m.user_edited ? … : null` 走 falsy 分支，界面不会崩、也不会谎称已编辑。
   * 后端把 `r.user_edited` 加进视图那一天，这个标记自动生效，前端不必再改。
   */
  user_edited: boolean;
}

/**
 * `AgentEvent` 的 12 个 `t` 标签，源头是 events.rs:400-412 的
 * `every_agent_event_variant_serializes_its_t_tag`（名单本身定义在 events.rs:358 那条测试里）：
 *
 *   "turn_start", "text_delta", "user_message", "tool_call",
 *   "permission_request", "permission_resolved", "tool_result",
 *   "usage", "context_compacted", "memory_saved", "error", "done"
 *
 * 本表是镜像：union 必须与那 12 支**逐一对应**，少一支就会让 reducer 的
 * `default` 静默丢数据（R111 漏 `user_message` 就是活例子）。
 *
 * 下面这份字面量清单只是**人读的注释**：`reducer.test.ts` 的 `WIRE_TAGS` 由 `AgentEvent`
 * 的联合类型派生（`Object.keys(probe)`），另有跨语言用例把这张表和 `events.rs` 里的名单
 * 做集合相等。也就是说「注释和 Rust 一致」这件事现在是机器判据，不再靠抄写（评审 F1③）。
 *
 * 不变量（events.rs:34,39）：`tool_call`/`permission_request` 的顶层 `risk`
 * 等于 `call.risk`，两者来自注册表的一次查表，所以前端读到的两个键必然一致。
 */
export type AgentEvent =
  | { t: "turn_start"; turn: number }
  | { t: "text_delta"; text: string }
  | { t: "user_message"; blocks: ContentBlock[] }
  | { t: "tool_call"; call: ToolCallView; risk: Risk }
  | { t: "permission_request"; id: string; call: ToolCallView; risk: Risk }
  | { t: "permission_resolved"; id: string; decision: Decision }
  | {
      t: "tool_result";
      call_id: string;
      is_error: boolean;
      ms: number;
      preview: string;
      images: ToolImage[];
    }
  | { t: "usage"; input_tokens: number; output_tokens: number }
  | { t: "context_compacted"; dropped: number; summary_chars: number }
  /**
   * P1：这一轮**新记住了几条**（events.rs 的 `MemorySaved { count }`）。它是「记忆在
   * 后台真的工作」的唯一可见证据 —— 没有它，整条写入管线对用户完全静默（spec §11 的
   * 元记忆取向）。`count` 不含画像：画像不是一条条的记忆。
   * 位置与 Rust 的 tags 表一致（`context_compacted` 与 `error` 之间）：跨语言用例比的是
   * **集合**（顺序不参与判定），所以挪位置不会制造假红；保持同序只是为了读镜像时一眼对得上。
   */
  | { t: "memory_saved"; count: number }
  | { t: "error"; where: ErrorWhere; message: string; fatal: boolean }
  | { t: "done"; stop_reason: StopReason };

/**
 * `AgentPayload`（ipc.rs:24-32）：事件通道上唯一的信封，也是全项目**只有这一层**用 camelCase
 * （`rename_all = "camelCase"`）；`event` 内部的键仍然是 snake_case。
 * 前端只 `listen(AGENT_CHANNEL)` 这一条通道（ipc.rs:145-146）。
 */
export interface AgentPayload {
  sessionId: string;
  event: AgentEvent;
}

/**
 * 事件通道名，镜像 ipc.rs:146 `const AGENT_CHANNEL: &str = "agent";`。
 * **本文件唯一的运行期导出**（Rust 侧那个常量是私有的，跨语言取不到，只能硬编码；
 * 理由见文件头）。除了它，本文件全是类型，编译后不产生任何代码。
 */
export const AGENT_CHANNEL = "agent";

/** turn 的说话人。`user_message` 开 user 支，`turn_start` 开 assistant 支（R111）。 */
export type TurnRole = "user" | "assistant";

/** 工具卡的状态。`running` 是 `tool_call` 落卡时的初值，`tool_result` 决定 ok/error。 */
export type ToolStatus = "running" | "ok" | "error";

/**
 * 一张工具卡。`brief` 的 Produces 清单把它和 `PermCard` 列成具名类型，所以这里给名字，
 * `UiBlock` 直接用这两个名字拼 —— T16 收窄时不必再重抄 `Extract<UiBlock, { kind: "tool" }>`（评审 F3）。
 * 三种写法**等价、可互换**：把变体内联进 `UiBlock` / 具名 interface 被 `UiBlock` 消费 /
 * 事后再补一个 `Extract<UiBlock, …>` 别名。保证这件事的是文件末尾那条 `CardAliasEquivalence`
 * 编译期断言（`SameShape` 双向互赋，一旦漂移直接 TS2344），不是这段注释。
 *
 * 这里不写成 `export type ToolCard = Extract<UiBlock, { kind: "tool" }>`，要禁的只是**下面这一对同时成立**：
 * (a) `UiBlock` 已经是「由具名变体拼出来的组合别名」（本文件正是这样），而 (b) 那个具名变体又反过来
 * 取 `Extract<UiBlock, …>`。那时别名左右互相引用，`tsc` 报 TS2456「Type alias circularly references
 * itself」（实测）。反过来，若 `UiBlock` 里那一支是**内联**写的，「内联变体 + 事后 `Extract` 别名」
 * 编译得干干净净 —— 所以「`Extract` 写法不可能」这个说法过宽，准确的说法只有「(a) 与 (b) 不能并存」。
 * 本文件的取舍：名字住在变体上、`UiBlock` 引用名字，末尾用编译期断言把两个方向钉回等价。
 */
export interface ToolImage {
  data: string;
  mime: ImageMime;
  path?: string;
}

export interface ToolCard {
  kind: "tool";
  callId: string;
  name: string;
  risk: Risk;
  input: unknown;
  status: ToolStatus;
  preview: string;
  images: ToolImage[];
  /**
   * 毫秒。`ms === 0` 在重放路径里表示「库里没记这一列」（db.rs:370），
   * 不是「跑了 0 毫秒」；reducer 原样透传，由 T16 渲染成「—」（R115）。
   */
  ms: number;
}

/** 一张权限卡。`state` 由 `permission_request`/`permission_resolved` 决定，`scope`、`reason` 二选一地出现（R98）。 */
export interface PermCard {
  kind: "perm";
  id: string;
  call: ToolCallView;
  risk: Risk;
  state: "pending" | "allowed" | "denied";
  /** 拒绝理由，只在 `state === "denied"` 时有线上来源。 */
  reason?: string;
  /** 放行范围，来自 `Decision::Allow { scope }`。 */
  scope?: Scope;
}

/** 界面上的一块。`tool`/`perm` 来自事件流，`text`/`image` 来自事件流或 `ContentBlock` 映射。 */
export type UiBlock =
  | { kind: "text"; text: string }
  | { kind: "image"; data: string; mime: ImageMime }
  | ToolCard
  | PermCard;

/** 两个方向都能互赋才算同一个形状（`[A] extends [B]` 关掉分发，避免 union 逐支比较的假负）。 */
type SameShape<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
/** 不满足约束就 TS2344 编译不过 —— 断言用，运行期零产物。 */
type Must<T extends true> = T;

/**
 * 机器钉住「具名别名 == 内联变体的 `Extract`」。将来有人把 `UiBlock` 里那两支换回内联写法、
 * 或给别名加了字段，让两边漂移，这里直接编译不过，而不是等 T16 用到才发现。
 */
export type CardAliasEquivalence = [
  Must<SameShape<ToolCard, Extract<UiBlock, { kind: "tool" }>>>,
  Must<SameShape<PermCard, Extract<UiBlock, { kind: "perm" }>>>,
];

/**
 * 一条气泡。`id` 是 reducer 自己的单调 `turnSeq`，**不是** 事件里的 `e.turn`：
 * `e.turn` 每次真人发言后从 1 重启（db.rs:385）、每次 `run()` 也从 1 起（session.rs:119），
 * 拿它当键或下标一定错位（R113）。
 */
export interface Turn {
  id: number;
  role: TurnRole;
  blocks: UiBlock[];
}

/** 索引值：turn 用 `Turn.id`（即 turnSeq）定位，绝不存 `e.turn`（R113）。 */
export interface BlockLocation {
  turnId: number;
  block: number;
}

/**
 * reducer 的唯一状态。纯数据、可序列化：不含 `Date`、不含函数、也不含 sessionId
 * （会话归属由 `AgentPayload.sessionId` 在监听那一层过滤，见 T16）。
 */
export interface UiState {
  /** 单调递增的 turn 计数器，同时是 `Turn.id` 的来源。 */
  turnSeq: number;
  turns: Turn[];
  /**
   * 只代表「输入框是否禁用」：turn_start/text_delta/tool_call 置真、done 置假，与 turn 边界无关（R112）。
   *
   * 注意这三支是**无条件**置真的，所以按 R112 枚举，`done` 之后的一条迟到 `text_delta`/`tool_call`
   * 会把 streaming 重新抬回 true（`reducer.test.ts` 里就有一条断言把这个字面结果钉着）。
   * 真机出口产不出这种帧：`ipc.rs:133-138` 的 `other => { flush(); send(other) }` 保证任何非
   * `TextDelta` 事件之前先落帧（`Done` 也走这臂），`session.rs:264-267` 在致命错误后必跟 `Done`。
   * ⇒ **这条不是业务规则，只是枚举的推论。** T16 若要做「回合结束后不许再锁输入框」的加固，
   * 改的是渲染层怎么用这个布尔，不要回头改 reducer（评审 F6）。
   */
  streaming: boolean;
  /** 瞬时事件，重放路径不会重建它（R114）。 */
  usage: { input: number; output: number } | null;
  /**
   * P1：最近一次 `memory_saved` 的条数与一个**本地递增序号** `at`。
   *
   * - 只留最新一条：同一轮连续几次写入不需要排队，用户要的是「刚才记了几条」。
   * - `at` 不是时钟。`reducer` 是纯函数（R119，源码扫描钉着「正文里不许出现 Date.now」），
   *   所以序号只能从状态自己推：`(上一个 at ?? 0) + 1`。它不是身份、不参与渲染文案，
   *   唯一作用是让「连续两次同 count」也是两个不同的值 —— 界面靠依赖变化重开 8 秒计时，
   *   拿 count 当键的话第二声轻提示会被 React 当成没变化而哑掉。
   * - 会话切换时随 `initialState()` 归零：它属于这一次会话的即时反馈，不是历史。
   *
   * **为什么是可选的**：`initialState()` 永远填 `null`，reducer 也永远写它；可选只服务
   * 「外部手工构造的 `UiState`」—— `MessageList.test.tsx` 那类夹具用 `Partial<UiState>`
   * 展开并集，多一个必填键就会让它们当场 TS2322，而那一份测试不在本切片的改动范围里。
   * 消费方因此把 `undefined` 与 `null` 读成同一件事（`App.tsx` 用 `if (!… ) return`）。
   */
  memorySaved?: { count: number; at: number } | null;
  /** 致命错误横幅。非致命错误走 `notices`，两者不互相降级。 */
  fatal: { message: string; where: ErrorWhere } | null;
  /**
   * 给 T16 的交接（评审 F5）：reducer **只增不减**，也不去重、不记严重度 —— 重放一整段历史时
   * 这里可以上百条（每个 `is_error` 的工具、每次 `context_compacted`、每条非致命 `error` 都 push 一条，
   * `session.rs:119` 的上限是 25 个工具轮）。reducer 保持「一份真相」，所以**折叠/截断/「已读」
   * 全归渲染层**：不要指望状态里替你把通知裁掉。
   */
  notices: string[];
  /**
   * 给 T16 的交接（评审 F5）：本会话内**只 set 不 delete**，因此它随会话长度单调增长，
   * 且 `permIndex` 里没有的卡不代表已过期。清空它的唯一途径是切会话时从 `initialState()` 重新起，
   * reducer 不提供裁剪入口 —— 键是 `call.id` / `permission_request.id`，裁剪会直接破坏 R113 的定位。
   */
  toolIndex: Record<string, BlockLocation>;
  /** 同 `toolIndex`：只增不减，会话切换靠 `initialState()` 清空（评审 F5）。 */
  permIndex: Record<string, BlockLocation>;
}
