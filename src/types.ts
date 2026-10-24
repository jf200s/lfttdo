// 本文件是 Rust 线格式的**手写镜像**。冲突时以 Rust 为准 —— Rust 侧的序列化测试是权威，
// 这里只是让前端不必跨语言推断形状。核对基线：HEAD 7db0439。
//
//   src-tauri/src/agent/events.rs:18-72   `pub enum AgentEvent`，tag 字段 `t`，`rename_all = "snake_case"`
//   src-tauri/src/agent/events.rs:358-421 `every_agent_event_variant_serializes_its_t_tag`（11 个标签的权威名单）
//   src-tauri/src/agent/events.rs:103-151 `ErrorWhere`/`StopReason`/`Scope`/`Decision`（Decision 是手写序列化，R98）
//   src-tauri/src/agent/types.rs:4-38     `ContentBlock`（tag 字段 `type`）/`ImageMime`/`ToolOutput`（tag 字段 `kind`）
//   src-tauri/src/agent/types.rs:158-173  `ToolCallView`/`Risk`
//   src-tauri/src/ipc.rs:24-32,146        `AgentPayload`（唯一 camelCase 的一层）+ 通道名 `"agent"`
//   src-tauri/src/ipc.rs:630-641          `SettingsView`（`#[serde(flatten)]` ⇒ 线上拍平一层）
//   src-tauri/src/settings.rs:251-255     `Mode`（`rename_all = "lowercase"`）
//   src-tauri/src/settings.rs:261-284     `Settings` 的 13 个键（T43 起含 `accent`，T26 起含 `roots`）
//   src-tauri/src/store/db.rs:239-246     `SessionRow`（无 `rename_all` ⇒ snake_case，R128）
//
// 本文件除类型外**只导出一个运行期值**：通道名 `AGENT_CHANNEL`（:115）。
// Rust 侧它是 `const AGENT_CHANNEL: &str = "agent";`（ipc.rs:146），**私有、没有 `pub`**，
// 跨语言根本取不到 ⇒ 前端只能硬编码这一处，不构成第二套真相。之所以留在镜像里而不是挪去
// T16 的 IPC 层：它和线格式同属「Rust 说了算的字符串」，放在这里才会在核对表时被一起看到
// （评审 F7）。除它以外本文件不产生任何运行期代码。
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

/** `ToolOutput`（types.rs:33-38）：tag 字段是 `kind`。 */
export type ToolOutput =
  | { kind: "text"; text: string }
  | { kind: "image"; data: string; mime: ImageMime };

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

/**
 * `Settings`（settings.rs:261-284）：13 个键，`serde` 无 `rename_all` ⇒ 全 snake_case；
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
   * T26（R276）：`read_file` 被允许读的目录表，**由用户自己列出**。默认 `[]`，
   * 而**空表意味着一律拒绝** —— 这不是没做完：Rust 侧的 `roots_default_empty_and_read_file_therefore_refuses_everything`
   * 与 `registry.rs::no_roots_denies_everything` 一起把它钉成设计（R90 取向：不替用户决定）。
   *
   * Rust 侧类型是 `Vec<PathBuf>`，序列化后就是字符串数组（那条形状测试钉着）；
   * 每一项在**读写两侧**都过过一次 `normalize_roots()`（canonicalize、去重、丢掉不存在的），
   * 所以到这里的一定是已规范化的绝对路径 —— TS 侧不规范化第二遍（同 `font_size` 的 R123 规矩）。
   *
   * 本切片**没有**界面控件：`SettingsPanel` 不渲染它，但必须原样带走它
   * （`SettingsPanel.test.tsx` 的 carry-through 那条盯着），否则用户手改进
   * `settings.json` 的目录表会在下一次点「保存」时被静默抹回空表 —— 而抹回空表的症状
   * 不是「设置没了」，是「read_file 突然什么都读不到」。
   */
  roots: string[];
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
 * `AgentEvent` 的 11 个 `t` 标签，源头是 events.rs:400-412 的
 * `every_agent_event_variant_serializes_its_t_tag`（名单本身定义在 events.rs:358 那条测试里）：
 *
 *   "turn_start", "text_delta", "user_message", "tool_call",
 *   "permission_request", "permission_resolved", "tool_result",
 *   "usage", "context_compacted", "error", "done"
 *
 * 本表是镜像：union 必须与那 11 支**逐一对应**，少一支就会让 reducer 的
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
  | { t: "tool_result"; call_id: string; is_error: boolean; ms: number; preview: string }
  | { t: "usage"; input_tokens: number; output_tokens: number }
  | { t: "context_compacted"; dropped: number; summary_chars: number }
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
export interface ToolCard {
  kind: "tool";
  callId: string;
  name: string;
  risk: Risk;
  input: unknown;
  status: ToolStatus;
  preview: string;
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
