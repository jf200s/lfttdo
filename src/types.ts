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
// Task 17 手测时还要对一次真实帧；若 Rust 加了第 12 支，`src/state/reducer.test.ts`
// 的 11 标签表驱动用例会自己喊出来（R118）。

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
 * `AgentEvent` 的 11 个 `t` 标签，逐字抄自 events.rs:400-412 的
 * `every_agent_event_variant_serializes_its_t_tag`：
 *
 *   "turn_start", "text_delta", "user_message", "tool_call",
 *   "permission_request", "permission_resolved", "tool_result",
 *   "usage", "context_compacted", "error", "done"
 *
 * 本表是镜像：union 必须与那 11 支**逐一对应**，少一支就会让 reducer 的
 * `default` 静默丢数据（R111 漏 `user_message` 就是活例子）。
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

/** 事件通道名，镜像 ipc.rs:146 `const AGENT_CHANNEL: &str = "agent";`。 */
export const AGENT_CHANNEL = "agent";

/** turn 的说话人。`user_message` 开 user 支，`turn_start` 开 assistant 支（R111）。 */
export type TurnRole = "user" | "assistant";

/** 工具卡的状态。`running` 是 `tool_call` 落卡时的初值，`tool_result` 决定 ok/error。 */
export type ToolStatus = "running" | "ok" | "error";

/** 界面上的一块。`tool`/`perm` 来自事件流，`text`/`image` 来自事件流或 `ContentBlock` 映射。 */
export type UiBlock =
  | { kind: "text"; text: string }
  | { kind: "image"; data: string; mime: ImageMime }
  | {
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
  | {
      kind: "perm";
      id: string;
      call: ToolCallView;
      risk: Risk;
      state: "pending" | "allowed" | "denied";
      /** 拒绝理由，只在 `state === "denied"` 时有线上来源。 */
      reason?: string;
      /** 放行范围，来自 `Decision::Allow { scope }`。 */
      scope?: Scope;
    };

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
  /** 只代表「输入框是否禁用」：turn_start/text_delta/tool_call 置真、done 置假，与 turn 边界无关（R112）。 */
  streaming: boolean;
  /** 瞬时事件，重放路径不会重建它（R114）。 */
  usage: { input: number; output: number } | null;
  /** 致命错误横幅。非致命错误走 `notices`，两者不互相降级。 */
  fatal: { message: string; where: ErrorWhere } | null;
  notices: string[];
  toolIndex: Record<string, BlockLocation>;
  permIndex: Record<string, BlockLocation>;
}
