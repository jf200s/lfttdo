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
