import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
// 类型全部来自 `../types` 的镜像：`Settings` / `SettingsView` / `SessionRow` / `SendBlock`
// 是 T16 按线格式补进那份镜像的（第 3 条裁定），**不在这里手造**——
// 在调用点就地写形状等于把「Rust 说了算」的事交给人肉记忆，改了 Rust 侧只会在这里静默坏掉。
import { AGENT_CHANNEL } from "../types";
import type {
  AgentEvent,
  AgentPayload,
  Decision,
  SendBlock,
  SessionRow,
  Settings,
  SettingsView,
} from "../types";

/**
 * 发一句话。`blocks` 收 `SendBlock[]` 而不是 `ContentBlock[]`（R132）：
 * 后者能让类型层表达「伪造一条 tool_result 塞进历史」，那种帧进了下一轮模型上下文
 * 就是界面替工具编了一次执行结果。
 *
 * 参数名走 camelCase（`sessionId`）：Tauri v2 把 JS 侧的 camelCase 键映射到 Rust 侧的
 * snake_case 形参。**这个映射在这里成立与否只有真机能答**（Task 17 端到端实测项），
 * 若不成立，改成显式 `{ session_id }` 并把结论写进 findings。
 */
export async function sendMessage(
  sessionId: string,
  blocks: SendBlock[],
): Promise<void> {
  await invoke("send_message", { sessionId, blocks });
}

/**
 * 把权限决定递回去。第二参名是 `requestId`（R131，`ipc.rs:423-428`），返回的 `bool`
 * 含义是「pending 里确实有这一笔被闭合」。
 * **收到 `false` 时调用方不许动卡片状态** —— 卡片的最终态只由 `permission_resolved` 事件驱动。
 * 界面自己把卡片翻成「已放行」就是替用户发明第二份真相，而且那一笔其实还挂着。
 */
export const respond = (requestId: string, decision: Decision) =>
  invoke<boolean>("respond", { requestId, decision });

/** 打断当前这一轮。令牌一次性（R63），打不到正在跑的轮次也是 Ok。 */
export const interrupt = () => invoke("interrupt");

/** 会话列表。线上键是 `updated_at`（db.rs:239-246 无 `rename_all`，R128）——写错不会报错，只会静默乱序。 */
export const sessions = () => invoke<SessionRow[]>("session_list");
export const createSession = () => invoke<string>("session_create");
export const deleteSession = (id: string) => invoke("session_delete", { id });
export const renameSession = (id: string, title: string) =>
  invoke("session_rename", { id, title });

/** 重放一整段历史。回来的是一段事件，前端用 `replay()` 换成 `UiState`。 */
export const replaySession = (id: string) =>
  invoke<AgentEvent[]>("session_messages", { id });

/**
 * `settings_get` 返回**拍平一层**的 `SettingsView`（`ipc.rs:488-500` 的 `#[serde(flatten)]`），
 * 不是 `Settings`。`api_key_masked` 与 `corrupt_backup` 都可以是 `null`（R99 / R102）。
 */
export const getSettings = () => invoke<SettingsView>("settings_get");
/** 反过来只写那 8 个键：把整个 view 原样送回去等于把掩码和备份路径当设置存盘。 */
export const setSettings = (settings: Settings) =>
  invoke("settings_set", { settings });
/** 明文 key 只进这一条命令：不打印、不日志、不进返回值（spec:473）。空串按清除处理（R88）。 */
export const setApiKey = (apiKey: string) =>
  invoke("settings_set_api_key", { apiKey });

/**
 * 订阅 `agent` 通道。通道名用 `../types` 里导入的常量，不再抄字面量（第 7 条：
 * Rust 侧那个常量是私有的，`AGENT_CHANNEL` 是这份镜像里唯一可用的名字来源）。
 *
 * **不按会话过滤是有意为之**：它把 `payload.sessionId` 一并交出去，过滤是 `App` 的义务
 * （`UiState` 里没有 sessionId 可区分，错过一帧就补不回来 —— 见 `App.tsx` 的那道闸门
 * 与 `App.test.tsx` 的跨会话用例）。
 */
export function subscribeAgent(
  onEvent: (sessionId: string, e: AgentEvent) => void,
): () => void {
  const un = listen<AgentPayload>(AGENT_CHANNEL, (p) =>
    onEvent(p.payload.sessionId, p.payload.event),
  );
  return () => {
    un.then((f) => f());
  };
}
