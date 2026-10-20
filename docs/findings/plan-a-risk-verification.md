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

探针：`src-tauri/examples/stream_probe.rs`（`cargo build --example stream_probe` 通过，`cargo run --example stream_probe` 可执行）。审查后已修正探针：请求现在**真的声明了 `get_time` 工具**并附带 user 轮「现在几点」（详见下方「探针设计缺口」一节）；流式遍历逻辑本身未动，仍是 `create_stream`。

**本节状态：探针就绪，四个判定问题均未实测。** 本机环境 `QWEN_API_KEY` 为空，无法向真端点发请求；为避免把「预期结果」写成「实测结果」，所有实测项统一标注 `未实测`，等补跑后回填。已实测的只有本机三件事：探针能编译、探针在无 key 时按契约 panic、`cargo test` 基线未退化（原文输出见本节末「本机实测原文输出」）。

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
- brief Step 4 的第二条判定「`idx` 恒为 `None`」在 async-openai 0.42.2 上**不可能以该形式被观测到**（原因见下方偏差 3）。补跑时要盯的是另一种表现：流中途出现一行 `STREAM_ERROR after chunks=… first_chunk_in=…: …missing field \`index\`…`（探针已改为捕获该 Err 后打印并 break，**不再中断汇总行**，见下方失败分支判定）。命中它等价于「端点确实有不带 index 的分片」。
- 零块流的汇总行现在是 `chunks=0 first_chunk_in=None`（不再打印 `0ns`）；看到 `None` 表示没有任何数据到达，别读成「首块瞬时」。

### Step 4 三条失败分支的判定与后果（均为条件句，补跑命中哪条执行哪条；本节未命中任何一条，因为尚无实测）

1. **若 `chunks == 1`** → 流式被缓冲（本地代理/网络中间层），spec 风险 #4 命中。处理：把该原始输出行记进上方四问表第 2 格，**并把「关闭流式的诊断开关」从 Plan C 提前列为必要项**（该开关目前规划在 Plan C，此分支命中前它不是必须的；命中后 Plan A/后续任务需要它来保证用户还能拿到非流式回退路径）。
2. **若出现 `STREAM_ERROR … missing field \`index\``**（即上方判据说明第二条）→ Task 7 的 `assistant_acc` 累加器**必须改用「以 `id` 出现为边界」的归组策略**，不能按 index 归组，且不能指望 async-openai 0.42.2 容忍这种 payload（要么换 crate/打补丁，要么在反序列化前自行解析 SSE JSON）。
3. **若 `create_stream` 直接返回 `Err`（或各形态 URL 结果不一）** → base_url 形态问题（spec 风险 #5）。处理：逐一试 brief 的三种变换（去掉尾部 `/`、补 `/v1`、去掉 `/v1`），把每种组合的最终 URL 与 `Ok`/`Err` 原文填进上方四组合表；**该表的结果直接决定 Task 6 `normalize_base_url` 要归一化哪些形态**——若不同斜杠/`v1` 形态的可用性不一致，`normalize_base_url` 必须补齐尾部斜杠与 `/v1` 的处理规则，按其选中形态写；若四行全 `Err`，问题在凭据/网络而非形态，`normalize_base_url` 只需最小规则（去尾 `/`）。

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

### 探针设计缺口（已修复——审查确认原探针结构上答不出问题 3）

原探针（照抄 brief）只发一条 system 消息、**没有在请求里声明 `tools`**，却在 prompt 里命令「调用 get_time 工具」。OpenAI 兼容模式下未声明的工具不可能被调用，重跑注定 `TOOLCALL` 行为 0，brief Step 3 预期的 `大量 TOOLCALL idx=0 …` 不可达——而问题 3 正是 Task 7 累加器的承重判据。现探针已补上（brief 仅禁止改成非流式，允许改写探针；流式遍历逻辑未动）：

- `.tools(vec![ChatCompletionTools::Function(ChatCompletionTool { function: FunctionObject { name: "get_time", parameters: Some(json!({"type":"object","properties":{},"additionalProperties":false})), .. } })])` —— `get_time` 现在是带 JSON Schema 的真实 function 工具（0.42.2 的 `tools` 字段类型是 `Option<Vec<ChatCompletionTools>>`，见 `chat_.rs:1083/590`）。
- messages 变为两条：system（命令调用工具）+ **user 轮「现在几点」**——没有 user 轮，模型无从触发调用。

**若端点按 OpenAI function-calling 协议实现流式，预期的 TOOLCALL 形态是**（预期，非实测）：首行 `TOOLCALL idx=0 id=Some("call_…") name=Some("get_time") args=Some("{…首片段")`，随后若干行 `idx=0`、`id`/`name` 为 `None`、`args` 是残缺字符串片段，拼接后恰好是合法 JSON（此处为空参数 `{}`）。据此回答分片性与 index 携带性。**若补上 tools 后 `TOOLCALL` 仍为 0 行**，那是「端点/模型不支持或不触发工具调用」的新结论，须如实记录，仍不得读成「分片不带 index」。

### 本机实测原文输出

实测一，探针编译通过、零 warning（本次修复后的版本）：

```
$ cargo build --example stream_probe
   Compiling qwen-do v0.1.0 (D:\qwen do 1\src-tauri)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.60s
EXIT=0
```

实测二，无 key 时按契约 panic（`QWEN_API_KEY` 为空；修复版头部注释变长，panic 位置从 19:45 移到 22:45，契约文本不变）：

```
$ cargo run --example stream_probe          # QWEN_API_KEY 为空
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.49s
     Running `target\debug\examples\stream_probe.exe`

thread 'main' (6332) panicked at examples\stream_probe.rs:22:45:
set QWEN_API_KEY: NotPresent
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
error: process didn't exit successfully: `target\debug\examples\stream_probe.exe` (exit code: 101)
EXIT=101
```

Task 1 基线未退化检查，`cargo test` 原文（三处 `0 passed; 0 failed`，退出码 0，无 warning）：

```
$ cargo test
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.49s
     Running unittests src\lib.rs (target\debug\deps\qwen_do_lib-c11f3a1723d17c7d.exe)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src\main.rs (target\debug\deps\qwen_do-18fa52120134a5f2.exe)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests qwen_do_lib

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
EXIT=0
```

## 补跑命令

```bash
cd src-tauri && QWEN_API_KEY=<真 key> cargo run --example stream_probe
```

**成本说明：这条命令一次只回答第 1–3 问；第 4 问的四组合表需要共四次运行。** 探针在启动时只读一次 `QWEN_BASE_URL`，且**故意没有**内置 URL 循环（四种形态各跑一次比在探针里做循环更接近 Task 6 的真实用法，也更简单）。每种形态单独跑一次、各自往四组合表里添一行「最终 URL → `Ok` 汇总行原文 或 `Err`/`STREAM_ERROR` 原文」：

```bash
QWEN_BASE_URL=https://<你的端点>/v1        cargo run --example stream_probe   # 原样
QWEN_BASE_URL=https://<你的端点>/v1/       cargo run --example stream_probe   # 带尾部斜杠（若默认无斜杠则与此互换）
QWEN_BASE_URL=https://<你的端点>           cargo run --example stream_probe   # 补 /v1 之前的形态
QWEN_BASE_URL=https://<你的端点>/compatible-mode   cargo run --example stream_probe   # 去掉 /v1
```

即完整补跑 ≈ 1 次主跑 + 4 次 base_url 形态跑（用默认 dashscope base 时其中两两 URL 相同，见四组合表下的注；用自建端点时四个都该打）。失败的那几次同样要记录原文，别只留成功的。stdout/stderr 原样贴进四问表格，不转述、不修剪 `TOOLCALL` 行。

## 风险 2：Tauri 二进制 IPC

**一句话结论：raw body 在 Windows/WebView2 上可用，且是明显更快的那一条。** 512KB（以及 4MB）的 `Uint8Array` 作为 `invoke` 的**第二参数整体**传入时，Rust 侧命令收到的是 `tauri::ipc::InvokeBody::Raw`，实测中位往返 **7.5ms**；base64 字段形态在同一尺寸下 IPC 往返中位 **20.3ms**，连同分块编码（中位 11.1ms）端到端实测 **31.2ms**（中位数不可相加，31.2 是逐轮直接量得的总和）。**Task 18 采用 `InvokeBody::Raw`；Task 19 不需要为 IPC 传输本身加 loading 态。**

本节全部延迟数字都是**实测**（CDP 程序化驱动，见下方「测量方法」）。协议形态判读单独放在「协议判读」一节并逐条给 file:line，两类证据不混用：**源码能证明走的是哪条通路，但证明不了延迟；延迟是测出来的，但解释不了为什么。**

**作用域限定（拿数字去算预算前先读这条）**：上面所有毫秒都是 **dev profile** 下测的（cargo 当时报的是 `Finished dev profile [unoptimized + debuginfo] target(s) in 36.68s`，原文见「测量方法」），而命令体只有两个**极简实现**——`ipc_probe_raw` 只做 `Vec::len()`、`ipc_probe_b64` 只做一次 base64 解码。换到 release 构建，Rust 侧那半段会变，base64 解码那条预计明显变快，所以**「2.6–2.7 倍」「4.2 倍」这两个比值本身在 release 下是多少：未实测**。真实 `normalize_image` 还要叠加解码 / 缩放 / 再编码，那部分也不在本节内（属 Task 18 的耗时）。→ Task 19 可以把「IPC 传输层不是瓶颈」当结论带走，但**不能把本节任何毫秒当成 release 的延迟预算**。

### 必答四问

| # | 问题 | 结论 | 证据类型 |
| --- | --- | --- | --- |
| 1 | 两种传法哪种可用 | **raw body 可用**（`invoke("ipc_probe_raw", bytes)` → `Ok(524288)`，7/7 与 15/15 全中）；base64 字段同样可用，仅作对照 | 实测 |
| 2 | 各自实测延迟（512KB，15 次） | raw：min 7.10 / **median 7.50** / max 10.10 ms。b64：IPC 往返 min 19.20 / **median 20.30** / max 21.20 ms；加分块编码后 min 29.90 / **median 31.20** / max 34.20 ms（每个 max 的成因见下方「离群值归因」，中位数不受它们影响） | 实测 |
| 3 | Task 18 是否取 `InvokeBody::Raw` | **是**。命令签名 `normalize_image(req: tauri::ipc::Request<'_>)`，前端 `invoke("normalize_image", bytes)`（bytes 为 `Uint8Array` 或 `ArrayBuffer`，两者都实测可用）。**约束：raw body 与 JSON 字段互斥**——同一命令不能既收字节又收兄弟参数（实测负控制 + 源码），max-edge / mime 之类参数只能内置、或走第二条命令。**传 buffer 的坑**：实测可用的是 `bytes.buffer` 且 `bytes` 是整块 `new Uint8Array(N)`；若 `bytes` 是切过的视图（`subarray` / 裁剪后的 buffer），`.buffer` 会带回**底层整个** ArrayBuffer 而不只是那一段——字节照发、长度照报，命令体察觉不到多收。Task 19 的裁剪图必须传视图本身（`Uint8Array`）或整块无残留的 buffer，**永远不要把 subarray 的 `.buffer` 交出去**（这条是 `.buffer` 语义判读；「subarray 直接走 invoke」这一形态本节未实测） | 实测 + 协议判读 |
| 4 | Task 19 是否需要 loading 态 | **传输层不需要**：512KB 单张 7.5ms，4MB 也只要 56ms，空载往返基线 1.1ms，肉眼不可能感知。**但 Task 18 侧的图像解码/缩放/编码耗时未实测**，Task 19 若要 pending/进度反馈，理由只能是那条未实测的归一化与模型往返，不得拿 IPC 延迟当依据 | 实测（限传输层） |

无待肉眼确认项：按 R13，Step 3 的「打开 DevTools 点按钮」已由 WebView2 远程调试端口程序化替代，`probe()` 的 `console.log` 由 CDP `Runtime.consoleAPICalled` 捕获原文（见下方原文）。

### 测量方法

```bash
# 起 app（本次新增依赖后 cargo 报的是：Finished `dev` profile [unoptimized + debuginfo] target(s) in 36.68s；之后增量编译秒级。
# 也就是说下面全部延迟数都是 dev profile + 极简命令体下测的，见上方「作用域限定」）
cd "D:/aido/qwen do 1" && WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222 npm run tauri dev
# 确认页面 target
curl -s http://localhost:9222/json   # → type=page url=http://localhost:1420/
# 驱动（Node 24 内置 fetch + WebSocket，零依赖，脚本在 .superpowers/ 下不入树）
node .superpowers/sdd/2026-10-19-plan-a-shell-chat-vision/tmp/cdp_probe.mjs console
PROBE_REPS=15 node .../cdp_probe.mjs measure
PROBE_BYTES=$((2*1024*1024)) PROBE_REPS=5 node .../cdp_probe.mjs size
node .../cdp_probe.mjs negctrl
```

- 计时方式：`performance.now()` 直接包住 `await invoke(...)`，即「JS 侧发出 → JS 侧拿到结果」的端到端往返，含 WebView2 网络栈、自定义协议、serde 与命令执行。
- base64 形态拆成两个数：**编码**（`chunkedB64`，8192 字节一段 `String.fromCharCode` + `btoa`）单独计时，**IPC 往返**单独计时；`b64WithChunkedEncode` 是同一轮里两者之和（512KB 那轮是直接量得的总和，2MB/4MB 那两轮是两段实测值相加）。
- 载荷固定 `bytes[i] = i % 251`（512KB），非全零，避免任何压缩/去优化通路干扰；`greet` 空载往返作对照基线。
- 运行环境原文：`navigator.userAgent = Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36 Edg/155.0.0.0`；`window.__TAURI_INTERNALS__.__TAURI_PATTERN__ = {"pattern":"brownfield"}`；`typeof window.ipc.postMessage === "function"` 为 `true`（存在，但本次没有走它）。

### 实测原文：brief 的 `probe()` 逐字执行

```
[console.debug] "[vite] connecting..."
[console.info] "%cDownload the React DevTools for a better development experience: https://react.dev/link/react-devtools" "font-weight:bold"
[console.debug] "[vite] connected."
[console.log] "raw:" 524288
[console.log] "b64:" 8192
```

`raw:` 直接给出 `524288`（= 512×1024），即 `InvokeBody::Raw` 分支命中；该命令的另一分支会返回 `Err("got json body: …")`，所以这个数不是「碰巧算对」——同一个命令在收到对象时明确回了 `got json body: {"bytesB64":"AAAA"}`（见下方负控制），两条分支都被实测打通了。

CDP 订阅的是 `Runtime.consoleAPICalled`，**上面五行就是那次 `console` 模式捕获到的全部 console 行**：前 3 行（`[vite] connecting...`、React DevTools 的 `%cDownload…`、`[vite] connected.`）来自 vite 开发服务器与 React 本身，不是 Tauri 输出；只有后 2 行是探针自己的 `console.log`。这 5 行里没有任何 Tauri 侧 warn，尤其没有 `IPC custom protocol failed, Tauri will now use the postMessage interface instead`。这只对**已捕获的行**成立，不能据此断言整个运行时从未打印该串；更硬的判据是逻辑性的：若自定义协议失败并回退到 `window.ipc.postMessage`，字节只能以 JSON 数字数组进 `InvokeBody::Json`（`ipc-protocol.js:55-68` + `protocol.rs:236-245`，源码判读），本命令必然回 `Err` —— 实测 15/15 与 7/7 全 `Ok`，**故本次没有发生回退**。

### 实测原文：512KB × 15 次（逐次原始数组 + summary，本小节自成闭环）

下面 5 个数组是 `measure()` 返回值里 `raw.ms` / `b64.ms` / `encode.chunkedMs` / `b64WithEncode.ms` / `greetBaseline.ms` 五个字段的**原文**（块内短标签对应关系：`b64+enc` = `b64WithEncode.ms`，`greet` = `greetBaseline.ms`），未四舍五入、未排序，索引 0 = 该轮第 1 次调用。任何 min / median / max / MB/s / 倍数都可以只靠这一块重新算出来：

```
payloadBytes = 524288   reps = 15   单位 = performance.now() 差值 ms
raw.ms         = [10.100000001490116,7.800000000745058,7.5,7.699999999254942,7.300000000745058,7.199999999254942,7.100000001490116,7.099999997764826,7.300000000745058,7.600000001490116,7.199999999254942,7.199999999254942,7.800000000745058,8,7.599999997764826]
b64.ms         = [20.800000000745058,20.5,20.600000001490116,20.300000000745058,20.300000000745058,19.699999999254942,19.699999999254942,20.199999999254942,19.199999999254942,20.299999997019768,19.699999999254942,20.399999998509884,21.100000001490116,21.199999999254942,20.400000002235174]
enc.chunkedMs  = [11.300000000745058,10.699999999254942,11.199999999254942,10.699999999254942,11.699999999254942,11.5,10.800000000745058,10.400000002235174,10.699999999254942,10.900000002235174,11.100000001490116,12.900000002235174,11.399999998509884,13,10.699999999254942]
b64+enc.ms     = [32.100000001490116,31.199999999254942,31.800000000745058,31,32,31.199999999254942,30.5,30.600000001490116,29.899999998509884,31.199999999254942,30.800000000745058,33.30000000074506,32.5,34.19999999925494,31.100000001490116]
greet.ms       = [2,1.0999999977648258,1.1000000014901161,1,1.1000000014901161,0.8999999985098839,1.1000000014901161,1.0999999977648258,1,1,1.1000000014901161,1.0999999977648258,1,1,1]
raw.results    = 15 个值全为 524288        b64.results = 15 个值全为 524288
transport      = {"hasWindowIpc":true,"internalsIpcType":"function","pattern":"{\"pattern\":\"brownfield\"}"}
```

`transport` 是探针在同一轮里读到的运行时形态（`window.ipc` 存在、`__TAURI_INTERNALS__.ipc` 是函数、`__TAURI_PATTERN__` 为 brownfield）——它只是环境记录，**不是**「走没走 postMessage」的判据，判据见上一小节。

```json
{
  "payloadBytes": 524288,
  "reps": 15,
  "summary": {
    "raw":                  { "n": 15, "min": 7.099999997764826,  "median": 7.5,                "max": 10.100000001490116 },
    "b64IpcOnly":           { "n": 15, "min": 19.199999999254942, "median": 20.300000000745058, "max": 21.199999999254942 },
    "b64WithChunkedEncode": { "n": 15, "min": 29.899999998509884, "median": 31.199999999254942, "max": 34.19999999925494 },
    "chunkedEncode":        { "n": 15, "min": 10.400000002235174, "median": 11.100000001490116, "max": 13 },
    "greetBaseline":        { "n": 15, "min": 0.8999999985098839, "median": 1.0999999977648258,  "max": 2 }
  },
  "encode": { "b64Length": 699052,
              "spread8kb": "ok, b64 len=10924",
              "spread512kb": "ERROR: RangeError: Maximum call stack size exceeded" }
}
```

（`summary` 由探针脚本自算后原样贴出，未改写。中位取排序后第 8 个值：raw 为 `7.5`、b64 为 `20.300000000745058`、b64+enc 为 `31.199999999254942`、enc 为 `11.100000001490116`、greet 为 `1.0999999977648258`。）

### 复跑一致性与离群值归因

同一台机器、同一个 app 进程、同一个页面 target（各轮捕获文件里的 `target id=7060680A25554B6AC33708EB0AB9FA2D` 一致）下跑了**两轮 512KB**，先后顺序是：**7 次那轮在前，15 次那轮在后**（依据：捕获文件的创建时间 `measure-7.txt` 22:38:54 → `dev.log` 22:39:51 → `measure-15.txt` 22:40:24；且驱动脚本的 `PROBE_REPS` 缺省值就是 7，15 是随后显式加参重跑的那一次）。task-3-report.md §4.4 把 7 次轮写成「15 次轮之后紧接着跑的」，**那句是错的**，以本节为准。

两点诚实的限制：(a) 这些创建时间来自 `.superpowers/…/tmp/`，仓库外、会被清掉，所以**时序本身无法只靠树内文件复核**，本节把它当作对「为什么两轮的 max 都在首位」的背景说明而非结论——所有 min/median/max/倍数都只用上面的数组算出，与谁先谁后无关；(b) 两轮之间 dev.log 记了一次 `22:39:51 [vite] (client) hmr update /src/App.tsx`，即 15 次轮跑的是 HMR 更新过的页面，因此「复跑一致」是在**同一次 app 进程、不同一次页面代码快照**之间比较，不是严格同代码的重复实验。

7 次轮的逐次数组同样原文照录（这样「复跑一致」这句话也可以就地重算）：

```
raw.ms        = [9.599999997764826,8.100000001490116,7.600000001490116,7.899999998509884,7.100000001490116,7.5,8.299999997019768]
b64.ms        = [21.300000000745058,20,20.599999997764826,20.5,19.5,20.299999997019768,19.400000002235174]
enc.chunkedMs = [11.699999999254942,10.800000000745058,11.5,11.700000002980232,10.899999998509884,12.100000001490116,11]
greet.ms      = [2.199999999254942,1,1,1.199999999254942,1,1,0.8000000007450581]
```

两轮对照（raw）：7 次轮 min 7.100 / median 7.900 / max 9.600；15 次轮 min 7.100 / median 7.500 / max 10.100 → **中位差 0.4ms**。(b64 往返：7 次轮 min 19.400 / median 20.300 / max 21.300，15 次轮 min 19.200 / median 20.300 / max 21.200 → 中位相同。) 两轮的 min 四舍五入后同为 7.100（源浮点值 `7.100000001490116` 与 `7.099999997764826`，是不同的浮点数但差 <4×10⁻⁹），说明分布下沿稳定；差异集中在高位，正好由下面的位置效应解释。

**离群值归因（边界先说清：`raw` 的 max 落在第几次是有规律可循的——五轮全在首位，因而可以给「一次开路」的解释；`b64` / `enc` 的 max 位置没有这个规律，其离散只能记为未归因噪声）**

1. **512KB raw 的 max 10.100ms（比 7.500 的中位高 35%）＝ 该轮的第 1 次调用。** 它在上面 `raw.ms` 数组的**索引 0**。把五轮放在一起看，这不是偶然：`raw` 档的 max 在**全部五轮里五次都落在该轮第 1 次调用**——512KB×15 的 10.100（位 1）、512KB×7 的 9.600（位 1）、512KB 附加轮的 9.800（位 1）、2MB 的 29.900（位 1）、4MB 的 58.900（位 1）。同一轮的 `greet` 空载基线也是首位最高（2.000 / 2.200 vs 中位 1.000–1.100）。所以可信的描述是「**每次调用序列的第 1 次偏高**」，与「首次用到某条通路要付一次性开销」相容；而且 15 次轮并不是整场 session 的第一次（7 次轮在它之前、`console` 模式更早），它照样在首位偏高，说明这是**按轮重复**的现象而非一次性冷启动。dev.log 记录了两轮之间 22:39:51 有一次 `[vite] (client) hmr update /src/App.tsx`，即两个「第 1 次」都紧跟在页面代码变动 + 一段空闲之后，与该解释一致。
2. **b64 与编码的 max 撑不起这种解释，本节不给它编原因。** 反例就在数据里：7 次轮的 b64 max 21.300 **也**在首位（所以「第 1 次偏高」不只属于 raw），但同一档 15 次轮的 b64 max 21.200 落在**第 14 次**、enc 的 max 13.000 落在第 14 次、`b64+enc` 的 max 34.200 落在第 14 次；尺寸档更明显（逐次数组见下一小节）：2MB b64 的 max 87.900 是**第 4 次**、4MB b64 的 max 162.900 是**第 3 次**，都不在首位。也就是说 b64 侧的位置既不固定于首位、也不固定于末位，**规律不存在**。2MB 的前三次还贴在一起（`74.59999999776483` / `74.60000000149012` / `74.60000000149012`，即 74.600 三位一致），之后才跳到 87.900、81.000；4MB b64 的 143.500–162.900（相对中位约 ±8%）同样无规律。**结论：这几处离散记为未归因的测量噪声**——本机探针没有插桩 GC、分配器、CPU 占用或其他进程竞争，因此既不能（也不该）把它们说成 GC、warm-up 或系统忙。唯一能说的是它们都不是首位，所以不能用第 1 条那个位置效应解释掉。
3. **对结论的影响：零。** 折算倍数用的全是中位数；n=5 时中位取排序后第 3 个值，而上面两个 b64 离群值都排在中位之上（2MB 排序后 74.600/74.600/74.600/**81.000/87.900**；4MB 143.500/147.700/**150.900**/155.200/162.900），n=15 时中位是第 8 个值，首尾离群都被丢掉。所以「2.6–2.7 倍 / 4.2 倍」以及 Task 18 采用的 `InvokeBody::Raw` 都不依赖任何单个 max 的成因是否被解释。

**补跑须知（不再依赖仓库外的文件）**：临时命令与前端计时 wrapper 已按 Step 4 从 `lib.rs` / `App.tsx` 删除且从未提交，重跑前要先照 brief Step 1/2 把 `ipc_probe_raw` / `ipc_probe_b64` 注册回去、并在页面暴露 `window.__ipcProbe`。驱动脚本 `cdp_probe.mjs` 与 `measure-*.txt` 留在 `.superpowers/sdd/2026-10-19-plan-a-shell-chat-vision/tmp/`，该目录被 `.superpowers/sdd/.gitignore`（内容 `*`）忽略，**随时会不在**——正因如此，本节才把逐次数组原文抄了进来：本节的算术不需要再去读 `.superpowers/` 下的任何文件，也不需要 task-3-report.md。

### 实测原文：尺寸放大（5 次/档，同一 target）

逐次数组原文（未四舍五入、未排序，索引 = 第 1…5 次调用；`enc` 是单独计时的分块编码，`b64` 是 IPC 往返）：

```
PROBE_BYTES=2097152（2MB，b64 串长 2796204）
  raw = [29.899999998509884,28.600000001490116,29,28.899999998509884,28.900000002235174]
  b64 = [74.59999999776483,74.60000000149012,74.60000000149012,87.90000000223517,81]
  enc = [44,43.30000000074506,46.099999997764826,50.19999999925494,47.599999997764826]
PROBE_BYTES=4194304（4MB，b64 串长 5592408）
  raw = [58.899999998509884,56.30000000074506,56.30000000074506,54.69999999925494,54.80000000074506]
  b64 = [150.89999999850988,147.69999999925494,162.89999999850988,143.5,155.19999999925494]
  enc = [87.09999999776483,93.30000000074506,94.70000000298023,87.89999999850988,88.10000000149012]
PROBE_BYTES=524288（附加轮：ArrayBuffer 形态与三张并发就在这一轮，见下一小节）
  raw = [9.799999997019768,7.900000002235174,6.800000000745058,7.099999997764826,7.600000001490116]
  b64 = [20.599999997764826,19.800000000745058,20,20.899999998509884,19.199999999254942]
  enc = [11.5,10.300000000745058,11.399999998509884,10.700000002980232,10.800000000745058]
```

汇总（min / median / max 全部由上面的数组算出，**四舍五入**到 0.001ms；n=5 时中位 = 排序后第 3 个值）：

```
2MB   raw            min 28.600 / median 28.900 / max 29.900   → 5 次全部 Ok(2097152)
      b64IpcOnly     min 74.600 / median 74.600 / max 87.900   → 5 次全部 Ok(2097152)
      chunkedEncode  min 43.300 / median 46.100 / max 50.200
      编码+IPC 相加 ≈ 120.7ms（两段各自实测中位相加，非单次直接实测）
4MB   raw            min 54.700 / median 56.300 / max 58.900   → 5 次全部 Ok(4194304)
      b64IpcOnly     min 143.500 / median 150.900 / max 162.900 → 5 次全部 Ok(4194304)
      chunkedEncode  min 87.100 / median 88.100 / max 94.700
      编码+IPC 相加 ≈ 239.0ms（同上，相加值）
512KB 附加轮  raw min 6.800 / median 7.600 / max 9.800；b64IpcOnly min 19.200 / median 20.000 / max 20.900；chunkedEncode min 10.300 / median 10.800 / max 11.500
```

与上一版本文档的差额都是同一类错误——把截断当成了四舍五入，现按原文浮点值改正：`chunkedEncode`(2MB) max 源值 `50.19999999925494` → **50.200**（原写 50.199）；`b64IpcOnly`(4MB) max 源值 `162.89999999850988` → **162.900**（原写 162.899）；`raw`(4MB) min 源值 `54.69999999925494` → **54.700**（原写 54.699）、max 源值 `58.899999998509884` → **58.900**（原写 58.899）；`chunkedEncode`(4MB) min 源值 `87.09999999776483` → **87.100**（原写 87.099）。这些改动只影响第 3 位小数，**不动任何中位数，因此不动下一段的 MB/s 与倍数**。

线性度（由上表实测中位值折算，算法 = 载荷字节 ÷ 中位毫秒，1 MB = 10⁶ 字节）：raw ≈ **70 / 73 / 74 MB/s**（512KB→524288/0.0075=69.9；2MB→2097152/0.0289=72.6；4MB→4194304/0.0563=74.5），b64 按**原始字节**折算 ≈ **26 / 28 / 28 MB/s**（524288/0.0203=25.8；2097152/0.0746=28.1；4194304/0.1509=27.8），按线上实际走的 **b64 串字节**折算 ≈ 34 / 37 / 37 MB/s（699052/0.0203、2796204/0.0746、5592408/0.1509）→ **raw 稳定快约 2.6–2.7 倍**（只比 IPC 往返），连编码一起算约 **4.2 倍**（31.2/7.5=4.16；120.7/28.9=4.18；239.0/56.3=4.25）。

### 实测原文：并发与形态边角（Task 19 的多图入口）

```
附加项（`arrayBufferRes` / `concurrent3` 与上面各档数组同轮测得；512KB 那行出自附加轮，2MB 那两行出自 2MB 轮）：
  arrayBufferRes        = 524288                 // invoke("ipc_probe_raw", b.buffer)，b = new Uint8Array(524288)
  concurrent3Res        = "524288,524288,524288"  // Promise.all 三个 raw invoke
  concurrent3Ms(512KB)  = 16.400 ms               // 源值 16.400000002235174；单张 7.6ms → 3 张并发 16.4ms（≈2.2 倍单张）
  arrayBufferRes(2MB)   = 2097152                 // 同一形态在 2MB 档复测
  concurrent3Ms(2MB)    = 66.500 ms               // 源值 66.5；单张 28.9ms → 3 张并发 66.5ms（≈2.3 倍单张）
```

`invoke(cmd, bytes.buffer)` **确实可用**，实测两档都拿到 `Ok(载荷字节数)`。但它可用的是「**整块 buffer**」这一形态：探针里的 `b` 是 `new Uint8Array(N)`，从未切片，所以 `b.buffer` 的字节数恰好等于 `b.byteLength`。这条边界对 Task 19 是实打实的坑——`subarray` / 裁剪过后的视图，它的 `.buffer` 指向**底层整个 ArrayBuffer**，与 `byteOffset` / `byteLength` 无关：

- `bytes.subarray(0, 1000).buffer` 发出去的是**整个原始 buffer**（可能是几 MB），而 Rust 侧 `InvokeBody::Raw` 的 `len()` 也会照实报那个更大的数；判据 `process-ipc-message-fn.js:8-16` 只看「整个 payload 是不是一个字节载体」，`ArrayBuffer` 一律算字节载体（源码判读），所以**多送出去的字节不会报错、也不会被任何一层察觉**，只会静默地多发、多算、多占内存。
- 正确写法二选一：**传视图本身**（`invoke(cmd, croppedUint8Array)`，`ArrayBuffer.isView` 命中 raw 分支）或传**一块干净的整 buffer**（如 `await file.arrayBuffer()`）。**永远不要把 subarray 的 `.buffer` 交给 invoke。**
- 「subarray 直接走 invoke」与「subarray.buffer 多送字节」这两个具体形态本节**未实测**（上面实测的只有整块 `b.buffer`）；前者的依据是 `process-ipc-message-fn.js:8-16` 的 `ArrayBuffer.isView` 判据，后者的依据是 `ArrayBuffer`/`TypedArray` 的 `.buffer` 语义。Task 19 若要用裁剪 buffer，请顺手加一次 `Ok(n)` 长度断言把它变成实测。

即「粘贴多张图」在这一层是**并发安全**的（三张各自拿到正确长度、无错乱）；墙钟为单张的 2.2–2.3 倍——既不是理想并发的 1 倍，也不是完全串行的 3 倍，说明传输层有部分排队。绝对量仍在几十毫秒级，Task 19 不必为多图做串行化。

### 实测原文：负控制（证明 Raw/Json 判据不是空转）+ 8KB 注意项

```json
{
  "json_object_to_raw_cmd": "got json body: {\"bytesB64\":\"AAAA\"}",
  "empty_typed_array": "Ok(0)",
  "typed_array_4_bytes": "Ok(4)",
  "digit_array_field_in_object": "Err(got json body: {\"a\":[1,2,3,4]})",
  "json_number_array_512kb": [
    { "buildMs": 7.2, "ipcMs": 254.8 },
    { "buildMs": 6.9, "ipcMs": 260.6 }
  ]
}
```

（`json_number_array_512kb` 的 `result` 字段原文是被截断的 `Err(got json body: {"a":[0,1,2,3,4,5,6,7,8,9,10,11,…)`，即 512KB 的数字数组被 Rust 当成 `InvokeBody::Json` 收了回去；截断由探针做，不影响 `ipcMs` 计时。）

三条要点：

1. **`String.fromCharCode(...bytes)` 在 512KB 上的真实报错原文是 `RangeError: Maximum call stack size exceeded`**（同一份数据切成 8192 字节展开则 `ok, b64 len=10924`）。brief 的 8KB 警告在本机 WebView2 上成立且可复现（7 次与 15 次两轮都命中）。→ Task 19 若在任何地方需要 base64（见下方 Android/回退两条），**必须**用 `File.slice` 分段 + 逐段 `btoa`，禁止一次性展开。
2. **JSON 数字数组是最差的形态**：512KB 走 `{a: Array.from(bytes)}` 实测往返 **254.8 / 260.6ms**，是同尺寸 raw 的**约 34 倍**（`Array.from` 本身只花 7ms，代价在序列化 + serde）。`InvokeArgs` 类型里 `number[]` 是合法项（`core.d.ts:268`），所以这是个「类型通过、性能归零」的坑，Task 18/19 不要碰。
3. 空 `Uint8Array` → `Ok(0)`：raw 路径对 0 字节载荷也成立，Task 18 不必为「空输入」特殊绕路。

### 协议判读（读源码所得，非实测；只用于解释实测为什么成立）

以下路径均在本机已安装的源里，Git Bash 正斜杠：

- `D:/aido/qwen do 1/node_modules/@tauri-apps/api/core.js:340-341` —— `invoke(cmd, args, options)` 只是转发到 `window.__TAURI_INTERNALS__.invoke`，JS API 层不做任何序列化决策。
- `tauri-2.12.4/scripts/core.js:81-101` —— `__TAURI_INTERNALS__.invoke` 把 payload 原样交给 `__TAURI_INTERNALS__.ipc({cmd, callback, error, payload, options})`。
- `tauri-2.12.4/scripts/ipc-protocol.js:20` —— `canUseCustomProtocol = osName !== 'android'`；`:22-41` —— 桌面走 `fetch(convertFileSrc(cmd,'ipc'), {method:'POST', body:data, headers})`，并设 `Content-Type` / `Tauri-Callback` / `Tauri-Error` / `Tauri-Invoke-Key` 四个头。
- `tauri-2.12.4/scripts/process-ipc-message-fn.js:8-16` —— **判据是「整个 payload 是不是一个字节载体」**：`message instanceof ArrayBuffer || ArrayBuffer.isView(message) || Array.isArray(message)` → `{contentType:'application/octet-stream', data: message}`；否则 `JSON.stringify`，且 `:26-29` 把 `Uint8Array`/`ArrayBuffer` 转成数字数组（这正是上面 254ms 那条路）。
- `tauri-2.12.4/src/ipc/protocol.rs:434`（`parse_invoke_request`）→ `:452-457`（缺 `Content-Type` 时默认 `APPLICATION_OCTET_STREAM`）→ **`:521-522` `content_type == APPLICATION_OCTET_STREAM` 时 `body.into()`，即 `InvokeBody::Raw`**；`:523-531` JSON → `InvokeBody::Json`；`:533` 其他 content type 直接报 `content type … is not implemented`。
- `tauri-2.12.4/src/ipc/mod.rs:165-172` —— `impl CommandArg for Request<'_>` 把整个 `InvokeBody` 交给命令，所以命令参数**只能有一个** `req: tauri::ipc::Request<'_>`；`tauri-2.12.4/src/ipc/command.rs:104-116` —— 任何带 key 的参数在 Raw body 上报错 `command … expected a value for key … but the IPC call used a bytes payload`。这解释了四问表第 3 条的「互斥」约束。
- `tauri-2.12.4/src/ipc/mod.rs:52-64` —— `InvokeBody` 只有 `Json(JsonValue)` / `Raw(Vec<u8>)` 两态；`:55-56` 的文档明确 **Android 拿不到 Raw**，官方建议那种平台改传 base64 `String`（本项目 Windows-only，但这条是 Task 18 唯一需要预留的降级理由）。
- `tauri-2.12.4/src/ipc/protocol.rs:185`（`handle_ipc_message`）与 `:236-245`（其 `struct Message { payload: serde_json::Value, … }`）—— **`window.ipc.postMessage` 回退通路只能携带 JSON**，字节会退化成数字数组（实测约 255ms/512KB）且不再进 `Raw` 分支。回退由 `ipc-protocol.js:55-68` 在 fetch 失败时触发并打 warn。→ Task 13 装配 IPC 层后，若 `normalize_image` 收到的是 `InvokeBody::Json`，第一嫌疑是自定义协议被 CSP/WebView2 策略挡了，而不是前端调用写错。
- `D:/aido/qwen do 1/node_modules/@tauri-apps/api/core.d.ts:268` —— `type InvokeArgs = Record<string, unknown> | number[] | ArrayBuffer | Uint8Array`；`:306` 是 `invoke` 签名。**`Uint8Array`/`ArrayBuffer` 本来就在公开类型里，前端不需要 `as any`**（带探针的代码 `npx tsc --noEmit` 零报错，实测）；但 `File`/`Blob` **不在**里面，Task 19 三个入口都得先 `await file.arrayBuffer()` 再传。
- 上述 `tauri-2.12.4/...` 的绝对前缀是 `$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`。

**未实测项**：Rust→JS 方向返回 `InvokeResponseBody::Raw`（`tauri-2.12.4/src/ipc/mod.rs:99-104` 的 `enum InvokeResponseBody { Json(String), Raw(Vec<u8>) }`）的形态与延迟——本节只测了 JS→Rust。Task 20（历史图片降级回显）若要往界面送字节，需另测，不得引用本节的毫秒数。

### 本任务的副作用（非 IPC 结论，但影响后续任务）

- `cargo add base64 image` 解析到 **`base64 0.23.1`**、**`image 0.25.10`**（写入 `src-tauri/Cargo.toml`，两者均保留给 Task 18）。brief 的 `use base64::Engine; base64::engine::general_purpose::STANDARD.decode(…)` 在 0.23.1 上**未改动即可编译**（0.23.1 仍有 `pub mod engine` 与 `pub use engine::Engine`，见 `$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/base64-0.23.1/src/lib.rs:298-299`）。
- `image` 的**默认 feature** 把 `rav1e 0.8.1` / `ravif` / `av1-grain` / `exr` / `tiff` / `gif` 整条编解码树拉了进来（本次 `cargo build` 走到 520 个构建单元，cargo 报 "Finished dev profile in 36.68s"——tauri 本身已热，新编的正是 image/rav1e 那条树）。本探针一行没用 `image`，因此**没有与它的 API 搏斗过**，不能据此评价其 API 好坏；但对 <15MB 的发布体积验收是实打实的风险 → **建议 Task 18 落地时按需收敛为 `default-features = false` + 只开所需的 jpeg/png/webp**（具体开哪些由 Task 18 按真实解码需求决定，此处只记录体积风险，不替它拍板）。
- Step 4 已把临时命令与前端探针彻底移除：`grep -rn "ipc_probe" src-tauri/src src/` 无命中，`src-tauri/src/lib.rs` 与 `src/App.tsx` 与 HEAD 逐字节相同；`cargo test` / `npm test` / `npm run build` 全绿。生产代码里**不存在**可被误当成 IPC 模式的探针样本，本项目第一个正式 IPC 层由 Task 13 建立。
