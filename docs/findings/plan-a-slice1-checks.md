# Plan A 切片 1 端到端验证记录（T17）

**日期**：2026-10-21 · **分支**：`feat/plan-a-shell-chat-vision` · **HEAD**：`0650036`
（§5 的第 4–6 条与 §7 是评审 #29 收尾轮补的，那之后的 HEAD 见 §7 抬头）

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

以下几条都是 T13/T16/T19 交付物的缺陷，不是在 T17 里顺手改 UI：它们各自先有一条**红着的测试**，
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
4. **点停止后文案没变，而这条从未被守**（评审 #29，spec:475 后半句「立即禁用并显示停止中…」）。
   `Composer.tsx` 渲染的标签一直是「停止」，`App.test.tsx` 那条只断 `toBeDisabled()`，
   于是删掉整个 `stopRequested` 状态都不会红。补的是按名字查（`{ name: "停止中…" }`）再断禁用，
   GREEN 落在 `Composer.tsx:230` 的 `{stopRequested ? "停止中…" : "停止"}`。
   同一条用例里那句重复的 `await waitFor(() => expect(h.interrupt).toHaveBeenCalled())` 也删了 ——
   它是对同一个断言的第二遍，守不住任何新东西。
5. **发送在途时归一化回来的图被无声吞掉**（评审 #29 / F7）。`submit()` 在 `await onSend` 之后
   调 `clearPending()`，而 `normalize_image` 是另一次异步 IPC：用户贴完图直接回车，那次 await
   还挂着 ⇒ 图回来后既不在发出去的载荷里、又被整区清空，缩略图当场消失。症状与第 2 条同一族
   （界面先承诺，再静默吞）。RED 是实测看到的（发送 resolve 后 `thumb-0` 查不到）。
   修成 `dropSent(sent)`：`submit` 在 `await` **之前**抓 `pendingRef.current` 快照，
   成功后按**身份**只摘掉这一批（`Composer.tsx:82,156,165`）。
   已知同族但**没动**的一半：`setText("")` 仍是整句清 —— `streaming` 期间输入框是禁用的，
   所以「在途打字」的窗口比图那条窄得多，且没有测试钉它；留给真机（§6）。
6. **`unlisten()` 的 reject 会漏成 unhandled rejection**（评审 #29，`ipc/commands.ts:102-105`）。
   清理原本是 `void un.then((f) => f())` —— 单参 `then` 没有 rejection 臂，`listen()` 本身
   被 reject（窗口已销毁等）时 Promise 的错没人接。症状不是界面而是进程级一条未捕获警告。
   补了一条用例在 `process` 上捕 `unhandledRejection` 并断它为空。牙是实测的：把那句改回
   只带一个参数的写法，全套 118 条里**只红这一条**，报 `Received: [Error { message: "窗口已销毁" }]`。
   另注：用例里那句 `await new Promise((r) => setTimeout(r, 0))` 不是装饰 —— 只
   `await Promise.resolve()` 会稳定地看不见这条 rejection。

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
- [ ] **拖放入口在真窗口里到底活不活**（评审 #29 记为风险，**本应用未实测**）：
  `tauri.conf.json` 从没写过 `dragDropEnabled`，也就是取默认值 `true`
  （`tauri-utils-2.10.2/src/config.rs:2408` 的 `drag_drop_enabled: true`）。同一个字段的文档原文：
  「Whether the drag and drop handlers used internally to generate `DragDropEvent`s are enabled on
  the webview. By default it is enabled. **Disabling it is required to use HTML5 drag and drop on
  the frontend on Windows since we replace the drag drop handler of WebView2.**」（`:1978-1980`）。
  而 `Composer.tsx:195-200` 走的正是 HTML5 的 `drop` + `dragover`。jsdom 不认识 Tauri 那一层拦截，
  所以这三条拖放测试**永远给不出证据** —— 它们只证明「handler 写对了」，不证明「handler 收得到」。
  三个出路各有代价，要用户选：
  ① `dragDropEnabled: false`：DOM 通路按上面那句原文立刻可用，代价是没被 `drop` 接住的拖放
     会回到 WebView2 的默认行为（很可能是**导航到 `file:///…`**，界面整个换页）—— 这是一条读文件
     的路径，与「前端零文件系统能力」那条红线相冲，且这一半也要真机确认；
  ② 保持拦截、在 `ipc.rs` 里听 `DragDropEvent` 拿路径、由 Rust 读字节：不动红线，但要新增一条命令
     或第 12 支事件，是 Plan B/C 的量；
  ③ 承认「三入口」在桌面端实际是两个（粘贴 + 文件选择器），把拖放从 spec 里删掉。
  真机判据：把一张 PNG 从资源管理器拖进窗口 ⇒ 出现待发缩略图，而不是页面被换成那张图。
- [ ] **凭据轮换**（与本切片无关但一直欠着）：那把真 key 曾在对话里出现过明文，
  建议去百炼控制台轮换一次；轮换后只需重跑 `cmdkey //generic:api_key.qwen-do //user:api_key //pass:<新 key>`，
  并提醒自己的 shell history 里留了明文。

---

## 7. 评审 #29 收尾轮定下的三条交付侧决定（都是决定，不是漏项）

1. **`security.csp: null` 是决定**（`tauri.conf.json:22`）。Plan C:997 明确要求这条「要在 findings
   里写明是决定而不是遗漏」。依据：本应用不加载任何远程内容 —— `frontendDist: ../dist`，
   `index.html:12` 只有 `<script src="/src/main.tsx">` 这一条本地资源，全仓库**非测试**前端源码里
   一个 `http(s)://` 字面量都没有（实测 grep 零命中）；进界面的远程内容只有模型回来的文字，
   走 React 默认转义，图片是 `data:` URL。加 CSP 必须先给 `img-src` 开 `data:`，那是 T15/T16
   之后才出现的需求空间。**代价写清楚**：一旦将来把任何外部 URL 塞进 `<img src>` 或 `<iframe>`
   （Plan B 的 `fetch_url` 预览是最可能的起点），这条决定要重新过一遍。
2. **capability 名单缩到两条**。prune 前五项：`core:default`、`core:event:default`、
   `dialog:allow-open`、`core:window:allow-set-size`、`core:window:allow-start-dragging`；
   现在只剩前两项。被摘掉的三条**没有消费者**：`grep -rn "getCurrentWindow|setCurrentSize|
   startDragging|@tauri-apps/api/window|plugin-dialog|plugin-opener|openFile|shell" src
   --include=*.ts --include=*.tsx` 在排除 `*.test.*` 之后零命中（实测）。`dialog:allow-open`
   的死因是 R144 把选文件入口换成原生 `<input type="file">`（前端因此拿不到路径），
   `tauri-plugin-dialog` 随之从 `Cargo.toml` 与 `ipc.rs::run()` 一起摘掉。
   **`core:event:default` 没有一并删**：它大概率已被 `core:default` 包含，但「删掉之后 `listen()`
   还响不响」只能在真窗口里证明，而在「事件是 Rust→前端的唯一通道」的应用里赌它没用不划算。
3. **`rmcp` 不再是运行期依赖**。Plan A 的 `src/` 对它引用数是 0（只有 `examples/mcp_probe.rs` 用），
   于是挪进 `[dev-dependencies]`；`cargo test` 会把 examples 一起编出来，265 / 0 / 1 全绿就是证据。
   Plan B 把它搬回 `[dependencies]` 时这条要一起撤销。

**体积的第二个真实数字**：`cargo build --release` ⇒ `src-tauri/target/release/qwen-do.exe`
**11,111,424 字节**（22:28 实测，`Finished` 在 3m21s 后）。上一条记录是 11,328,000 字节，
减 **216,576 字节（−1.9%）**，来源就是上面第 2、3 两条。
**但这仍然不是 15MB 那条线的量**：plan-a:25 说的是「安装包」，而 `target/release/bundle/`
在本机从没产出过（NSIS/WiX 未跑）。所以三条交付基线（安装包体积 / 冷启动 5 次中位数 /
空闲常驻内存之和）**至今未实测**，由 Plan C T46 收口。

**2026-10-22 复测：体积数字未变，仍是 11,111,424 字节。** 这一轮改的是
`classify_openai_error` 的 `Decode` 一支（多一段 `｜请求地址: …` 与一次 `truncate_display`），
`grep -ac 请求地址 src-tauri/target/release/qwen-do.exe` 给 **1** —— 也就是说这个数是从**含本轮
改动**的那个产物上量的，不是 22:28 那只旧 exe 的读数。**没对上的一笔如实留着**：cargo 这次报的是
`Finished` in **0.85s**，而 exe 的 mtime 是 08:44:20（本轮启动构建之后），两者对不上一笔 3 分钟的
链接；我能证明的是「产物里有新字符串」，不能证明的是「这 0.85s 里发生了什么」。**不因此重算
−1.9% 那个减法**：本轮新增的是一个 12 字节字符串加一次调用，落在 PE 段对齐的空隙里是合理预期，
但「合理预期」不是实测，体积若要再进任何预算表，请从 `bundle/` 的安装包数字重新量。

