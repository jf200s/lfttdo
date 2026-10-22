# Plan C 交付基线：体积、冷启动、常驻内存与 §9 十二条判定表

T46 那一轮只做一件事：**把「安装包 <15MB」这条验收线第一次真的量出来**。
在此之前这个项目里所有出现过的体积数字都是 `target/debug/qwen-do.exe`，
而 debug 产物从来不参与验收（`plan-a-risk-verification.md` 自己在 :38 就写了这句话）。
T47 接手了 §3（冷启动、常驻内存）与 §6（§9 十二条的判定表）。

**测量对象**：`npx tauri build` 的产物 —— 安装包文件本身，以及它吐出来的那只 exe。
不是 `cargo build --release` 的 exe（那只不能参与任何验收，原因见 §3.0）、不是 `dist/`、不是安装后占用。

---

## 1. 安装包体积（验收线 <15 MB）

### 1.1 命令与耗时（原样记录）

```
$ export PATH="$HOME/bin:$PATH"
$ CARGO_HTTP_MULTIPLEXING=false CARGO_NET_RETRY=5 CARGO_HTTP_TIMEOUT=60 npx tauri build
09:54:54                                              ← 起跑
...
✓ built in 162ms                                      ← beforeBuildCommand（npm run build）
   Compiling tauri v2.12.4
   Compiling tauri-macros v2.7.3
   Compiling qwen-do v0.1.0 (D:\qwen do 1\src-tauri)
    Finished `release` profile [optimized] target(s) in 2m 07s
       Built application at: D:\qwen do 1\src-tauri\target\release\qwen-do.exe
        Info Patching ... with bundle type information: nsis
        Info Verifying NSIS package
 Downloading https://github.com/tauri-apps/binary-releases/releases/download/nsis-3.13/nsis-3.13.zip
        Info validating hash
        Info extracting NSIS
 Downloading https://github.com/tauri-apps/nsis-tauri-utils/releases/download/nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll
        Info validating hash
        Info Target: x64
     Running makensis to produce D:\qwen do 1\src-tauri\target\release\bundle\nsis\qwen-do_0.1.0_x64-setup.exe
    Finished 1 bundle at:
        D:\qwen do 1\src-tauri\target\release\bundle\nsis\qwen-do_0.1.0_x64-setup.exe (3.92 MiB)
09:57:14                                              ← 收工
```

墙钟 **2 分 20 秒**（cargo 自己报 `Finished` in 2m07s）。NSIS 工具链是这次**第一次**下载
（`%LOCALAPPDATA%\tauri` 之前不存在），所以这一趟含两次 GitHub 拉取；后续构建会命中缓存。

### 1.2 判定

```
$ ls -l src-tauri/target/release/bundle/nsis/
-rwxr-xr-x 1 user 197121 4111344 Oct 22 09:57 qwen-do_0.1.0_x64-setup.exe
```

| 项 | 值 |
| --- | --- |
| 产物绝对路径 | `D:\qwen do 1\src-tauri\target\release\bundle\nsis\qwen-do_0.1.0_x64-setup.exe` |
| 真实字节数 | **4,111,344 B** |
| 换算 | 4,111,344 ÷ 1,048,576 = **3.92 MiB**（÷10⁶ = 4.11 MB） |
| 验收线 | 15 MB |
| **判定** | **达标**，用掉约 **26%**（按 MiB 口径），余量 **≈ 11 MiB** |

这是 MVP 验收表里第一条**有真实数字**的运行时体积线。

### 1.3 这个包里有几层，各多大

| 层 | 字节 | 说明 |
| --- | --- | --- |
| `target/release/qwen-do.exe` | 11,268,608 | 主程序，**内嵌** `dist/`（Tauri 的 asset 打包） |
| `dist/`（未压缩） | 580,273 | 前端全部产物：`index-BVMxlhG3.js` + `index-C0PcqmZW.css` |
| NSIS 安装包 | 4,111,344 | 上面那只 exe 经 LZMA（NSIS 默认 `compression`）压出来的 |

exe ÷ 安装包 ≈ **2.74×**。release profile 是 `[profile.release]` 里那五行
（`codegen-units = 1`、`lto = true`、`opt-level = 3`、`panic = "abort"`、`strip = true`），
本轮没动它。

**WebView2 运行时不在这 3.92 MiB 里**：`bundle.windows.webviewInstallMode` 没配 ⇒ 取 schema
默认 `{"silent": true, "type": "downloadBootstrapper"}`（证据见 1.5），也就是安装时**联网下载**
引导器。这台机器上 WebView2 已经随 Edge 存在，所以装的时候看不出差别；换一台干净的 Windows
就会多一次下载。这条属于「验收线只数安装包」的口径之内，但要说清楚，别让人以为 3.92 MiB 里
含浏览器内核。

### 1.4 为了量到它改了什么（两处，都在 `tauri.conf.json`）

```json
"bundle": {
  "active": true,
  "targets": ["nsis"],              // 原来是 "all"
  "windows": { "nsis": { "installMode": "currentUser" } },
  ...
}
```

- **`targets: "all"` → `["nsis"]`**：`all` 在 Windows 上会同时产 NSIS 与 MSI，MSI 那条要拉 WiX
  工具链，一次构建变成两件事、还会多一个不参与验收的产物。改完的证据是硬的：
  `bundle/` 下**只有** `nsis/` 一个目录，构建日志里 `Finished 1 bundle`（不是 2）。
- **`installMode: "currentUser"`**：个人桌面应用，装到 `%LOCALAPPDATA%`、不弹 UAC、
  元数据写 `HKCU`。代价是这台机器上的其他账户看不到这个安装。`perMachine` 那一档
  （进 `Program Files`、每次装弹 UAC）现在没有需求方。

### 1.5 键名不是照抄计划文本的

计划自己要求「键名自己对照 Tauri v2 的 schema 验证」。仓库里没有
`src-tauri/tauri_conf_schema.json`，但 CLI 带着一份权威的：
`node_modules/@tauri-apps/cli/config.schema.json`。从它读出来的事实：

- `definitions.NsisConfig.properties` 的完整名单里**有** `installMode`
  （同排还有 `template`、`languages`、`compression`、`startMenuFolder`、`minimumWebview2Version` 等）；
- `definitions.NSISInstallerMode` 的取值恰好三个：`currentUser` / `perMachine` / 一个组合档；
  `installMode` 的 `default` 就是 `"currentUser"` ⇒ **写进配置等于把默认值显式化**，
  这一条要如实说：它不改变行为，改变的是「以后有人改这行时能不能看出这是决定」。
- `definitions.WindowsConfig.properties` 里有 `nsis` 与 `webviewInstallMode`，后者
  `default = {"silent": true, "type": "downloadBootstrapper"}`（1.3 那句的证据）。

配置写错时 `tauri build` 会报错而不是静默忽略 —— 这次构建 rc=0，所以这两个键名**被接受**，
这是比 schema 更强的一层证据。

### 1.6 余量会被谁吃掉（下一轮再超线时先看这里）

本轮**没有**超线，所以计划里那条「不许靠调压缩开关掩盖问题」的分支不触发。
但余量不该被当成免费：往后要加的东西已经在路上 ——
Plan B 的 `htmd` / `url` / `ipnet`（`fetch_url` 那条链）、`rmcp`（**dev-dependency，
不进产物**，所以它不占这 11 MiB）、以及任何新前端依赖会先进 `dist/` 再被压进 exe。
真要超线时的处置顺序照计划：当前字节 → `cargo tree` 里最大的直接依赖 →
候选（`image` 的 `gif` feature 是否留、`reqwest` 的 `default-tls` 换 `rustls`）——
那是**一次停下来讨论**，不是自己下的重构决定。

---

## 2. 三条 Plan A 遗留的配置缺口（本轮逐条给现状，不粉）

1. **`build.withGlobalTauri` 不存在** ⇒ 框架默认 `false`，前端拿不到 `window.__TAURI__`。
   源码侧证据：`grep -rn "withGlobalTauri" src-tauri/ src/` 零命中；
   `grep -rn "__TAURI__" src/` 同样零命中（前端只用 `@tauri-apps/api` 的 ESM 导入）。
   **运行时那一面：未实测** —— 验它只能临时打开再跑一次真窗口，本轮没有跑真窗口。
2. **`app.security.csp: null`（`tauri.conf.json:22`）是决定，不是遗漏**：不注入 CSP。
   理由：这是一个离线单机、内容源只有用户自己配的那一个 endpoint 的界面，
   而 CSP 的真正防线（禁止任意远端脚本）在 Tauri 里由 `asset:`/`ipc:` 协议与 capabilities 承担。
   写在这里是为了下一个读配置的人不必再猜一次。
   反过来说，它也是**本轮唯一一条没有测试守卫**的安全设定 —— 想收口就得先决定 CSP 内容，
   那是另一件事。
3. **release 是否带 devtools**：`grep -rn devtools src-tauri/src src-tauri/Cargo.toml src-tauri/tauri.conf.json`
   零命中，且 `tauri = { version = "2", features = [] }`（没开 `devtools` feature）
   ⇒ 按框架默认，release 无 devtools。**运行时未实测**（要开真窗口按 F12 才算证据）。

---

## 3. 冷启动与常驻内存（T47，2026-10-22 实测）

### 3.0 先记一条会让「release 产物」这四个字失去含义的事实

今天**第一次**测出来的那批数字全部作废，原因不在测量方法，在被测对象：
`cargo build --release` 产出的 `target/release/qwen-do.exe` 是一个**离了 dev server 就打不开界面**的
二进制。把 vite 关掉再启动它，页面是 `chrome-error://chromewebdata/`、`document.title` 是
`localhost`（CDP 读 `location.href` 原文，2026-10-22 15:07）。机制读源码钉死，不是猜：

| 事实 | 出处 |
| --- | --- |
| `dev: cfg!(not(feature = "custom-protocol"))` | `tauri-macros-2.7.3/src/context.rs:155` |
| `#[cfg(dev)]` 下 `get_app_url()` 返回 `build.dev_url`，否则返回 `tauri://localhost` | `tauri-2.12.4/src/manager/mod.rs:345-358` |
| 本仓库 `src-tauri/Cargo.toml` **没有 `[features]` 段** ⇒ `custom-protocol` 只能由外部注入 | `grep -n '^\[features\]' src-tauri/Cargo.toml` 零命中 |
| 注入者就是 CLI：`"cargo" build --bins --features tauri/custom-protocol --release` | `Get-CimInstance Win32_Process` 抓到的命令行原文（15:15 那次构建） |

⇒ **只有 `npx tauri build` 的产物才是发布产物**，`cargo build --release` 那只不参与任何验收。
这条顺带对上了 §5.7 那个一直没解释的数：同一份代码，cargo 版 exe 11,125,760 B，CLI 版
11,287,040 B，差的 **161,280 B** 与「内嵌 `dist/` 582,295 B 经 brotli 压完」的量级一致。

本节所有数字都在 15:15 那次 `npx tauri build` 之后重取，取之前用 CDP 读过
`location.href == "http://tauri.localhost/"` 作为产物质地证明（连同 `min-height: 0`、
`docScroll: false` 一起读，见 3.5）。

### 3.1 冷启动（判据 <1.5 s）

**测法**：`docs/findings/scripts/cold-start.ps1`（窗口句柄 / `Responding` 两档，5 次取中位数）
+ 一段一次性 CDP 探针（`%TEMP%\qwd_coldkey.mjs`，**不进仓库**）量第三档
「输入框真的吃到一次按键」。每轮之前都确认机器上没有第二个 `qwen-do` 实例 ——
脚本自己会 abort（`ABORT: N qwen-do process(es) already running`），CDP 那一档靠手工核对。

```
$ powershell -NoProfile -ExecutionPolicy Bypass -File docs/findings/scripts/cold-start.ps1 -Runs 5
exe: D:\qwen do 1\src-tauri\target\release\qwen-do.exe
run 1: window-handle 127 ms, responding 147 ms
run 2: window-handle 120 ms, responding 138 ms
run 3: window-handle 110 ms, responding 132 ms
run 4: window-handle 111 ms, responding 127 ms
run 5: window-handle 110 ms, responding 127 ms
median window-handle: 111 ms   median responding: 132 ms   bar: 1500 ms
```

「按键可达」那档，同样 5 次（每次都是全新进程，端口用完即随进程消失）：

```
run 1: composer 551 | keystroke 584 | replayed 585
run 2: composer 563 | keystroke 587 | replayed 588
run 3: composer 518 | keystroke 524 | replayed 525
run 4: composer 582 | keystroke 610 | replayed 610
run 5: composer 511 | keystroke 523 | replayed 524
```

| 档位 | 5 次原始值（ms） | 中位数 | 对 1.5 s |
| --- | --- | --- | --- |
| 主窗口句柄出现 | 127 / 120 / 110 / 111 / 110 | **111 ms** | ✅ |
| 且 `Responding` 为真 | 147 / 138 / 132 / 127 / 127 | **132 ms** | ✅ |
| **输入框吃到一次按键**（spec :534 那句的口径） | 584 / 587 / 524 / 610 / 523 | **584 ms** | ✅，用掉 39% |
| 会话历史已重放上屏 | 585 / 588 / 525 / 610 / 524 | 585 ms | 顺带 |

四条测法边界，按对结果的影响方向写清：

1. 起点是 `Start-Process` / `spawn`，**不是真的双击图标** —— 少算 explorer 那一下，方向是**低估**，
   量级几十毫秒。584 ms 对 1500 ms 有 2.6 倍余量，这个方向的偏差改不了结论。
2. 终点是 CDP `Input.insertText` 走浏览器输入路径后 `textarea.value` 变化，**不是物理键盘**，
   也不含「焦点是否真的在输入框上」的无障碍判定（探针另外断言过 `document.activeElement === ta`）。
3. 轮询粒度 20 ms ⇒ 所有数字都是**上界 + 20 ms 的量化误差**。
4. **冷文件系统缓存的首次启动：未实测。** 上面 10 次全是 warm。要真做冷只有两条路：
   Sysinternals `EmptyStandbyList`（本机没有，且不装第三方可执行）或重启。
   「`Copy-Item` 一份新副本再测」这条路我试过思路后**放弃**：写入本身就经缓存管理器，
   副本的页在 `Copy` 完就已经热了，那样报出来的「冷」是假的。

### 3.2 空闲常驻内存（判据 <150 MB）—— ❌

**测法**（计划 T47 Step 2 点名的坑，照做）：以 `qwen-do` 的 PID 为根，按 `ParentProcessId`
**递归**收整棵树再 `WorkingSet64` 求和；**不按进程名过滤** ——
`msedgewebview2.exe` 是全机 WebView2 应用共享的镜像名，按名字求和会把别人的内核进程记到我们账上。
脚本 `%TEMP%\qwd_mem.ps1`（一次性，不落仓库），窗口打开 **60 秒**、无对话，取 3 次。

```
root pid 45592, up 60 s
sample 1: 7 processes, WorkingSet sum 394.3 MB, PrivateBytes sum 211.2 MB
sample 2: 7 processes, WorkingSet sum 394.4 MB, PrivateBytes sum 210.4 MB
sample 3: 7 processes, WorkingSet sum 394.5 MB, PrivateBytes sum 210.4 MB
```

| 角色（命令行 `--type=` 判的） | PID | WorkingSet | PrivateBytes |
| --- | --- | --- | --- |
| **我们的进程**（browser-root，无 `--type`） | 45592 | **29.0 MB** | **6.5 MB** |
| WebView2 browser-root | 26560 | 136.4 | 40.8 |
| gpu-process | 45760 | 76.7 | **107.6** |
| renderer | 42184 | 74.2 | 32.4 |
| utility ×2（网络服务 / 其它） | 29780 / 44440 | 41.6 / 20.9 | 11.7 / 7.5 |
| crashpad-handler | 37016 | 15.1 | 3.4 |
| **合计** | 7 个进程 | **394.4 MB** | **210.4 MB** |

**判定：❌ 超标 2.6 倍**，而且这不是「我们再优化一下」能收的差距：

- 树里 **93%（365 MB）是 WebView2 运行时自己的进程**，我们写的那部分只有 29 MB 工作集 /
  6.5 MB 私有字节。Plan A 风险清单第 8 条当时写的预估是「Rust core + WebView2 实测 60–120 MB」，
  **那个预估错了**，错在把 WebView2 当成一个进程。
- 两个口径都试过：`WorkingSet64` 求和会**重复计共享页**（虚高），所以同时报了 `PrivateBytes`
  求和 = 210.4 MB —— 这是更保守、更接近任务管理器「内存」列的口径，**仍然超**。
- 唯一有希望的那个开关试了：`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--disable-gpu`
  同样跑 60 s 空闲 ⇒ **380.2 MB WS / 119.9 MB Private**（省 14.2 MB WS / 90.5 MB Private），
  但 gpu-process **并没有消失**（仍占 53.9 MB WS），只是从硬件合成退回软件合成。
  也就是说：**没有任何一个运行时旋钮能把 394 MB 拉到 150 MB**，能省的那 14 MB 是 3.5%。
  代价是渲染走 CPU，界面滚动/长回答上屏会明显变糙 —— 拿这条去换 14 MB 不值。
- 测量状态如实记：侧栏 10 个会话，打开时自动恢复的是上一轮测试留下的那一屏内容
  （含 markdown 列表）。**没有**测「空数据库首启动」那一档，那一档只会低不会高，
  低多少未实测。

⇒ 这一条**必须由用户重述**，三个方向（我不替用户选）：
(a) 把判据改成「我们自己的进程 <30 MB + 安装包 <15 MB」，即「轻」定义在**我们可控的部分**；
(b) 保留 150 MB 但明确它当前不达标、写进已知限制；
(c) 换掉 WebView2（QtWebEngine / 自绘 / Flutter）—— 那是换技术栈，不在任何计划里。

### 3.3 流式不掉帧 + 中断 1 s（Step 3）

计划 Step 3 允许两条路：临时打开 `withGlobalTauri`，或改用「T16 的 rAF 合帧测试 + 一次真流式的
主线程帧间隔」。**走了第二条** ⇒ `tauri.conf.json` 一行未改，那条「临时改动未提交」的尾巴不存在。
DevTools Performance 面板那条路本来也走不通：release 无 devtools（§2 第 3 条）。

测法：CDP 连进 15:15 那只产物 exe，点「新建对话」→ 发一条要求 150 条列表的长 prompt →
等真实文本开始上屏（`.message-list` 的 `MutationObserver` 批次 ≥40）→ 在**真渲染窗口**内
跑 rAF 采样环 → 点「停止」。跑了两轮（第二轮把点击后的观测窗从 200 ms 拉长到 2 s）。

| 判据 | 实测 | 判定 |
| --- | --- | --- |
| 主线程单帧 <16 ms | rAF 间隔：963 / 1055 个样本，median **3.3 ms**、p95 **3.4 ms**、max **3.5 / 10.0 ms**，**>16 ms 的帧 0 个** | ✅（口径警告见下） |
| React 渲染频率 ≤10 次/秒 | DOM 提交批次 **26.0 / 27.3 次每秒** | **❌ 未达字面阈值**，见 3.3.1 |
| 中断在 1 s 内生效 | 点击 → 最后一次 DOM 提交 **+4.3 / +5.5 ms**；点击 → 「停止」按钮卸载 **+8 / +15.3 ms**；其后 **2 s 内 `.message-list` 零提交**（`commitsAfterClick = 1`，就是那一下） | ✅ |

**口径警告（必须连着读上面那行才不算自欺）**：rAF 间隔**不等于** DevTools 的「主线程单帧耗时」，
它是「两次回调之间的墙钟」。本机 rAF 稳定跑在 **300 Hz**（median 3.3 ms）—— 要么这是一块
300 Hz 屏，要么这个窗口没被垂直同步封顶。无论哪种，「≤16 ms」在这台机器上是一条**天然宽松**的线，
所以我连绝对分布一起贴出来，而不只贴一个 ✅。反过来它也是**强**证据的那一面：整段真流式渲染里
最长的一帧只有 10 ms，说明主线程从没被一次 React 提交卡住过一帧以上。

#### 3.3.1 26 次/秒 vs spec 的 ≤10 次/秒：一次口径分叉，不是回归

`src/ipc/batching.ts` 的合帧边界选在**一帧一次**（文件头注释自己写着「使 React 渲染频率 ≤ 帧率」），
而 spec :539 写的是「≤10 次/秒」。Rust 侧 `ipc.rs` 的 `Coalescer::MAX_WAIT = 30 ms` 已经把
50–100 次/秒的 `text_delta` 压到 ~33 次/秒，前端再按帧合并 ⇒ 实测 26–27 次/秒正好落在那条上限之下。
**症状上这条已经赢了**（不掉帧、上屏跟手），但阈值字面没满足。收口只有两种：
给合帧加一个 100 ms 的时间下限（改实现、字面达标、代价是最长 100 ms 的上屏延迟），
或者把 spec :539 那句改成「≤ 帧率且实测无 >16 ms 帧」（改标准）。**这是用户的判据，我不自己选。**

#### 3.3.2 「停止中…」这个中间态实测不可见

两轮都记到 `stopBecameDisabledMs = null`：`Composer.tsx:136` 确实把 `stopRequested` 置真、
按钮也确实带 `disabled`，但从点击到 `.stop` 卸载只有 8–15 ms，**没有任何一帧**能被观测成
「灰着的『停止中…』」。spec :475 那句「点击后立即禁用并显示停止中…」在用户眼里其实是
「按钮立刻变回发送」。不是缺陷（反馈更快、且已生成的文本原样保留，截图核过），
但那是 spec 的一句话没有对应可观测行为的一条，记在这里免得下一轮又当成 bug 去「修」。

### 3.4 本轮数字对应哪一份代码

> 这一节是 §3.1–§3.3 那三组数字的**出处**，不是仓库里最新的字节数 —— 最新的在 §3.6。

| 事实 | 值 |
| --- | --- |
| 来源 commit | `0107e03`（工作树除 `docs/findings/scripts/` 外干净） |
| 构建命令 | `npx tauri build`，rc=0，15:15 |
| `target/release/qwen-do.exe` | 11,287,040 B |
| `bundle/nsis/qwen-do_0.1.0_x64-setup.exe` | **4,118,863 B（3.93 MiB）**，比 §5.7 那次 +2,991 B |
| `dist/` | 582,295 B（+2,022 B，就是 `0107e03` 的三行 `min-height: 0` 与一处色值） |

⇒ **§1 那条 15 MB 的线在新代码上重测仍然大幅达标**，判定不变（3.93 MiB / 15 MB，用掉 26%）。

### 3.5 顺带：R235 那两处修复在**产物**里成立

字节级 grep 这条路是错的（内嵌资产是 brotli 压缩的，产物里搜不到 `min-height:0` 也搜不到色值），
所以判据只能是行为。在 15:15 那只产物 exe 里用 CDP 读计算样式：

```
href "http://tauri.localhost/"  minHeightMessageList "0px"  overflowMessageList "auto"  docScroll false
```

⇒ 外壳滚动容器真的能缩了、文档不再跟着滚。这条证据从「dev 实例 + vite」换成了「发布产物」，
之前那条只是同一件事的弱证据。

### 3.6 §3.4 的字节数已经过期：`c7bdfe6` 重打一次

`c7bdfe6`（把 Composer 里那只原生 file 控件收进 token 体系）之后重新 `npx tauri build`，
同一台机器、同一份工具链，只差前端那 20 来行 CSS：

| 事实 | 值 | 对 §3.4 |
| --- | --- | --- |
| 构建命令 | `npx tauri build`，rc=0，16:02 | 同 |
| `target/release/qwen-do.exe` | 11,287,040 B | **一字不差**（为什么没变：未测，只是记录） |
| `bundle/nsis/qwen-do_0.1.0_x64-setup.exe` | **4,118,009 B（3.93 MiB）** | −854 B |
| `dist/` | 582,348 B | +53 B |

`dist` 涨 53 B、安装包反而降 854 B：内嵌资产是 brotli 压的，LZMA 再压一层，
字节的增减方向和源文件字节数不同向 —— 这不是矛盾，是压缩。

同一只产物里的计算样式（CDP，`href` 仍是 `http://tauri.localhost/`）：

```
input  fontSize 0px  border 0px none  bg rgba(0,0,0,0)  shadow none  85×29
button fontSize 15px color rgb(232,230,227) bg rgb(29,29,33) radius 6px content "normal" cursor pointer
```

外壳 85×29 和按钮同宽 ⇒ UA 那行「未选择文件」占的宽度是 0，界面上真的没了；
`content: "normal"` 是「那行死声明已经删掉」的正面证据（它当初能算出值、却不上屏）。

⇒ **§6 第 1 行的证据换成这一只**（3.93 MiB / 15 MB，判定不变）。
⇒ §3.1 冷启动、§3.2 内存、§3.3 流式那三组数字**没有**在 `c7bdfe6` 上重跑：它们量的都不是
CSS 层的东西，而重跑一次冷启动要 5 次真启动、重跑内存要 60 s 空闲窗，本轮没有拿到
「改了 20 行 CSS 所以那几个数会变」的理由。**下一轮若动 Rust 侧或依赖，这三组一律重测。**

---

## 6. spec §9 十二条判定表（T47 Step 4；#29 与 T48 的唯一输入）

一条一格，**不合并、不省略未做的**。判定只有四种：`✅` / `❌` / `未实测` / `不适用`，
需要人坐在那台机器前面的，在判定后再加一个 `待人工`。

| # | 判据（spec :531-563 原文） | 测法 | 实测值 | 判定 |
| --- | --- | --- | --- | --- |
| 1 | 安装包 <15MB | NSIS 产物文件字节数 | 4,118,009 B = **3.93 MiB**（§3.6，`c7bdfe6` 重打；`0107e03` 那次是 4,118,863 B） | ✅ |
| 2 | 冷启动 <1.5s | 进程启动 → 输入框吃到一次按键，5 次中位数（§3.1） | **584 ms**（另两档：句柄 111 ms / `Responding` 132 ms） | ✅；**冷文件系统缓存的首次启动 未实测** |
| 3 | 空闲常驻内存 <150MB | 递归 `ParentProcessId` 取整棵进程树求和，空闲 60 s，3 次（§3.2） | **394.4 MB** WorkingSet / 210.4 MB PrivateBytes；其中我们自己的进程 29 MB | ❌ 超标 2.6 倍，且 `--disable-gpu` 只省 14 MB ⇒ **判据本身要用户重述** |
| 4 | 流式输出不掉帧 | 真流式窗口内 rAF 帧间隔 + T16 合帧单测（§3.3） | >16 ms 的帧 **0 个**（max 3.5 / 10.0 ms）；但渲染频率 **26–27 次/秒** | 帧间隔 ✅ / 「≤10 次/秒」❌ ⇒ 口径分叉，见 §3.3.1 |
| 5 | 中断在 1s 内生效 | 点「停止」→ 前端最后一次文本 DOM 提交；其后 2 s 观测窗（§3.3） | **+4.3 / +5.5 ms**，2 s 内**零**新增提交 | ✅ |
| 6 | 长对话不失忆、不 400 | 连续 100 轮：存在覆盖被丢弃区间的摘要消息，且第 100 轮 input tokens ≤ 预算 | 摘要压缩**未实现** —— `plan_compaction` 在 `src-tauri/` 零命中，`ContextCompacted` 只有事件变体与映射、**没有生产者**（`context.rs:45`、`session.rs:294` 自己写着「Plan A 无消费者，属切片 5」）；今天只有滑窗丢弃 + 图片降级 | ❌ 未实现（Plan C **T40–T42** 未开工）；人工那一半 `待人工` |
| 7 | 全程零弹窗 | 100 轮 + 若干次联网下 `permission_request` 计数 = 0 | Plan A 结构性为 0（生产 registry 不注册工具，权限门不触发）；前端 `grep "confirm(\|alert(\|prompt("` **零命中** ⇒ 连 `window.confirm` 这类逃逸都没有。**但从未在真 key 全流程下数过一次** | 未实测 `待人工`（T48） |
| 8 | 看图可用 | 一次贴 4 张图，模型答对图中内容；单张超限时 UI 指名是哪张、为什么 | 归一化 / 三入口 / 历史降级 的自动化测试在（T18–T20）；**真模型看图从未跑过** —— 本机 Ollama 只有 `qwen3-abliterated:8b` 这一个文本模型，没有 VL 模型 | 未实测 `待人工`（且 #28 阻塞在「要不要拉一个 VL 模型」这个决定上） |
| 9 | MCP 可用 | 接一个真实搜索 server，模型调用其工具并引用来源 | Plan B 未开工；`rmcp` 仍在 `[dev-dependencies]`（只有 `examples/mcp_probe.rs` 用），**不进产物、也没有生产用户** | 不适用（Plan B） |
| 10 | 无孤儿子进程 | 连 ≥1 个 stdio MCP server 后关窗，进程树无残留 npx/node | 同上 —— 没有 stdio 连接可关 | 不适用（Plan B） |
| 11 | 主题生效 | 3 套预设 × 亮暗 × 强调色 × 字号，切换后即时生效且无需重启 | 预设 × 亮暗 = **6 块**真窗口截图逐块核过（#49 / R235）；字号 = `--font-size`，`USER_TOKENS` 里唯一的用户 token，设置面板可改、即时生效；**强调色取色器不存在** —— `theme.ts:39` 是 `USER_TOKENS = ["--font-size"]`，`--accent` 只由 `presets.css` 六块钉值 | 部分：预设 / 亮暗 / 字号 ✅；强调色 ❌ 未实现（Plan C **T43–T44** 未开工） |
| 12 | api_key 不落明文 | key 在 Windows 凭据管理器；对配置目录与日志**全文搜索该 key 字符串，0 命中** | 结构性证据在：T12 只写 keyring、IPC 出口返回掩码、`settings.rs` 有一条断言序列化结果**不含** `api_key` 的钉子。**「全文搜索 0 命中」这一步从未跑过**（且当前存的那把 key 被两个 region 都 401 拒、曾被贴进对话 ⇒ 欠轮换，轮换前跑它没有意义） | 未实测 `待人工`（T48，前置是轮换） |

**合计**：✅ 3 条（1、2、5）· 部分 ✅ 2 条（4、11）· ❌ 2 条（3 超标、6 未实现）·
不适用 2 条（9、10，Plan B）· 未实测/待人工 3 条（7、8、12）。

### 6.1 为了让这张表好看，本轮**没有**做的四件事

1. **没有**把 `--disable-gpu` 写进 `tauri.conf.json` 或任何启动参数去换那 14 MB —— 它改变渲染路径，
   拿界面手感换一个仍然不达标的数字是亏的。
2. **没有**把第 3 条的判据改小或改口径（比如「只数我们自己的进程 ⇒ 29 MB ✅」）。
   那 29 MB 我写在括号里，不写在判定列。
3. **没有**把第 4 条整格判成 ✅。帧间隔那半边是真的，「≤10 次/秒」那半边实测 26 次/秒，
   两半分开报。
4. **没有**用「Plan A 结构性为 0」充当第 7 条的证据。结构性推理我写在实测值那一格里，
   判定列是 `未实测`。

### 6.2 这张表往下怎么走

- 第 3、4 两条**需要用户判**：一个是要不要重述 150 MB，一个是要不要给合帧加 100 ms 时间下限
  （或改 spec 那句阈值）。这两条都不是我能替用户收的。
- 第 6、11 两条各有明确的未开工任务（T40–T42 / T43–T44），要动就是**开 Plan C 的那两片**，
  不是在本轮顺手补。
- 第 7、8、12 三条是 **T48 的正文**，且第 12 条前置在 key 轮换、第 8 条前置在一个 VL 模型。
- 第 9、10 两条属于 Plan B，与「MCP 客户端目前不在这个产品里」是同一个事实的两种写法。


---

## 4. 本轮产物对应哪一份代码

| 事实 | 值 |
| --- | --- |
| 前端来自 | commit `51a7eef`（T45 设置面板），构建时工作树只有 `tauri.conf.json` 一处脏 |
| `dist/assets/*.js` 里含面板 | `grep -ac 上下文预算` = 1、`grep -ac settings_set_api_key` = 1 |
| 安装包 | `qwen-do_0.1.0_x64-setup.exe`，4,111,344 B，2026-10-22 09:57 |

也就是说：**这个 3.92 MiB 的包已经带设置面板**，T47 拿它直接量冷启动，不必重打。
（**这一句现在过期了一半**：见 §5 —— 那 3.92 MiB 量的是 `51a7eef` 的代码，T46b 在它之后
又叠了一层窗口状态。没重测，所以别把这两个事实混着读。）

---

## 5. 窗口状态（T46b：spec §6.6 的「记住尺寸位置」）

### 5.1 为什么是自己写的 ~80 行

`tauri-plugin-window-state` 存在，但本计划的 Global Constraints 禁止新增直接依赖
（每条依赖都要重算 §1 那个数），而越屏恢复的策略本来就必须是我们的代码 —— 插件的恢复判定不可控。
动工前的现状用 grep 钉住：`window_state` / `set_size` / `outer_position` 在 `src-tauri/` 零命中，
`tauri.conf.json:17-18` 把 `width: 800 / height: 600` 写死，`capabilities/default.json` 里也没有
任何窗口权限。也就是说 spec :461 那半句「记住尺寸位置」当时**既没实现也没认领**。

### 5.2 三个 API 形状是验过的，计划里的写法有两处不对

按 `tauri-2.12.4` 与 `tauri-runtime-2.12.2` 的 vendored 源码逐条核对（行号为证）：

| 要用的东西 | 实际形状 | 位置 |
| --- | --- | --- |
| 事件枚举 | `Resized(PhysicalSize<u32>)` / `Moved(PhysicalPosition<i32>)` / `CloseRequested { api }` / `Destroyed`，且整个枚举 `#[non_exhaustive]` ⇒ 必须有兜底臂、结构体变体要写 `{ .. }` | `tauri-2.12.4/src/app.rs:111` |
| 事件回调签名 | `Fn(&Window<R>, &WindowEvent)` —— **事件是按引用给的**，计划正文按值写的 | `app.rs:2197` |
| 取几何 / 落几何 | `outer_position() -> Result<PhysicalPosition<i32>>`、`outer_size() -> Result<PhysicalSize<u32>>`、`current_monitor() -> Result<Option<Monitor>>`、`set_size(S: Into<Size>)`、`set_position(Pos: Into<Position>)` | `window/mod.rs:1580/1594/1686/1924/1963` |
| 显示器枚举 | `available_monitors() -> Result<Vec<Monitor>>`（恢复用它，不用 `current_monitor() -> Result<Option<Monitor>>`，理由见 5.3）；`Monitor` 在 `tauri` 这层字段是 `pub(crate)`，只给访问器，`work_area()` 返回 `&PhysicalRect`，其 `position`/`size` 才是公开字段 | `window/mod.rs:1718`、`window/mod.rs:56-101` |
| 窗口何时存在 | conf 里的窗口在 `setup` 钩子**之前**创建（`WebviewWindowBuilder::from_config`，`app.rs:2692`）⇒ 恢复只能落在 `setup` 里，而那里一定拿得到 `main` | `app.rs:2688-2698` |

`m.work_area`（按字段读）这一版**编译不过**，是本轮真实撞到的一条：
`error[E0616]: field 'work_area' of struct 'Monitor' is private`（`cargo test` rc=101）⇒ 改成
`m.work_area()`。留在这里的原因：计划说「若 v2 的形状与这里不同，按实际的写」，这条就是那条
说明的账单。

坐标系决定：取和放**全程用物理像素**（`outer_*` 给物理值，`set_*(PhysicalSize/PhysicalPosition)`
收物理值时不缩放），所以 `window_state.rs` 不需要 `scale_factor`。存 logical 值的后果是换一块
缩放不同的屏幕后窗口摆到错位置 —— 正是本模块要防的病。

### 5.3 取舍和它们的代价

- **只在关闭事件落盘**（`should_persist`：`CloseRequested`/`Destroyed` 为真，`Resized`/`Moved` 为假），
  并且**不做定时器去抖**。一次拖拽会发来几百个 `Resized`，逐个写盘把「记住位置」做成
  「每移一次写一次磁盘」；而去抖定时器是给「关窗时还在写」新造的一类竞态。
  代价如实记着：**崩溃或强杀会丢窗口位置**，症状是「下一次回到默认尺寸」，不是数据丢失。
  `Destroyed` 这一臂很可能已取不到几何，此时**不写**而不是写一份 `(0,0,0,0)`。
- **越屏判定对着每一块接着的屏做**（`available_monitors()`，`window/mod.rs:1718` → `pick`），
  而不是计划正文写的「`current_monitor()` 那一块」。这是本轮改掉的**一个真缺陷**：恢复发生在
  窗口刚创建、还停在系统默认位置的时候，那时 `current_monitor()` 给的是主屏，于是摆在副屏上的
  存档会被判成越界并丢掉 —— 症状不是崩溃，是「每次重启都从副屏被拽回主屏」，等于这条需求
  对双屏用户白做，而 §5.1 那句「拔掉副屏」说的就是这批用户。
  `pick` 仍然保守：一块屏都装不下 ⇒ `None` ⇒ 不恢复；枚举不到任何显示器同样不恢复。
  一个无法证明「某块屏上看得见」的几何比默认尺寸更危险。
  `sane()` 丢掉越界存档之后落到的正是 conf 那一行 —— 所以 `"width": 800, "height": 600` **保留**，
  它的身份是「没有存档时的尺寸」，删掉它换来的是 Tauri 自己的默认值，那是第三份真相。
  存档路径是 `%APPDATA%\com.qwa.do\window.json`，读不到/解不开一律按「没有」处理
  （`load` 返回 `Option`，没有错误分支），理由写在测试里：一个存坏的配置文件不该有能力
  让应用打不开。
- 原子写照 `settings.rs:478-490`（同目录 temp → `write_all` → `sync_all` → `rename`），
  但**不带** `SettingsStore` 那把实例锁：调用点只有窗口关闭事件，而窗口事件在同一根事件循环
  线程上串行派发，进程内不存在两个并发写者。
- **只存几何，不存窗口状态**（最小化 / 最大化一律不落盘）。最小化时关窗，Windows 报的是
  `(-32000, -32000)` 一类的哨兵位置，这里不加特例 —— 写进去，下次启动由 `sane` 判成越界，
  代价是那一次回到默认尺寸。换来的简洁是实在的：不用引一层「现在是不是 minimized」的查询，
  也不多一份要维护的状态编码。

### 5.4 证据（本轮逐条实跑）

`cargo test --manifest-path src-tauri/Cargo.toml` 全量：
**`test result: ok. 272 passed; 0 failed; 1 ignored`**（本任务前是 267 条，+6；
`window_state::tests` 6 条全绿）。`rustfmt --edition 2024 --check` 逐文件数出的差异：
`window_state.rs` 5 处、`ipc.rs` 1 处、`lib.rs` 0 处（`lib.rs` 会连带检查它声明的模块，
所以单独数它时才用 HEAD 里那份副本对比过，确认既有代码本来就 rustfmt-clean）；
`rustfmt` 落定后再 `--check` 三个文件全归零。`git diff -U0 -- src-tauri/src/ipc.rs` 的 6 个 hunk
全在本次新增的区段里，既有代码一行没被 reformat。

四条变异判据，每条都实测并贴出红的那一条，然后 `cp` 还原 + `cmp` 证字节一致：

| 变异 | 结果 |
| --- | --- |
| `sane` 的「一个角在内即保留」改成「左上角在内才保留」 | `a_geometry_fully_off_the_current_work_area_is_discarded ... FAILED`（`x:1919` 那条），rc=101 |
| 删掉 `load` 的解析失败分支（解不开时回一份 800×600 几何） | `a_corrupt_or_missing_state_file_is_absent_not_an_error ... FAILED`，panic 落在 `assertion left == right failed: 解不开就按『没有』处理…`，rc=101 |
| `should_persist` 让 `Resized` 也返回真 | `only_the_close_events_persist ... FAILED`，panic 落在 `assertion failed: !should_persist(Ev::Resized)`，rc=101 |
| `pick` 只看第一块屏（`works.first()`）—— 也就是把 §5.3 那个缺陷放回去 | `a_geometry_on_any_attached_monitor_is_kept ... FAILED`，panic 落在存档几何那一行（`window_state.rs:165`），rc=101 |

RED 也是跑过的：先只写测试模块（`lib.rs` 加一行 `pub mod window_state;`），
`cargo test` 报 **25 个 E0422/E0425/E0433「cannot find … in this scope」**、rc=101 ——
是「功能不存在」而不是笔误；`pick` 那一条同样是先红（**3 个 `cannot find function 'pick'`**、rc=101）
再实现。

### 5.5 接线的人工两步跑完了，而且抓到一个只有真窗口能暴露的缺陷

**方法**（2026-10-22，无新增依赖、无新增自动化测试）：用一段一次性 PowerShell + `user32`
（脚本在 `%TEMP%\qwd_geom.ps1`，**不进仓库**）读真实窗口矩形。先
`SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` —— 不调这一步，
非 DPI-aware 的宿主进程读到的是系统虚拟化的坐标，量出来的每个数都不能信。
被测量的是 `target/debug/qwen-do.exe`（配 `npm run dev` 的 1420），一台显示器
`PRIMARY w=2560 h=1600`。

三条分支各自实测，原文数字如下：

| 分支 | 存档内容 | 启动后实测 | 判定 |
| --- | --- | --- | --- |
| 越界丢弃（Step 5 的第 ② 步） | `x:3000 y:200 900×700` | 窗口在 `342,342 / 1222×956`，完整可见；`window.json` 一行未动 | 通过：`pick` 判越界 ⇒ 不恢复 ⇒ 交给系统摆放。**注意落点不是 conf 的 800×600**，见下面那条 |
| 关窗落盘（第 ① 步的落盘半边） | 移动后实测 `342,342 / 1000×800` | 关窗后存档写成 `x:342 y:342 width:1000 height:800` | 通过：写的是**关窗瞬间的真实几何**，不是我请求 `SetWindowPos` 的那组值 |
| 启动恢复（第 ① 步的恢复半边） | `x:342 y:342 1000×800` | 窗口回到 `left=342 top=342`（位置精确）但 `w=1022 h=856`（尺寸不对） | **失败 —— 见下** |

**缺陷**：位置对、尺寸每次启动净增一圈边框。连着三次开关量到
`1000×800 → 1022×856 → 1044×912`（每轮 +22 宽 / +56 高，在这台屏上）。成因是
存档侧取 `outer_size()`、恢复侧 `set_size()` 生效在**客户区**尺寸上（tao 的 `resize` 语义），
于是边框宽度被反复叠加；涨到 `sane` 把工作区夹满才停。
`ipc.rs:734` 改成 `inner_size()`（位置仍用 `outer_position()`，那一半边实测是精确往返的），
重编译后再跑同一循环：

```
=== launch 1 ===  RECT_BEFORE left=400 top=300 right=1422 bottom=1156 w=1022 h=856
=== saved after close ===  {"x":400,"y":300,"width":1000,"height":800}
=== launch 2 ===  RECT_BEFORE left=400 top=300 right=1422 bottom=1156 w=1022 h=856
```

两次启动量到的矩形**逐位相同**，存档也把 `1000×800` 原样还回来 ⇒ 往返闭合。

两条要说清的代价：

- 这条缺陷**没有回归测试守着**。`window_state.rs` 的纯函数不知道 outer/inner 的存在，
  `AppHandle` 在测试里仍然构造不出来（§5.5 原本写的那句「接线没有自动化证据」到今天依然成立，
  只是现在多了上面那一次真窗口证据）。可重放的跑法就是上面那段 PowerShell，
  要把它做成常驻脚本得先决定放哪、怎么在 CI 里没有桌面会话时跳过 —— 不在本任务范围内。
- **改完之后 `window.json` 里 `width/height` 的含义从 outer 变成 inner**。这是个人应用、
  只有一份存档、下一版自己会写回正确语义，所以不做迁移也不加版本号。
  后果仅是：旧版写下的存档在新版首启动会小一圈边框（约 22×56），下一次关窗即自愈。

顺带纠正 §5.3 里一句我写得太满的话：越界存档被丢弃后，窗口落点**不是** conf 的 800×600，
而是系统给这个窗口类的默认级联位置与尺寸（本机实测 `342,342 / 1222×956`，正是上一个实例的
尺寸加级联偏移）。`width: 800, height: 600` 那两行仍然是「没有存档时 Tauri 自己用的尺寸」，
但「丢弃越界存档 ⇒ 回到 800×600」这个推断错了：Win32 的 `CW_USEDEFAULT` 会记住同一窗口类
上一个实例的摆放。判据要盯的是**窗口可见**，不是具体数字 —— 上面那一行按这个标准算通过。

### 5.7 体积线在 T46b 之后重打了一次（补上 §5.5 原来欠的那笔）

同一台机器、同一份 `tauri.conf.json`（`targets: ["nsis"]`），`npx tauri build` 全文成功、rc=0：

| 产物 | T46（10-22 上午，T46b 之前） | 本次（T46b + inner_size 修复之后） | 差 |
| --- | --- | --- | --- |
| `bundle/nsis/qwen-do_0.1.0_x64-setup.exe` | 4,111,344 B（3.92 MiB） | **4,115,872 B（3.93 MiB）** | +4,528 B（+0.11%） |
| `target/release/qwen-do.exe` | 11,268,608 B | 11,286,016 B | +17,408 B |
| `dist/` | 580,273 B | 580,273 B | 0（前端一行未改） |

Tauri 自己打印的那行也一致：`Finished 1 bundle at: …(3.93 MiB)`。
**验收线仍然大幅达标**：3.93 MiB 对 15 MB，用掉 26%，还剩约 11 MiB。
零新依赖这件事在数字上兑现了 —— 约 230 行 Rust 换 4.5 KB 安装包体积。
§4 里那条「3.92 MiB 是 T46b 之前的量法」的过期警告到此解除，**引用体积请以本节为准**。



### 5.6 顺带数清 #29 那条旧账（不关它，也不动 capabilities）

计划 Step 4 说做完本任务后「`core:window:allow-set-size`、`allow-start-dragging`、
`dialog:allow-open` 三条仍无前端消费者，数成三条」。**这条已经不成立**：
`capabilities/default.json:6` 现在恰好两项 —— `["core:default", "core:event:default"]`，
文件描述自己写着「无 shell、无文件系统、无窗口尺寸/拖拽权限」，`dialog` 那条在评审 #29 就摘了。
所以**无消费者权限是 0 条，不是 3 条**。本任务的恢复全在 Rust 侧（Rust 不受 capabilities 约束），
`capabilities/default.json` 一行未动。

## 7. 第一次真的装上、真的跑起来（2026-10-22 22:35–22:57，HEAD `4ad7010`）

### 7.0 为什么这一节之前不存在

ledger :2218 那条原话是「**没有真的运行过那个 setup.exe**」。§1 到 §3.6 量的都是文件字节数 ——
装没装上、装完能不能跑、跑起来长什么样，一条都没验过。本轮得到许可（「先把qwen-do关了再重装」）
才第一次走完 打包 → 静默安装 → 启动 → 真窗口取数。

### 7.1 体积线（T43/T44/#64 三片落地后的复测）

| 量法 | 字节 | 对比 |
| --- | --- | --- |
| `bundle/nsis/qwen-do_0.1.0_x64-setup.exe` | **4,127,228 B（3.94 MiB）** | 比 §3.6 的 4,118,009 多 **+9,219 B**；比 §1.1 的 T46 基线 4,111,344 多 +15,884 B |

15 MB 的线用掉 26%。**引用体积以本节为准**，§5.7 那句「以本节为准」的有效期到此结束。

### 7.2 安装（这台机器上的第一次）

| 项 | 值 |
| --- | --- |
| 命令 | `powershell -NoProfile -Command "Start-Process -FilePath '…\qwen-do_0.1.0_x64-setup.exe' -ArgumentList '/S' -Wait"` → rc=0 |
| 为什么不是 `cmd /c start //wait … /S` | MSYS 不重写引号串里的 `//`，`start` 把 `/S` 当成自己的开关，报「无效开关 - "/"」，rc=1 |
| 安装目录 | `%LOCALAPPDATA%\qwen-do\`：`qwen-do.exe` 11,311,616 B、`uninstall.exe` 79,242 B |
| 卸载登记 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\qwen-do` |

### 7.3 装进去的那只 exe 与 build 出来的那只差 3 个字节

| 文件 | 字节数 | md5 |
| --- | --- | --- |
| `%LOCALAPPDATA%\qwen-do\qwen-do.exe` | 11,311,616 | `9e02efe7…` |
| `target/release/qwen-do.exe` | 11,311,616 | `d7aaafa2…` |

同尺寸、不同哈希，看着像「装的不是新代码」。逐字节比对的结论是：**只有 3 个字节不同，
且全落在偏移 10,081,375–10,081,377 这一段连续区间里** —— bundler 复制时对 PE 打的一处补丁，
不是另一份构建。而「装的是新代码」这件事的判据本来就不该是哈希（内嵌资源是 brotli 压过的，
字节级 grep 也无效），是行为：见 §7.5。

### 7.4 冷启动的两条时间线（装出来的二进制，不是 dev）

| 量 | 第一次启动 | 第二次启动 |
| --- | --- | --- |
| 文档 `responseEnd` | — | 29 ms |
| CSS `responseEnd` | — | 37 ms |
| JS `responseEnd` | — | 42 ms |
| DOMContentLoaded | 58 ms | 59 ms |
| first-paint / FCP | 212 ms | 188 ms |

**「首帧不再闪」在真窗口里成立，但理由是排序不是运气**：`seedFromCache` 是 `main.tsx` 里的同步
语句，而 `<script type="module">` 按规范必须在 `DOMContentLoaded` 之前执行完；两次实测的 DCL
（58 / 59 ms）都远早于 first-paint（212 / 188 ms）⇒ 浏览器能画出的第一帧已经带着主题。
绝对毫秒会随机器变，这条不等式不会。

### 7.5 强调色链路在 WebView2 里跑通的两个方向

正向（面板取消勾选 → 原生 setter 写值 + dispatch `input` → 点保存）：

| 观测点 | 值 |
| --- | --- |
| `settings.json`（磁盘） | `"accent": "#c2410c"` |
| 行内 `--accent` / `-hover` / `-text` | `#c2410c` / `#d85722` / `#ffffff` |
| 缓存 `qwen-do.theme` | `{"theme":"noir","mode":"dark","fontSize":15,"accent":"#c2410c"}`（四键白名单兑现，无 base_url、无模型名） |
| 界面 | 会话项边框与文字、输入框 focus 环、正文链接即刻全部转成橙色，无需重启 |

hover 按 `mode` 取方向（dark→更亮）这一条实测：ΔL(#c2410c→#d85722) = **0.0625** ≥ `MIN_LUMA_DELTA` 0.06。
文字取黑白里更强的那支：L = 0.1528 时白 5.18:1 > 黑 4.06:1 ⇒ `#ffffff`，AA 达标。
（顺带修正我自己的一次口算：写 brief 时我以为这支色会选黑字，代码是对的，我估错了。）

反向（行内清空 → 回落预设），这是 jsdom 给不了的那一层：

| 行内 `--accent` | computed `--accent` | computed `--accent-hover` |
| --- | --- | --- |
| `""` | `#8a5a2b`（paper 预设） | `#6d4218`（浅色档→更暗，方向对） |

「清掉行内就退回预设」从这一刻起有实测，不再只是 `ACCENT_TOKENS ⊆ THEME_TOKENS` 的结构论证。

### 7.6 原生控件的真实外观（本轮唯一只能靠眼睛的部分）

| 项 | 实测 |
| --- | --- |
| `<input type="color">` 空态 | DOM 属性写的是 `""`，`input.value` 读回来是 **`#000000`** |
| 它在 WebView2 里的样子 | 50×27 的圆角胶囊，里面一小块黑色矩形 —— 不是整块填色 |
| 色块 chip | 16×16，`background-color: var(--accent)`，圆角 6px，始终跟着**生效值** |
| 复选框 | 勾上时是 **Windows 蓝**，不跟主题 |

三条结论：
1. 「native 控件的 DOM 值与我们的 state 语义不同构」这条从 jsdom 推出来的判断，在真引擎里逐字兑现 ——
   三态 `customHex` 不是多余的谨慎。
2. 复选框的蓝色是本轮唯一看得见的破绽。一行 `accent-color: var(--accent)` 可修（Chromium 93+ 支持，零 hex）。
3. 取色器胶囊里那块黑、旁边色块显示真值，同屏两个颜色，是「刚取消勾选、还没选过色」这一态的必然观感。
   修它要把**当前生效**的预设色喂进 `defaultValue`，那得在面板里读 computed style（jsdom 测不到它）。
   **本轮没动，留给 T48 人工验收一起判。**

### 7.7 WebView2 的 CDP 缺一个能力（记下来免得下轮再试）

`Page.addScriptToEvaluateOnNewDocument` **返回 `{"identifier":"1"}`、`Page.setBypassCSP` 也回 ok，
但脚本永远不执行**：`Page.reload` 之后上一次留下的 `window.__marker` 没了（确实换了文档），
`window.__probe` 却还是 `undefined`。⇒ document-start 注入这条路在 Tauri 的 WebView2 上不可用。

代价是播种的**归因**测不到：把缓存涂成 `#00a0ff`（磁盘仍是 `#c2410c`）再冷启动，100 ms 的第一次
采样读到的已经是磁盘值 —— `settings_get` 比采样更早落地。所以 §7.4 证的是「第一帧有主题」，
不是「这个值是缓存给的」。缓存本身的行为有 13 条单测加 `tokens.test.ts` 那条 main.tsx 接线门兜着，
这条限制只影响浏览器侧的归因，不影响结论成立。

### 7.8 本轮进仓库的东西、和用户有关的东西

- 唯一新文件：`docs/findings/scripts/cdp-probe.mjs`（零依赖，Node 24 内置 fetch + WebSocket；
  子命令 `eval | arm | trace | shot`）。9222 是本机**无鉴权**端口，随进程生灭，
  **没有**写进 `tauri.conf.json`，也不进任何配置。
- `settings.json`：本轮把 `accent` 从 `""` 改成 `#c2410c` 又改回 `""`，走的是界面本身而不是改文件
  （顺手把「勾回跟随预设并保存」这条路径也在真窗口里跑了一遍）。
- 测量中途用户自己把 theme/mode 从 noir/dark 改成了 paper/light —— **保留用户的选择，未回滚**。
- 当前开着的这个窗口是用 `--remote-debugging-port=9222` 起的；从快捷方式正常启动就没有这个端口。

