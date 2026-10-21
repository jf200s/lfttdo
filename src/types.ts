// 本文件是 Rust 线格式的**手写镜像**。冲突时以 Rust 为准 —— Rust 侧的序列化测试是权威，
// 这里只是让前端不必跨语言推断形状。核对基线：HEAD 7db0439。
//
//   src-tauri/src/agent/events.rs:18-72   `pub enum AgentEvent`，tag 字段 `t`，`rename_all = "snake_case"`
//   src-tauri/src/agent/events.rs:358-421 `every_agent_event_variant_serializes_its_t_tag`（11 个标签的权威名单）
//   src-tauri/src/agent/events.rs:103-151 `ErrorWhere`/`StopReason`/`Scope`/`Decision`（Decision 是手写序列化，R98）
//   src-tauri/src/agent/types.rs:4-38     `ContentBlock`（tag 字段 `type`）/`ImageMime`/`ToolOutput`（tag 字段 `kind`）
//   src-tauri/src/agent/types.rs:158-173  `ToolCallView`/`Risk`
//   src-tauri/src/ipc.rs:24-32,146        `AgentPayload`（唯一 camelCase 的一层）+ 通道名 `"agent"`
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
 *
 * 为什么不写成 `export type ToolCard = Extract<UiBlock, { kind: "tool" }>`：那样 `UiBlock`
 * 与 `ToolCard` 互相引用，`tsc` 报 TS2456「Type alias circularly references itself」（实测）。
 * 于是名字住在变体上、`UiBlock` 引用名字，末尾再用编译期断言把两个方向钉回等价。
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
