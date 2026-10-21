# Plan A 切片 1 端到端验证记录（T17）

**日期**：2026-10-21 · **分支**：`feat/plan-a-shell-chat-vision` · **HEAD**：`0650036`

**这份记录分两半，边界必须写清**（R153）：机器判据已经跑完并有原始输出；真窗口那几条
**未执行（需人工在场）**，本文只把它们写成可复跑的手顺，**没有任何一条用源码推断冒充运行结果**。

---

## 1. Step 2a —— 生产侧全形状测试（已执行，机器判据）

交付：`src-tauri/src/agent/events.rs` 的
`every_event_serializes_its_full_json_shape`（commit `518a50e`）。这是 F9 欠的债（R160）。

内容：11 个 `AgentEvent` 变体各取实例，`serde_json::to_value` 后与 `json!` 字面量**整块相等**。
`permission_resolved` 破例给三份 decision（`allow/once`、`allow/session`、`deny+reason`），
因为 `Deny` 的 `scope` 是**缺席**而不是 `null` —— 那是全部事件里唯一有可选键的一层；
`user_message` 那一份顺带把 `ContentBlock` 的 `type` 标签与 `ToolOutput` 的 `kind` 标签两支
（文本 / 图片）钉进线，`error` 那一份钉住 `at` → `"where"` 的重命名。

**牙是怎么量的**（这条是全部价值的证明）：把生产代码里 `ToolResult.ms` 重命名成 `elapsed_ms`，
全套 256 条测试里**只红这一条**。
⇒ 在此之前，嵌套层没有任何生产侧守卫：改名、挪层、加 `flatten` 都能一路绿到前端。
`seen.len() == 11` 那条去重断言负责「新增了变体却忘了补形状」。

实测数字：`agent::events` **11 passed**；全套 **256 passed / 0 failed / 1 ignored**；
`rustfmt --edition 2024 --check` rc=0。变异后已还原，与 HEAD 逐字节相同。

## 2. 配置与凭据的状态（已执行，只读）

| 项 | 实测结果 |
|---|---|
| `%APPDATA%\com.qwa.do\settings.json` | 存在，字段名只有 `base_url`、`model`（值不贴，R164） |
| `api_key` 是否落在这个文件里 | **否** —— 磁盘上直接验证了 spec §6.6 那条红线（`settings.rs:680` 的测试之外又一层证据） |
| 端点 | host `dashscope.aliyuncs.com`，path `compatible-mode/v1`（已是归一后的形态，R157 无需再查） |
| 模型 | `qwen-plus` —— **不是视觉型号**，T21 看图前必须换（见 §6） |
| 凭据管理器 | `cargo run --example keyring_probe -- status` 报「库里有一条**不是本探针写的**条目」，探针只打印长度、不打印值（R156）⇒ 应用读得到 |
| R159 的临时取帧残留 | `git grep -n "console.log" -- src/` **零命中** |
| 该提交的垃圾 | `git ls-files` 里没有 `target/`、`node_modules/`、`dist/`、`settings.json`、`.env` |

## 3. 重放边界（按 R161，这是设计不是缺陷）

`events_from_messages`（`store/db.rs:335`）重建 `turn_start` / `user_message` / `text_delta` /
`tool_call` / `tool_result`，**不重建** `usage` / `permission_*` / `error` / `context_compacted`（R114），
且重放出来的 `tool_result.ms` 恒为 0（库里没有这一列，`db.rs:371`）。

⇒ 真机上「重开旧会话后 usage 徽章为空、工具耗时显示『—』」是**预期表现**，
不许当缺陷去补第二套读取路径（spec:421 要消灭的正是那个）。
`preview` 两侧同源（`db.rs:372` 走 `ToolOutput::preview()`，全仓库只此一份实现，R77），
真机上要确认的只是截断长度看起来一致。

## 4. 零弹窗的判据（按 R158 替换原文）

原文「模型幻觉调用不存在的工具 ⇒ 看到 `tool_result is_error=true` 而非弹窗」**不成立**：
未知工具名按 `Risk::Exec`，而 `Exec` 在权限门里必定发 `permission_request` 并等应答
（`session.rs:1090-1101` 自己的测试就钉着这件事）。替换后的判据：

- 正常对话里权限卡数 = 0；
- 真出现一张**不算缺陷**，要记的是「模型在没有声明工具的情况下发起了调用」，
  并当场验证 deny 之后回合能收尾、卡片状态正确落定（`respond` 返回 `false` 时卡片不许变化，R131）；
- 空 registry 时 `tools` 字段**整体省略**（`openai_compat.rs:287-289`，测试 `empty_tools_list_is_omitted`），
  不会因为发 `tools: []` 被兼容端点拒 —— 这半条已经实测过。

## 5. 本任务期间发现并已修的界面缺陷（R163 走的路：记账 + 回修复轮）

两条都是 T16/T19 交付物的缺陷，不是在 T17 里顺手改 UI：它们各自先有一条**红着的测试**，
修完才绿，所以记在这里。

1. **发出去的图片在气泡里根本画不出来**（`1aafbe2`）。`MessageBubble.tsx:158` 拼的是
   `data:${block.mime};base64,…`，而线上的 `mime` 是裸子类型 `"png"` ⇒ src 实际是
   `data:png;base64,…`，**非法 data URL**。症状：发过图的那条气泡只有文字、重开会话也没图，
   而待发缩略图（`Composer.tsx:114` 那份拼对了 `image/${mime}`）一切正常，极易误判成后端没存图。
   RED 是实测看到的（`Received: src="data:png;base64,AAAA"`）。修后直播与重放两条路的 src 同源同形。
2. **「已忽略 N 张」和逐文件错误永不消失**（`0650036`）。`Composer` 的 `errors`/`ignored`
   全仓库没有任何 reset ⇒ 某一次投 5 张被挡掉 1 张，此后每条消息都挂着那句话。
   修成发送成功后随草稿一起清；发**失败**时不清（另有一条测试守这半句）。
3. 顺带修掉一条**永绿的装饰断言**：jsdom 的 file input 读 `value` 恒为 `""`，
   所以「选完文件后 value 为空」那条断言删掉生产代码里的 `e.target.value = ""` 也不会红 ——
   而那正是「第二次选同一个文件没反应」的真 bug。改成把 `value` 做成 own 可写属性后，牙是实测的。

## 6. 未执行（需人工在场）—— 可复跑的手顺

以下每条都要真窗口 + 真 key，控制器与子 agent 都不代跑（R153）。

- [ ] **Step 1 / T17 第一条**：`npm run tauri dev`，发「你好」→ 逐字上屏、无掉帧、输入框在 `done` 前禁用。
- [ ] **多轮上下文**：发「我是 1993 年出生的」→ 再问「我哪年出生」→ 答对。
- [ ] **打断**：中途点停止 → 1s 内不再出现新文字、状态条显示「已停止」。
- [ ] **Step 2b 抽样**：真机帧原文（若要看，临时 `console.log` 取，**收尾前删干净**）。
- [ ] **Step 3 重放**：关窗重开旧会话，按 §3 的边界核对「该有的有、不该有的没有」。
- [ ] **Step 4**（R162 降过成本）：**≥6 轮**真人发言 + 1 次中途打断 + 1 次关窗重放；零弹窗按 §4 判据记。
- [ ] **「未配置」路径现象**：把 `settings.json` 的 `base_url` 临时清空后点发送，
  看界面把 `NotConfigured` 显示成了什么人话（`openai_compat.rs:164-167` → `ipc.rs:378` reject）。
  这是切片 6 设置页的输入，**不许**当「能跑就行」略过（R163）。
- [ ] **T21（切片 2 / vision）**：先把 `model` 从 `qwen-plus` 换成视觉型号
  （如 `qwen-vl-plus`；`qwen-plus` 不吃图片，症状是图发出去而模型答非所问），
  再跑「拖一张图 + 一句『图里是什么』」→ 回答对得上，且重开这条会话图还在。
- [ ] **凭据轮换**（与本切片无关但一直欠着）：那把真 key 曾在对话里出现过明文，
  建议去百炼控制台轮换一次；轮换后只需重跑 `cmdkey //generic:api_key.qwen-do //user:api_key //pass:<新 key>`，
  并提醒自己的 shell history 里留了明文。
