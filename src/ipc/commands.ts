import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
// 类型全部来自 `../types` 的镜像：`Settings` / `SettingsView` / `SessionRow` / `SendBlock`
// 是 T16 按线格式补进那份镜像的（第 3 条裁定），**不在这里手造**——
// 在调用点就地写形状等于把「Rust 说了算」的事交给人肉记忆，改了 Rust 侧只会在这里静默坏掉。
import { AGENT_CHANNEL } from "../types";
import type {
  AgentEvent,
  AgentPayload,
  ContentBlock,
  Decision,
  McpConfigStatus,
  McpConfigText,
  McpServerView,
  McpTrust,
  MemoryView,
  SendBlock,
  SessionOutputView,
  SessionRow,
  Settings,
  SettingsView,
  ShellProfile,
  SkillsView,
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

/** 打开外部链接；协议白名单在 Rust 侧（只允许 http/https），前端不直接导航 WebView。 */
export const openUrl = (url: string) => invoke("open_url", { url });

export const revealPath = (sessionId: string, path: string) =>
  invoke("reveal_path", { sessionId, path });

export const pickFolder = () => invoke<string | null>("pick_folder");

export const sessionOutputView = (id: string) =>
  invoke<SessionOutputView>("session_output_view", { id });

export const setSessionWorkspace = (id: string, path: string | null) =>
  invoke("session_workspace_set", { id, path });

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
 * 长期记忆的管理命令（设计文档 §7：记忆对用户必须可审计，物理删除的入口只在这里）。
 * 形状镜像 `MemoryView` 住在 `../types`（与 `SessionRow` 同一分工：形状住 types，这里只放函数）。
 * 「失效不删除」是**库内**的 Graphiti 纪律（旧行留 `invalid_at`，检索不再返回）；但设置页的
 * 「删除 / 清空全部」是**物理删** —— 隐私出口的最后一环：二期的 `memory_forget` 工具只置失效，
 * 管不到这两条（§6），「忘掉我说的卡号」的兜底就是这里。
 */
export const listMemories = () => invoke<MemoryView[]>("memories_list");
export const deleteMemory = (id: number) => invoke("memories_delete", { id });
export const clearMemories = () => invoke("memories_clear");
/**
 * 常驻画像的当前正文（`memory_profile` 恒单行，设计文档 §3）。命令先备齐并钉住命令名，
 * 面板本切片**不消费**它（画像展示不在这一片 —— `SettingsPanelProps` 只加三个回调，
 * 见该文件的裁定注释）。
 */
export const memoryProfile = () => invoke<string>("memory_profile");

/**
 * P1：会话切走时的**第二次轻量抽取**（`ipc.rs::memory_flush_session`）。返回本次新记住
 * 了几条（`usize` 线上是数字）。
 *
 * 参数键写成 `sessionId` —— 与上面 `sendMessage` 对 `session_id` 的写法**同一条惯例**：
 * Tauri v2 把 JS 侧的 camelCase 键映射到 Rust 的 snake_case 形参。写成 `session_id`
 * 在这里反而对不上（命令按形参名取值），症状是切换时抽取永远收到空会话。
 *
 * **调用方必须 fire-and-forget**：切换会话是主路径，不能为一次后台抽取多等一个来回；
 * 后端也把一切失败都收成 `Ok(0)`（它连 `Err` 都不往上抛），所以 `catch` 只是防御性的 ——
 * 命令本身不存在、或 IPC 层出错时才走得到那里。
 */
export const flushMemory = (sessionId: string) =>
  invoke<number>("memory_flush_session", { sessionId });

/**
 * P1：用户亲手改一条记忆的正文。后端会把这一行打上 `user_edited = 1`、置信度抬到 1.0，
 * 于是模型此后的 UPDATE/INVALIDATE 只会降级成「并存」—— 用户的话从此是权威。
 * 空正文由后端 `reject_empty_body` 拒收；前端只负责把按钮禁用（真校验只有一处）。
 */
export const updateMemoryBody = (id: number, body: string) =>
  invoke("memory_update_body", { id, body });

/**
 * P1：用户手动新增一条记忆，返回新行 id。`kind` 的白名单与回落都在 Rust
 * （`fact`/`preference`/`prospective`，不认识的一律回落 `fact`），前端不校验第二遍。
 *
 * `dueAt` 是毫秒时间戳（spec §12.5：面板新增的待办也要能到期提醒），
 * `null` = 待办但没有到期时刻（先记下来，不提醒）。
 * **换算义务在调用方**（`SettingsPanel`），且必须按 **UTC** 解释字面值 ——
 * 与 Rust 侧 `parse_iso8601_ms` 的既有裁定同口径。这里只负责把数字原样递过去，
 * 参数键写 `dueAt`（Tauri v2 的 camelCase → snake_case 映射，同 `flushMemory`）。
 */
export const addMemory = (kind: string, body: string, dueAt: number | null) =>
  invoke<number>("memory_add", { kind, body, dueAt });

/**
 * 把一张图的原始字节交给 Rust 归一化，回来的就是可以直接进 `send_message` 的那一支
 * `ContentBlock`（`image`，已按上限缩放/转码）。
 *
 * **IPC 形状只许这一种**（R147，T3 实测：raw body 与 JSON 字段互斥）：第二参必须是
 * 整块 `ArrayBuffer`/`Uint8Array` **本身**。写成 `{ bytes }` 会进 `InvokeBody::Json`，
 * 被 `ipc.rs:564` 直接 Err —— 症状是「每次贴图都报错」，原因藏在形状里。
 *
 * 这里**不做**任何本地大小预判（R195）：那道闸在 Rust，且它的顺序在格式识别之前，
 * 前端再摆一道只会造出第二套真相和与之冲突的文案。
 */
export const normalize = (bytes: ArrayBuffer) =>
  invoke<ContentBlock>("normalize_image", bytes);

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
    // `listen()` 自己也可能 reject（窗口已销毁是真实的一种）。这条清理函数住在
    // `useEffect` 的返回位上，没人会 await 它，所以少一支 `onrejected` 就是一次
    // unhandled rejection —— 症状不是崩，是控制台被无视的错误淹掉，而这条路径平时不响。
    // 这里刻意不往上抛：卸载时 `App` 已经不再订阅，通知面无从可上。
    void un.then(
      (f) => f(),
      () => {},
    );
  };
}

/**
 * MCP 服务器的四条命令（T35）。进程执行全在 Rust 侧 —— 前端没有、也不该有
 * `plugin-shell` 的能力（`capabilities/default.json` 的说明与守卫测试一起钉着这条）。
 */
export function mcpServers(): Promise<McpServerView[]> {
  return invoke<McpServerView[]>("mcp_servers");
}

export function mcpSetTrust(name: string, trust: McpTrust): Promise<void> {
  return invoke<void>("mcp_set_trust", { name, trust });
}

/** 返回这台 server 声明的工具数（连不上 ⇒ reject，文案已脱敏、可直接上界面）。 */
export function mcpReconnect(name: string): Promise<number> {
  return invoke<number>("mcp_reconnect", { name });
}

export function mcpDisconnect(name: string): Promise<void> {
  return invoke<void>("mcp_disconnect", { name });
}

/**
 * MCP 配置编辑器（应用内 JSON 编辑）。`mcpConfigStatus` 只给面板做提示，不含正文；
 * 只有 `mcpConfigRead` 会把含 env/headers 的原文带到前端，且只在用户点「编辑配置」时调用。
 * 保存的严格校验在 Rust；失败返回多行原因，不写盘。
 */
export function mcpConfigStatus(): Promise<McpConfigStatus> {
  return invoke<McpConfigStatus>("mcp_config_status");
}

export function mcpConfigRead(): Promise<McpConfigText> {
  return invoke<McpConfigText>("mcp_config_read");
}

export function mcpConfigSave(text: string): Promise<void> {
  return invoke<void>("mcp_config_save", { text });
}

/** 技能面板：列表每次现扫磁盘，前端不另存一份。 */
export function skillsList(): Promise<SkillsView> {
  return invoke<SkillsView>("skills_list");
}

/** 在资源管理器里打开技能目录。路径由 Rust 侧 `AppState` 定死，前端不传任何参数。 */
export function skillsOpenFolder(): Promise<void> {
  return invoke<void>("skills_open_folder");
}

/** 启用/停用一个技能（按文件夹名）。选择表落在数据目录的 skills.json。 */
export function skillsSetEnabled(
  folder: string,
  enabled: boolean,
): Promise<void> {
  return invoke<void>("skills_set_enabled", { folder, enabled });
}

/**
 * Shell 预设表（spec §5.5 的 `shell_presets`）。**只读副本**：8 条内置预设是 Rust
 * `tools/shell.rs` 里的编译期常量，前端不抄第二份 —— 界面只展示，没有任何可编辑控件；
 * 自定义 profile 走 `settings_set` 的 `shells` 键，不经过这里。
 */
export function shellPresets(): Promise<ShellProfile[]> {
  return invoke<ShellProfile[]>("shell_presets");
}

/**
 * 校验一条（自定义）shell profile（spec §5.5 的 `shell_profile_validate`）：
 * Rust 侧 `validate_custom`（形状）+ `ensure_executable`（程序存在且可执行）。
 * 失败 `Err`（中文原因，可直接上屏），成功 `Ok(())`。**真校验只有 Rust 这一处**：
 * 前端只做「程序路径为空时按钮禁用」级别的使能，保存时不调用它（R284 同规）。
 * 参数键 `profile` 两端同名，无 camelCase 映射问题。
 */
export function shellProfileValidate(profile: ShellProfile): Promise<void> {
  return invoke<void>("shell_profile_validate", { profile });
}
