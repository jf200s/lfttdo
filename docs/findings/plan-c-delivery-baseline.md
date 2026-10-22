# Plan C 交付基线：安装包体积与三条配置缺口

本轮（T46）只做一件事：**把「安装包 <15MB」这条验收线第一次真的量出来**。
在此之前这个项目里所有出现过的体积数字都是 `target/debug/qwen-do.exe`，
而 debug 产物从来不参与验收（`plan-a-risk-verification.md` 自己在 :38 就写了这句话）。

**测量对象**：`npx tauri build` 的 NSIS 产物文件本身。不是 exe、不是 `dist/`、不是安装后占用。

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

## 3. 冷启动与常驻内存 —— 本轮没有数字

`§9` 的另两条线（冷启动、常驻内存）由 **T47** 填进本文件，测量脚本走
`docs/findings/scripts/cold-start.ps1`。本节故意留空：
上一轮已经吃过一次「把没跑的写成跑过」的账，宁可留一个空节标题。

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

