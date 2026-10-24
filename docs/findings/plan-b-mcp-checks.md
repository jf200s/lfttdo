# Plan B · MCP 桥的机器检查（T31）

本文件只记 `examples/mcp_bridge_probe.rs` 的**原始读数**与它们各自能证到哪一步。
单测（`src/mcp/bridge.rs` 内联，7 条）回答的是选型/隔离/超时/脱敏；这里回答的是
「本机 PATH 上真的长得像假表吗」「真握手能不能数到工具」「直接子进程到底是谁」。

运行命令（仓库根）：

```text
cargo run --manifest-path src-tauri/Cargo.toml --example mcp_bridge_probe
```

包默认 `@modelcontextprotocol/server-memory`（plan :884 指定的那条），`--server <name>` 可换。

## 结论一句话

真 `npx` 经 `spawn_command` 被解析成**绝对路径的 `npx.cmd`**，`-y` 自动落在 args 第一段，
握手 12.3 s 数到 **9 个工具**，**直接子进程是 `cmd.exe`**（真 node 是孙进程 ⇒ T34 的 JobObject 前提成立）；
`CREATE_NO_WINDOW` 只能在本层钉住「决定」，**「窗口真的不出现」这一条本轮拿不到判据**，归 T37 真窗口验收。

## §1 选型（R305/R306）—— 真解析器，不是假表

```text
PKG=@modelcontextprotocol/server-memory
CONNECT_TIMEOUT_SECS=60
PLAN program="C:\\Program Files\\nodejs\\npx.cmd"
PLAN args=["-y", "@modelcontextprotocol/server-memory"]
PLAN creation_flags=0x08000000 create_no_window=true
```

- program 是**绝对路径且以 `.cmd` 收尾**：与 T4 findings:603-609 的 `where npx` 两条候选
  （`…\npx`、`…\npx.cmd`）一致，落的是能起的那条。**无扩展名那条没被试**，本轮不改变 T4 的判读。
- args 第一段就是 `-y`：用户配置里只写了包名（探针 `args = [pkg]`），注入发生在 `spawn_command` 里，
  位置紧贴 program（R306：`npx pkg -y` 里的 `-y` 是传给包自己的，与 `npx -y pkg` 不等价）。
- `creation_flags=0x08000000` 且 `create_no_window=true`：这是**决定**，不是应用结果（见 §4）。

## §2 真握手（plan :887 的 `tools=N` 与握手耗时）

```text
NODE_BEFORE=6
Knowledge Graph MCP Server running on stdio
MS=12310
RESULT=ok
TOOLS=9
TOOL_NAMES=["add_observations", "create_entities", "create_relations", "delete_entities", "delete_observations", "delete_relations", "open_nodes", "read_graph", "search_nodes"]
STATUS=Some(Ready { tools: 9 })
```

- 12.3 s 是**首次**跑（`npx -y` 要装包）。这个数只说明「冷装能装进 60 s 的 `CONNECT_TIMEOUT_SECS`」，
  **不能**当成稳态握手延迟：T35 的界面文案与 T37 的验收都别引用这个数当典型值。
- `NODE_BEFORE=6`：本机在探针启动前**已有 6 个 `node.exe`**（与本次无关的常驻进程）。
  所以本节的 `NODE_AFTER=6` 只能读成「本次没有新增残留」，**不是**「机器上没有 node」。

## §3 直接子进程 = `cmd.exe` —— T34 的前提（plan :887 点名）

```text
PIDS=[("probe", 16928)]
TASKLIST[probe 16928]="cmd.exe","16928","Console","1","7,176 K"
```

被 spawn 的是 `npx.cmd`，但 `CreateProcess` 为 `.cmd` 另起的是 `cmd.exe` 宿主 —— 与 T4 findings:685-694 同形。
⇒ 真 node 服务器是**孙进程**，rmcp 的 `graceful_shutdown`/`kill` 只打直接子进程（T34 的理由），
桥把 pid 存进台账（R309）正是为了让 T34 能 `AssignProcessToJobObject`。

## §4 `CREATE_NO_WINDOW`：本轮能证到哪、证不到哪（R307/R313）

```text
WINDOW_TITLE[without flags=0x00000000]="cmd.exe","10072","Console","1","5,180 K","Unknown","HOSTNAME\user","0:00:00","<GBK 乱码>"
WINDOW_TITLE[with flags=0x08000000]="cmd.exe","34416","Console","1","5,184 K","Unknown","HOSTNAME\user","0:00:00","<GBK 乱码>"
WINDOW_TITLE_NOTE=两行相同不能证明标志没生效：探针跑在控制台里，子进程本来就继承父控制台；真判据在 T37 真窗口验收
```

- 两行 Window Title 相同 ⇒ **这条取证分辨不出**。原因是结构性的：探针本身跑在控制台里，
  子进程不带 `CREATE_NO_WINDOW` 也会继承父控制台，带了则拿到一个「有控制台、无窗口」的状态；
  `tasklist /V` 读的是控制台标题，两种情况都会拿到父控制台那个标题。
- 因此本轮对 `CREATE_NO_WINDOW` 的结论只有两档证据：
  1. **单测**钉住「选型决定里恒置位」（`SpawnPlan.creation_flags & 0x0800_0000`）。
     但这只钉住**决定**：控制器把 `into_command()` 里真正应用标志的那一句删掉后，
     7 条单测**全绿**（见台账 R313）——`SpawnPlan` 的字段还在，测试读的就是那个字段。
  2. **源码**上真 spawn 的唯一出口是 `SpawnPlan::into_command()`，`#[cfg(windows)]` 支里那一句是唯一调用点。
     这一条是**代码结构的事实**，不是测试证据。
- **「黑窗真的不出现」= 未实测**，归 T37 的真窗口验收（那里的进程是 GUI 宿主，差别才可见）。
  别把本节读成「已验证零弹窗」。

## §5 关闭与残留计数

```text
STATUS_AFTER_SHUTDOWN=Some(Disconnected { reason: "已关闭" })
NODE_AFTER=6
```

`shutdown()` 只走 rmcp 的 `cancel()`（`graceful_shutdown` → 关 stdin → 等 → kill 直接子进程），
**不做进程树回收**（T34）。本机前后都是 6，说明这次运行没有新增残留；
但「每次退出都不留」不由此证明（T4 findings:649-652 已经记过同样的天花板）。

## §6 R312 的记账：rmcp 已进主二进制

T31 把 `rmcp` 从 `[dev-dependencies]` 挪进 `[dependencies]`（桥是产品代码，MCP 是功能）。
**后果**：T46/T47 那份安装包体积与冷启动读数是在**没有** rmcp 的产物上测的，全部过期。
**T37 的门禁必须包含**：`npm run tauri build` 的安装包体积 + 冷启动与常驻内存复测，
并与 `docs/findings/plan-c-delivery-baseline.md` 的原始读数逐项对账。

## 本轮**没有**测的

- `CREATE_NO_WINDOW` 的真实窗口效果（见 §4）。
- 真实网络故障下的错误文案（只测了「解析不到」与「对端不回」两条，都是注入的）。
- 并发多 server 的真机握手（单测用注入的 300 ms 超时腿覆盖了隔离与超时；真 npx 只跑了一条）。
- 安装包体积 / 冷启动（见 §6）。
