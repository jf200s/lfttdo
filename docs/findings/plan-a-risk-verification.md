# Plan A 风险验证记录

本文件记录 Plan A 各任务中「必须先实测才能继续」的风险结论与基线数据。
Task 1（脚手架 + 双测试器接线）只负责建立基线与记录待人工确认项；风险 1–3 的实测结论由 Task 2/3/4 追加。

## 基线

### Task 1：工具链与脚手架版本（后续任务以此为对照）

| 项目 | 值 |
| --- | --- |
| 脚手架生成器 | `create-tauri-app@4.7.5`，模板 `react-ts`，manager `npm` |
| Tauri CLI（npm） | `@tauri-apps/cli` 2.12.3 |
| Tauri JS API（npm） | `@tauri-apps/api` 2.12.4 |
| Tauri crate（Rust） | `tauri` 2.12.4（`Cargo.lock`），`tauri-build` 2.7.3，`wry` 0.57.0，`tao` 0.37.1 |
| Vite | 8.3.4 |
| Vitest | 5.0.5 |
| `@vitejs/plugin-react` | 6.1.2 |
| React / React DOM | 19.3.1 |
| TypeScript | 6.0.3 |
| jsdom / @testing-library/react | 30.1.3 / 16.3.3 |
| rustc / cargo | 1.99.1 / 1.99.1，host `stable-x86_64-pc-windows-msvc` |
| `tauri.conf.json` identifier | `com.qwa.do` |
| Cargo 包名 / lib 名 | `qwen-do` / `qwen_do_lib` |

### Task 1：debug 产物体积（仅用于确认工具链输出正常）

`ls -la src-tauri/target/debug/ | grep -i qwen` 实测：

```
-rwxr-xr-x 1 user 197121  12822528 Oct 19 21:14 qwen-do.exe
-rw-r--r-- 2 user 197121 115077120 Oct 19 21:14 qwen_do.pdb
-rwxr-xr-x 1 user 197121    166912 Oct 19 21:05 qwen_do_lib.dll
```

- `qwen-do.exe`（debug）：**12,822,528 字节 ≈ 12.23 MB**。
- 旁边还有 115 MB 的 `qwen_do.pdb`（调试符号，不计入分发体积）。
- **debug 体积不能用于验收**：<15MB 的判定对象是 `tauri build` 的发布产物（且发布配置已开启 `lto`、`codegen-units=1`、`strip=true`、`panic=abort`）。此处只是确认 MSVC 链接器能产出可执行文件。

## Task 1：双测试器与 dev 启动的实测结果（程序化验证）

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| vitest 流水线接通 | `npm test`（先写 `toBe(3)`） | FAIL：`AssertionError: expected 2 to be 3`，退出码 1 → 证明流水线真的在执行断言 |
| vitest 为绿 | `npm test`（改为 `toBe(2)`） | `Test Files 1 passed (1)` / `Tests 1 passed (1)`，退出码 0 |
| cargo 流水线接通 | `cd src-tauri && cargo test` | `Finished 'test' profile in 0.38s`，lib/bin/doc 三处均 `0 passed; 0 failed`，退出码 0，无 warning |
| 前端类型检查+构建 | `npm run build` | `tsc` 无错、`✓ 19 modules transformed`，退出码 0 |
| dev 服务器起来 | `npm run tauri dev` + `curl http://localhost:1420/` | 首次轮询即 HTTP 200；`/src/main.tsx`、`/src/App.tsx` 均 200；`index.html` 含 `<div id="root">` |
| Rust 侧真的在跑 | 同上，读 dev 日志 + `tasklist` | `Finished 'dev' profile in 2.30s` → `Running target\debug\qwen-do.exe`；`tasklist` 中 `qwen-do.exe` PID 30408 存活，工作集 30 MB |
| 无 panic | grep dev 日志 `panic\|backtrace\|error` | 0 命中 |
| 收尾干净 | `taskkill /F /T /PID 3420`（含 16 个子进程） | `tasklist \| grep -i qwen` 无残留，1420 端口不再响应 |

dev 日志中唯一的额外信息行是 `[vite] dependency optimized: vitest` —— Vite 的依赖预扫描扫到了 `src/state/smoke.test.ts` 里的 `import ... from "vitest"`，做了一次一次性预优化并触发页面 reload。不是错误，若后续觉得干扰可在 `optimizeDeps.exclude` 里排除 `vitest`。

## 需人工确认（Task 1）

**以下各项均未经肉眼确认。** Task 1 的窗口验证是程序化的（HTTP 200 + 进程存活 + 日志无 panic），没有截图、没有目视检查渲染结果，因此无法断言界面「看起来正常」。请在第一次 `npm run tauri dev` 时人工核对：

1. 桌面窗口是否真的弹出，标题栏文字为 `qwen-do`，客户区约 800×600。
2. 页面是否渲染出脚手架内容：标题 `Welcome to Tauri + React`、Vite/Tauri/React 三个 logo、输入框与 `Greet` 按钮（即 WebView2 正常工作、无白屏）。
3. 在输入框里敲名字点 `Greet`，是否回显 `Hello, <名字>! You've been greeted from Rust!`——这是本项目第一次真正的 IPC 往返。
4. **关于「版本号可见」**：当前 `create-tauri-app@4.7.5` 的 react-ts 模板页面**不再显示 Tauri/React 版本号**（brief 的预期基于旧模板）。版本已改由 `package.json` 与 `Cargo.lock` 实测取值，见上方基线表；若仍需要在界面上看版本，需要另加代码，不属于 Task 1 范围。

## 风险 1：Rust 流式

探针：`src-tauri/examples/stream_probe.rs`（`cargo build --example stream_probe` 通过，`cargo run --example stream_probe` 可执行）。

**本节状态：探针就绪，四个判定问题均未实测。** 本机环境 `QWEN_API_KEY` 为空，无法向真端点发请求；为避免把「预期结果」写成「实测结果」，所有实测项统一标注 `未实测`，等补跑后回填。已实测的只有两件事：探针能编译、探针在无 key 时按契约 panic（原文见本节末「本机已实测的两条」）。

### 必答四问

| # | 问题 | 结论 | 补跑后需附的原始输出行 |
| --- | --- | --- | --- |
| 1 | 首块延迟实测值（spec 风险 #4） | **未实测（缺 QWEN_API_KEY，待补跑）** | `chunks=… first_chunk_in=…` 中的 `first_chunk_in` 原值 |
| 2 | chunk 是否真的多块（spec 风险 #4） | **未实测（缺 QWEN_API_KEY，待补跑）** | 同一行的 `chunks=N`（判据 `N > 1`） |
| 3 | tool_call 分片是否带 index（spec 风险 #3，决定 Task 7 归组策略） | **未实测（缺 QWEN_API_KEY，待补跑）** | 全部 `TOOLCALL idx=… id=… name=… args=…` 行，一条不删 |
| 4 | base_url 四种组合哪种可用（spec 风险 #5） | **未实测（缺 QWEN_API_KEY，待补跑）** | 每种组合一行：最终 URL → `Ok` 或 `Err` 原文 |

第 4 问待填的表格（四种组合即 brief Step 4 列出的三种变换加「原样」；`<你的端点>` 指补跑时实际使用的那个 base，dashscope 默认值一并给出做参照）：

| 组合 | `QWEN_BASE_URL` 取值 | 结果 |
| --- | --- | --- |
| 原样（探针内置默认） | `https://dashscope.aliyuncs.com/compatible-mode/v1` | 未实测（缺 QWEN_API_KEY，待补跑） |
| 去掉尾部 `/` | `<你的端点>` 去掉末尾斜杠（默认值本就不带，故与「原样」同） | 未实测（缺 QWEN_API_KEY，待补跑） |
| 补 `/v1` | 在没有 `/v1` 的 base 后面补上，例 `https://dashscope.aliyuncs.com/compatible-mode` + `/v1`（结果即第一行） | 未实测（缺 QWEN_API_KEY，待补跑） |
| 去掉 `/v1` | 例 `https://dashscope.aliyuncs.com/compatible-mode` | 未实测（缺 QWEN_API_KEY，待补跑） |

注：以 dashscope 默认 base 为输入时，这四行只有 2 个不同的 URL（`…/v1` 与 `…/compatible-mode`）；「四种组合」真正有意义的前提是补跑者用的是自己的端点。请如实记录你实际打过的每个 URL，包括命中 404/连接失败的那些，别只记成功的一个。

### 判据说明（补跑时避免误读）

- `first_chunk_in` 的计时起点是 `create_stream` **返回之后**（SSE 响应头已到手），不是请求发出时。代理缓冲命中时它的值会≈整轮耗时，这与 brief 的判定意图一致；但它不包含 DNS/TLS/首字节时间，别拿它当端到端延迟。
- brief Step 4 的第二条判定「`idx` 恒为 `None`」在 async-openai 0.42.2 上**不可能以该形式被观测到**（原因见下方偏差 3）。补跑时要盯的是另一种表现：`stream.next()` 返回 `Err`、进程以 `Error: ... missing field \`index\`` 退出。命中它等价于「端点确实有不带 index 的分片」，Task 7 必须改用「以 `id` 出现为边界」的归组策略，并且不能指望这个 crate 帮你容忍这种 payload。

### 编译期确认的事实（源码判读，非网络实测）

| 项目 | 值 |
| --- | --- |
| `async-openai` 解析版本 | **0.42.2**（`cargo add` 解析，未手写版本号） |
| 生效的 feature 组合 | **`chat-completion`, `realtime`** |
| `tokio` / `futures` / `serde_json` | 1.53.3（`features=["full"]`） / 0.3.34 / 1.0.154 |
| 被拉进来的传输栈 | `reqwest` 0.13.5、`rustls` 0.23.46、`rustls-platform-verifier` 0.7.1、`aws-lc-rs` 1.18.2、`tokio-tungstenite` 0.28.0、`eventsource-stream` 0.2.3 |

feature 名：brief 的 `--features chat-completion,real-time` 被 cargo 拒绝 —— `error: unrecognized feature for crate async-openai: real-time`，并提示 `a feature with a similar name exists: realtime`。改为 `chat-completion,realtime`（`chat-completion` 存在且是流式聊天所需；`realtime` 是改名后的那一个）。

与 brief 源码的 API 偏差（共 4 条：前 3 条不改就编译不过，已在探针里就地注释；第 4 条只是改名，遍历代码不受影响。Task 6 要按这些写，不要照抄 brief）：

1. **类型不再平铺在 `types::` 下**：`async_openai::types::CreateChatCompletionRequestArgs` 等已移入 `async_openai::types::chat::`（`src/types/mod.rs` 是 `pub mod chat`，且没有平铺 re-export）。编译器原文：`no CreateChatCompletionRequestArgs in types`。
2. **`ChatCompletionRequestSystemMessage::new(..)` 不存在**：0.42.2 里这个类型只有 derive_builder 的 `ChatCompletionRequestSystemMessageArgs` 和 `impl From<&str> for ChatCompletionRequestSystemMessage`（`src/types/chat/impls.rs:162`）。探针改用 `::from("..")`；`ChatCompletionRequestMessage::System(..)` 的变体形态未变，仍是 `System(ChatCompletionRequestSystemMessage)`。
3. **`ChatCompletionMessageToolCallChunk.index` 是 `u32`，不是 `Option<u32>`**，字段上没有 `#[serde(default)]`（`src/types/chat/chat_.rs:1301`）。即：带 index 的分片能解，**不带 index 的分片直接反序列化失败**，而不是给你一个 `None`。这条决定了问题 3 的判据形态（见上方判据说明）。
4. 顺带的类型改名（字段名没变，所以遍历代码不用改，只改路径与构造）：`ChatChoiceDelta` → `ChatCompletionStreamResponseDelta`，`ChatCompletionChunk` → `CreateChatCompletionStreamResponse`，`ChatCompletionDeltaToolCall` → `ChatCompletionMessageToolCallChunk`；流类型 `ChatCompletionResponseStream = Pin<Box<dyn Stream<Item = Result<CreateChatCompletionStreamResponse, OpenAIError>> + Send>>`，`create_stream(&self, request: CreateChatCompletionRequest)` 仍为 `Chat` 上的固有 `async fn`（`src/chat.rs:75`），它会把 `request.stream` 强制置为 `Some(true)`。

### 探针设计缺口（补跑前请先读这条）

brief 的探针只发了一条 system 消息，**没有在请求里声明 `tools`**。OpenAI 兼容模式下没有工具定义时模型通常不会吐 `tool_calls`，因此补跑很可能只看到 `TEXT …` 行、`TOOLCALL` 行为 0。**这不等于「分片不带 index」**。若命中该情况，先给请求补 `.tools(vec![...])`（`ChatCompletionTool` + `FunctionObject`）再判定问题 3，否则会把「没触发工具调用」误读成「工具调用不分片」。本任务没有擅自改这一点，以保持与 brief 一致。

### 本机已实测的两条

```
$ cargo run --example stream_probe          # QWEN_API_KEY 为空
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.51s
     Running `target\debug\examples\stream_probe.exe`

thread 'main' (5948) panicked at examples\stream_probe.rs:19:45:
set QWEN_API_KEY: NotPresent
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
error: process didn't exit successfully: `target\debug\examples\stream_probe.exe` (exit code: 101)
```

依赖加入后 Task 1 基线未退化：`cargo test` → 三处 `0 passed; 0 failed`，退出码 0，无 warning。

## 补跑命令

```bash
cd src-tauri && QWEN_API_KEY=<真 key> cargo run --example stream_probe
```
