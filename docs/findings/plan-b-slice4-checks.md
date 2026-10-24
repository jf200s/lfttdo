# Plan B · 切片 4 端到端检查（T37）

**日期**：2026-10-24 22:00–22:20（本机，用户在场）
**被测对象**：`npm run tauri build` 的 release 产物（`src-tauri/target/release/qwen-do.exe` + NSIS 安装包）
与 `npm run tauri dev` 起的开发窗口（CDP 9222，仅回环，随进程消失；**没有**写进任何配置）。
**模型**：本机 Ollama `huihui_ai/qwen3-abliterated:14b-q4_K_M`（百炼 key 仍 401，云端本轮不参与）。
**MCP server**：`npx -y @modelcontextprotocol/server-everything stdio`，`mcp.json` 放在
`%APPDATA%\com.qwa.do\mcp.json`。

## 0. 先记 plan 的一个错：`@mcpserver/fetch` 这个包**不存在**

plan :1406 让填 `{"command":"npx","args":["-y","@mcpserver/fetch"]}`。实测：

```text
$ npm view @mcpserver/fetch version            -> npm error 404
$ npm view @modelcontextprotocol/server-fetch version -> npm error 404
```

所以「用 MCP 搜索并引用来源 URL」这一条**本轮没有按原样执行**。替代与边界：

- MCP 通路本身用官方 `server-everything` 验（16 个工具，握手、调用、权限、回收都跑通）；
- 「联网抓取并引用 URL」由**内置 `fetch_url`** 覆盖——那是切片 1/3 的验收面（T24/T28 已实测，见
  `plan-b-fetch-quality.md` 与 `plan-b-slice3-checks.md`），本轮**不重跑**；
- 结论按字面写：**MCP 可用 = 成立**；「MCP 形态的搜索服务」在本构建里没有可装的包，未验证。

## 1. 连接与工具数

面板原文（截图 `…\t37-mcp-panel.png`，166 KB）：

```text
MCP 服务器
everything 提供 16 个工具
已连接 · 信任：全部允许
[本服务器全部允许] [每次询问] [禁止] [重连] [断开]
```

命令层读数（页面内 `invoke("mcp_servers")`）：

```json
[{"name":"everything","status":"ready","tools":16,"trust":"ask","reason":null}]
```

`tools:16` 与 T4 探针的 `tools=16` 一致；工具名走 T30 命名，例如
`mcp__everything__echo`、`mcp__everything__get-sum`、`mcp__everything__get-tiny-image`
（连字符与下划线都在端点字符集内，直通分支）。

## 2. 信任三档（判据：`permission_request` 事件数 + 卡片落定 + tool_result）

整个过程 `permission_request` 共 **2 条**，两条都发生在 `ask` 档；`block` 与 `auto` 档新增 **0 条**。

| 档位 | 实际事件 | 结果 |
| --- | --- | --- |
| `ask` | 卡片 #1 出现（`mcp__everything__echo`，input `{"message":"T37-ECHO-123"}`）；点「仅本次」 | 卡片 `allowed`、「仅本次放行」；工具卡 `status=ok`、`preview=Echo: T37-ECHO-123`（5 ms） |
| `ask` + 拒绝 | 模型在同一轮**又调了一次**同一工具 ⇒ 卡片 #2（`Scope::Once` 只放行一次，第二调必须再问）；点「拒绝」→「提交拒绝理由」 | 卡片 `denied`；工具卡 `status=error`、`preview=用户拒绝执行：用户拒绝`；模型继续说话，**没有挂死** |
| `block` | `mcp_set_trust(everything,"block")` 后模型调用两次 | **没有任何卡片**；工具卡两条 `status=error`，正文 `用户拒绝执行：server \`everything\` 被设为 Block，调用被拒绝` |
| `auto` | `mcp_set_trust(everything,"auto")` 后模型调用 | **没有卡片**；工具卡 `status=ok`、`preview=Echo: T37-AUTO`（1 ms） |

截图 `…\t37-ask-card.png`（201 KB）是「ask 卡片 + 拒绝理由行内输入」的现场。
`respond` 返回 `false` 的路径（重复应答）没有制造额外卡片变化——与 R131 一致。

**顺带确认一个真实行为**：模型会「失败后重试一次」。在 `Once` 作用域下这会再弹一次卡
（不是 bug，是 Once 的定义）；在 `block` 下两次都被拒并各自回填 `tool_result`。

## 3. 无孤儿子进程（spec 验收线原话）

用 **PID 差集**，不用「看起来干净」：

```text
连接前的 node.exe（本机既有，Qoder 自己的）:
  33744 32564 33888 22640 6808 7520                         (6 个)
连接 server-everything 之后:
  33744 32564 33888 22640 6808 7520 11888 19824 21032 36972 38760 28128   (12 个)
taskkill /IM qwen-do.exe /F  → 等 4 秒:
  33744 32564 33888 22640 6808 7520                         (6 个)
cmd.exe: 关闭前 11 条 → 关闭后 5 条
```

新增的 6 个 node（npx 安装链 + 服务器本身）在进程被强杀后 **4 秒内全部消失**，回到基线。
这正是 T34 的 JobObject（`KILL_ON_JOB_CLOSE`）在工作：进程死亡 ⇒ 句柄关闭 ⇒ 整棵树由内核终止。
**没有**用 `taskkill /T` 兜底。

## 4. MCP 图片结果：UI 收到了，**模型没看见**（R319 实测坐实）

调用 `mcp__everything__get-tiny-image`，工具卡（`status=ok`，11 ms）的 preview 原文：

```text
Here's the image you requested:
[图片 png]
The image above is the MCP logo.
```

`Multi` 的形状、图片块的占位、文本块都在——**界面这一侧是对的**。
但模型自己的思考链（逐字）说明了它看到的是什么：

> 「工具返回的是图片数据，而当前角色只支持文本，无法直接显示图片。所以需要根据已有的信息来回答。……
> 即使没有看到图片，也可以根据常识回答。」

最终回答「这张图里画的是 MCP 标志，颜色是蓝色和白色」——这是从 **server 附带的文本**里读出来的，
不是看图。⇒ **plan :1430 的期望（模型能描述图片内容）本轮不成立**。
根因是 `openai_compat.rs:387-392` 的协议约束（`role:"tool"` 只能带文本），修复需要
「工具图片作为 user 轮 `ContentBlock::Image` 注入 + R150 的旧图降级一起扩」，见 R319 的未清账。

## 5. 性能三条（与 Plan A/C 基线并排）

| 指标 | 基线（T46/T47） | 本轮（T37） | 差 |
| --- | --- | --- | --- |
| 安装包（NSIS） | 3.92 MiB / 4,111,344 B | **4.78 MiB / 5,013,230 B** | **+0.86 MiB / +21.9%** |
| 冷启动·主窗口句柄（5 次中位） | 111 ms | **110 ms**（154/121/110/108/107） | −1 ms |
| 冷启动·Responding（5 次中位） | 132 ms | **138 ms**（206/162/137/138/107） | +6 ms |
| 空闲常驻·树 WorkingSet（1 次 60 s） | 394.4 MB（3 次） | **416.8 MB**（7 进程） | +22.4 MB |
| 空闲常驻·树 PrivateBytes | 210.4 MB | **230.6 MB** | +20.2 MB |

- 体积的 +0.86 MiB 能归因到具体两件事：`rmcp`（client + child-process 传输，R312）与
  `windows` 的三个 feature（R322）。冷启动**没有**变慢（仍 110 ms，1.5 s 那条线用掉 7%）。
- 内存仍**超** <150 MB 那条线（416.8 MB WS / 230.6 MB Private），与我们进程自己的那部分
  （qwen-do 31.5 MB）相比，93% 仍是 WebView2 运行时；结论与 T47 §3.2 相同，只是绝对值涨了 ~22 MB。
- 本轮与 Plan A/C 的差异只做了 **1 次** 60 s 内存采样（基线是 3 次）；单次读数的抖动无法从本轮数据里估计，
  如实记在这里，不把它冒充成三次中位数。

## 6. 本轮**没有**做的

- `@mcpserver/fetch` 不存在（§0），「MCP 搜索 + 引用 URL」未按原样执行；内置 `fetch_url` 的联网引用
  沿用切片 3 的证据，本轮不重跑。
- 冷启动第三档「输入框真的吃到一次按键」（spec :534 的口径）：本轮只量了句柄与 Responding 两档。
- HTTP MCP 传输：R325 显式降级，本构建不可用。
- 信任档位在**重启应用之后**是否保持：`server_trust` 住在内存的 `RuleTable` 里，**不落盘**
  （本轮没有测，也没有设计持久化）——要么 T35 之后补一个设置项，要么明确「每次启动重新授权」。
  这一条记成待办，不算通过。
