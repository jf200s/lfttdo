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
