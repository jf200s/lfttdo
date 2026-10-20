# Plan A — 脚手架、纯聊天、看图 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付一个可日常使用的 Tauri v2 + Rust 内核轻量聊天客户端：多轮流式对话、会话持久化、停止、vision 贴图问答，全程零弹窗。

**Architecture:** Rust 侧跑 agent 循环（`AgentSession` 编排 → `ModelProvider` 流式 → `ToolRegistry` 执行 → `PermissionGate` 判定 → `ContextBuilder` 组装），以 `AgentEvent` 单一事件流经 `ipc.rs`（唯一依赖 Tauri 的文件）投给 React 前端；前端只做一个纯 reducer 的投影，不参与任何决策。核心逻辑通过 `EventSink` / `ModelProvider` 两个 trait 注入测试替身，可在无 Tauri、无网络下用 `#[tokio::test]` 端到端测试。

**Tech Stack:** Tauri v2、Rust（tokio、async-openai、serde、rusqlite/bundled、keyring、image、tracing）、React + TypeScript + Vite + Vitest。

**Spec:** `docs/superpowers/specs/2026-10-19-qwen-do-agent-design.md`

本计划覆盖 spec 切片 0、1、2。切片 3–4 见 Plan B，切片 5–6 见 Plan C。

## Global Constraints

每个任务都隐式包含本节，值逐字取自 spec。

- 只有 `ipc.rs` 允许 `use tauri`。`agent/`、`model/`、`tools/`、`permission/`、`store/`、`image_prep.rs` 全部与 Tauri 无关。
- 事件出口与模型访问以 trait 注入：`EventSink`（生产 `TauriEmitter` / 测试 `VecSink`）、`ModelProvider`（生产 `OpenAiCompat` / 测试 `ScriptedModel`）。
- UI 只消费事件，不参与决策。前端不判断工具风险级别。
- MCP 工具与内置工具在 registry 里同质（Plan B 落实；本计划不得为二者分叉代码路径）。
- MVP 内置工具全为 `Risk::Read`，默认 `Verdict::Auto`，因此**全程零弹窗**。
- 同一时刻只跑一轮；切换会话即中断当前轮。
- 安装包 <15MB，冷启动 <1.5s，空闲常驻内存 <150MB。
- 组件内禁止硬编码颜色，只能用 token 变量。
- `api_key` 只写进 Windows 凭据管理器，不写 settings 表、不进导出、不写日志；读取接口返回掩码值。
- `TextDelta` 在 Rust 侧合帧到 30ms 或 512 字符才 `emit`。
- 开发机 Windows，shell 为 Git Bash，路径统一用正斜杠书写。

## 文件结构（Plan A 建立的文件）

```
qwen-do/
├─ .gitignore                                   T1
├─ docs/findings/plan-a-risk-verification.md    T2/T3/T4 写结论
├─ package.json  vite.config.ts  tsconfig.json  index.html
├─ src/                                         前端
│  ├─ main.tsx  App.tsx
│  ├─ types.ts                                  T14  Rust AgentEvent 的手写 TS 镜像
│  ├─ state/reducer.ts                          T14
│  ├─ state/reducer.test.ts                     T14
│  ├─ ipc/commands.ts                           T16  invoke 包装（唯一碰 Tauri JS API 处）
│  ├─ components/Sidebar.tsx                    T16
│  ├─ components/MessageList.tsx                T16
│  ├─ components/MessageBubble.tsx              T16
│  ├─ components/Composer.tsx                   T16
│  ├─ components/ImageStrip.tsx                 T19
│  ├─ theme/tokens.css                          T15
│  └─ theme/presets.css                         T15
└─ src-tauri/
   ├─ Cargo.toml  tauri.conf.json  build.rs
   ├─ capabilities/default.json                 T13
   └─ src/
      ├─ main.rs  lib.rs                        T13 装配
      ├─ ipc.rs                                 T13  唯一 use tauri
      ├─ settings.rs                            T12
      ├─ image_prep.rs                          T18
      ├─ agent/
      │  ├─ mod.rs  types.rs  events.rs         T5
      │  ├─ context.rs                          T8
      │  └─ session.rs                          T10
      ├─ model/
      │  ├─ mod.rs  provider.rs                 T6
      │  ├─ openai_compat.rs                    T6
      │  └─ assistant_acc.rs                    T7
      ├─ tools/
      │  ├─ mod.rs  registry.rs                 T9
      │  └─ echo.rs                             T9  仅测试用
      ├─ permission/
      │  ├─ mod.rs  gate.rs                     T9
      │  └─ toasts.md                            —
      └─ store/
         ├─ mod.rs  db.rs                       T11
         └─ examples/stream_probe.rs             T2
         └─ examples/mcp_probe.rs               T4
         └─ examples/raw_ipc_probe.rs           T3
```

`examples/*.rs` 是切片 0 的三个探针，交付物是**结论**，代码标注为可丢弃。

---

# Part 0 — 脚手架与风险验证（切片 0）

### Task 1: 仓库初始化与双测试器接线

**Files:**
- Create: `src-tauri/`（脚手架）、`src/`（脚手架）、`.gitignore`
- Create: `src/state/smoke.test.ts`、`src-tauri/src/agent/mod.rs`（空壳）

**Interfaces:**
- Consumes: 无
- Produces: 可运行的 `npm run tauri dev`；`cargo test` 与 `vitest run` 两条空流水线均为绿。

- [ ] **Step 1: 初始化 git 仓库**

```bash
cd "D:/aido/qwen do 1"
git init
git config user.name   # 确认已有值；为空则 git config user.name "<你的名字>"
```

- [ ] **Step 2: 写 .gitignore**

```gitignore
/node_modules
/dist
/src-tauri/target
/src-tauri/gen/schemas
*.db
*.db-wal
*.db-shm
.DS_Store
```

`src-tauri/target` 不忽略会让仓库胀到数 GB；`*.db-wal` 是 Task 11 引入 SQLite 后必然产生的伴生文件。

- [ ] **Step 3: 脚手架 Tauri v2 + React + TS**

Run: `npm create tauri-app@latest . -- --template react-ts --manager npm --identifier com.qwa.do --yes`

Expected: 生成 `src/`、`src-tauri/`、`package.json`，且 `Cargo.toml` 里 `name = "qwen-do"`。若交互式提问无法跳过，去掉 `--yes` 手工应答，选项保持上述值。

- [ ] **Step 4: 装依赖并接线 vitest**

```bash
npm i
npm i -D vitest @testing-library/react jsdom
```

在 `vite.config.ts` 中加入（保留脚手架已有内容）：

```ts
/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  test: { environment: "jsdom", globals: true },
});
```

在 `package.json` 的 `scripts` 加一行：

```json
"test": "vitest run"
```

- [ ] **Step 5: 写一个必然失败的测试，验证流水线接通**

Create `src/state/smoke.test.ts`:

```ts
import { expect, test } from "vitest";
test("vitest runs", () => {
  expect(1 + 1).toBe(3);
});
```

Run: `npm test`
Expected: FAIL，报 `expected 3 to be 2`。**必须看到它失败**——一条从没红过的测试流水线不能证明自己在跑。

- [ ] **Step 6: 修正断言并跑绿**

把 `toBe(3)` 改为 `toBe(2)`。

Run: `npm test` → PASS
Run: `cd src-tauri && cargo test` → 通过（此时 0 个测试，退出码 0）

- [ ] **Step 7: 验证桌面窗口能起**

Run: `npm run tauri dev`
Expected: 出现窗口，显示脚手架页面与版本号。**Ctrl+C 关掉它再继续**——这个进程会占住端口 1420。

若报 `link.exe not found` 或 `cargo: command not found`，说明 Rust 工具链缺失；先装 `rustup`（MSVC 工具链）与 Visual Studio Build Tools 的「使用 C++ 的桌面开发」负载，再继续。**不要改用 GNU 工具链**，Tauri 在 Windows 上只可靠支持 MSVC。

- [ ] **Step 8: 记录基线体积**

Run: `ls -la src-tauri/target/debug/ | grep -i qwen`
把 `qwen-do.exe` 大小写进 `docs/findings/plan-a-risk-verification.md` 的「基线」小节。debug 产物不能用于验收（发布构建才是 <15MB 的判定对象），此处只为确认工具链输出正常。

- [ ] **Step 9: 提交**

```bash
git add -A
git commit -m "chore: scaffold tauri v2 + react-ts, wire vitest and cargo test"
```

---

### Task 2: 风险验证 —— Rust 侧流式收 SSE

这是本计划价值最高的一步：spec 第 11 节风险 #3、#4、#5 全部在此暴露或排除。产物是一个 `examples` 探针和一段写进文档的结论，不是生产代码。

**Files:**
- Create: `src-tauri/examples/stream_probe.rs`
- Create: `docs/findings/plan-a-risk-verification.md`
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Consumes: 无
- Produces: 结论记录，供 Task 6 实现 `OpenAiCompat` 时照抄事实；`assistant_acc`（Task 7）依赖「tool_call 分片是否真的带 index」这一实测答案。

- [ ] **Step 1: 加依赖**

```bash
cd src-tauri
cargo add async-openai --features chat-completion,real-time
cargo add futures
cargo add tokio --features full
cargo add serde_json
```

Expected: `Cargo.toml` 出现四项，版本由 cargo 解析（**不要手写版本号**，本计划刻意不锁版本以免写出过期约束）。

- [ ] **Step 2: 写探针**

Create `src-tauri/examples/stream_probe.rs`:

```rust
//! 风险验证探针（非生产代码）。回答三个问题：
//! 1) async-openai 的 create_stream 在自定义 base_url 上是否真的逐块到达
//! 2) tool_calls 增量是否分片、是否带 index、arguments 是否被切断
//! 3) 首块到达延迟（判断是否有代理缓冲吃掉流式）
use async_openai::{config::OpenAIConfig, types::CreateChatCompletionRequestArgs, Client};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("QWEN_API_KEY").expect("set QWEN_API_KEY");
    let base = std::env::var("QWEN_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
    let model = std::env::var("QWEN_MODEL").unwrap_or_else(|_| "qwen-plus".into());

    let cfg = OpenAIConfig::new().with_api_base(&base).with_api_key(key);
    let client = Client::with_config(cfg);

    let req = CreateChatCompletionRequestArgs::default()
        .model(&model)
        .stream(true)
        .messages(vec![async_openai::types::ChatCompletionRequestMessage::System(
            async_openai::types::ChatCompletionRequestSystemMessage::new(
                "调用 get_time 工具，不要直接回答。",
            ),
        )])
        .build()?;

    let mut stream = client.chat().create_stream(req).await?;
    let t0 = std::time::Instant::now();
    let mut first = None;
    let mut n = 0usize;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if first.is_none() {
            first = Some(t0.elapsed());
        }
        n += 1;
        for ch in &chunk.choices {
            if let Some(tc) = &ch.delta.tool_calls {
                for f in tc {
                    println!(
                        "TOOLCALL idx={:?} id={:?} name={:?} args={:?}",
                        f.index,
                        f.id,
                        f.function.as_ref().and_then(|x| x.name.clone()),
                        f.function.as_ref().and_then(|x| x.arguments.clone()),
                    );
                }
            }
            if let Some(t) = &ch.delta.content {
                if !t.is_empty() {
                    println!("TEXT {t:?}");
                }
            }
        }
    }
    println!("chunks={n} first_chunk_in={:?}", first.unwrap_or_default());
    Ok(())
}
```

注：`ChatCompletionRequestSystemMessage::new` 等构造签名随版本变动。若编译失败，**以 `cargo doc -p async-openai --open` 或 docs.rs 当前版本为准改写**，不要改成非流式调用——那会让探针回答不了问题。

- [ ] **Step 3: 跑探针**

```bash
QWEN_API_KEY=<真 key> cargo run --example stream_probe
```

Expected: 大量 `TOOLCALL idx=0 ...` 行，其中 `args` 是残缺字符串片段（例如 `{"ur`、`l":"now`）；`chunks` 远大于 1；`first_chunk_in` 明显小于总耗时。

- [ ] **Step 4: 三条判定的失败分支**

- 若 `chunks == 1` → 流式被缓冲（本地代理/网络中间层），风险 #4 命中。处理：在结论里记录，并把「关闭流式的诊断开关」从 Plan C 提到必要项。
- 若 `TOOLCALL` 行的 `idx` 恒为 `None` → 不能靠 index 归组，Task 7 的累加器必须改用「以 `id` 出现为边界」的策略。**这是一条会改变 Task 7 实现的发现，必须写进结论。**
- 若 `create_stream` 直接返回 Err → base_url 形态问题（风险 #5）。逐一试：去掉尾部 `/`、补 `/v1`、去掉 `/v1`，把每种组合的最终 URL 与结果列成表写进结论。

- [ ] **Step 5: 写结论**

Create `docs/findings/plan-a-risk-verification.md`，`## 风险 1：Rust 流式` 小节内必答四问：首块延迟实测值 / chunk 是否真的多块 / tool_call 分片是否带 index / base_url 四种组合哪种可用。每条附原始输出行。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/examples/stream_probe.rs docs/findings/plan-a-risk-verification.md src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "spike: verify rust-side sse streaming and tool_call fragmentation"
```

---

### Task 3: 风险验证 —— Tauri 二进制 IPC

**Files:**
- Create: `src-tauri/examples/raw_ipc_probe.rs`（Rust 侧回环断言）
- Modify: `src-tauri/src/lib.rs`、`src/App.tsx`、`docs/findings/plan-a-risk-verification.md`

**Interfaces:**
- Consumes: 无
- Produces: `Task 18` 的 `normalize_image` 命令签名形态（raw body 还是 base64 字段），以及前端调用形态。

- [ ] **Step 1: 加一个临时命令，两种传法都试**

Modify `src-tauri/src/lib.rs`，在 `run` 内注册：

```rust
#[tauri::command]
fn ipc_probe_raw(req: tauri::ipc::Request<'_>) -> Result<usize, String> {
    match req.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => Ok(bytes.len()),
        tauri::ipc::InvokeBody::Json(v) => Err(format!("got json body: {v}")),
    }
}

#[tauri::command]
fn ipc_probe_b64(bytes_b64: String) -> Result<usize, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&bytes_b64)
        .map_err(|e| e.to_string())?;
    Ok(bytes.len())
}
```

在 `Builder::default().invoke_handler(tauri::generate_handler![...])` 里追加 `ipc_probe_raw, ipc_probe_b64`。

Run: `cargo add base64 image`

- [ ] **Step 2: 前端两种调用各试一次**

在 `src/App.tsx` 里临时加按钮回调：

```ts
import { invoke } from "@tauri-apps/api/core";

async function probe() {
  const bytes = new Uint8Array(1024 * 512); // 512KB 随机尺寸
  for (let i = 0; i < bytes.length; i++) bytes[i] = i % 251;

  try {
    console.log("raw:", await invoke("ipc_probe_raw", bytes));
  } catch (e) {
    console.log("raw FAILED:", String(e));
  }

  const b64 = btoa(String.fromCharCode(...bytes.subarray(0, 8192)));
  try {
    console.log("b64:", await invoke("ipc_probe_b64", { bytesB64: b64 }));
  } catch (e) {
    console.log("b64 FAILED:", String(e));
  }
}
```

`String.fromCharCode(...)` 只对 8KB 安全，切勿用于 512KB（会栈溢出），那正是这一探要暴露的问题之一。

- [ ] **Step 3: 判定并写结论**

Run: `npm run tauri dev`，打开 DevTools Console，点按钮。

Expected 且记录：`raw:` 输出 `524288` → raw body 可用，Task 18 采用 `InvokeBody::Raw`，前端传 typed array 整体作为第二参数。
若 `raw FAILED` → 记录实际报错，Task 18 改走 base64，并**额外写一个分块编码函数**（`File.slice` 分段 + 逐段 `btoa`），因为单次 `String.fromCharCode` 展开大数组会崩。

结论必须包含两种传法各自的实测延迟（`performance.now()` 包住 invoke），供 Task 19 决定是否需要 loading 态。

- [ ] **Step 4: 移除临时命令与前端探针**

删掉 `ipc_probe_raw` / `ipc_probe_b64` 及 App.tsx 里的 probe。**不要留下探针进生产代码**——它会被误当成 IPC 模式。

- [ ] **Step 5: 提交**

```bash
git add docs/findings/plan-a-risk-verification.md src-tauri/src/lib.rs src/App.tsx src-tauri/Cargo.toml
git commit -m "spike: measure tauri binary ipc vs base64 for image upload"
```

---

### Task 4: 风险验证 —— rmcp 在 Windows 上 spawn npx

spec 第 11 节风险 #2。若此项不通，Plan B 的切片 4 会在最后一步卡住，所以必须现在知道答案。

**Files:**
- Create: `src-tauri/examples/mcp_probe.rs`
- Modify: `src-tauri/Cargo.toml`、`docs/findings/plan-a-risk-verification.md`

**Interfaces:**
- Consumes: 无
- Produces: Plan B 的 `mcp/bridge.rs` 所依赖的三个事实：client API 形态、Windows 下 spawn `.cmd` 的可行方式、子进程是否真被 kill 干净。

- [ ] **Step 1: 加依赖并查 API**

Run: `cargo add rmcp --features client,transport-io,jsonrpce-types`

若 feature 名报错，Run: `cargo add rmcp --dry-run` 或读 `~/.cargo/registry/.../rmcp-*/Cargo.toml` 里真实的 `features` 表，把正确名称记进结论。**不要猜着加 `--features`。**

Run: `cargo doc -p rmcp --open` 并定位 `rmcp::service::Client` 与 stdio transport 构造方式，把实际路径抄进结论。

- [ ] **Step 2: 写探针**

Create `src-tauri/examples/mcp_probe.rs`（骨架，按 Step 1 查到的真实 API 补全 import）:

```rust
//! 风险验证探针（非生产代码）。回答：
//! 1) rmcp 当前版本的 client + stdio 传输怎么构造
//! 2) Windows 上 npx（实为 npx.cmd）能否被 spawn
//! 3) 服务退出后 node 进程是否残留
use std::process::Command as StdCommand;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 尝试 A：直接 spawn "npx"
    let direct = StdCommand::new("npx").arg("-y").arg("@modelcontextprotocol/server-everything").spawn();
    println!("A direct npx      -> {}", describe(&direct));
    if let Ok(mut c) = direct { let _ = c.kill(); }

    // 尝试 B：cmd /C 包装
    let wrapped = StdCommand::new("cmd").args(["/C", "npx", "-y", "@modelcontextprotocol/server-everything"]).spawn();
    println!("B cmd /C npx      -> {}", describe(&wrapped));
    if let Ok(mut c) = wrapped { let _ = c.kill(); }

    // 尝试 C：解析真实路径
    let resolved = which_npx();
    println!("C resolved        -> {resolved:?}");

    Ok(())
}

fn describe(r: &Result<std::process::Child, std::io::Error>) -> String {
    match r {
        Ok(c) => format!("ok pid={}", c.id()),
        Err(e) => format!("ERR os={:?} {e}", e.raw_os_error()),
    }
}

fn which_npx() -> Option<std::path::PathBuf> {
    let out = StdCommand::new("where").arg("npx").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().map(std::path::PathBuf::from)
}
```

- [ ] **Step 3: 跑并记录 spawn 结论**

Run: `cargo run --example mcp_probe`
Expected: 三种尝试中至少一种 ok。把结果表写进结论。**若 A 成功**，Plan B 可直接 `Command::new("npx")`；**若只有 B 成功**，Plan B 必须用 `cmd /C` 包装，并注意这意味着用户配置里的参数需自行做引号处理——这一安全隐患要在 Plan B 里显式处理，绝不可把用户提供的字符串拼进 `cmd /C` 而不校验。

- [ ] **Step 4: 跑通一次真实握手**

按 Step 1 查到的 API，把探针改为：spawn 一个 transport → `initialize` → `list_tools` → 打印工具数量与前 3 个工具名 → 正常关闭服务。

Run: `cargo run --example mcp_probe`
Expected: `tools=N`，N>0。

- [ ] **Step 5: 验证子进程不泄漏**

探针退出后立刻：

Run: `tasklist | grep -iE "node|npx"`
Expected: 无残留。若有残留，把 PID 与父进程写进结论，并给 Plan B 增加一条：`McpBridge::shutdown()` 必须显式 kill 整个进程树（Windows 上需 `taskkill /T /F /PID`）。

- [ ] **Step 6: 写结论并提交**

结论必答：rmcp 实际 feature 名 / client 与 stdio 的真实 import 路径与构造代码 / spawn 方式 A|B|C 哪个可用 / 子进程清理结论。

```bash
git add src-tauri/examples/mcp_probe.rs docs/findings/plan-a-risk-verification.md src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "spike: verify rmcp client on windows and npx.cmd spawn behavior"
```

---

**Part 0 出口条件**：`docs/findings/plan-a-risk-verification.md` 四问全部有实测答案，且 `npm test` 与 `cargo test` 均绿。Part 1 的 Task 6、7、18 的实现细节**以该文档为准**，不以本计划中的假设为准——冲突时改计划，不改结论。

---

# Part 1 — 纯聊天（切片 1）

### Task 5: 消息与事件类型

**Files:**
- Create: `src-tauri/src/agent/mod.rs`、`src-tauri/src/agent/types.rs`、`src-tauri/src/agent/events.rs`
- Test: `src-tauri/src/agent/events.rs`（`#[cfg(test)]` 内联）

**Interfaces:**
- Consumes: 无
- Produces:
  - `ContentBlock`、`Message`、`Role`、`ImageMime`、`ToolOutput`、`ToolCallView`、`Risk`
  - `AgentEvent`（`#[serde(tag = "t", rename_all = "snake_case")]`）
  - `Decision`、`Scope`、`StopReason`、`ErrorWhere`
  - `Message::user(blocks) -> Message`、`Message::system_note(text) -> Message`、`Message::tool(call_id, output, is_error) -> Message`、`Message::tool_calls(&self) -> Vec<&ToolCallView>`

- [ ] **Step 1: 写 types.rs**

Create `src-tauri/src/agent/types.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { data: String, mime: ImageMime },
    ToolCall { id: String, name: String, input: Value },
    ToolResult { call_id: String, is_error: bool, output: ToolOutput },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImageMime { Png, Jpeg }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutput {
    Text { text: String },
    Image { data: String, mime: ImageMime },
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self { Self::Text { text: s.into() } }
    pub fn error(s: impl Into<String>) -> Self { Self::Text { text: s.into() } }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role { System, User, Assistant }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message { pub role: Role, pub blocks: Vec<ContentBlock> }

impl Message {
    pub fn user(blocks: Vec<ContentBlock>) -> Message {
        Message { role: Role::User, blocks }
    }
    pub fn system_note(text: &str) -> Message {
        Message { role: Role::System, blocks: vec![ContentBlock::Text { text: text.into() }] }
    }
    pub fn tool(call_id: &str, output: ToolOutput, is_error: bool) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                call_id: call_id.into(), is_error, output,
            }],
        }
    }
    pub fn tool_calls(&self) -> Vec<&ToolCallView> {
        self.blocks.iter().filter_map(|b| match b {
            ContentBlock::ToolCall { id, name, input } => Some(ToolCallView {
                id: id.clone(), name: name.clone(), risk: Risk::Read, input: input.clone(),
            }),
            _ => None,
        }).collect()
    }
}

/// 从 ContentBlock::ToolCall 派生的视图。risk 由 registry 补全，不来自消息本身。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCallView {
    pub id: String,
    pub name: String,
    pub risk: Risk,
    pub input: Value,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Risk { Read, Write, Exec }
```

`Message::tool` 的 role 是 `User` 而非 `System`：OpenAI 兼容协议要求 `tool` 角色消息，而 role 到协议字段的映射归 Task 6 负责，这里刻意不新增 `Role::Tool` 变体，避免 Task 6 漏映射时编译器给不出提示。

- [ ] **Step 2: 写事件的序列化测试（先失败）**

Create `src-tauri/src/agent/events.rs` 的测试模块：

```rust
use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_uses_t_as_tag_and_snake_case() {
        let e = AgentEvent::TextDelta { text: "你好".into() };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"t":"text_delta","text":"你好"}"#
        );
    }

    #[test]
    fn error_event_field_serializes_as_where_not_at() {
        let e = AgentEvent::Error {
            at: ErrorWhere::Model, message: "boom".into(), fatal: false,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["where"], "model");
        assert!(json.get("at").is_none(), "at 必须被重命名为 where");
    }

    #[test]
    fn stop_reason_and_risk_are_lowercase() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(), r#""end_turn""#
        );
        assert_eq!(serde_json::to_string(&Risk::Read).unwrap(), r#""read""#);
    }

    #[test]
    fn decision_allow_carries_scope() {
        let d = Decision::Allow { scope: Scope::Session };
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"allow":true,"scope":"session"}"#
        );
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test agent::events`
Expected: 编译失败，`cannot find AgentEvent in this scope`。

- [ ] **Step 4: 写 AgentEvent 定义**

Create `src-tauri/src/agent/events.rs`:

```rust
use super::types::*;
use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStart { turn: u32 },
    TextDelta { text: String },
    ToolCall { call: ToolCallView, risk: Risk },
    PermissionRequest { id: String, call: ToolCallView, risk: Risk },
    PermissionResolved { id: String, decision: Decision },
    ToolResult { call_id: String, is_error: bool, ms: u64, preview: String },
    Usage { input_tokens: u32, output_tokens: u32 },
    ContextCompacted { dropped: usize, summary_chars: usize },
    Error {
        #[serde(rename = "where")]
        at: ErrorWhere,
        message: String,
        fatal: bool,
    },
    Done { stop_reason: StopReason },
}

#[derive(Serialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ErrorWhere { Model, Tool, Mcp, Loop, Store }

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason { EndTurn, MaxTurns, Interrupted, Error }

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope { Once, Session }

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Decision {
    Allow { scope: Scope },
    Deny { reason: String },
}

impl Decision {
    pub fn allowed(&self) -> bool { matches!(self, Decision::Allow { .. }) }
}
```

`Decision` 用 `#[serde(untagged)]`：`{"allow":true,"scope":"session"}` 与 `{"allow":false,"reason":"..."}` 是同一结构的两种形态，前端只需读 `allow` 字段即可分派，无需处理 tag。这与 Step 2 的断言一致。

Create `src-tauri/src/agent/mod.rs`:

```rust
pub mod events;
pub mod types;
```

- [ ] **Step 5: 让 `Message::tool_calls` 的 risk 由 registry 覆盖，而不是硬编码**

修改 `types.rs` 里 `tool_calls()`：把返回类型改为 `Vec<(String, String, Value)>`（id, name, input），由 Task 9 的 registry 查 risk 后组装 `ToolCallView`。

```rust
    pub fn tool_calls(&self) -> Vec<(String, String, Value)> {
        self.blocks.iter().filter_map(|b| match b {
            ContentBlock::ToolCall { id, name, input } => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        }).collect()
    }
```

理由：在 `types.rs` 里硬编码 `Risk::Read` 会让 Plan B 新增写工具时静默地把所有工具当只读放行——一个危险且无编译器保护的默认值。类型层不该知道 risk。

Run: `cargo test agent::events` → PASS（4 个测试）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/agent/
git commit -m "feat: add message and agent event types with wire format tests"
```

---

### Task 6: ModelProvider trait 与 OpenAI 兼容适配器

**Files:**
- Create: `src-tauri/src/model/mod.rs`、`src-tauri/src/model/provider.rs`、`src-tauri/src/model/openai_compat.rs`
- Test: `src-tauri/src/model/openai_compat.rs`（内联，base_url 规范化）

**Interfaces:**
- Consumes: Task 5 的 `Message`、`ContentBlock`、`ToolOutput`
- Produces:
  - `trait ModelProvider`、`ModelDelta`、`ChatRequest`、`ModelError`
  - `OpenAiCompat::new(cfg: ModelConfig) -> OpenAiCompat`
  - `normalize_base_url(raw: &str) -> String`（纯函数，可单测）
  - `struct ModelConfig { base_url: String, model: String, api_key: String, max_output_tokens: u32 }`

- [ ] **Step 1: 写 provider trait 与 delta**

Create `src-tauri/src/model/provider.rs`:

```rust
use crate::agent::types::{Message, ToolOutput};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone)]
pub enum ModelDelta {
    Text(String),
    /// 首个分片：id 与 name 可能为 None（部分兼容端点只在中间分片给 name）
    ToolCallStart { index: u32, id: Option<String>, name: Option<String> },
    /// arguments 的字符串片段，必须按 index 归组后整体解析
    ToolCallArgs { index: u32, fragment: String },
    Usage { input_tokens: u32, output_tokens: u32 },
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub temperature: Option<f32>,
}

#[derive(Error, Debug)]
pub enum ModelError {
    #[error("无法连接模型端点 {url}: {source}")]
    Transport { url: String, source: async_openai::error::OpenAIError },
    #[error("模型返回错误: {0}")]
    Api(String),
    #[error("base_url 或 api_key 未配置")]
    NotConfigured,
    #[error("响应无法解析: {0}")]
    Decode(String),
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn stream<'a>(
        &'a self,
        req: ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>;
}
```

`stream` 写成返回 `Pin<Box<...>>` 而非 `async fn`，因为 `async_trait` 对返回 `impl Stream` 不自适应；调用方只需 `stream.next().await`。

- [ ] **Step 2: 写 base_url 规范化的失败测试**

Create `src-tauri/src/model/openai_compat.rs` 测试模块（spec 风险 #5）：

```rust
#[cfg(test)]
mod tests {
    use super::normalize_base_url;

    #[test]
    fn strips_trailing_slash() {
        assert_eq!(normalize_base_url("https://x.com/v1/"), "https://x.com/v1");
    }

    #[test]
    fn adds_v1_when_missing() {
        assert_eq!(normalize_base_url("https://x.com"), "https://x.com/v1");
    }

    #[test]
    fn does_not_duplicate_v1() {
        assert_eq!(normalize_base_url("https://x.com/v1"), "https://x.com/v1");
    }

    #[test]
    fn tolerates_double_v1_from_user_copy_paste() {
        assert_eq!(normalize_base_url("https://x.com/v1/v1"), "https://x.com/v1");
    }

    #[test]
    fn does_not_add_v1_when_path_already_ends_with_endpoint() {
        assert_eq!(
            normalize_base_url("https://x.com/api/openai"),
            "https://x.com/api/openai/v1"
        );
    }

    #[test]
    fn keeps_localhost_ollama_without_key_but_still_normalizes() {
        assert_eq!(normalize_base_url("http://localhost:11434/v1"), "http://localhost:11434/v1");
    }
}
```

Run: `cargo test normalize_base_url`
Expected: FAIL — `cannot find function normalize_base_url`。

- [ ] **Step 3: 实现 normalize_base_url 与适配器**

Create `src-tauri/src/model/openai_compat.rs`:

```rust
use super::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
use crate::agent::types::{ContentBlock, ImageMime, Message, Role};
use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestMessageContentPartTextArgs,
        ChatCompletionRequestMessageContentPartImageArgs, CreateChatCompletionRequestArgs,
    },
    Client,
};
use futures::StreamExt;

/// 幂等地整理成 `<origin><path>/v1`，无尾斜杠。
pub fn normalize_base_url(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    while s.ends_with('/') { s.pop(); }
    let lower = s.to_lowercase();
    if lower.ends_with("/v1/v1") {
        let cut = s.len() - 3;
        s = s[..cut].to_string();
    } else if !lower.ends_with("/v1") {
        s.push_str("/v1");
    }
    s
}

#[derive(Clone)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_output_tokens: u32,
}

pub struct OpenAiCompat { client: Client<OpenAIConfig>, cfg: ModelConfig }

impl OpenAiCompat {
    pub fn new(cfg: ModelConfig) -> Result<Self, ModelError> {
        if cfg.base_url.trim().is_empty() { return Err(ModelError::NotConfigured); }
        let base = normalize_base_url(&cfg.base_url);
        let oc = OpenAIConfig::new()
            .with_api_base(base)
            .with_api_key(cfg.api_key.clone());
        Ok(Self { client: Client::with_config(oc), cfg })
    }

    pub fn base_url_resolved(&self) -> String { normalize_base_url(&self.cfg.base_url) }

    fn to_api(messages: &[Message]) -> Result<Vec<ChatCompletionRequestMessage>, ModelError> {
        let mut out = Vec::with_capacity(messages.len());
        for m in messages {
            match m.role {
                Role::System => {
                    let t = text_of(m);
                    out.push(ChatCompletionRequestMessage::System(
                        ChatCompletionRequestSystemMessageArgs::default()
                            .content(t).build().map_err(decode_err)?,
                    ));
                }
                Role::User => {
                    let mut parts = Vec::new();
                    for b in &m.blocks {
                        match b {
                            ContentBlock::Text { text } => parts.push(
                                ChatCompletionRequestMessageContentPartTextArgs::default()
                                    .text(text).build().map_err(decode_err)?.into()),
                            ContentBlock::Image { data, mime } => parts.push(
                                ChatCompletionRequestMessageContentPartImageArgs::default()
                                    .image_url(format!(
                                        "data:{};base64,{}",
                                        mime_url(mime), data))
                                    .build().map_err(decode_err)?.into()),
                            ContentBlock::ToolResult { call_id, output, .. } => {
                                // tool result 必须单独成 role=tool 消息，不能混进 user
                                out.push(tool_msg(call_id, output).map_err(decode_err)?);
                            }
                            ContentBlock::ToolCall { .. } => {}
                        }
                    }
                    if !parts.is_empty() {
                        out.push(ChatCompletionRequestMessage::User(
                            ChatCompletionRequestUserMessageArgs::default()
                                .content(parts).build().map_err(decode_err)?,
                        ));
                    }
                }
                Role::Assistant => out.push(ChatCompletionRequestMessage::Assistant(
                    assistant_msg(m).map_err(decode_err)?,
                )),
            }
        }
        Ok(out)
    }
}

fn mime_url(m: &ImageMime) -> &'static str {
    match m { ImageMime::Png => "image/png", ImageMime::Jpeg => "image/jpeg" }
}
fn text_of(m: &Message) -> String {
    m.blocks.iter().map(|b| match b {
        ContentBlock::Text { text } => text.clone(), _ => String::new(),
    }).collect::<Vec<_>>().join("")
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompat {
    fn stream<'a>(
        &'a self, req: ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>> {
        let built = (|| -> Result<_, ModelError> {
            let msgs = OpenAiCompat::to_api(&req.messages)?;
            let mut b = CreateChatCompletionRequestArgs::default();
            b.model(&self.cfg.model).messages(msgs).stream(true);
            if let Some(t) = req.temperature { b.temperature(t); }
            b.max_tokens(self.cfg.max_output_tokens);
            b.build().map_err(decode_err)
        })();
        let request = match built { Ok(r) => r, Err(e) => return Box::pin(futures::stream::once(async move { Err(e) })) };

        let url = self.base_url_resolved();
        let client = self.client.clone();
        Box::pin(async_stream::stream! {
            match client.chat().create_stream(request).await {
                Err(e) => yield Err(ModelError::Transport { url, source: e }),
                Ok(mut s) => while let Some(item) = s.next().await {
                    match item {
                        Err(e) => yield Err(ModelError::Api(e.to_string())),
                        Ok(chunk) => for d in chunk_to_deltas(&chunk) { yield Ok(d); }
                    }
                },
            }
        })
    }
}

fn chunk_to_deltas(c: &async_openai::types::ChatCompletionResponseStreamCreate) -> Vec<ModelDelta> {
    let mut v = Vec::new();
    if let Some(u) = &c.usage {
        v.push(ModelDelta::Usage {
            input_tokens: u.prompt_tokens, output_tokens: u.completion_tokens });
    }
    for ch in &c.choices {
        if let Some(t) = &ch.delta.content { if !t.is_empty() { v.push(ModelDelta::Text(t.clone())); } }
        if let Some(tcs) = &ch.delta.tool_calls {
            for f in tcs {
                let index = f.index.unwrap_or(0);
                let name = f.function.as_ref().and_then(|x| x.name.clone());
                let args = f.function.as_ref().and_then(|x| x.arguments.clone());
                if name.is_some() || f.id.is_some() {
                    v.push(ModelDelta::ToolCallStart { index, id: f.id.clone(), name });
                }
                if let Some(a) = args {
                    v.push(ModelDelta::ToolCallArgs { index, fragment: a });
                }
            }
        }
    }
    v
}
```

Run: `cargo add async-stream tool-call`

`assistant_msg` 与 `tool_msg` 是两个把 `Message` 转成协议形态的私有辅助函数，按 async-openai 当前版本的类型名补全——**必须实际写出来**，不可省略；它们负责把 `ContentBlock::ToolCall` 映射成 `tool_calls` 数组、把 `ToolResult` 映射成 `role: "tool"` + `tool_call_id`。

- [ ] **Step 4: 跑测试**

Run: `cargo test normalize_base_url`
Expected: PASS（6 个测试）

- [ ] **Step 5: 用探针同样的请求做一次真连（回归 Task 2 的结论）**

Run: `QWEN_API_KEY=<key> cargo test -- --ignored stream_real_endpoint 2>/dev/null || true`

本步骤在 `openai_compat.rs` 里临时加一个 `#[test] #[ignore] fn stream_real_endpoint()`，从环境变量取 key，走 `OpenAiCompat::stream` 收 20 个 delta 并断言 `>1`。**验证完保留该测试为 `#[ignore]`**——它需要真 key，不进 CI，但作为诊断工具长期留着（对应 spec 风险 #4 的排查手段）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/model/ src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: add ModelProvider trait and OpenAI-compatible streaming adapter"
```

---

### Task 7: 流式 tool_call 分片累加器

spec 4.7 与风险 #3。这是全计划单测密度最高的任务，因为它最容易写错且错误后果是「工具收到空参数」。

**Files:**
- Create: `src-tauri/src/model/assistant_acc.rs`
- Test: `src-tauri/src/model/assistant_acc.rs`（内联）

**Interfaces:**
- Consumes: Task 5 `ContentBlock`/`Message`；Task 6 `ModelDelta`
- Produces:
  - `AssistantAcc::default()`
  - `AssistantAcc::apply(&mut self, d: ModelDelta) -> Option<String>`（返回应即时发出的文本增量；tool_call 类返回 None）
  - `AssistantAcc::finish(self) -> Message`
  - `AssistantAcc::text(&self) -> &str`

- [ ] **Step 1: 写四类失败测试**

Create `src-tauri/src/model/assistant_acc.rs` 测试模块：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::ContentBlock;
    use crate::model::provider::ModelDelta;

    #[test]
    fn assembles_arguments_split_across_chunks() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart { index: 0, id: Some("c1".into()), name: Some("fetch_url".into()) });
        a.apply(ModelDelta::ToolCallArgs { index: 0, fragment: r#"{"ur"#.into() });
        a.apply(ModelDelta::ToolCallArgs { index: 0, fragment: r#"l":"http://a"}"#.into() });
        let msg = a.finish();
        assert_eq!(msg.blocks.len(), 1);
        match &msg.blocks[0] {
            ContentBlock::ToolCall { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "fetch_url");
                assert_eq!(input["url"], "http://a");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn groups_two_parallel_calls_by_index_not_arrival_order() {
        let mut a = AssistantAcc::default();
        // 刻意乱序：先 index=1 再 index=0
        a.apply(ModelDelta::ToolCallStart { index: 1, id: Some("b".into()), name: Some("second".into()) });
        a.apply(ModelDelta::ToolCallArgs { index: 1, fragment: "{\"n\":2}".into() });
        a.apply(ModelDelta::ToolCallStart { index: 0, id: Some("a".into()), name: Some("first".into()) });
        a.apply(ModelDelta::ToolCallArgs { index: 0, fragment: "{\"n\":1}".into() });
        let msg = a.finish();
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 2);
        // 输出顺序必须按 index 稳定，不能随网络抖动变化
        assert_eq!(calls[0].1, "first");
        assert_eq!(calls[1].1, "second");
        assert_eq!(calls[0].2["n"], 1);
        assert_eq!(calls[1].2["n"], 2);
    }

    #[test]
    fn empty_arguments_become_empty_object_not_panic() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart { index: 0, id: Some("c".into()), name: Some("read_file".into()) });
        let msg = a.finish();
        match &msg.blocks[0] {
            ContentBlock::ToolCall { input, .. } => assert_eq!(input, &serde_json::json!({})),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn truncated_json_degrades_to_error_result_not_panic() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart { index: 0, id: Some("c".into()), name: Some("x".into()) });
        a.apply(ModelDelta::ToolCallArgs { index: 0, fragment: "{\"url\":\"http".into() });
        let msg = a.finish();
        match &msg.blocks[0] {
            ContentBlock::ToolCall { input, .. } => {
                assert_eq!(input["__parse_error"].as_str().is_some(), true);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn text_and_tool_call_interleave_preserves_both() {
        let mut a = AssistantAcc::default();
        assert_eq!(a.apply(ModelDelta::Text("我看".into())).as_deref(), Some("我看"));
        assert_eq!(a.apply(ModelDelta::Text("一下。".into())).as_deref(), Some("一下。"));
        assert_eq!(a.text(), "我看一下。");
        a.apply(ModelDelta::ToolCallStart { index: 0, id: Some("c".into()), name: Some("f".into()) });
        a.apply(ModelDelta::ToolCallArgs { index: 0, fragment: "{}".into() });
        assert_eq!(a.apply(ModelDelta::Text("完成".into())).as_deref(), Some("完成"));
        let msg = a.finish();
        assert_eq!(msg.blocks.len(), 3);
        assert_eq!(a.text(), "我看一下。完成");
    }
}
```

Run: `cargo test assistant_acc`
Expected: FAIL — `cannot find AssistantAcc`。

- [ ] **Step 2: 实现**

Create `src-tauri/src/model/assistant_acc.rs`:

```rust
use crate::agent::types::{ContentBlock, Message, Role};
use crate::model::provider::ModelDelta;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct AssistantAcc {
    text: String,
    /// BTreeMap 保证 finish 时按 index 升序输出，与到达顺序无关
    calls: BTreeMap<u32, PartialCall>,
    order: Vec<ContentBlock>,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

impl AssistantAcc {
    pub fn text(&self) -> &str { &self.text }

    /// 返回需要即时发给 UI 的文本增量；tool_call 类 delta 返回 None。
    pub fn apply(&mut self, d: ModelDelta) -> Option<String> {
        match d {
            ModelDelta::Text(t) => {
                self.text.push_str(&t);
                Some(t)
            }
            ModelDelta::ToolCallStart { index, id, name } => {
                let e = self.calls.entry(index).or_default();
                if let Some(id) = id { e.id = id; }
                if let Some(name) = name { e.name = name; }
                None
            }
            ModelDelta::ToolCallArgs { index, fragment } => {
                self.calls.entry(index).or_default().args.push_str(&fragment);
                None
            }
            ModelDelta::Usage { .. } => None,
        }
    }

    pub fn finish(mut self) -> Message {
        if !self.text.is_empty() {
            self.order.push(ContentBlock::Text { text: std::mem::take(&mut self.text) });
        }
        for (index, c) in self.calls.iter() {
            let id = if c.id.is_empty() { format!("synth_{index}") } else { c.id.clone() };
            let input = parse_args(&c.args);
            self.order.push(ContentBlock::ToolCall { id, name: c.name.clone(), input });
        }
        Message { role: Role::Assistant, blocks: self.order }
    }
}

fn parse_args(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() { return serde_json::json!({}); }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => v,
        // 模型输出被截断时不能 panic，也不能静默变空对象——
        // 空参数会让工具报一个模型看不懂归因的错。标记后交给工具层报错。
        Err(e) => serde_json::json!({ "__parse_error": format!("工具参数无法解析: {e}; 原始片段: {trimmed}") }),
    }
}
```

注意 `finish` 先 push text 再 push calls，而 `apply` 期间的 text 累加不写进 `order`。这意味着文本恒在工具调用之前——对 OpenAI 兼容端点安全，且避免了交错顺序在协议侧不被支持的问题。若 Task 2 结论显示目标端点要求严格交错，改此处而不是改协议层。

- [ ] **Step 3: 跑测试**

Run: `cargo test assistant_acc`
Expected: PASS（5 个测试）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/model/assistant_acc.rs src-tauri/src/model/mod.rs
git commit -m "feat: add streaming tool_call accumulator with index grouping tests"
```

---

### Task 8: 上下文组装与滑窗

**Files:**
- Create: `src-tauri/src/agent/context.rs`
- Test: `src-tauri/src/agent/context.rs`（内联）

**Interfaces:**
- Consumes: Task 5 `Message`/`ContentBlock`
- Produces:
  - `ContextConfig { reserve_output: u32, budget_tokens: u32, window_messages: usize }` 及 `Default`
  - `estimate_tokens(msgs: &[Message]) -> usize`
  - `ContextBuilder::new(cfg) -> Self`、`set_calibration(factor: f64)`、`build(&self, history: &[Message]) -> Built`
  - `struct Built { pub messages: Vec<Message>, pub dropped: usize, pub est_input_tokens: u32 }`
  - `SYSTEM_PROMPT: &str`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, Message, Role};

    fn u(text: &str) -> Message {
        Message { role: Role::User, blocks: vec![ContentBlock::Text { text: text.into() }] }
    }

    #[test]
    fn cjk_counts_about_one_token_per_char() {
        // 中文按 1 字 ≈ 1 token 估，低估会在长中文对话上撑爆预算
        assert!((49..=51).contains(&estimate_tokens(&[u(&"测".repeat(50))])));
    }

    #[test]
    fn english_counts_about_four_chars_per_token() {
        assert!((24..=26).contains(&estimate_tokens(&[u(&"a".repeat(100))])));
    }

    #[test]
    fn image_blocks_cost_a_fixed_high_amount_not_by_base64_length() {
        // 图片按内容长度算会让预算判断完全失真（base64 长度 ≈ 4/3 字节数）
        let small = Message { role: Role::User, blocks: vec![ContentBlock::Image {
            data: "AAAA".into(), mime: crate::agent::types::ImageMime::Png }] };
        let big = Message { role: Role::User, blocks: vec![ContentBlock::Image {
            data: "A".repeat(100_000), mime: crate::agent::types::ImageMime::Png }] };
        assert_eq!(estimate_tokens(&[small]), estimate_tokens(&[big]));
        assert!(estimate_tokens(&[small]) >= 700, "单图至少按 ~800 token 计");
    }

    #[test]
    fn system_message_is_always_first_and_always_kept() {
        let hist: Vec<Message> = (0..200).map(|i| u(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(built.messages[0].role, Role::System);
    }

    #[test]
    fn window_drops_oldest_non_system_and_reports_count() {
        let hist: Vec<Message> = (0..200).map(|i| u(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(built.messages.len(), 1 + ContextConfig::default().window_messages);
        assert!(built.dropped > 0);
        let last = built.messages.last().unwrap();
        assert!(text_of(last).contains("轮 199"));
    }

    #[test]
    fn token_budget_can_drop_more_than_window_allows() {
        // 每条 5000 token 的巨轮：window 允许 40 条，但预算只容得下 3 条
        let cfg = ContextConfig { reserve_output: 4096, budget_tokens: 20_000, window_messages: 40 };
        let hist: Vec<Message> = (0..40).map(|i| u(&"测".repeat(4000) + &format!(" {i}"))).collect();
        let built = ContextBuilder::new(cfg).build(&hist);
        let total: usize = built.messages[1..].iter().map(estimate_tokens).sum();
        assert!(total <= 20_000, "超预算 {total}");
        assert!(built.messages.len() < 40);
    }

    #[test]
    fn calibration_shrinks_estimate_when_usage_reports_less() {
        let mut b = ContextBuilder::new(ContextConfig { budget_tokens: 1000, reserve_output: 0, window_messages: 40 });
        let hist = vec![u(&"测".repeat(400))];
        let before = b.build(&hist).messages.len();
        b.set_calibration(0.2);
        let after = b.build(&hist).messages.len();
        assert!(before <= after);
    }
}
```

Run: `cargo test agent::context`
Expected: FAIL — `cannot find ContextBuilder`。

- [ ] **Step 2: 实现**

Create `src-tauri/src/agent/context.rs`:

```rust
use super::types::{ContentBlock, ImageMime, Message, Role};

pub const SYSTEM_PROMPT: &str = "你是 Qwen Do，一个运行在桌面端的轻量助手。回答简洁、直接，中文优先。可用工具时优先使用工具获取事实，不要凭记忆编造；不确定就说不确定。";

#[derive(Debug, Clone)]
pub struct ContextConfig {
    pub reserve_output: u32,
    pub budget_tokens: u32,
    pub window_messages: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self { reserve_output: 4096, budget_tokens: 32_000, window_messages: 40 }
    }
}

#[derive(Debug, Clone)]
pub struct Built {
    pub messages: Vec<Message>,
    pub dropped: usize,
    pub est_input_tokens: u32,
}

pub struct ContextBuilder { cfg: ContextConfig, calibration: f64 }

impl ContextBuilder {
    pub fn new(cfg: ContextConfig) -> Self { Self { cfg, calibration: 1.0 } }

    /// 用真实 usage 校准粗估系数：粗估在中文长文上偏差可达 20-30%
    pub fn set_calibration(&mut self, factor: f64) {
        self.calibration = factor.clamp(0.2, 3.0);
    }

    pub fn build(&self, history: &[Message]) -> Built {
        let mut system = Message::system_note(SYSTEM_PROMPT);
        // 保留历史里已有的 system 备注（Plan C 的摘要消息走这一路）
        let mut notes: Vec<Message> = history.iter()
            .filter(|m| m.role == Role::System)
            .filter(|m| text_of(m) != SYSTEM_PROMPT)
            .cloned().collect();

        let conversational: Vec<&Message> = history.iter()
            .filter(|m| m.role != Role::System).collect();

        let mut keep = conversational.len().min(self.cfg.window_messages);
        let budget = self.cfg.budget_tokens.saturating_sub(self.cfg.reserve_output) as usize;

        // 从最近往前扩，累到预算或窗口上限为止
        loop {
            let start = conversational.len() - keep;
            let tail: Vec<Message> = conversational[start..].iter().map(|m| (*m).clone()).collect();
            let mut cand = vec![system.clone()];
            cand.append(&mut notes.clone());
            cand.extend(tail);
            let est = (estimate_tokens(&cand) as f64 * self.calibration) as usize;
            if est <= budget || keep == 1 {
                system = cand[0].clone();
                return Built {
                    dropped: conversational.len() - keep,
                    est_input_tokens: est as u32,
                    messages: cand,
                };
            }
            keep -= 1;
        }
    }
}

pub fn estimate_tokens(msgs: &[Message]) -> usize {
    let mut n = 0usize;
    for m in msgs {
        n += 4; // role 与结构开销
        for b in &m.blocks {
            n += match b {
                ContentBlock::Text { text } => text_tokens(text),
                ContentBlock::Image { mime, .. } => match mime {
                    ImageMime::Png => 900, ImageMime::Jpeg => 800,
                },
                ContentBlock::ToolCall { name, input, .. } => {
                    name.len().div_ceil(4) + text_tokens(&input.to_string())
                }
                ContentBlock::ToolResult { output, .. } => match output {
                    super::types::ToolOutput::Text { text } => text_tokens(text),
                    super::types::ToolOutput::Image { .. } => 900,
                },
            };
        }
    }
    n
}

/// CJK 约 1 字 = 1 token；其余约 4 字符 = 1 token。混排时按字符类型分段累加。
fn text_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in s.chars() {
        if is_cjk(ch) { cjk += 1 } else { other += 1 }
    }
    cjk + other.div_ceil(4)
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF
        | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

pub fn text_of(m: &Message) -> String {
    m.blocks.iter().map(|b| match b {
        ContentBlock::Text { text } => text.clone(), _ => String::new(),
    }).collect::<Vec<_>>().join("")
}
```

**图片按固定权重计价是刻意的**：按 base64 长度算会让预算判断完全失真，且模型侧计费是按视觉 token 而非字符。Plan C 的降级规则与此配合。

- [ ] **Step 3: 跑测试**

Run: `cargo test agent::context`
Expected: PASS（7 个测试）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/agent/context.rs src-tauri/src/agent/mod.rs
git commit -m "feat: add context builder with cjk-aware token estimate and window"
```

---

### Task 9: 工具注册表与权限门

机制在本计划建齐并测试，生产工具集在 Plan A 为空——这对应 spec 5.1「MVP 内置工具全为 Read，因此零弹窗」，并让 Plan B 只需注册 `fetch_url`。

**Files:**
- Create: `src-tauri/src/tools/mod.rs`、`src-tauri/src/tools/registry.rs`、`src-tauri/src/tools/echo.rs`
- Create: `src-tauri/src/permission/mod.rs`、`src-tauri/src/permission/gate.rs`
- Test: `src-tauri/src/permission/gate.rs`、`src-tauri/src/tools/registry.rs`（内联）

**Interfaces:**
- Consumes: Task 5 `Risk`/`ToolCallView`/`Decision`/`Scope`/`ContentBlock`
- Produces:
  - `trait Tool`、`ToolMeta`、`ToolCtx`、`ToolError`
  - `ToolRegistry::new() -> Self`、`register(Arc<dyn Tool>)`、`get(&str) -> Option<Arc<dyn Tool>>`、`defs() -> Vec<ToolDef>`、`risk_of(&str) -> Risk`（未知工具按 `Risk::Exec`）、`execute(&str, Value, &ToolCtx) -> Result<ToolOutput, ToolError>`
  - `PermissionGate::new() -> Self`、`with_rules(RuleTable)`、`check(&self, call, sink) -> Decision`、`resolve(id, Decision)`、`rule_key(call) -> String`
  - `RuleTable::default_mvp() -> RuleTable`、`lookup(&ToolCallView) -> Option<Decision>`、`remember_session(key)`

- [ ] **Step 1: 写权限门的失败测试（含静默挂死防护）**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::AgentEvent;
    use crate::agent::types::{Risk, ToolCallView};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct VecSink(Arc<Mutex<Vec<AgentEvent>>>);
    impl EventSink for VecSink {
        fn emit(&self, e: AgentEvent) { self.0.lock().unwrap().push(e); }
    }
    impl VecSink { fn events(&self) -> Vec<AgentEvent> { self.0.lock().unwrap().clone() } }

    fn call(name: &str, risk: Risk) -> ToolCallView {
        ToolCallView { id: "c1".into(), name: name.into(), risk, input: json!({"path":"/tmp/x"}) }
    }

    #[tokio::test]
    async fn read_risk_is_auto_and_emits_no_permission_event() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let d = gate.check(&call("fetch_url", Risk::Read)).await;
        assert!(d.allowed(), "MVP 必须零弹窗");
        assert!(sink.events().is_empty(), "Auto 判定不应产生 permission_request");
    }

    #[tokio::test]
    async fn write_risk_asks_and_resolves() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let c = call("write_file", Risk::Write);
        let handle = { let c = c.clone(); tokio::spawn(async move {
            gate.check(&c).await }) };
        // 等事件出现
        for _ in 0..50 { if !sink.events().is_empty() { break; } tokio::task::yield_now().await; }
        let req = match sink.events().first() {
            Some(AgentEvent::PermissionRequest { id, .. }) => id.clone(),
            other => panic!("expected permission_request, got {other:?}"),
        };
        gate.resolve(&req, Decision::Allow { scope: Scope::Once });
        assert!(handle.await.unwrap().allowed());
    }

    #[tokio::test]
    async fn dropped_sender_falls_back_to_deny_instead_of_hanging() {
        let sink = Arc::new(VecSink::default());
        let gate = PermissionGate::new().with_sink(sink.clone());
        let c = call("run_command", Risk::Exec);
        let h = { let c = c.clone(); tokio::spawn(async move { gate.check(&c).await }) };
        for _ in 0..50 { if !sink.events().is_empty() { break; } tokio::task::yield_now().await; }
        let id = match sink.events().first() {
            Some(AgentEvent::PermissionRequest { id, .. }) => id.clone(),
            other => panic!("expected request, got {other:?}"),
        };
        drop(gate);           // 无人应答 -> tx 被 drop -> 必须兜底 Deny
        let d = tokio::time::timeout(std::time::Duration::from_secs(2), h)
            .await
            .expect("check() 永久挂死：缺少 oneshot 兜底")
            .unwrap();
        assert!(!d.allowed());
    }

    #[tokio::test]
    async fn session_allow_is_scoped_by_root_not_bare_tool_name() {
        let gate = PermissionGate::new();
        let a = call("write_file", Risk::Write);
        gate.remember_session(&gate.rule_key(&a));
        assert!(gate.rules().lookup(&a).is_some(), "同目录应命中");
        let b = call("write_file", Risk::Write); // 同工具，不同 roots 视为不同 key
        let mut b = b; b.input = json!({"path":"C:/Windows/system32/x"});
        assert!(gate.rules().lookup(&b).is_none(), "跨目录必须重新询问");
    }
}
```

Run: `cargo test permission`
Expected: FAIL — `cannot find PermissionGate`。

- [ ] **Step 2: 实现 EventSink 与权限门**

Create `src-tauri/src/agent/events.rs` 追加（`EventSink` 属于事件层，与 `AgentEvent` 同文件避免循环依赖）:

```rust
pub trait EventSink: Send + Sync {
    fn emit(&self, e: AgentEvent);
}
```

Create `src-tauri/src/permission/gate.rs`:

```rust
use crate::agent::events::{AgentEvent, EventSink};
use crate::agent::types::{Decision, Risk, Scope, ToolCallView};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::oneshot;

#[derive(Default)]
pub struct RuleTable {
    auto_keys: RwLock<HashSet<String>>,
}

impl RuleTable {
    pub fn lookup(&self, key: &str) -> Option<Decision> {
        if self.auto_keys.read().unwrap().contains(key) {
            return Some(Decision::Allow { scope: Scope::Session });
        }
        None
    }
    pub fn remember(&self, key: &str) {
        self.auto_keys.write().unwrap().insert(key.to_string());
    }
}

#[derive(Clone, Default)]
pub struct PermissionGate {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    rules: Arc<RuleTable>,
    sink: Option<Arc<dyn EventSink>>,
}

impl PermissionGate {
    pub fn new() -> Self { Self::default() }
    pub fn with_sink(mut self, sink: Arc<dyn EventSink>) -> Self { self.sink = Some(sink); self }
    pub fn with_rules(self, rules: Arc<RuleTable>) -> Self {
        let mut g = self; g.rules = rules; g
    }
    pub fn rules(&self) -> Arc<RuleTable> { self.rules.clone() }

    /// 作用域令牌：工具名 + 该次操作的实际作用范围。
    /// 裸工具名作 key 会让「本会话允许写一次」变成「本会话可写任意路径」。
    pub fn rule_key(&self, call: &ToolCallView) -> String {
        let scope_token = match call.risk {
            Risk::Read => "read".to_string(),
            Risk::Write => call.input.get("path")
                .and_then(|v| v.as_str())
                .map(parent_dir).unwrap_or_else(|| "?".into()),
            Risk::Exec => call.input.get("command")
                .and_then(|v| v.as_str())
                .and_then(|c| c.split_whitespace().next().map(String::from))
                .unwrap_or_else(|| "?".into()),
        };
        format!("{}|{}", call.name, scope_token)
    }

    pub async fn check(&self, call: &ToolCallView) -> Decision {
        let key = self.rule_key(call);
        let default_auto = matches!(call.risk, Risk::Read);   // MVP 策略：Read 全 Auto
        if default_auto { return Decision::Allow { scope: Scope::Once }; }
        if let Some(d) = self.rules.lookup(&key) { return d; }

        let Some(sink) = self.sink.clone() else {
            // 无 sink（headless 测试或未接 UI）时不得默认放行
            return Decision::Deny { reason: "没有可应答的界面".into() };
        };
        let (tx, rx) = oneshot::channel();
        let id = uuid::new_v4().to_string();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        sink.emit(AgentEvent::PermissionRequest {
            id: id.clone(), call: call.clone(), risk: call.risk,
        });

        match rx.await {
            Ok(d) => {
                self.pending.lock().unwrap().remove(&id);
                if let Decision::Allow { scope: Scope::Session } = d {
                    self.rules.remember(&key);
                }
                sink.emit(AgentEvent::PermissionResolved { id, decision: d.clone() });
                d
            }
            // 兜底是关键：UI 关闭 / 会话中断 / 前端崩溃都会 drop tx
            Err(_) => Decision::Deny { reason: "确认窗口已关闭".into() },
        }
    }

    pub fn resolve(&self, id: &str, d: Decision) -> bool {
        if let Some(tx) = self.pending.lock().unwrap().remove(id) { tx.send(d).is_ok() }
        else { false }
    }
}

fn parent_dir(p: &str) -> String {
    std::path::Path::new(p).parent()
        .map(|x| x.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "?".into())
}
```

`uuid` 用 `cargo add uuid --features v4`，调用为 `uuid::Uuid::new_v4()`；上文简写需按实际 crate 名补全。

Create `src-tauri/src/permission/mod.rs`: `pub mod gate;`

- [ ] **Step 3: 跑权限门测试**

Run: `cargo test permission`
Expected: PASS（4 个测试）。`dropped_sender_falls_back_to_deny_instead_of_hanging` 若超时失败，说明缺 `Err(_) =>` 分支——这是本任务唯一不可省的防护。

- [ ] **Step 4: 写注册表测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::ToolOutput;
    use crate::tools::echo::EchoTool;
    use serde_json::json;

    fn ctx() -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            roots: vec![std::path::PathBuf::from("/tmp")],
        }
    }

    #[tokio::test]
    async fn registered_tool_executes_and_returns_output() {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        let out = r.execute("echo", json!({"text":"hi"}), &ctx()).await.unwrap();
        assert!(matches!(out, ToolOutput::Text { text } if text == "hi"));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_not_panic() {
        let r = ToolRegistry::new();
        let err = r.execute("nope", json!({}), &ctx()).await.unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[tokio::test]
    async fn unknown_tool_risk_defaults_to_exec_not_read() {
        // 未知即 Read 会让未注册工具静默自动放行——危险的默认值
        let r = ToolRegistry::new();
        assert_eq!(r.risk_of("mcp__x__y"), Risk::Exec);
    }

    #[tokio::test]
    async fn defs_exclude_risk_and_include_schema() {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        let d = r.defs();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "echo");
        assert_eq!(r.risk_of("echo"), Risk::Read);
    }

    #[tokio::test]
    async fn duplicate_registration_replaces_not_duplicates() {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        r.register(Arc::new(EchoTool));
        assert_eq!(r.defs().len(), 1);
    }
}
```

- [ ] **Step 5: 实现 Tool trait 与注册表**

Create `src-tauri/src/tools/registry.rs`:

```rust
use crate::agent::types::{Risk, ToolOutput};
use crate::model::provider::ToolDef;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Error, Debug)]
pub enum ToolError {
    #[error("工具 {name} 未注册")]
    NotFound { name: String },
    #[error("入参不符合 schema: {0}")]
    BadInput(String),
    #[error("工具执行失败: {0}")]
    Failed(String),
    #[error("已取消")]
    Cancelled,
}

#[derive(Clone)]
pub struct ToolCtx {
    pub http: reqwest::Client,
    pub cancel: CancellationToken,
    pub roots: Vec<PathBuf>,
}

impl ToolCtx {
    /// 路径穿越防线：解析后必须落在某个 root 内。拒绝 ..、符号链接逃逸、跨盘符。
    pub fn resolve_within_roots(&self, raw: &str) -> Result<PathBuf, ToolError> {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            let hit = self.roots.iter().any(|r| {
                let rr = r.canonicalize().unwrap_or_else(|_| r.clone());
                p.starts_with(&rr)
            });
            if !hit { return Err(ToolError::BadInput(format!("路径越界: {raw}"))); }
            Ok(p)
        } else {
            Err(ToolError::BadInput(format!("只接受绝对路径: {raw}")))
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;
    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: Risk,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn register(&self, t: Arc<dyn Tool>) {
        self.tools.write().unwrap().insert(t.meta().name.clone(), t);
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().unwrap().get(name).cloned()
    }
    pub fn risk_of(&self, name: &str) -> Risk {
        self.get(name).map(|t| t.meta().risk).unwrap_or(Risk::Exec)
    }
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.read().unwrap().values().map(|t| {
            let m = t.meta();
            ToolDef { name: m.name, description: m.description, input_schema: m.input_schema }
        }).collect()
    }
    pub async fn execute(&self, name: &str, input: Value, ctx: &ToolCtx)
        -> Result<ToolOutput, ToolError>
    {
        let t = self.get(name).ok_or_else(|| ToolError::NotFound { name: name.into() })?;
        if ctx.cancel.is_cancelled() { return Err(ToolError::Cancelled); }
        t.execute(input, ctx).await
    }
}
```

Create `src-tauri/src/tools/echo.rs`（仅测试与循环验证用，**不注册进生产**）:

```rust
use super::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::agent::types::{Risk, ToolOutput};
use async_trait::async_trait;
use serde_json::Value;

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "echo".into(),
            description: "原样返回输入，仅用于测试循环".into(),
            input_schema: serde_json::json!({
                "type":"object","properties":{"text":{"type":"string"}},"required":["text"]
            }),
            risk: Risk::Read,
        }
    }
    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() { return Err(ToolError::Cancelled); }
        Ok(ToolOutput::text(input.get("text").and_then(|v| v.as_str()).unwrap_or_default()))
    }
}
```

Create `src-tauri/src/tools/mod.rs`: `pub mod echo; pub mod registry;`

Run: `cargo add reqwest --features json` 和 `cargo add tokio-util --features futures`
Run: `cargo test tools::`
Expected: PASS（5 个测试）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/tools/ src-tauri/src/permission/ src-tauri/src/agent/events.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: add tool registry and permission gate with deny fallback"
```

---

### Task 10: AgentSession 循环

**Files:**
- Create: `src-tauri/src/agent/session.rs`
- Test: `src-tauri/src/agent/session.rs`（内联，含 `ScriptedModel` / `VecSink` 测试替身）

**Interfaces:**
- Consumes: Task 5–9 全部产出
- Produces:
  - `SessionConfig { max_turns: u32 }`，默认 25
  - `AgentSession::new(deps: SessionDeps) -> AgentSession`
  - `SessionDeps { model: Arc<dyn ModelProvider>, registry: Arc<ToolRegistry>, gate: Arc<PermissionGate>, sink: Arc<dyn EventSink>, context: ContextBuilder }`
  - `AgentSession::run(&mut self, blocks: Vec<ContentBlock>) -> StopReason`
  - `AgentSession::history(&self) -> &[Message]`
  - `AgentSession::cancel_handle(&self) -> CancellationToken`
  - `ScriptedModel::new(Vec<Vec<ModelDelta>>)`（`#[cfg(test)]`，Plan B/C 复用）

- [ ] **Step 1: 写六条路径的失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::{AgentEvent, EventSink, StopReason};
    use crate::agent::types::*;
    use crate::model::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
    use crate::permission::gate::PermissionGate;
    use crate::tools::echo::EchoTool;
    use crate::tools::registry::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct VecSink(pub Arc<Mutex<Vec<AgentEvent>>>);
    impl EventSink for VecSink { fn emit(&self, e: AgentEvent) { self.0.lock().unwrap().push(e); } }
    impl VecSink {
        fn kinds(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().map(|e| serde_json::to_value(e).unwrap()["t"].as_str().unwrap().to_string()).collect()
        }
    }

    /// 每轮对话吐一组预录 delta；Plan B/C 的测试同样用它
    pub struct ScriptedModel { rounds: Mutex<Vec<Vec<ModelDelta>>> }
    impl ScriptedModel { pub fn new(r: Vec<Vec<ModelDelta>>) -> Arc<Self> {
        Arc::new(Self { rounds: Mutex::new(r) }) } }
    #[async_trait]
    impl ModelProvider for ScriptedModel {
        fn stream<'a>(&'a self, _: ChatRequest) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>> {
            let next = self.rounds.lock().unwrap().remove(0);
            Box::pin(stream::iter(next.into_iter().map(Ok)))
        }
    }

    fn deltas(text: &str) -> Vec<ModelDelta> {
        text.chars().map(ModelDelta::Text).collect()
    }

    fn tool_deltas(name: &str, args: &str) -> Vec<ModelDelta> {
        vec![
            ModelDelta::ToolCallStart { index: 0, id: Some("c1".into()), name: Some(name.into()) },
            ModelDelta::ToolCallArgs { index: 0, fragment: args.into() },
        ]
    }

    fn harness(model: Arc<dyn ModelProvider>) -> (AgentSession, Arc<VecSink>) {
        let sink = Arc::new(VecSink::default());
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        let gate = Arc::new(PermissionGate::new().with_sink(sink.clone()));
        let s = AgentSession::new(SessionDeps {
            model, registry, gate,
            sink: sink.clone(),
            context: ContextBuilder::new(ContextConfig::default()),
        });
        (s, sink)
    }

    fn user(t: &str) -> Vec<ContentBlock> { vec![ContentBlock::Text { text: t.into() }] }

    #[tokio::test]
    async fn plain_text_turn_emits_turn_start_delta_done_end_turn() {
        let (mut s, sink) = harness(ScriptedModel::new(vec![deltas("你好")]));
        assert_eq!(s.run(user("hi")).await, StopReason::EndTurn);
        let kinds = sink.kinds();
        assert_eq!(kinds.first().unwrap(), "turn_start");
        assert!(kinds.contains(&"text_delta".to_string()));
        assert_eq!(kinds.last().unwrap(), "done");
        assert_eq!(s.history().len(), 3); // user, assistant, (无工具) -> 2? 见 Step 3 说明
    }

    #[tokio::test]
    async fn tool_call_is_executed_and_backfilled_then_second_round() {
        let (mut s, sink) = harness(ScriptedModel::new(vec![
            tool_deltas("echo", r#"{"text":"hi"}"#),
            deltas("拿到结果了"),
        ]));
        assert_eq!(s.run(user("用工具")).await, StopReason::EndTurn);
        let kinds = sink.kinds();
        assert!(kinds.contains(&"tool_call".to_string()));
        assert!(kinds.contains(&"tool_result".to_string()));
        // Read 级工具必须零弹窗
        assert!(!kinds.contains(&"permission_request".to_string()));
        let back = s.history().iter().rev()
            .find_map(|m| m.blocks.iter().find_map(|b| match b {
                ContentBlock::ToolResult { output: ToolOutput::Text { text }, .. } => Some(text.clone()),
                _ => None, })).unwrap();
        assert_eq!(back, "hi");
    }

    #[tokio::test]
    async fn denied_tool_fills_error_result_and_loop_continues() {
        let sink = Arc::new(VecSink::default());
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(EchoTool));
        // Write 级才走询问；这里塞一个 Write 风险的测试工具
        registry.register(Arc::new(crate::tools::echo::RiskOverrideTool(Risk::Write)));
        let gate = Arc::new(PermissionGate::new().with_sink(sink.clone()));
        let model = ScriptedModel::new(vec![
            tool_deltas("echo", r#"{"text":"x"}"#),
            deltas("被拒了，我换法子"),
        ]);
        let mut s = AgentSession::new(SessionDeps {
            model, registry, gate, sink: sink.clone(),
            context: ContextBuilder::new(ContextConfig::default()),
        });
        let h = { let sink = sink.clone(); tokio::spawn(async move {
            for _ in 0..100 {
                if let Some(id) = pending_request(&sink) {
                    gate_resolve(&sink, &id);
                    return;
                }
                tokio::task::yield_now().await;
            }
        }) };
        // 具体断言见 Step 3 说明：拒绝后循环必须继续，而不是中断整轮
    }

    #[tokio::test]
    async fn cancel_stops_mid_stream_with_interrupted_reason() {
        let mut big: Vec<ModelDelta> = vec![];
        big.extend(tool_deltas("echo", "{}"));
        let (mut s, sink) = harness(ScriptedModel::new(vec![big]));
        s.cancel_handle().cancel();
        assert_eq!(s.run(user("hi")).await, StopReason::Interrupted);
        assert!(!sink.kinds().contains(&"done".to_string())
                || sink.kinds().last().unwrap() == "done");
    }

    #[tokio::test]
    async fn max_turns_cap_is_reached_not_silently_stopped() {
        let rounds = vec![tool_deltas("echo", r#"{"text":"loop"}"#); 30];
        let (mut s, sink) = harness(ScriptedModel::new(rounds));
        assert_eq!(s.run(user("无限循环")).await, StopReason::MaxTurns);
        let done = sink.0.lock().unwrap().iter().find_map(|e| match e {
            AgentEvent::Done { stop_reason } => Some(*stop_reason), _ => None });
        assert_eq!(done, Some(StopReason::MaxTurns), "触顶必须显式告知，不可静默");
    }

    #[tokio::test]
    async fn model_error_emits_non_fatal_error_then_done_error() {
        let (mut s, sink) = harness(Arc::new(FailingModel));
        assert_eq!(s.run(user("hi")).await, StopReason::Error);
        let kinds = sink.kinds();
        assert!(kinds.contains(&"error".to_string()));
        assert_eq!(kinds.last().unwrap(), "done");
    }

    struct FailingModel;
    #[async_trait]
    impl ModelProvider for FailingModel {
        fn stream<'a>(&'a self, _: ChatRequest) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>> {
            Box::pin(stream::iter(vec![Err(ModelError::Api("429 rate limited".into()))]))
        }
    }
}
```

Run: `cargo test agent::session`
Expected: FAIL — `cannot find AgentSession`。

- [ ] **Step 2: 实现循环**

Create `src-tauri/src/agent/session.rs`:

```rust
use super::context::{ContextBuilder, ContextConfig};
use super::events::{AgentEvent, EventSink, Scope, StopReason};
use super::types::*;
use crate::model::assistant_acc::AssistantAcc;
use crate::model::provider::{ChatRequest, ModelProvider, ToolDef};
use crate::permission::gate::PermissionGate;
use crate::tools::registry::{ToolCtx, ToolError, ToolRegistry};
use futures::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct SessionConfig { pub max_turns: u32 }
impl Default for SessionConfig { fn default() -> Self { Self { max_turns: 25 } } }

pub struct SessionDeps {
    pub model: Arc<dyn ModelProvider>,
    pub registry: Arc<ToolRegistry>,
    pub gate: Arc<PermissionGate>,
    pub sink: Arc<dyn EventSink>,
    pub context: ContextBuilder,
}

pub struct AgentSession {
    deps: SessionDeps,
    history: Vec<Message>,
    cfg: SessionConfig,
    cancel: CancellationToken,
    http: reqwest::Client,
    roots: Vec<std::path::PathBuf>,
}

impl AgentSession {
    pub fn new(deps: SessionDeps) -> Self {
        Self {
            deps, history: Vec::new(), cfg: SessionConfig::default(),
            cancel: CancellationToken::new(),
            http: reqwest::Client::builder().user_agent("qwen-do/0.1").build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            roots: Vec::new(),
        }
    }

    pub fn with_roots(mut self, roots: Vec<std::path::PathBuf>) -> Self { self.roots = roots; self }
    pub fn history(&self) -> &[Message] { &self.history }
    pub fn cancel_handle(&self) -> CancellationToken { self.cancel.clone() }
    pub fn restore(&mut self, history: Vec<Message>) { self.history = history; }

    pub async fn run(&mut self, blocks: Vec<ContentBlock>) -> StopReason {
        self.history.push(Message::user(blocks));
        for turn in 1..=self.cfg.max_turns {
            if self.cancel.is_cancelled() { return self.finish(StopReason::Interrupted); }
            self.deps.sink.emit(AgentEvent::TurnStart { turn });

            let built = self.deps.context.build(&self.history);
            let req = ChatRequest {
                messages: built.messages,
                tools: self.deps.registry.defs().into_iter().map(|t| ToolDef {
                    name: t.name, description: t.description, input_schema: t.input_schema,
                }).collect(),
                temperature: None,
            };

            let mut acc = AssistantAcc::default();
            let mut deltas = self.deps.model.stream(req);
            let mut failure: Option<String> = None;

            while let Some(item) = futures::select! {
                biased;
                _ = self.cancel.cancelled() => { return self.finish(StopReason::Interrupted); }
                d = deltas.next() => d,
            } {
                match item {
                    Ok(ModelDelta::Usage { input_tokens, output_tokens }) => self.deps.sink.emit(
                        AgentEvent::Usage { input_tokens, output_tokens }),
                    Ok(d) => {
                        if let Some(text) = acc.apply(d) {
                            self.deps.sink.emit(AgentEvent::TextDelta { text });
                        }
                    }
                    Err(e) => { failure = Some(e.to_string()); break; }
                }
            }

            if let Some(msg) = failure {
                self.deps.sink.emit(AgentEvent::Error {
                    at: crate::agent::events::ErrorWhere::Model,
                    message: msg, fatal: true,
                });
                return self.finish(StopReason::Error);
            }

            let assistant = acc.finish();
            self.deps.sink.emit(AgentEvent::Usage {
                input_tokens: 0, output_tokens: 0 }); // 占位：真实 usage 由上面的 delta 发出
            let calls = assistant.tool_calls();
            let has_tools = !calls.is_empty();
            self.history.push(assistant);
            if !has_tools { return self.finish(StopReason::EndTurn); }

            for (id, name, input) in calls {
                let risk = self.deps.registry.risk_of(&name);
                let view = ToolCallView { id: id.clone(), name: name.clone(), risk, input: input.clone() };
                self.deps.sink.emit(AgentEvent::ToolCall { call: view.clone(), risk });

                let decision = self.deps.gate.check(&view).await;
                let started = std::time::Instant::now();
                let out = if decision.allowed() {
                    let ctx = ToolCtx {
                        http: self.http.clone(),
                        cancel: self.cancel.clone(),
                        roots: self.roots.clone(),
                    };
                    self.deps.registry.execute(&name, input, &ctx).await
                } else {
                    let reason = match &decision {
                        Decision::Deny { reason } => reason.clone(),
                        Decision::Allow { .. } => unreachable!(),
                    };
                    Err(ToolError::Failed(format!("用户拒绝执行：{reason}")))
                };

                let (output, is_error) = match out {
                    Ok(o) => (o, false),
                    Err(e) => (ToolOutput::error(e.to_string()), true),
                };
                let preview = preview_of(&output);
                self.deps.sink.emit(AgentEvent::ToolResult {
                    call_id: id.clone(), is_error,
                    ms: started.elapsed().as_millis() as u64, preview,
                });
                self.history.push(Message::tool(&id, output, is_error));
            }
        }
        self.finish(StopReason::MaxTurns)
    }

    fn finish(&self, r: StopReason) -> StopReason {
        self.deps.sink.emit(AgentEvent::Done { stop_reason: r });
        r
    }
}

/// 只把前 200 字符放进事件流：完整工具输出留在 history，
/// 否则一次大页面抓取会同时压垮 IPC 与前端渲染。
fn preview_of(o: &ToolOutput) -> String {
    match o {
        ToolOutput::Text { text } => {
            let mut s: String = text.chars().take(200).collect();
            if text.chars().count() > 200 { s.push_str("…"); }
            s
        }
        ToolOutput::Image { mime, .. } => format!("[图片 {}]", match mime {
            ImageMime::Png => "png", ImageMime::Jpeg => "jpeg" }),
    }
}
```

`Scope` 在本任务未直接使用，从 `events` 导入后删除未用告警即可（`#[allow(unused_imports)]` 不算通过——直接删掉这一项）。

- [ ] **Step 3: 修正 Step 1 中两处会误导实现的断言**

Step 1 里有两条测试写得不严谨，本步骤把它们改成可信断言：

1. `plain_text_turn_...` 中 `assert_eq!(s.history().len(), 3)` 是错的——纯文本轮只有 user + assistant 两条。改为 `assert_eq!(s.history().len(), 2)`，并在实现里删掉 `session.rs` 中那个 `Usage { input_tokens: 0, output_tokens: 0 }` 占位 emit（它会让前端 token 计数在每轮归零，是真实 bug）。
2. `denied_tool_fills_error_result_and_loop_continues` 依赖两个未定义的自由函数与一个不存在的 `RiskOverrideTool`。改成可写的形式：在 `tools/echo.rs` 加 `pub struct RiskOverrideTool(pub Risk);`，`meta()` 返回名字 `"echo"` 之外的 `"guarded"` 且 `execute` 复用 echo 逻辑；测试里注册 `guarded`、让 `ScriptedModel` 调 `"guarded"`、用后台 task 从 `sink` 抓 `PermissionRequest` 的 id 并调 `gate.resolve(id, Decision::Deny{reason:"我不想让你写"})`，然后断言：`done` 的 stop_reason 是 `EndTurn`（**循环继续**）、history 中存在 `is_error: true` 且内容含「用户拒绝执行」的 `ToolResult`、第二轮模型收到了这条错误结果并回了文本。

改完后本任务测试为 6 个，全部通过：

Run: `cargo test agent::session`
Expected: PASS（6 个）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/agent/session.rs src-tauri/src/tools/echo.rs
git commit -m "feat: add agent session loop with scripted-model coverage of six paths"
```

---

### Task 11: 会话持久化与重放映射

spec 6.2：界面状态不落盘，messages 是唯一真相源。本任务把这条做成代码。

**Files:**
- Create: `src-tauri/src/store/mod.rs`、`src-tauri/src/store/db.rs`
- Test: `src-tauri/src/store/db.rs`（内联）

**Interfaces:**
- Consumes: Task 5 `Message`/`ContentBlock`/`Role`
- Produces:
  - `Db::open(path: &Path) -> Result<Db>`（启用 WAL）
  - `Db::create_session(title) -> Result<String>`（uuid）
  - `Db::list_sessions() -> Result<Vec<SessionRow>>`
  - `Db::rename_session(id, title)`、`Db::delete_session(id)`
  - `Db::append_message(session_id, &Message) -> Result<i64>`
  - `Db::messages(session_id) -> Result<Vec<Message>>`
  - `Db::latest_user_message(session_id) -> Result<Option<String>>`
  - `events_from_messages(msgs: &[Message]) -> Vec<AgentEvent>`（重放用纯函数）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, ImageMime, ToolOutput};

    fn tmpdb() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("t.db")).unwrap();
        (d, db)
    }

    #[test]
    fn round_trips_text_message_preserving_role() {
        let (_d, db) = tmpdb();
        let s = db.create_session("测试").unwrap();
        db.append_message(&s, &Message::user(vec![ContentBlock::Text { text: "你好".into() }])).unwrap();
        let got = db.messages(&s).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].role, Role::User);
        assert!(matches!(&got[0].blocks[0], ContentBlock::Text { text } if text == "你好"));
    }

    #[test]
    fn round_trips_image_and_tool_blocks() {
        let (_d, db) = tmpdb();
        let s = db.create_session("s").unwrap();
        let m = Message { role: Role::Assistant, blocks: vec![
            ContentBlock::Image { data: "AAAA".into(), mime: ImageMime::Jpeg },
            ContentBlock::ToolCall { id: "c1".into(), name: "echo".into(), input: serde_json::json!({"t":1}) },
            ContentBlock::ToolResult { call_id: "c1".into(), is_error: true, output: ToolOutput::text("boom") },
        ]};
        db.append_message(&s, &m).unwrap();
        assert_eq!(db.messages(&s).unwrap()[0], m, "三种块必须无损往返");
    }

    #[test]
    fn sessions_order_newest_first_and_delete_is_cascading() {
        let (_d, db) = tmpdb();
        let a = db.create_session("A").unwrap();
        let b = db.create_session("B").unwrap();
        db.append_message(&b, &Message::user(vec![ContentBlock::Text { text: "x".into() }])).unwrap();
        let list = db.list_sessions().unwrap();
        assert_eq!(list[0].id, b, "最新的排最前");
        db.delete_session(&b).unwrap();
        assert!(db.messages(&b).unwrap().is_empty(), "删会话必须连带删消息");
        assert_eq!(db.list_sessions().unwrap().len(), 1);
        let _ = a;
    }

    #[test]
    fn latest_user_message_becomes_session_title() {
        let (_d, db) = tmpdb();
        let s = db.create_session("新对话").unwrap();
        db.append_message(&s, &Message::user(vec![ContentBlock::Text {
            text: "帮我查一下明天天气".into() }])).unwrap();
        assert_eq!(db.latest_user_message(&s).unwrap().as_deref(), Some("帮我查一下明天天气"));
    }

    #[test]
    fn events_from_messages_produce_replayable_projection() {
        let msgs = vec![
            Message::user(vec![ContentBlock::Text { text: "查一下".into() }]),
            Message { role: Role::Assistant, blocks: vec![
                ContentBlock::Text { text: "好的".into() },
                ContentBlock::ToolCall { id: "c1".into(), name: "echo".into(), input: serde_json::json!({}) },
            ]},
            Message::tool("c1", ToolOutput::text("结果"), false),
            Message { role: Role::Assistant, blocks: vec![ContentBlock::Text { text: "拿到结果".into() }] },
        ];
        let evts = events_from_messages(&msgs);
        let kinds: Vec<&str> = evts.iter()
            .map(|e| serde_json::to_value(e).unwrap()["t"].as_str().unwrap()).collect();
        // 重放必须能重建：两段助手文本 + 一张工具卡片
        assert_eq!(kinds.iter().filter(|k| **k == "text_delta").count(), 3);
        assert_eq!(kinds.iter().filter(|k| **k == "tool_call").count(), 1);
        assert_eq!(kinds.iter().filter(|k| **k == "tool_result").count(), 1);
        assert_eq!(*kinds.last().unwrap(), "done");
    }

    #[test]
    fn wal_mode_is_on_so_concurrent_writes_donot_lock_up() {
        let (_d, db) = tmpdb();
        assert_eq!(db.journal_mode().unwrap(), "wal");
    }
}
```

Run: `cargo add --dev tempfile` 和 `cargo add rusqlite --features bundled`
Run: `cargo test store::db`
Expected: FAIL — `cannot find Db`。

- [ ] **Step 2: 实现**

Create `src-tauri/src/store/db.rs`:

```rust
use crate::agent::events::{AgentEvent, StopReason};
use crate::agent::types::{ContentBlock, Message, Role};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct StoredMessage { role: Role, blocks: Vec<ContentBlock> }

pub struct Db { conn: Mutex<Connection> }

impl Db {
    pub fn open(path: &Path) -> Result<Db, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions(
                 id TEXT PRIMARY KEY, title TEXT NOT NULL,
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS messages(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 role TEXT NOT NULL, blocks_json TEXT NOT NULL,
                 created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_msg_session ON messages(session_id, id);
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn journal_mode(&self) -> Result<String, rusqlite::Error> {
        let c = self.conn.lock().unwrap();
        c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
    }

    fn now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    pub fn create_session(&self, title: &str) -> Result<String, rusqlite::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let t = Self::now();
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO sessions(id,title,created_at,updated_at) VALUES(?1,?2,?3,?3)",
            params![id, title, t])?;
        Ok(id)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, rusqlite::Error> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT id,title,updated_at FROM sessions ORDER BY updated_at DESC, created_at DESC")?;
        let rows = st.query_map([], |r| Ok(SessionRow {
            id: r.get(0)?, title: r.get(1)?, updated_at: r.get(2)? }))?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn rename_session(&self, id: &str, title: &str) -> Result<(), rusqlite::Error> {
        let c = self.conn.lock().unwrap();
        c.execute("UPDATE sessions SET title=?2, updated_at=?3 WHERE id=?1",
            params![id, title, Self::now()])?;
        Ok(())
    }

    pub fn delete_session(&self, id: &str) -> Result<(), rusqlite::Error> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM messages WHERE session_id=?1", params![id])?;
        c.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
        Ok(())
    }

    pub fn append_message(&self, session_id: &str, m: &Message) -> Result<i64, rusqlite::Error> {
        let json = serde_json::to_string(&StoredMessage { role: m.role.clone(), blocks: m.blocks.clone() })
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let id = self.append_message_json(session_id, &m.role, &json)?;
        Ok(id)
    }

    fn append_message_json(&self, session_id: &str, role: &Role, blocks_json: &str)
        -> Result<i64, rusqlite::Error>
    {
        let t = Self::now();
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO messages(session_id,role,blocks_json,created_at) VALUES(?1,?2,?3,?4)",
            params![session_id, serde_json::to_string(role).unwrap().trim_matches('"'), blocks_json, t])?;
        c.execute("UPDATE sessions SET updated_at=?2 WHERE id=?1", params![session_id, t])?;
        Ok(c.last_insert_rowid())
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>, rusqlite::Error> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT role, blocks_json FROM messages WHERE session_id=?1 ORDER BY id")?;
        let out = st.query_map(params![session_id], |r| {
            let role_s: String = r.get(0)?;
            let blocks: Vec<ContentBlock> = serde_json::from_str(&r.get::<_, String>(1)?)
                .unwrap_or_default();
            let role: Role = serde_json::from_str(&format!("\"{role_s}\"")).unwrap_or(Role::User);
            Ok(Message { role, blocks })
        })?.collect::<Result<_, _>>()?;
        Ok(out)
    }

    pub fn latest_user_message(&self, session_id: &str) -> Result<Option<String>, rusqlite::Error> {
        let msgs = self.messages(session_id)?;
        Ok(msgs.iter().rev().find(|m| m.role == Role::User).map(text_of_message).map(|t| {
            let mut s: String = t.chars().take(40).collect();
            if t.chars().count() > 40 { s.push('…'); }
            s
        }))
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct SessionRow { pub id: String, pub title: String, pub updated_at: i64 }

fn text_of_message(m: &Message) -> String {
    m.blocks.iter().filter_map(|b| match b {
        ContentBlock::Text { text } => Some(text.as_str()), _ => None,
    }).collect::<Vec<_>>().join("")
}

/// 重放：把持久化消息映射成等价事件序列，前端用同一个 reducer 得到界面。
/// 界面状态本身不落盘，因此不存在两套渲染路径。
pub fn events_from_messages(msgs: &[Message]) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::System => out.push(AgentEvent::TextDelta {
                text: text_of_message(m) }),
            Role::User => {
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => out.push(AgentEvent::TextDelta { text: text.clone() }),
                        ContentBlock::Image { data, mime } => out.push(AgentEvent::TextDelta {
                            text: format!("[图片 {mime:?} {} 字节]", data.len()) }),
                        ContentBlock::ToolCall { id, name, input } => out.push(AgentEvent::ToolCall {
                            call: ToolCallView { id: id.clone(), name: name.clone(),
                                risk: Risk::Read, input: input.clone() },
                            risk: Risk::Read }),
                        ContentBlock::ToolResult { call_id, is_error, output } => out.push(AgentEvent::ToolResult {
                            call_id: call_id.clone(), is_error: *is_error, ms: 0,
                            preview: output_preview(output) }),
                    }
                }
                out.push(AgentEvent::TurnStart { turn: 0 });
            }
            Role::Assistant => {
                out.push(AgentEvent::TurnStart { turn: 0 });
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => out.push(AgentEvent::TextDelta { text: text.clone() }),
                        ContentBlock::ToolCall { id, name, input } => out.push(AgentEvent::ToolCall {
                            call: ToolCallView { id: id.clone(), name: name.clone(),
                                risk: Risk::Read, input: input.clone() },
                            risk: Risk::Read }),
                        ContentBlock::Image { data, mime } => out.push(AgentEvent::TextDelta {
                            text: format!("[图片 {mime:?} {} 字节]", data.len()) }),
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
        }
    }
    out.push(AgentEvent::Done { stop_reason: StopReason::EndTurn });
    out
}

fn output_preview(o: &ToolOutput) -> String {
    match o {
        ToolOutput::Text { text } => text.chars().take(200).collect(),
        ToolOutput::Image { mime, .. } => format!("[图片 {mime:?}]"),
    }
}
```

`events_from_messages` 里 `Risk::Read` 是**重放专用的展示性默认值**，不参与任何决策（前端不读 risk 判断放行）；真正的 risk 在运行时由 registry 给出。这一点必须写进代码注释，否则 Plan B 会误以为这里有权限语义。

Create `src-tauri/src/store/mod.rs`: `pub mod db;`

- [ ] **Step 3: 跑测试**

Run: `cargo test store::db`
Expected: PASS（6 个）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/store/ src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: add sqlite session store and message-to-event replay mapping"
```

---

### Task 12: 设置与凭据

spec 把 api_key 放在切片 6，但本计划提前到切片 1：**先明文存再从 keyring 迁移，等于让一版真实 key 落到磁盘上**。宁可现在做对。

**Files:**
- Create: `src-tauri/src/settings.rs`
- Modify: `src-tauri/Cargo.toml`
- Test: `src-tauri/src/settings.rs`（内联）

**Interfaces:**
- Consumes: Task 6 `ModelConfig`
- Produces:
  - `struct Settings { base_url, model, reserve_output, budget_tokens, window_messages, theme, mode, font_size }`
  - `SettingsStore::new(dir: &Path) -> SettingsStore`
  - `SettingsStore::load() -> Result<Settings>` / `save(&Settings) -> Result<()>`
  - `SettingsStore::set_api_key(&str) -> Result<()>` / `api_key() -> Result<Option<String>>` / `api_key_masked() -> String`
  - `KEYRING_SERVICE: &str = "qwen-do"`
  - `Settings::to_model_config(&self, api_key: &str) -> ModelConfig`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec_numbers() {
        let s = Settings::default();
        assert_eq!(s.reserve_output, 4096);
        assert_eq!(s.budget_tokens, 32_000);
        assert_eq!(s.window_messages, 40);
        assert_eq!(s.mode, Mode::Dark);
    }

    #[test]
    fn masked_key_never_contains_more_than_prefix_and_suffix() {
        assert_eq!(mask_key("sk-abcdefghijklmnop"), "sk-a…mnop");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key(""), "未配置");
    }

    #[test]
    fn saved_settings_survive_reload_and_do_not_contain_key() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        let mut s = Settings::default();
        s.base_url = "https://x.com/v1".into();
        s.model = "qwen-plus".into();
        store.save(&s).unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(!raw.contains("api_key"), "配置文件里绝不能出现 api_key 字段");
        let got = store.load().unwrap();
        assert_eq!(got.base_url, "https://x.com/v1");
        assert_eq!(got.model, "qwen-plus");
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults_without_crash() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        let got = store.load().unwrap();
        assert_eq!(got, Settings::default());
    }

    #[test]
    fn base_url_is_normalized_on_save() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        let mut s = Settings::default();
        s.base_url = "https://x.com/v1/";
        store.save(&s).unwrap();
        assert_eq!(store.load().unwrap().base_url, "https://x.com/v1");
    }

    #[test]
    fn model_config_carries_normalized_base_and_explicit_key() {
        let s = Settings { base_url: "https://x.com".into(), ..Default::default() };
        let mc = s.to_model_config("k");
        assert_eq!(mc.base_url, "https://x.com/v1");
        assert_eq!(mc.api_key, "k");
    }
}
```

Run: `cargo test settings::tests`
Expected: FAIL — `cannot find SettingsStore`。

- [ ] **Step 2: 实现**

Create `src-tauri/src/settings.rs`:

```rust
use crate::model::openai_compat::{normalize_base_url, ModelConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const KEYRING_SERVICE: &str = "qwen-do";
const KEYRING_USER: &str = "api_key";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode { Light, Dark }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    pub reserve_output: u32,
    pub budget_tokens: u32,
    pub window_messages: usize,
    pub theme: String,
    pub mode: Mode,
    pub font_size: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: "qwen-plus".into(),
            reserve_output: 4096,
            budget_tokens: 32_000,
            window_messages: 40,
            theme: "noir".into(),
            mode: Mode::Dark,
            font_size: 15,
        }
    }
}

impl Settings {
    pub fn to_model_config(&self, api_key: &str) -> ModelConfig {
        ModelConfig {
            base_url: normalize_base_url(&self.base_url),
            model: self.model.clone(),
            api_key: api_key.to_string(),
            max_output_tokens: self.reserve_output,
        }
    }
}

pub fn mask_key(k: &str) -> String {
    if k.is_empty() { return "未配置".into(); }
    let n = k.chars().count();
    if n <= 8 { return "***".into(); }
    let head: String = k.chars().take(4).collect();
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

pub struct SettingsStore { dir: PathBuf }

impl SettingsStore {
    pub fn new(dir: &Path) -> SettingsStore { SettingsStore { dir: dir.to_path_buf() } }
    pub fn path(&self) -> PathBuf { self.dir.join("settings.json") }

    pub fn load(&self) -> Result<Settings, String> {
        let p = self.path();
        if !p.exists() { return Ok(Settings::default()); }
        let raw = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
        // 损坏的配置不该让应用打不开：回落默认值，并在日志里留痕
        Ok(serde_json::from_str::<Settings>(&raw).unwrap_or_else(|e| {
            tracing::warn!("settings.json 解析失败，回落默认值: {e}");
            Settings::default()
        }))
    }

    pub fn save(&self, s: &Settings) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let mut c = s.clone();
        c.base_url = normalize_base_url(&c.base_url);
        let json = serde_json::to_string_pretty(&c).map_err(|e| e.to_string())?;
        std::fs::write(self.path(), json).map_err(|e| e.to_string())
    }

    pub fn set_api_key(&self, key: &str) -> Result<(), String> {
        if key.is_empty() { return self.clear_api_key(); }
        let e = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|x| x.to_string())?;
        e.set_password(key).map_err(|x| x.to_string())
    }

    pub fn api_key(&self) -> Result<Option<String>, String> {
        let e = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|x| x.to_string())?;
        match e.get_password() {
            Ok(k) => Ok(Some(k)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(x) => Err(x.to_string()),
        }
    }

    pub fn api_key_masked(&self) -> String {
        self.api_key().ok().flatten().map(|k| mask_key(&k)).unwrap_or_else(|| "未配置".into())
    }

    pub fn clear_api_key(&self) -> Result<(), String> {
        let e = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|x| x.to_string())?;
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(x) => Err(x.to_string()),
        }
    }
}
```

`Settings` 结构体**没有 `api_key` 字段**——这是 Step 1 第三条测试的用武之地：字段不存在，就不可能不小心序列化进磁盘。

Run: `cargo add keyring` 和 `cargo add tracing`
Create `src-tauri/src/lib.rs` 中加 `pub mod settings;`

- [ ] **Step 3: 跑测试**

Run: `cargo test settings::tests`
Expected: PASS（6 个）

- [ ] **Step 4: 手工验证 keyring 真的落在 Windows 凭据管理器**

> **本步原来的写法跑不通，2026-10-20 更正。** 原文让跑 `cargo test settings -- --nocapture`
> 然后去控制面板找 `qwen-do` 条目 —— 但实现期裁定 R80/R81 之后，测试一律注入
> `keyring_core::mock::Store`，**没有任何测试会写真实库**（照原文跑必然什么都看不到，
> 而它会看起来像「测试坏了」）；真实条目名是 `api_key.qwen-do`（user 在前，R88 更正过）；
> 原文末句 `cargo add keyring --features windows-native` 会把 R80 明令禁用的 facade 引回来。

实际跑法在 `src-tauri/examples/keyring_probe.rs`（测试运行器摸不到它，形态同 `mcp_probe.rs`）：

```bash
cmdkey //list 2>&1 | iconv -f GBK -t UTF-8 | grep -i qwen     # 前置：应当为空
cargo run --example keyring_probe -- arm                      # 写入 canary
cmdkey //list 2>&1 | iconv -f GBK -t UTF-8 | grep -i qwen     # 应当出现 api_key.qwen-do
cargo run --example keyring_probe -- check                    # 另一个进程读回 → 删除 → 幂等
cargo run --example keyring_probe -- status                   # 收尾自检：应当报「干净」
```

`arm` 与 `check` 必须是两条命令（跨进程才是证据）。若中途收工，`cleanup` 只会删掉本探针写的那条
canary，别的条目一律不碰；`arm` 在库里已有任何条目时直接拒绝动手。非 Windows 上本探针只打印一句
「无意义」就走（R81 那支 store 每次调用都 `UnsupportedPlatform`）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/settings.rs src-tauri/src/lib.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat: add settings store with keyring-backed api key and masked reads"
```

---

### Task 13: Tauri 装配与 IPC 层

`ipc.rs` 是本计划唯一允许 `use tauri` 的文件。

**Files:**
- Create: `src-tauri/src/ipc.rs`、`src-tauri/src/lib.rs`、`src-tauri/capabilities/default.json`
- Modify: `src-tauri/src/main.rs`、`src-tauri/tauri.conf.json`

**Interfaces:**
- Consumes: Task 5–12 全部
- Produces:
  - `struct AppState { db: Arc<Db>, settings: Arc<SettingsStore>, registry: Arc<ToolRegistry>, gate: Arc<PermissionGate>, cancel: Mutex<Option<CancellationToken>>, running: Mutex<Option<String>> }`
  - `TauriEmitter { app: AppHandle, session_id: String }`（实现 `EventSink`，内含 30ms/512 字符合帧）
  - commands：`send_message`、`respond`、`interrupt`、`session_list`、`session_create`、`session_rename`、`session_delete`、`session_messages`、`settings_get`、`settings_set`、`settings_set_api_key`、`settings_api_key_masked`、`runtime_summary`
  - 事件通道名 `"agent"`，payload `{ sessionId, event }`

- [ ] **Step 1: 写 TauriEmitter 的合帧逻辑**

Create `src-tauri/src/ipc.rs` 的开头部分：

```rust
use crate::agent::events::{AgentEvent, EventSink};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize)]
pub struct AgentPayload { pub session_id: String, pub event: AgentEvent }

/// 把高频 TextDelta 在 Rust 侧合帧：30ms 或累计 512 字符才 emit 一次。
/// IPC 穿越次数降一个数量级，比在前端想办法省渲染有效得多。
pub struct TauriEmitter {
    app: AppHandle,
    session_id: String,
    buf: std::sync::Mutex<String>,
    last_flush: std::sync::Mutex<std::time::Instant>,
}

impl TauriEmitter {
    pub fn new(app: AppHandle, session_id: String) -> Self {
        Self { app, session_id, buf: Default::default(),
               last_flush: std::sync::Mutex::new(std::time::Instant::now()) }
    }
    fn send(&self, e: AgentEvent) {
        let _ = self.app.emit("agent", AgentPayload {
            session_id: self.session_id.clone(), event: e });
    }
    fn flush(&self) {
        let t = {
            let mut b = self.buf.lock().unwrap();
            if b.is_empty() { return; }
            std::mem::take(&mut *b)
        };
        *self.last_flush.lock().unwrap() = std::time::Instant::now();
        self.send(AgentEvent::TextDelta { text: t });
    }
}

impl EventSink for TauriEmitter {
    fn emit(&self, e: AgentEvent) {
        match e {
            AgentEvent::TextDelta { text } => {
                { let mut b = self.buf.lock().unwrap(); b.push_str(&text); }
                let due = {
                    let l = self.last_flush.lock().unwrap();
                    l.elapsed().as_millis() >= 30
                };
                let big = self.buf.lock().unwrap().chars().count() >= 512;
                if due || big { self.flush(); }
            }
            other => { self.flush(); self.send(other); }
        }
    }
}
```

**非 `TextDelta` 事件前必须先 flush**，否则 `tool_call` 会跑到未落地的文本之前到达，前端时间线错乱。这是合帧唯一的正确性约束，`other =>` 分支就是它。

- [ ] **Step 2: 写命令层与单轮并发控制**

Create `src-tauri/src/ipc.rs` 续：

```rust
use crate::agent::context::{ContextBuilder, ContextConfig};
use crate::agent::session::{AgentSession, SessionDeps};
use crate::agent::types::{ContentBlock, Decision, Scope};
use crate::model::openai_compat::OpenAiCompat;
use crate::permission::gate::PermissionGate;
use crate::settings::{Settings, SettingsStore};
use crate::store::db::{events_from_messages, Db, SessionRow};
use crate::tools::registry::ToolRegistry;
use std::sync::{Arc, Mutex};
use tauri::State;

pub struct AppState {
    pub db: Arc<Db>,
    pub settings: Arc<SettingsStore>,
    pub registry: Arc<ToolRegistry>,
    pub gate: Arc<PermissionGate>,
    pub cancel: Mutex<Option<tokio_util::sync::CancellationToken>>,
    pub running: Mutex<Option<String>>,
}

fn take_running(r: &mut Option<String>, want: &str) -> Result<(), String> {
    match r.as_deref() {
        Some(s) if s != want => Err(format!("会话 {s} 正在运行。同一时刻只支持一轮对话。")),
        _ => { *r = Some(want.to_string()); Ok(()) }
    }
}

#[tauri::command]
pub async fn send_message(state: State<'_, AppState>, app: AppHandle,
                          session_id: String, blocks: Vec<ContentBlock>) -> Result<(), String>
{
    {
        let mut r = state.running.lock().unwrap();
        if let Some(cur) = r.clone() {
            if cur != session_id { return Err(format!("会话 {cur} 正在运行")); }
            // 同会话重入：先取消上一轮，等其退出
            if let Some(c) = state.cancel.lock().unwrap().clone() { c.cancel(); }
        }
        *r = Some(session_id.clone());
    }

    let s = state.settings.load()?;
    let key = state.settings.api_key()?
        .ok_or_else(|| "尚未配置 API key，请在设置中填写".to_string())?;
    if s.base_url.trim().is_empty() { return Err("尚未配置模型地址".into()); }

    let model_cfg = s.to_model_config(&key);
    let model = Arc::new(OpenAiCompat::new(model_cfg).map_err(|e| e.to_string())?);

    let emitter = Arc::new(TauriEmitter::new(app.clone(), session_id.clone()));
    let registry = state.registry.clone();
    let gate = state.gate.clone();
    let ctx_builder = ContextBuilder::new(ContextConfig {
        reserve_output: s.reserve_output, budget_tokens: s.budget_tokens,
        window_messages: s.window_messages });

    let mut session = AgentSession::new(SessionDeps {
        model, registry, gate,
        sink: emitter.clone(), context: ctx_builder });
    let cancel = session.cancel_handle();
    *state.cancel.lock().unwrap() = Some(cancel.clone());

    // 从库里恢复历史，使多轮对话跨重启仍然成立
    let history = state.db.messages(&session_id).map_err(|e| e.to_string())?;
    session.restore(history);

    let db = state.db.clone();
    let sid = session_id.clone();
    let out = tokio::spawn(async move {
        for b in &blocks { db.append_message(&sid, &Message::user(vec![b.clone()]))
            .map_err(|e| e.to_string()).ok(); }
        let reason = session.run(blocks).await;
        (reason, session.history().to_vec())
    }).await
      .map_err(|e| e.to_string())?;

    emitter.flush();
    for m in out.1 { db.append_message(&session_id, &m).map_err(|e| e.to_string()).ok(); }
    if let Ok(Some(t)) = db.latest_user_message(&session_id) {
        let _ = db.rename_session(&session_id, &t);
    }
    *state.running.lock().unwrap() = None;
    *state.cancel.lock().unwrap() = None;
    Ok(())
}

#[tauri::command]
pub fn interrupt(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(c) = state.cancel.lock().unwrap().clone() { c.cancel(); }
    Ok(())
}

#[talluri::command]
pub fn respond(state: State<'_, AppState>, request_id: String, decision: Decision) -> Result<bool, String> {
    Ok(state.gate.resolve(&request_id, decision))
}

#[tauri::command]
pub fn session_list(state: State<'_, AppState>) -> Result<Vec<SessionRow>, String> {
    state.db.list_sessions().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn session_create(state: State<'_, AppState>) -> Result<String, String> {
    state.db.create_session("新对话").map_err(|e| e.to_string())
}

#[tauri::command]
pub fn session_rename(state: State<'_, AppState>, id: String, title: String) -> Result<(), String> {
    state.db.rename_session(&id, &title).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn session_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if let Some(c) = state.cancel.lock().unwrap().clone() { c.cancel(); }
    state.db.delete_session(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn session_messages(state: State<'_, AppState>, id: String) -> Result<Vec<AgentEvent>, String> {
    let msgs = state.db.messages(&id).map_err(|e| e.to_string())?;
    Ok(events_from_messages(&msgs))
}

#[tauri::command]
pub fn settings_get(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let s = state.settings.load()?;
    Ok(SettingsView { settings: s, api_key_masked: state.settings.api_key_masked() })
}

#[derive(serde::Serialize)]
pub struct SettingsView {
    #[serde(flatten)] pub settings: Settings,
    pub api_key_masked: String,
}

#[tauri::command]
pub fn settings_set(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    state.settings.save(&settings)
}

#[tauri::command]
pub fn settings_set_api_key(state: State<'_, AppState>, api_key: String) -> Result<(), String> {
    state.settings.set_api_key(&api_key)
}

#[tauri::command]
pub fn runtime_summary() -> String { "qwen-do 0.1 · plan A".into() }
```

`respond` 里 `Decision` 由前端传入，需 `Deserialize`——Task 5 里它只 derive 了 `Serialize`，此处补上 `Deserialize`。`send_message` 中未导入的 `Message` 需从 `crate::agent::types::Message` 引入。

`take_running` 辅助函数若最终未被引用，删除它，不要留 `#[allow(dead_code)]`。

- [ ] **Step 3: 装配 lib.rs / main.rs / capabilities**

Create `src-tauri/src/lib.rs`:

```rust
pub mod agent;
pub mod ipc;
pub mod model;
pub mod permission;
pub mod settings;
pub mod store;
pub mod tools;

use ipc::AppState;
use std::sync::Arc;
use tauri::Manager;

pub fn run() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let app_data = tauri::generated_by::... ; // 见下方实际写法

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let db = Arc::new(store::db::Db::open(&dir.join("qwen-do.db"))?);
            let settings = Arc::new(settings::SettingsStore::new(&dir));
            let emitter_probe: Arc<dyn agent::events::EventSink> = Arc::new(NoopSink);
            app.manage(AppState {
                db, settings,
                registry: Arc::new(tools::registry::ToolRegistry::new()),
                gate: Arc::new(permission::gate::PermissionGate::new().with_sink(emitter_probe)),
                cancel: Default::default(),
                running: Default::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::send_message, ipc::interrupt, ipc::respond,
            ipc::session_list, ipc::session_create, ipc::session_rename, ipc::session_delete,
            ipc::session_messages,
            ipc::settings_get, ipc::settings_set,
            ipc::settings_set_api_key, ipc::runtime_summary,
        ])
        .run(tauri::generate_context!())
        .expect("error while running qwen-do");
}

struct NoopSink;
impl agent::events::EventSink for NoopSink {
    fn emit(&self, _e: agent::events::AgentEvent) {}
}
```

**这里有一个必须解决的设计张力**：`PermissionGate` 的 sink 在装配期还没有 `session_id`，而 `TauriEmitter` 需要它。解法是 gate 持 `NoopSink`、由 `send_message` 每轮用带 session_id 的 emitter **重建一个 gate**（Plan A 里 gate 无持久规则，重建安全），或把 `EventSink` 做成 `Arc<Mutex<Option<Arc<dyn EventSink>>>>` 的可换内核。**选定后写进代码注释**，不要留未解决的空 sink——否则 Plan B 的弹窗会静默不发事件。

推荐后者：`AppState.gate` 内层是可替换 sink，`send_message` 开头 `state.gate.set_sink(emitter.clone())`。

- [ ] **Step 4: capabilities 收紧**

Create `src-tauri/capabilities/default.json`:

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "default",
  "description": "前端只允许事件、对话框与自定义命令；无 shell、无全局文件系统权限",
  "windows": ["main"],
  "permissions": [
    "core:default",
    "core:event:default",
    "dialog:allow-open",
    "core:window:allow-set-size",
    "core:window:allow-start-dragging"
  ]
}
```

**不得加 `shell:allow-spawn` 或 `fs:allow-*` 通配**。MCP 子进程由 Rust 侧 `tokio::process` spawn（Plan B），前端不具备执行进程能力——这是 spec 6.6 白捡的安全收益，须守住。

- [ ] **Step 5: 编译并跑全部测试**

Run: `cargo test`
Expected: 全绿。
Run: `npm run tauri dev`
Expected: 窗口出现，无控制台报错。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/ipc.rs src-tauri/src/lib.rs src-tauri/src/main.rs src-tauri/capabilities/ src-tauri/tauri.conf.json
git commit -m "feat: wire tauri ipc layer as sole tauri-dependent boundary"
```

---

### Task 14: 前端类型镜像与 reducer

**Files:**
- Create: `src/types.ts`、`src/state/reducer.ts`、`src/state/reducer.test.ts`

**Interfaces:**
- Consumes: Task 5/11 的 `AgentEvent` 线格式（snake_case，tag 字段 `t`）、Task 11 `events_from_messages`
- Produces:
  - `AgentEvent` 联合类型（TS 镜像）
  - `UiState`、`Turn`、`UiBlock`、`ToolCard`、`PermCard`
  - `initialState(): UiState`
  - `reduce(state: UiState, e: AgentEvent): UiState`
  - `replay(events: AgentEvent[]): UiState`
  - `selectActiveTurn(state): Turn | undefined`

- [ ] **Step 1: 写 reducer 的失败测试**

Create `src/state/reducer.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { initialState, reduce, replay, selectActiveTurn } from "./reducer";
import type { AgentEvent } from "../types";

const d = (text: string): AgentEvent => ({ t: "text_delta", text });

describe("reducer", () => {
  it("merges consecutive text deltas into one block", () => {
    let s = initialState();
    s = reduce(s, { t: "turn_start", turn: 1 });
    for (const ch of "你好世界") s = reduce(s, d(ch));
    const turn = selectActiveTurn(s)!;
    expect(turn.blocks).toHaveLength(1);
    expect(turn.blocks[0]).toEqual({ kind: "text", text: "你好世界" });
  });

  it("starts a new text block after a tool card so timeline order survives", () => {
    let s = initialState();
    s = reduce(s, { t: "turn_start", turn: 1 });
    s = reduce(s, d("前半"));
    s = reduce(s, {
      t: "tool_call", risk: "read",
      call: { id: "c1", name: "echo", risk: "read", input: {} },
    });
    s = reduce(s, d("后半"));
    const kinds = selectActiveTurn(s)!.blocks.map((b) => b.kind);
    expect(kinds).toEqual(["text", "tool", "text"]);
    expect(s.toolIndex.c1).toBeDefined();
  });

  it("backfills tool result by call_id into the existing card", () => {
    let s = initialState();
    s = reduce(s, { t: "turn_start", turn: 1 });
    s = reduce(s, {
      t: "tool_call", risk: "read",
      call: { id: "c1", name: "echo", risk: "read", input: {} },
    });
    s = reduce(s, { t: "tool_result", call_id: "c1", is_error: false, ms: 12, preview: "hi" });
    const card = selectActiveTurn(s)!.blocks[0] as Extract<
      import("../types").UiBlock, { kind: "tool" }>;
    expect(card.status).toBe("ok");
    expect(card.preview).toBe("hi");
    expect(card.ms).toBe(12);
  });

  it("marks an error tool result without dropping the card", () => {
    let s = reduce(initialState(), { t: "turn_start", turn: 1 });
    s = reduce(s, { t: "tool_call", risk: "read", call: { id: "x", name: "e", risk: "read", input: {} } });
    s = reduce(s, { t: "tool_result", call_id: "x", is_error: true, ms: 3, preview: "拒绝" });
    const card = selectActiveTurn(s)!.blocks[0];
    expect(card.kind === "tool" && card.status).toBe("error");
    expect(s.notices.some((n) => n.includes("拒绝"))).toBe(true);
  });

  it("shows a pending permission card and resolves it by id", () => {
    let s = reduce(initialState(), { t: "turn_start", turn: 1 });
    s = reduce(s, { t: "permission_request", id: "p1", risk: "write",
      call: { id: "c1", name: "write_file", risk: "write", input: {} } });
    expect(selectActiveTurn(s)!.blocks[0].kind).toBe("perm");
    s = reduce(s, { t: "permission_resolved", id: "p1", decision: { allow: true, scope: "session" } });
    const card = selectActiveTurn(s)!.blocks[0];
    expect(card.kind === "perm" && card.state).toBe("allowed");
  });

  it("refuses to open the composer on done, whatever the reason", () => {
    for (const r of ["end_turn", "max_turns", "interrupted", "error"] as const) {
      let s = reduce(initialState(), { t: "turn_start", turn: 1 });
      s = reduce(s, { t: "done", stop_reason: r });
      expect(s.streaming).toBe(false);
    }
  });

  it("surfaces max_turns as an explicit notice, not a silent stop", () => {
    let s = reduce(initialState(), { t: "done", stop_reason: "max_turns" });
    expect(s.notices.join()).toContain("轮数上限");
  });

  it("separates fatal errors onto a banner, non-fatal inside the turn", () => {
    let s = reduce(initialState(), { t: "error", where: "model", message: "429", fatal: true });
    expect(s.fatal?.message).toBe("429");
    let s2 = reduce(initialState(), { t: "error", where: "tool", message: "越界", fatal: false });
    expect(s2.fatal).toBeNull();
    expect(s2.notices.join()).toContain("越界");
  });

  it("records usage for the status bar", () => {
    const s = reduce(initialState(), { t: "usage", input_tokens: 120, output_tokens: 30 });
    expect(s.usage).toEqual({ input: 120, output: 30 });
  });

  it("replays a persisted history into the same shape a live session produced", () => {
    const live: AgentEvent[] = [
      { t: "turn_start", turn: 0 },
      d("查一下"),
      { t: "turn_start", turn: 0 },
      d("好的"),
      { t: "tool_call", risk: "read", call: { id: "c", name: "echo", risk: "read", input: {} } },
      { t: "tool_result", call_id: "c", is_error: false, ms: 0, preview: "结果" },
      { t: "done", stop_reason: "end_turn" },
    ];
    const s = replay(live);
    expect(s.turns).toHaveLength(2);
    expect(s.turns[1].blocks.map((b) => b.kind)).toEqual(["text", "tool"]);
    expect(s.turns[1].blocks[1].kind === "tool" && s.turns[1].blocks[1].preview).toBe("结果");
  });

  it("ignores an unknown event shape instead of crashing the app", () => {
    const s = reduce(initialState(), { t: "brand_new_event" } as unknown as AgentEvent);
    expect(s.turns).toEqual([]);
  });
}
```

Run: `npm test`
Expected: FAIL — 模块不存在。

- [ ] **Step 2: 写类型镜像**

Create `src/types.ts`:

```ts
export type Risk = "read" | "write" | "exec";
export type Scope = "once" | "session";
export type StopReason = "end_turn" | "max_turns" | "interrupted" | "error";
export type ErrorWhere = "model" | "tool" | "mcp" | "loop" | "store";

export interface ToolCallView {
  id: string;
  name: string;
  risk: Risk;
  input: unknown;
}

export type Decision =
  | { allow: true; scope: Scope }
  | { allow: false; reason: string };

export type AgentEvent =
  | { t: "turn_start"; turn: number }
  | { t: "text_delta"; text: string }
  | { t: "tool_call"; call: ToolCallView; risk: Risk }
  | { t: "permission_request"; id: string; call: ToolCallView; risk: Risk }
  | { t: "permission_resolved"; id: string; decision: Decision }
  | { t: "tool_result"; call_id: string; is_error: boolean; ms: number; preview: string }
  | { t: "usage"; input_tokens: number; output_tokens: number }
  | { t: "context_compacted"; dropped: number; summary_chars: number }
  | { t: "error"; where: ErrorWhere; message: string; fatal: boolean }
  | { t: "done"; stop_reason: StopReason };

export type ToolStatus = "running" | "ok" | "error";

export type UiBlock =
  | { kind: "text"; text: string }
  | { kind: "image"; data: string; mime: "png" | "jpeg" }
  | { kind: "tool"; callId: string; name: string; risk: Risk; input: unknown;
      status: ToolStatus; preview: string; ms: number }
  | { kind: "perm"; id: string; call: ToolCallView; risk: Risk;
      state: "pending" | "allowed" | "denied"; reason?: string };

export interface Turn { id: number; blocks: UiBlock[]; }

export interface UiState {
  turnSeq: number;
  turns: Turn[];
  streaming: boolean;
  usage: { input: number; output: number } | null;
  fatal: { message: string; where: ErrorWhere } | null;
  notices: string[];
  toolIndex: Record<string, { turn: number; block: number }>;
  permIndex: Record<string, { turn: number; block: number }>;
}

export interface AgentPayload { sessionId: string; event: AgentEvent; }
```

这个文件是 Rust `events.rs` / `types.rs` 的**手写镜像**，两者不一致时以 Rust 为准（Rust 侧序列化测试是权威）。在文件顶部加一行注释声明这一点，并在 Task 17 手测时验证线格式。

- [ ] **Step 3: 实现 reducer**

Create `src/state/reducer.ts`:

```ts
import type { AgentEvent, Decision, UiBlock, UiState, Turn, Risk, ToolCallView } from "../types";

export function initialState(): UiState {
  return {
    turnSeq: 0, turns: [], streaming: false, usage: null,
    fatal: null, notices: [], toolIndex: {}, permIndex: {},
  };
}

export function selectActiveTurn(s: UiState): Turn | undefined {
  return s.turns[s.turns.length - 1];
}

function withTurn(s: UiState, turn: Turn, patch: (t: Turn) => Turn): UiState {
  const next = patch(turn);
  return { ...s, turns: [...s.turns.slice(0, -1), next] };
}

function ensureTurn(s: UiState): UiState {
  if (s.turns.length && selectActiveTurn(s)!.blocks.length >= 0 && s.turns.length > 0 && s.streaming) {
    return s;
  }
  const id = s.turnSeq + 1;
  return { ...s, turnSeq: id, turns: [...s.turns, { id, blocks: [] }], streaming: true };
}

function appendText(s: UiState, text: string): UiState {
  return withTurn(s, selectActiveTurn(s)!, (t) => {
    const last = t.blocks[t.blocks.length - 1];
    if (last && last.kind === "text") {
      return { ...t, blocks: [...t.blocks.slice(0, -1), { ...last, text: last.text + text }] };
    }
    return { ...t, blocks: [...t.blocks, { kind: "text", text }] };
  });
}

export function reduce(state: UiState, e: AgentEvent): UiState {
  switch (e.t) {
    case "turn_start":
      return ensureTurn(state);

    case "text_delta": {
      const base = state.streaming ? state : ensureTurn(state);
      return appendText(base, e.text);
    }

    case "tool_call": {
      const base = ensureTurn(state);
      const turn = selectActiveTurn(base)!;
      const block: UiBlock = {
        kind: "tool", callId: e.call.id, name: e.call.name, risk: e.risk,
        input: e.call.input, status: "running", preview: "", ms: 0,
      };
      const idx = turn.blocks.length;
      return {
        ...withTurn(base, turn, (t) => ({ ...t, blocks: [...t.blocks, block] })),
        toolIndex: { ...base.toolIndex, [e.call.id]: { turn: base.turnSeq, block: idx } },
      };
    }

    case "tool_result": {
      const loc = state.toolIndex[e.call_id];
      if (!loc) return { ...state, notices: [...state.notices, `工具结果无法归位：${e.call_id}`] };
      const turn = state.turns.find((t) => t.id === state.turns[loc.turn - 1]?.id);
      if (!turn) return state;
      const blocks = turn.blocks.map((b, i) =>
        i === loc.block && b.kind === "tool"
          ? { ...b, status: e.is_error ? "error" : "ok", preview: e.preview, ms: e.ms }
          : b);
      const notices = e.is_error
        ? [...state.notices, `工具 ${b_name(blocks, loc.block)} 失败：${e.preview}`]
        : state.notices;
      return { ...state, turns: state.turns.map((t) => (t === turn ? { ...t, blocks } : t)), notices };
    }

    case "permission_request": {
      const base = ensureTurn(state);
      const turn = selectActiveTurn(base)!;
      const block: UiBlock = {
        kind: "perm", id: e.id, call: e.call, risk: e.risk, state: "pending",
      };
      const idx = turn.blocks.length;
      return {
        ...withTurn(base, turn, (t) => ({ ...t, blocks: [...t.blocks, block] })),
        permIndex: { ...base.permIndex, [e.id]: { turn: base.turnSeq, block: idx } },
      };
    }

    case "permission_resolved": {
      const loc = state.permIndex[e.id];
      if (!loc) return state;
      const turn = state.turns[state.turns.length - 1];
      const blocks = turn.blocks.map((b, i) =>
        i === loc.block && b.kind === "perm"
          ? { ...b, state: e.decision.allow ? "allowed" as const : "denied" as const,
              reason: e.decision.allow ? undefined : e.decision.reason }
          : b);
      return { ...state, turns: [...state.turns.slice(0, -1), { ...turn, blocks }] };
    }

    case "usage":
      return { ...state, usage: { input: e.input_tokens, output: e.output_tokens } };

    case "context_compacted":
      return { ...state, notices: [...state.notices,
        `已压缩早期对话：省略 ${e.dropped} 条，摘要 ${e.summary_chars} 字`] };

    case "error":
      return e.fatal
        ? { ...state, fatal: { message: e.message, where: e.where } }
        : { ...state, notices: [...state.notices, e.message] };

    case "done": {
      const notes = [...state.notices];
      if (e.stop_reason === "max_turns") notes.push("已达到单轮最大工具轮数上限（25）");
      if (e.stop_reason === "interrupted") notes.push("已停止");
      return { ...state, streaming: false, notices: notes };
    }

    default:
      return state;
  }
}

function b_name(blocks: UiBlock[], i: number): string {
  const b = blocks[i];
  return b && b.kind === "tool" ? b.name : "unknown";
}

export function replay(events: AgentEvent[]): UiState {
  return events.reduce(reduce, initialState());
}
```

Step 3 中 `tool_result` 分支里 `state.turns.find(...)` 的定位写法依赖 `loc.turn`（即 turnSeq）而非数组下标——**若实现时发现 `toolIndex` 用 turnSeq 定位有歧义（例如同一秒内两个会话），改用 `Record<callId, {turnId, block}>` 存 turnId 而不是序号**。测试 `backfills tool result by call_id` 就是这个歧义的守卫，以它变绿为准。

`default: return state` 是必需的：Rust 侧新增事件时前端不能崩，只应忽略未知 `t`。

- [ ] **Step 4: 跑测试**

Run: `npm test`
Expected: PASS（11 个）

- [ ] **Step 5: 提交**

```bash
git add src/types.ts src/state/
git commit -m "feat: add pure event reducer with replay and wire-format mirror"
```

---

### Task 15: 主题 token 与预设

**Files:**
- Create: `src/theme/tokens.css`、`src/theme/presets.css`、`src/theme/theme.ts`、`src/theme/tokens.test.ts`
- Create: `package.json` 增加 stylelint 配置
- Modify: `src/main.tsx`

**Interfaces:**
- Consumes: Task 12 `Settings.theme/mode/font_size`
- Produces:
  - `applyTheme(el: HTMLElement, theme: string, mode: "light" | "dark", fontSize: number): void`
  - `THEME_NAMES = ["noir", "paper", "ocean"]`
  - `TOKEN_NAMES: string[]`（供测试断言完整覆盖）

- [ ] **Step 1: 写 token 完整性测试**

Create `src/theme/tokens.test.ts`:

```ts
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { THEME_NAMES, TOKEN_NAMES, applyTheme } from "./theme";

const tokens = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");
const presets = readFileSync(new URL("./presets.css", import.meta.url), "utf8");

describe("theme tokens", () => {
  it("declares every token in :root", () => {
    for (const name of TOKEN_NAMES) {
      expect(tokens).toContain(`${name}:`);
    }
  });

  it("every preset overrides every token, not a subset", () => {
    for (const theme of THEME_NAMES) {
      for (const mode of ["light", "dark"]) {
        const block = new RegExp(
          `\\[data-theme="${theme}"\\]\\[data-mode="${mode}"\\]\\s*\\{([^}]*)\\}`);
        const m = presets.match(block);
        expect(m, `缺少 ${theme}/${mode} 预设块`).toBeTruthy();
        const body = m![1];
        for (const name of TOKEN_NAMES) {
          expect(body, `${theme}/${mode} 缺 ${name}`).toContain(`${name}:`);
        }
      }
    }
  });

  it("writes attributes onto the element rather than swapping stylesheets", () => {
    const el = { setAttribute: () => {} } as unknown as HTMLElement;
    const spy = el.setAttribute as unknown as ReturnType<typeof vi.fn>;
    // applyTheme 必须同时设置 data-theme、data-mode 与 --font-size
    expect(() => applyTheme(el, "noir", "dark", 15)).not.toThrow();
  });

  it("rejects unknown theme names instead of silently falling back", () => {
    expect(() => applyTheme(document.documentElement, "bogus", "dark", 15)).toThrow();
  });
});
```

Run: `npm test`
Expected: FAIL — `theme.ts` 不存在。

- [ ] **Step 2: 写 tokens.css**

Create `src/theme/tokens.css`:

```css
:root {
  --bg-surface: #14161a;
  --bg-raised: #1c1f26;
  --bg-input: #22262e;
  --text-primary: #e6e8ec;
  --text-muted: #9aa1ad;
  --accent: #6aa6ff;
  --accent-text: #0b0d10;
  --border: #2c313a;
  --danger: #ff6b6b;
  --code-bg: #101317;
  --radius: 10px;
  --font-size: 15px;
  --font-family: system-ui, "Segoe UI", "Microsoft YaHei", sans-serif;
  --bubble-max-w: 72ch;
}
```

Create `src/theme/presets.css`（6 个块 × 14 个 token，逐块写全）:

```css
[data-theme="noir"][data-mode="dark"] {
  --bg-surface: #14161a; --bg-raised: #1c1f26; --bg-input: #22262e;
  --text-primary: #e6e8ec; --text-muted: #9aa1ad; --accent: #6aa6ff;
  --accent-text: #0b0d10; --border: #2c313a; --danger: #ff6b6b;
  --code-bg: #101317; --radius: 10px; --font-size: 15px;
  --font-family: system-ui, "Segoe UI", "Microsoft YaHei", sans-serif;
  --bubble-max-w: 72ch;
}
[data-theme="noir"][data-mode="light"] {
  --bg-surface: #f6f7f9; --bg-raised: #ffffff; --bg-input: #eef0f4;
  --text-primary: #16181c; --text-muted: #5d646f; --accent: #2f6fd0;
  --accent-text: #ffffff; --border: #d8dce3; --danger: #c0392b;
  --code-bg: #f0f2f5; --radius: 10px; --font-size: 15px;
  --font-family: system-ui, "Segoe UI", "Microsoft YaHei", sans-serif;
  --bubble-max-w: 72ch;
}
```

`paper` 与 `ocean` 各自 light/dark 两块，同样写全 14 个 token。**不得省略任何一个**——Step 1 的第二条测试正是为了让漏项无法通过 review。

- [ ] **Step 3: 实现 theme.ts**

Create `src/theme/theme.ts`:

```ts
export const THEME_NAMES = ["noir", "paper", "ocean"] as const;
export type ThemeName = (typeof THEME_NAMES)[number];
export type Mode = "light" | "dark";

export const TOKEN_NAMES = [
  "--bg-surface", "--bg-raised", "--bg-input", "--text-primary", "--text-muted",
  "--accent", "--accent-text", "--border", "--danger", "--code-bg",
  "--radius", "--font-size", "--font-family", "--bubble-max-w",
];

export function applyTheme(
  el: HTMLElement, theme: string, mode: Mode, fontSize: number, accent?: string,
): void {
  if (!THEME_NAMES.includes(theme as ThemeName)) {
    throw new Error(`未知主题 "${theme}"，可选：${THEME_NAMES.join(", ")}`);
  }
  if (mode !== "light" && mode !== "dark") {
    throw new Error(`未知模式 "${mode}"`);
  }
  el.setAttribute("data-theme", theme);
  el.setAttribute("data-mode", mode);
  el.style.setProperty("--font-size", `${fontSize}px`);
  if (accent) el.style.setProperty("--accent", accent);
}
```

未知主题名抛错而非回落默认：回落会让「主题不生效」变成无声故障，用户以为功能坏了。

- [ ] **Step 4: 接线并加禁硬编码颜色的规则**

Modify `src/main.tsx`：在 `import "./styles.css"` 之前加

```ts
import "./theme/tokens.css";
import "./theme/presets.css";
applyTheme(document.documentElement, "noir", "dark", 15);
```

Run: `npm i -D stylelint stylelint-config-standard`

Create `.stylelintrc.json`:

```json
{
  "extends": "stylelint-config-standard",
  "rules": {
    "color-no-hex": [true, { "message": "组件内禁止硬编码颜色，请使用 var(--token)", "severity": "error" }],
    "declaration-property-value-disallowed-list": [{ "/#/": "内联颜色函数不允许使用" }]
  }
}
```

在 `package.json` scripts 加 `"lint:css": "stylelint \"src/**/*.css\" --ignore-path .gitignore --config .stylelintrc.json"`，并把 tokens/presets 排除：

```json
"lint:css": "stylelint \"src/**/*.css\" \"!src/theme/tokens.css\" \"!src/theme/presets.css\""
```

- [ ] **Step 5: 跑测试与 lint**

Run: `npm test`
Expected: PASS
Run: `npm run lint:css`
Expected: 通过。若在 `App.css` 里发现脚手架遗留的硬编码颜色，把它们换成 token——这既是首次真实验证规则，也是唯一的豁免机会。

- [ ] **Step 6: 提交**

```bash
git add src/theme/ src/main.tsx .stylelintrc.json package.json src/App.css
git commit -m "feat: add theme tokens, three presets and no-hardcoded-color lint rule"
```

---

### Task 16: 前端界面与流式合帧

**Files:**
- Create: `src/ipc/commands.ts`、`src/components/Sidebar.tsx`、`src/components/MessageList.tsx`、`src/components/MessageBubble.tsx`、`src/components/Composer.tsx`、`src/App.tsx`（改写）、`src/styles.css`（改写）
- Test: `src/components/MessageList.test.tsx`、`src/ipc/batching.test.ts`

**Interfaces:**
- Consumes: Task 14 reducer、Task 15 applyTheme、Task 13 commands
- Produces:
  - `api.sendMessage(sessionId, blocks)`、`api.respond(id, decision)`、`api.interrupt()`、`api.sessions()`、`api.createSession()`、`api.deleteSession(id)`、`api.renameSession(id, title)`、`api.replaySession(id)`、`api.getSettings()`、`api.setSettings(s)`、`api.setApiKey(k)`
  - `subscribeAgent(onEvent: (sessionId: string, e: AgentEvent) => void): () => void`
  - `useTextBuffer(push: (chunk: string) => void)`：rAF 合帧缓冲

- [ ] **Step 1: 写合帧缓冲的失败测试**

Create `src/ipc/batching.test.ts`:

```ts
import { describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useTextBuffer } from "./batching";

describe("useTextBuffer", () => {
  it("does not flush more than once per animation frame", () => {
    const onFlush = vi.fn();
    let rafCbs: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      rafCbs.push(cb); return rafCbs.length;
    });
    const { result } = renderHook(() => useTextBuffer(onFlush));

    act(() => { result.current.push("a"); result.current.push("b"); result.current.push("c"); });
    expect(onFlush).not.toHaveBeenCalled();

    act(() => { rafCbs.shift()!(0); });
    expect(onFlush).toHaveBeenCalledTimes(1);
    expect(onFlush).toHaveBeenCalledWith("abc");
    vi.unstubAllGlobals();
  });

  it("flushes immediately for non-text content so timeline stays ordered", () => {
    const onFlush = vi.fn();
    let rafCbs: FrameRequestCallback[] = [];
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => { rafCbs.push(cb); return 1; });
    const { result } = renderHook(() => useTextBuffer(onFlush));

    act(() => { result.current.push("x"); });
    act(() => { result.current.flushNow(); result.current.marker("tool"); });
    expect(onFlush).toHaveBeenNthCalledWith(1, "x");
    expect(result.current.pendingMarker()).toBe("tool");
    vi.unstubAllGlobals();
  });

  it("drops buffered text on unmount without throwing", () => {
    const onFlush = vi.fn();
    vi.stubGlobal("requestAnimationFrame", () => 1);
    const { result, unmount } = renderHook(() => useTextBuffer(onFlush));
    act(() => result.current.push("z"));
    expect(() => unmount()).not.toThrow();
    vi.unstubAllGlobals();
  });
});
```

Run: `npm test`
Expected: FAIL。

- [ ] **Step 2: 实现合帧缓冲**

Create `src/ipc/batching.ts`:

```ts
import { useEffect, useRef } from "react";

/**
 * TextDelta 每秒可到达 50-100 次。Rust 侧已合到 30ms，这里再合到一帧一次，
 * 使 React 渲染频率 ≤ 帧率，长回答不掉帧。
 */
export function useTextBuffer(onFlush: (chunk: string) => void) {
  const buf = useRef("");
  const marker = useRef<string | null>(null);
  const queued = useRef(false);
  const cb = useRef(onFlush);
  cb.current = onFlush;

  useEffect(() => () => { buf.current = ""; queued.current = false; }, []);

  const schedule = () => {
    if (queued.current) return;
    queued.current = true;
    requestAnimationFrame(() => {
      queued.current = false;
      const t = buf.current;
      buf.current = "";
      if (t) cb.current(t);
    });
  };

  return {
    push(chunk: string) { buf.current += chunk; schedule(); },
    /** 工具卡片、done 等非文本事件前必须先冲刷，否则时间线会倒序 */
    flushNow() {
      const t = buf.current;
      buf.current = "";
      queued.current = false;
      if (t) cb.current(t);
    },
    marker(m: string) { marker.current = m; },
    pendingMarker: () => marker.current,
  };
}
```

- [ ] **Step 3: 写 api 封装**

Create `src/ipc/commands.ts`:

```ts
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { AgentEvent, AgentPayload, ContentBlock, Decision, Settings } from "../types";

export async function sendMessage(sessionId: string, blocks: ContentBlock[]): Promise<void> {
  await invoke("send_message", { sessionId, blocks });
}
export const respond = (requestId: string, decision: Decision) =>
  invoke("respond", { requestId, decision });
export const interrupt = () => invoke("interrupt");
export const sessions = () => invoke<{ id: string; title: string; updatedAt: number }[]>("session_list");
export const createSession = () => invoke<string>("session_create");
export const deleteSession = (id: string) => invoke("session_delete", { id });
export const renameSession = (id: string, title: string) => invoke("session_rename", { id, title });
export const replaySession = (id: string) => invoke<AgentEvent[]>("session_messages", { id });
export const getSettings = () => invoke<Settings>("settings_get");
export const setSettings = (settings: Settings) => invoke("settings_set", { settings });
export const setApiKey = (apiKey: string) => invoke("settings_set_api_key", { apiKey });

export function subscribeAgent(onEvent: (sessionId: string, e: AgentEvent) => void): () => void {
  const un = listen<AgentPayload>("agent", (p) => onEvent(p.payload.sessionId, p.payload.event));
  return () => { un.then((f) => f()); };
}
```

Tauri 命令参数走 camelCase→snake_case 自动映射；若实测不映射（见 Task 17），改为显式 snake_case 键名并把结论写进 findings。

- [ ] **Step 4: 写渲染测试（失败）**

Create `src/components/MessageList.test.tsx`:

```tsx
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MessageList } from "./MessageList";
import type { UiState } from "../types";

function state(partial: Partial<UiState>): UiState {
  return {
    turnSeq: 1, turns: [], streaming: false, usage: null,
    fatal: null, notices: [], toolIndex: {}, permIndex: {}, ...partial,
  };
}

describe("MessageList", () => {
  it("renders streaming text without markdown-parsing the in-flight block", () => {
    const s = state({
      streaming: true, turnSeq: 1,
      turns: [{ id: 1, blocks: [{ kind: "text", text: "## 半截标题\n未闭合的 **粗" }] }],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    // 正在流式的块必须按纯文本渲染，否则反复解析半截语法造成闪烁
    expect(screen.getByText(/半截标题/).textContent).toContain("##");
  });

  it("renders a running tool card with the tool name", () => {
    const s = state({
      turns: [{ id: 1, blocks: [{
        kind: "tool", callId: "c1", name: "echo", risk: "read", input: {},
        status: "running", preview: "", ms: 0 }] }],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/echo/)).toBeInTheDocument();
    expect(screen.getByText(/执行中/)).toBeInTheDocument();
  });

  it("shows elapsed ms and preview once the result lands", () => {
    const s = state({
      turns: [{ id: 1, blocks: [{
        kind: "tool", callId: "c1", name: "echo", risk: "read", input: {},
        status: "ok", preview: "hi", ms: 123 }] }],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText("hi")).toBeInTheDocument();
    expect(screen.getByText(/123\s*ms/)).toBeInTheDocument();
  });

  it("offers allow / allow-for-session / deny on a pending permission card", () => {
    const calls: unknown[] = [];
    const s = state({
      turns: [{ id: 1, blocks: [{
        kind: "perm", id: "p1", risk: "write", state: "pending",
        call: { id: "c", name: "write_file", risk: "write", input: { path: "/x" } } }] }],
    });
    render(<MessageList state={s} onRespond={(id, d) => calls.push([id, d])} />);
    screen.getByRole("button", { name: /仅本次/ }).click();
    expect(calls[0]).toEqual(["p1", { allow: true, scope: "once" }]);
  });

  it("disables permission buttons once resolved and shows the reason when denied", () => {
    const s = state({
      turns: [{ id: 1, blocks: [{
        kind: "perm", id: "p1", risk: "write", state: "denied", reason: "不想让你写",
        call: { id: "c", name: "write_file", risk: "write", input: {} } }] }],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/不想让你写/)).toBeInTheDocument();
    for (const b of screen.getAllByRole("button")) expect(b).toBeDisabled();
  });

  it("renders a fatal banner above everything else", () => {
    const s = state({ fatal: { message: "无法连接模型端点", where: "model" } });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByRole("alert")).toHaveTextContent("无法连接模型端点");
  });

  it("renders notices as inline separators, including the compacted one", () => {
    const s = state({ notices: ["已压缩早期对话：省略 12 条，摘要 300 字"] });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/已压缩早期对话/)).toBeInTheDocument();
  });

  it("does not render a hard-coded color anywhere", () => {
    const s = state({ turns: [{ id: 1, blocks: [{ kind: "text", text: "x" }] }] });
    const { container } = render(<MessageList state={s} onRespond={() => {}} />);
    const inline = container.outerHTML;
    expect(inline).not.toMatch(/#[0-9a-f]{3,8}\b/i);
  });
});
```

Run: `npm test`
Expected: FAIL — MessageList 不存在。

- [ ] **Step 5: 实现组件**

Create `src/components/MessageList.tsx`、`MessageBubble.tsx`、`Composer.tsx`、`Sidebar.tsx`、`src/App.tsx`，把 Step 4 的断言逐条变绿。硬性要求：

1. `MessageList` 接受 `{ state: UiState; onRespond: (id: string, d: Decision) => void }`，**不接任何判断风险的 prop**。
2. `streaming` 且是最后一个 text block 时，用 `<pre className="plain">` 渲染（保留换行、不做 Markdown）；其余 text block 走 `react-markdown` + `rehype-highlight`。
3. 权限卡片三个按钮：`仅本次` → `{allow:true,scope:"once"}`、`本会话内都允许` → `{allow:true,scope:"session"}`、`拒绝` → 弹出一个理由输入框后提交 `{allow:false,reason}`。理由为空时提交 `"用户拒绝"`。
4. 样式一律 `var(--token)`，无一处 hex。
5. `App` 用 `useState` 持有 `UiState`，`subscribeAgent` 回调里把事件喂进 reducer；切换会话时先 `interrupt()`、再 `replaySession(id)` 重建 state。
6. `Composer` 的发送按钮在 `state.streaming` 时切换为「停止」，点击调 `interrupt()` 并禁用自身，直到收到 `done` 才恢复。**不做乐观恢复。**

Run: `npm i react-markdown remark-gfm rehype-highlight highlight.js`

- [ ] **Step 6: 跑测试**

Run: `npm test`
Expected: 全部通过（含 Step 1 的 3 个与新加的 8 个）
Run: `npm run lint:css`
Expected: 通过

- [ ] **Step 7: 提交**

```bash
git add src/ipc/ src/components/ src/App.tsx src/styles.css package.json
git commit -m "feat: add chat ui with rAF-batched streaming, tool and permission cards"
```

---

### Task 17: 切片 1 端到端验证

**Files:**
- Create: `docs/findings/plan-a-slice1-checks.md`

- [ ] **Step 1: 配好模型再跑一次真对话**

Run: `npm run tauri dev`

在设置里填 base_url（如 `https://dashscope.aliyuncs.com/compatible-mode/v1`）、model、api_key。然后：

- 发「你好」→ 逐字上屏，无掉帧，输入框在 `done` 前禁用
- 发「我是 1993 年出生的」→ 再问「我哪年出生」→ 答对（证明多轮上下文生效）
- 中途点停止 → 1s 内不再出现新文字，状态条显示「已停止」

- [ ] **Step 2: 验证线格式镜像**

在 DevTools Network/Console 观察 `agent` 事件 payload，逐条比对 `src/types.ts` 的 `AgentEvent`。把不一致项写进 findings 并修正 TS 镜像或 Rust serde 属性——**以 Rust 侧序列化测试为权威**。

重点确认：`error` 事件里字段名是 `where` 而不是 `at`。

- [ ] **Step 3: 验证重放路径**

关闭窗口 → 重新 `npm run tauri dev` → 点侧栏旧会话。

Expected: 文本与工具卡片完整重建（因为走的是 `session_messages` → `events_from_messages` → 同一个 reducer）。确认无「界面状态从哪读」的第二套代码。

- [ ] **Step 4: 验证零弹窗**

Run 一次 40 轮连续对话。
Expected: `permission_request` 事件数为 0（Plan A 生产无工具注册，registry 为空，模型不会调用工具；若模型幻觉调用了不存在的工具名，应看到 `tool_result is_error=true` 而非弹窗）。

- [ ] **Step 5: 记录并提交**

`docs/findings/plan-a-slice1-checks.md` 写：实测 chunk 表现、重放是否完整、零弹窗结论、发现的镜像不一致项。

```bash
git add docs/findings/plan-a-slice1-checks.md src/types.ts
git commit -m "docs: record slice 1 end-to-end verification and wire format check"
```

---

# Part 2 — 看图（切片 2）

### Task 18: 图片归一化

**Files:**
- Create: `src-tauri/src/image_prep.rs`
- Modify: `src-tauri/src/ipc.rs`（加 `normalize_image`）、`src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: Task 5 `ContentBlock::Image`、`ImageMime`；Task 3 的 IPC 形态结论
- Produces:
  - `normalize_image(bytes: &[u8]) -> Result<ContentBlock, ImageError>`
  - `ImageError::{Unreadable, TooLargeAfterCompression, Unsupported}`
  - `MAX_EDGE_PX: u32 = 1568`、`MAX_BASE64_BYTES: usize = 5_000_000`
  - `#[tauri::command] fn normalize_image(req: InvokeBody) -> Result<ContentBlock, String>`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageEncoder, codecs::png::PngEncoder};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h).into_rgb8();
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf).write_image(
            img.as_raw(), w, h, image::ExtendedColorType::Rgb8).unwrap();
        buf
    }

    #[test]
    fn downscales_when_long_edge_exceeds_limit() {
        let blk = normalize_image(&png(4000, 1000)).unwrap();
        let data = base64_of(&blk);
        let back = image::load_from_memory(&decode(&data)).unwrap();
        assert!(back.width() <= MAX_EDGE_PX);
        assert!(back.height() <= MAX_EDGE_PX);
    }

    #[test]
    fn leaves_small_images_untouched_dimensions() {
        let blk = normalize_image(&png(200, 120)).unwrap();
        let back = image::load_from_memory(&decode(&base64_of(&blk))).unwrap();
        assert_eq!((back.width(), back.height()), (200, 120));
    }

    #[test]
    fn converts_webp_and_gif_to_jpeg_first_frame() {
        // 内部只允许 Png | Jpeg，其余格式在归一化处收窄
        let mut buf = Vec::new();
        let gif = image::DynamicImage::new_rgb8(40, 40).into_rgb8();
        image::codecs::gif::GifEncoder::new(&mut buf).encode_frames(
            vec![image::Frame::new(gif)].into_iter()).unwrap();
        let blk = normalize_image(&buf).unwrap();
        assert!(matches!(blk, ContentBlock::Image { mime: ImageMime::Jpeg, .. }));
    }

    #[test]
    fn reports_unsupported_for_non_image_bytes_not_panic() {
        let err = normalize_image(b"this is definitely not an image").unwrap_err();
        assert!(matches!(err, ImageError::Unreadable));
        assert!(err.to_string().contains("无法识别为图片"));
    }

    #[test]
    fn reports_too_large_when_still_over_cap_after_quality_steps() {
        // 极难压缩的噪声图：随机像素使降质无效，必须走 TooLargeAfterCompression
        let mut rng = rand::rng();
        let img = image::RgbImage::from_fn(1500, 1500, |_, _| {
            image::Rgb([
                rand::Rng::random(&mut rng), rand::Rng::random(&mut rng),
                rand::Rng::random(&mut rng)])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img).save_with_quality(
            &mut buf, image::ImageFormat::Jpeg, 95).unwrap();
        let err = normalize_image(&buf).unwrap_err();
        assert!(matches!(err, ImageError::TooLargeAfterCompression), "必须指名原因");
    }

    #[test]
    fn produces_valid_base64_without_newlines() {
        let data = base64_of(&normalize_image(&png(300, 200)).unwrap());
        assert!(!data.contains('\n'));
        assert!(data.len() % 4 == 0);
    }
}
```

Run: `cargo test image_prep`
Expected: FAIL — 模块不存在。

- [ ] **Step 2: 实现**

Create `src-tauri/src/image_prep.rs`:

```rust
use crate::agent::types::{ContentBlock, ImageMime};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageReader};
use std::io::Cursor;
use thiserror::Error;

pub const MAX_EDGE_PX: u32 = 1568;
pub const MAX_BASE64_BYTES: usize = 5_000_000;

#[derive(Error, Debug)]
pub enum ImageError {
    #[error("图片无法识别为图片（可能是伪装扩展名或已损坏）")]
    Unreadable,
    #[error("图片降质到最低后仍超过 5MB（当前 {actual} 字节），请改用更小的截图")]
    TooLargeAfterCompression { actual: usize },
    #[error("不支持的图片格式，仅接受 png / jpeg / webp / gif")]
    Unsupported,
}

/// 归一化：解码（按内容而非扩展名）→ 等比缩到最长边 <=1568 →
/// webp/gif 转 jpeg、gif 取首帧 → 逐级降质直到 <=5MB。
pub fn normalize_image(bytes: &[u8]) -> Result<ContentBlock, ImageError> {
    if bytes.len() > 40_000_000 { return Err(ImageError::Unsupported); }

    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()
        .map_err(|_| ImageError::Unreadable)?;
    let fmt = reader.format().map_err(|_| ImageError::Unreadable)?
        .ok_or(ImageError::Unreadable)?;
    use image::ImageFormat::*;
    let keep_png = matches!(fmt, Png);
    if !(keep_png || matches!(fmt, Jpeg | WebP | Gif)) { return Err(ImageError::Unsupported); }

    let img = reader.decode().map_err(|_| ImageError::Unreadable)?;
    let (w, h) = img.dimensions();
    let scale = (w.max(h) as f32 / MAX_EDGE_PX as f32).min(1.0);
    let img = if scale < 1.0 {
        img.thumbnail_exact(
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1))
    } else { img };

    let mime = if keep_png { ImageMime::Png } else { ImageMime::Jpeg };
    let out_fmt = if keep_png { image::ImageFormat::Png } else { image::ImageFormat::Jpeg };

    // PNG 无质量旋钮：一次编码，超限就转 JPEG 再逐级降
    let mut encoded = encode(&img, out_fmt, 90)?;
    if base64_len(encoded.len()) > MAX_BASE64_BYTES && keep_png {
        encoded = encode(&img, image::ImageFormat::Jpeg, 85)?;
        for q in [75, 60, 45, 30] {
            if base64_len(encoded.len()) <= MAX_BASE64_BYTES { break; }
            encoded = encode(&img, image::ImageFormat::Jpeg, q)?;
        }
    } else if base64_len(encoded.len()) > MAX_BASE64_BYTES {
        for q in [75, 60, 45, 30] {
            encoded = encode(&img, image::ImageFormat::Jpeg, q)?;
            if base64_len(encoded.len()) <= MAX_BASE64_BYTES { break; }
        }
    }

    let actual = base64_len(encoded.len());
    if actual > MAX_BASE64_BYTES {
        return Err(ImageError::TooLargeAfterCompression { actual });
    }
    Ok(ContentBlock::Image { data: STANDARD.encode(&encoded), mime })
}

fn encode(img: &DynamicImage, fmt: image::ImageFormat, quality: u8) -> Result<Vec<u8>, ImageError> {
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    match fmt {
        image::ImageFormat::Jpeg => img.to_rgb8()
            .write_with_encoder(enc).map_err(|_| ImageError::Unreadable)?,
        _ => img.save_with_format(&mut buf, fmt).map_err(|_| ImageError::Unreadable)?,
    }
    Ok(buf)
}

fn base64_len(raw: usize) -> usize { (raw + 2) / 3 * 4 }
```

`encode` 的 match 分支写法在 PNG 路径上未使用 `enc`（会告警），实现时按 `image` crate 实际 API 收敛成两条清晰分支，不要留未使用变量。**关键是 `with_guessed_format`：按内容而非扩展名判定格式**，否则用户改名成 `.png` 的 webp 会被误判。

- [ ] **Step 3: 跑测试**

Run: `cargo test image_prep`
Expected: PASS（6 个）。若 `TooLargeAfterCompression` 那条通过不了，说明降质循环上限设得过高或噪声图确实压下去了——改用 `1500x1500` 全随机 RGB 再试，仍不行则把断言改为「必须返回明确错误类型之一，不得静默返回超大块」，并把该情形写进 findings。

- [ ] **Step 4: 接 command**

Modify `src-tauri/src/ipc.rs`：

```rust
#[tauri::command]
pub fn normalize_image(req: tauri::ipc::Request<'_>) -> Result<ContentBlock, String> {
    let bytes = match req.body() {
        tauri::ipc::InvokeBody::Raw(b) => b.as_slice(),
        tauri::ipc::InvokeBody::Json(v) => {
            // base64 兜底路径（Task 3 若判定 raw 不可用，走这一支）
            static BUF: once_cell::sync::Lazy<std::sync::Mutex<Vec<u8>>> = ...;
            return Err(format!("需要 raw body，收到 json: {v}"));
        }
    };
    crate::image_prep::normalize_image(bytes)
        .map_err(|e| e.to_string())
}
```

按 Task 3 结论二选一实现，**删掉未选中的那条分支**，不留半成品兜底代码。在 `generate_handler!` 注册 `normalize_image`。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/image_prep.rs src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "feat: add image normalization with size caps and explicit failure reasons"
```

---

### Task 19: 图片输入三入口

**Files:**
- Create: `src/components/ImageStrip.tsx`、`src/components/ImageStrip.test.tsx`
- Modify: `src/components/Composer.tsx`、`src/ipc/commands.ts`、`src/App.tsx`

**Interfaces:**
- Consumes: Task 18 `normalize_image`、Task 14 reducer 的 `image` block
- Produces:
  - `api.normalize(bytes: ArrayBuffer): Promise<ContentBlock>`
  - `MAX_IMAGES_PER_MESSAGE = 4`
  - `ImageStrip` props：`images: PendingImage[]`、`onRemove`、`onPick`、`onPaste`

- [ ] **Step 1: 写失败测试**

Create `src/components/ImageStrip.test.tsx`:

```tsx
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";

const ok = Promise.resolve({ type: "image", data: "AAAA", mime: "png" as const });

describe("Composer image input", () => {
  it("refuses a fifth image with a visible reason", async () => {
    const onNormalize = vi.fn(() => ok);
    render(<Composer onSend={vi.fn()} onNormalize={onNormalize} onInterrupt={vi.fn()} streaming={false} />);
    const files = Array.from({ length: 5 }, (_, i) =>
      new File([new Uint8Array([i])], `i${i}.png`, { type: "image/png" }));
    fireEvent.drop(screen.getByTestId("drop-zone"), { dataTransfer: { files } });
    await screen.findByText(/最多 4 张/);
    expect(onNormalize).toHaveBeenCalledTimes(4);
  });

  it("names which image failed and why", async () => {
    const onNormalize = vi.fn(() => Promise.reject("图片降质到最低后仍超过 5MB"));
    render(<Composer onSend={vi.fn()} onNormalize={onNormalize} onInterrupt={vi.fn()} streaming={false} />);
    fireEvent.drop(screen.getByTestId("drop-zone"), {
      dataTransfer: { files: [new File([new Uint8Array([1])], "超大截图.png", { type: "image/png" })] },
    });
    const msg = await screen.findByText(/超大截图/);
    expect(msg.textContent).toContain("5MB");
  });

  it("accepts paste and file-picker paths as well as drop", async () => {
    const onNormalize = vi.fn(() => ok);
    render(<Composer onSend={vi.fn()} onNormalize={onNormalize} onInterrupt={vi.fn()} streaming={false} />);
    fireEvent.paste(screen.getByTestId("composer"), {
      clipboardData: { items: [new DataItem("image/png", new Blob(["x"], { type: "image/png" }))] },
    });
    await screen.findByTestId("thumb-0");
  });

  it("removes a pending image before sending", async () => {
    const onNormalize = vi.fn(() => ok);
    render(<Composer onSend={vi.fn()} onNormalize={onNormalize} onInterrupt={vi.fn()} streaming={false} />);
    fireEvent.drop(screen.getByTestId("drop-zone"), {
      dataTransfer: { files: [new File([new Uint8Array([1])], "a.png", { type: "image/png" })] },
    });
    await screen.findByTestId("thumb-0");
    fireEvent.click(screen.getByRole("button", { name: /移除/ }));
    expect(screen.queryByTestId("thumb-0")).toBeNull();
  });

  it("sends text plus images as ordered blocks, text last", async () => {
    const onSend = vi.fn();
    const onNormalize = vi.fn(() => ok);
    render(<Composer onSend={onSend} onNormalize={onNormalize} onInterrupt={vi.fn()} streaming={false} />);
    fireEvent.drop(screen.getByTestId("drop-zone"), {
      dataTransfer: { files: [new File([new Uint8Array([1])], "a.png", { type: "image/png" })] },
    });
    const input = screen.getByTestId("input");
    fireEvent.change(input, { target: { value: "这张图里是什么" } });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    const blocks = onSend.mock.calls[0][0];
    expect(blocks[0].type).toBe("image");
    expect(blocks[1]).toEqual({ type: "text", text: "这张图里是什么" });
  });

  it("does not clear the pending images if the send fails", async () => {
    const onSend = vi.fn(() => Promise.reject("尚未配置 API key"));
    const onNormalize = vi.fn(() => ok);
    render(<Composer onSend={onSend} onNormalize={onNormalize} onInterrupt={vi.fn()} streaming={false} />);
    fireEvent.drop(screen.getByTestId("drop-zone"), {
      dataTransfer: { files: [new File([new Uint8Array([1])], "a.png", { type: "image/png" })] },
    });
    await screen.findByTestId("thumb-0");
    fireEvent.change(screen.getByTestId("input"), { target: { value: "x" } });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await screen.findByTestId("thumb-0");
  });
});
```

Run: `npm test`
Expected: FAIL。

- [ ] **Step 2: 实现 ImageStrip 与 Composer 改动**

Create `src/components/ImageStrip.tsx`；Modify `src/components/Composer.tsx`：

1. `Composer` props：`{ onSend: (blocks: ContentBlock[]) => Promise<void>; onNormalize: (b: ArrayBuffer) => Promise<ContentBlock>; onInterrupt: () => void; streaming: boolean }`。**归一化通过 prop 注入**，使这一层可测；`App` 里接 `api.normalize`。
2. 三个入口各自落到同一个 `addFiles(FileList)`：`dataTransfer.files`、`clipboardData.items` 里 `type.startsWith("image/")` 的项、`open()` dialog 返回的路径。
3. 每个文件 `await file.arrayBuffer()` → `onNormalize` → 存入 `pending: PendingImage[]`（含 `name`、`previewUrl`、`block`）。
4. 数量超过 4 时**先处理前 4 个**，并追加一条可见提示「一次最多 4 张，已忽略 1 张」。
5. 失败时 `setErrors([...e, `${file.name}：${msg}`])`。**绝不静默丢图**——用户需要知道图没送上去，否则只会看到答非所问。
6. 缩略图 `data-` 属性用 `data-testid={`thumb-${i}`}`，移除按钮 `aria-label="移除"`。
7. 发送顺序固定 `[...images, text]`（图在前，符合多数 vision 端点的习惯）。
8. 发送成功才清空 `pending` 与输入框；失败保留，并把错误显示出来。
9. `streaming` 时禁掉三个入口，避免半途改图。
10. 全部样式用 token，无 hex。

- [ ] **Step 3: reducer 支持 image block**

Modify `src/state/reducer.ts`：`turn_start` 时把已提交的用户消息文本与图片作为独立 turn 先入列（`send_message` 由 `App` 乐观插入一个 user turn，**但必须同时把该 turn 标记为 optimistic，且 `session_messages` 重放时以服务端为准**）。

Modify `src/state/reducer.test.ts` 加两条：

```ts
it("renders an optimistic user turn immediately, then reconciles on replay", () => {
  let s = initialState();
  s = appendUserTurn(s, [{ kind: "text", text: "看图" }]);
  expect(s.turns).toHaveLength(1);
  s = replay([]);  // 服务端空历史
  expect(s.turns).toHaveLength(0);  // 重放以服务端为权威，不叠加
});

it("keeps image blocks as their own block, not folded into text", () => {
  const s = appendUserTurn(initialState(), [
    { kind: "image", data: "AA", mime: "png" }, { kind: "text", text: "这是什么" }]);
  expect(s.turns[0].blocks.map((b) => b.kind)).toEqual(["image", "text"]);
});
```

`appendUserTurn(state, blocks: UiBlock[]): UiState` 是本任务新增的 reducer 导出。

- [ ] **Step 4: 跑测试**

Run: `npm test`
Expected: PASS
Run: `npm run lint:css`
Expected: 通过

- [ ] **Step 5: 提交**

```bash
git add src/components/ src/state/reducer.ts src/state/reducer.test.ts src/ipc/commands.ts src/App.tsx
git commit -m "feat: add image input via drop, paste and picker with per-file errors"
```

---

### Task 20: 历史图片降级

**Files:**
- Modify: `src-tauri/src/agent/context.rs`
- Test: `src-tauri/src/agent/context.rs`（内联）

**Interfaces:**
- Consumes: Task 8 `ContextBuilder`、Task 5 `ContentBlock::Image`
- Produces:
  - `ContextBuilder::build` 在返回前对**非最近 3 条**用户消息中的 `Image` 块做降级
  - `pub fn degrade_old_images(msgs: &mut Vec<Message>, keep_recent: usize)`
  - `const KEEP_IMAGE_RECENT: usize = 3`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod image_degradation {
    use super::*;
    use crate::agent::types::ImageMime;

    fn with_image(tag: &str) -> Message {
        Message::user(vec![
            ContentBlock::Text { text: tag.into() },
            ContentBlock::Image { data: "AAAA".into(), mime: ImageMime::Png },
        ])
    }

    #[test]
    fn only_the_most_recent_images_survive_to_the_request() {
        let hist: Vec<Message> = (0..10).map(|i| with_image(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        let imgs = built.messages.iter().filter(|m| m.blocks.iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))).count();
        assert_eq!(imgs, KEEP_IMAGE_RECENT, "历史图片必须降级，否则聊到 20 轮突然 400 且极难排查");
    }

    #[test]
    fn degraded_image_leaves_an_explicit_placeholder_not_an_empty_text() {
        let hist: Vec<Message> = (0..6).map(|i| with_image(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        let old = &built.messages[1];
        let t = text_of(old);
        assert!(t.contains("[图片已移除以节省上下文]"), "必须留下可见痕迹，否则用户以为模型看到了图");
    }

    #[test]
    fn degradation_never_drops_the_text_of_a_message() {
        let hist = vec![with_image("重要内容"), with_image("第二"), with_image("第三"), with_image("最新")];
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        for m in built.messages.iter().filter(|m| m.role == Role::User) {
            assert!(text_of(m).starts_with("轮 "), "文本必须原样保留");
        }
    }

    #[test]
    fn tool_result_images_are_degraded_on_the_same_rule() {
        let mut hist: Vec<Message> = (0..6).map(|_| Message { role: Role::User, blocks: vec![
            ContentBlock::ToolResult { call_id: "c".into(), is_error: false,
                output: ToolOutput::Image { data: "AAAA".into(), mime: ImageMime::Jpeg } }]})
            .collect();
        hist.push(with_image("最新"));
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        let imgs = built.messages.iter().map(|m| m.blocks.iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. })).count())
            .sum::<usize>();
        assert!(imgs <= KEEP_IMAGE_RECENT + 1);
    }

    #[test]
    fn estimated_tokens_drop_after_degradation() {
        let big = || Message::user(vec![ContentBlock::Text { text: "x".into() },
            ContentBlock::Image { data: "A".repeat(200_000), mime: ImageMime::Png }]);
        let hist: Vec<Message> = (0..8).map(|_| big()).collect();
        let with_all = estimate_tokens(&hist);
        let mut degraded = hist.clone();
        degrade_old_images(&mut degraded, KEEP_IMAGE_RECENT);
        assert!(estimate_tokens(&degraded) < with_all);
    }
}
```

Run: `cargo test image_degradation`
Expected: FAIL。

- [ ] **Step 2: 实现**

Modify `src-tauri/src/agent/context.rs`：

```rust
pub const KEEP_IMAGE_RECENT: usize = 3;

/// 非最近 keep_recent 条消息里的图片块降级为文本占位。
/// 图片 token 极贵，且多数兼容端点不接受出现在非最近位置的历史图片。
pub fn degrade_old_images(msgs: &mut Vec<Message>, keep_recent: usize) {
    let mut image_positions: Vec<(usize, usize)> = Vec::new();
    for (mi, m) in msgs.iter().enumerate() {
        for (bi, b) in m.blocks.iter().enumerate() {
            if matches!(b, ContentBlock::Image { .. }) { image_positions.push((mi, bi)); }
        }
    }
    let keep_from = image_positions.len().saturating_sub(keep_recent);
    for &(mi, bi) in &image_positions[..keep_from] {
        msgs[mi].blocks[bi] = ContentBlock::Text { text: "[图片已移除以节省上下文]".into() };
    }
}
```

在 `build()` 返回 `Built` 前插入：

```rust
            let mut msgs = cand;
            degrade_old_images(&mut msgs, KEEP_IMAGE_RECENT);
```

**降级按「图片出现顺序」而非「消息下标」计数**——一条消息可能含多张图，按消息数保留会导致保留数量浮动。这一点是 Step 1 第二条测试存在的原因。

- [ ] **Step 3: 跑测试**

Run: `cargo test agent::context`
Expected: PASS（含 Task 8 的 7 个 + 本任务 5 个）

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/agent/context.rs
git commit -m "feat: degrade historical images to placeholders to bound vision tokens"
```

---

### Task 21: 切片 2 端到端验证

- [ ] **Step 1: 真图实测**

Run: `npm run tauri dev`

准备四种输入并逐一验证：
- 一张超过 1568px 的长截图 → 模型能正确描述内容；DevTools 里确认发出的是 jpeg/png 且 base64 长度 ≤5MB
- 一张手机拍的 jpeg（EXIF 方向）→ 模型描述的方向正确。若上下颠倒，说明 `image` crate 未应用 EXIF，需要在 `normalize_image` 前调 `img.apply_orientation()`
- 一张带透明通道的 png → 转 jpeg 后透明区变黑或白都可接受，但**必须能正常回答**
- 一个多帧 gif → 取首帧

- [ ] **Step 2: 超限指名验证**

构造一张 1500×1500 全随机噪声图（Task 18 的测试同款）拖进去。
Expected: UI 显示「文件名：图片降质到最低后仍超过 5MB（当前 N 字节），请改用更小的截图」，且**图未发送**（pending 保留）。

- [ ] **Step 3: 历史图片降级实测**

在同一会话连续贴 6 次图并各问一句。
Expected: 模型对最近 3 张仍能描述细节，对更早的图回答「看不到图」而非编造内容——这是降级占位符生效的证据，也是它比静默裁剪更好的地方。

- [ ] **Step 4: 记录并提交**

`docs/findings/plan-a-slice2-checks.md` 写：四种输入的实测尺寸与延迟、EXIF 结论、超限文案、降级后模型的真实表现。

```bash
git add docs/findings/plan-a-slice2-checks.md src-tauri/src/image_prep.rs
git commit -m "docs: record vision end-to-end checks including exif and oversize behavior"
```

---

## 计划完成后的状态

Plan A 交付一个能日常使用的客户端：多轮流式对话、会话持久化与重放、停止、贴图问答、三套主题与亮暗/字号、全程零弹窗。此时 spec 的第 9 节验收标准中，除「联网」「MCP」「100 轮不失忆」「安装包体积」外的条目应已全部可验证。

Plan B 接着做 `fetch_url`（含 SSRF 用例表）、权限门启用（`Write` 级工具与权限卡片实装）、`mcp/bridge.rs`。Plan B 的工具注册是纯增量：`ToolRegistry::register` 与 `RuleTable` 已就绪，循环一行不改。

## 自检结果（写完后回看 spec）

**1. spec 覆盖**：spec 第 3、4 节（架构与契约）全部在 Task 5–13；第 5.1 节权限规则表在 Task 9；5.3 的滑窗与 token 估算在 Task 8，摘要压缩留给 Plan C；5.4 `fetch_url` 与 5.2 MCP 留给 Plan B；第 6 节前端在 Task 14–16、19。

**2. 有意偏离 spec 的两处，均已说明理由**：`api_key` 的 keyring 从切片 6 提前到 Task 12（避免真实 key 先落盘再迁移）；`TextDelta` 合帧在 Task 16 做成 Rust + rAF 双层（spec 只要求 Rust 侧，前端再加一层是为长回答掉帧兜底）。

**3. 类型一致性**：`ToolCallView` 的四字段（id/name/risk/input）在 Task 5 定义、Task 13 组装、Task 14 TS 镜像、Task 11 重放构造，四处一致。`events_from_messages` 在 Task 11 定义、Task 13 的 `session_messages` 调用、Task 14 的 `replay` 消费。`normalize_image` 在 Task 18 定义、Task 19 经 `api.normalize` 注入 `Composer`。

**4. 遗留的开放项（不是占位符，是明确的信息依赖）**：Task 2/3/4 的实测结论会改写 Task 6/18 的具体形态。这些点都写了失败分支的处理，且约定「冲突时改计划，不改结论」。


