# LFTTDO

Windows 桌面 AI 助理：聊天、看图、联网抓取、MCP 工具调用，全部运行在本机。
界面是深色玻璃拟态，侧栏可收缩为 56px 窄栏；配置、会话、人设都保存在本机。

> 状态：个人项目，已在本机与 U 盘便携环境中实测可用。采用 **MIT 许可证**（详见文末）。

## 功能

- **聊天**：流式输出、多会话、长对话自动摘要压缩、上下文预算可调。
- **看图**：粘贴 / 拖拽 / 选文件三种入口，统一走同一条图片归一化（缩放、转码、体积闸门）；视觉模型可描述图片。
- **联网抓取**：内置 `fetch_url`，按内容抽取正文；每跳重定向都过 SSRF 判定（拒绝非 http(s)、本机、内网与元数据地址）。
- **MCP 客户端**：stdio 传输，启动后台连接、工具命名走 `mcp__{server}__{tool}`、每台 server 可设信任档（全部允许 / 每次询问 / 禁止），信任设置持久化。
- **工具权限门**：Read 自动放行；Write/Exec 弹窗确认；MCP 工具默认每次询问；未知工具按最严一档处理。
- **人设**：系统提示词可由外部文件提供（便携版 `人设.txt` 启动时注入），应用内不提供该入口。
- **技能包**：`skills/<名字>/SKILL.md` 放一份说明，索引自动进系统提示；模型需要时调用
  `use_skill` 读取正文，用户这句话里带到技能名或 `keywords` 时正文直接自动加载。
  只读本地文本，不执行脚本；加/改技能不用重启（下一条消息生效）。
  侧栏「技能」面板可查看当前技能、逐个启用/停用、刷新并打开技能文件夹。
- **界面**：深色玻璃、居中聊天列、Enter 发送 / Shift+Enter 换行、自动滚底（上翻时不抢）、整窗拖图、思考过程可折叠。

## 技术栈

| 层 | 选型 |
|---|---|
| 桌面壳 | Tauri v2 |
| 内核 | Rust（会话循环、工具注册表、权限门、MCP 桥、SQLite 持久化、图片处理） |
| 前端 | React + TypeScript + Vite |
| 模型 | 任意 OpenAI 兼容端点（推荐本机 Ollama） |
| MCP | `rmcp` 3.5.1（client + stdio 传输） |

## 快速开始

前置：Node.js、Rust（MSVC 工具链）、Windows 10/11。

```bash
npm install
npm run tauri dev          # 开发
npm run tauri build        # 打包（NSIS 安装器）
```

首次打开后进入「设置」，填写：

- 接口地址：`http://127.0.0.1:11434/v1`（本机 Ollama）或任意兼容端点
- 模型名：你已下载的模型
- 云端端点需要填自己的 API Key（存入 Windows 凭据管理器，不写配置文件）

## 便携版（U 盘）

仓库里的 `src-tauri/src/bin/launcher.rs` 是一个无窗口启动器，用于「双击一个 exe、整套一起起停」：

- 读取同目录下的 `人设.txt` 并注入配置；
- 若 `127.0.0.1:11435` 无服务，则以 `OLLAMA_MODELS` 指向便携模型目录启动 Ollama；
- 启动应用并等待退出；用 kill-on-close 的 JobObject 把应用与 Ollama 绑在启动器生命周期上——
  关主窗口、杀启动器或启动器崩溃，整棵进程树都会退出。

构建与摆放（目录需保持相对位置）：

```text
aidox/
  LFTTDO.exe            cargo build --release --bin lfttdo-launcher，部署时改名
  LFTTDO/
    lfttdo.exe          npm run tauri build --no-bundle
    portable.txt        ← 存在则数据写在 exe 旁（便携模式）
    data/settings.json
  人设.txt
  skills/               技能包（与 人设.txt 同层，见下）
    翻译/
      SKILL.md
ollama/                 便携 Ollama
ollama-models/          模型目录
```

## 技能包（skills）

每个技能是一个文件夹，里面放一份 `SKILL.md`（UTF-8）。开头可选 frontmatter：

```markdown
---
name: 翻译助手
description: 中英互译，保留术语表
---
先判断原文语言，再翻译；术语表里的词必须按表译。
```

- `name` / `description` 缺省时依次取第一个一级标题、文件夹名、正文第一句。
- 技能名和文件夹名都能作为 `use_skill` 的参数（ASCII 大小写不敏感）。
- 系统提示里只放「名字 + 一句话描述」；正文（单份上限 64 KiB）在模型调用
  `use_skill` 时才读入上下文。
- 每个技能可在侧栏「技能」里单独启用/停用；新技能默认启用。停用的技能既不出现在
  系统提示里，`use_skill` 也会拒绝。选择表 `skills.json` 放在数据目录（与
  `mcp.json` 同层），不写回技能文件夹。
- **本地小模型的触发稳定性**：`keywords`（触发词）命中时，正文由软件直接放进本轮系统提示，
  不赌模型自己想起来调工具；单轮最多自动加载 2 份、每份 16 KiB，超出部分提示模型自己去读。
- **「工作方式」段**（`context.rs::TOOL_POLICY`）固定追加在人设之后：先调工具拿依据再回答、
  技能先读正文、MCP 工具与内置工具同等对待。人设是整段替换内置提示词的，这段规矩不能被
  人设一起抹掉 —— 这是本地模型「有工具却凭记忆答」的首要原因。
- 面板里的「用这个技能」会把 `使用「技能名」技能：` 填进输入框并聚焦，不替用户发送。
- 便携版目录在 `<U 盘根>/skills`，常规安装在数据目录的 `skills/`。目录不存在时
  `use_skill` 不会出现在模型工具表里。
- 扫描时每个 `SKILL.md` 都会 canonicalize 并校验仍在技能根内，指向根外的
  符号链接 / junction 会被跳过；模型传入的名字从不参与拼接路径。

> 注意：launcher 的 bin 名不能叫 `LFTTDO`——Windows 文件名不区分大小写，会与 `lfttdo.exe` 撞成同一个文件。

## 测试

```bash
cd src-tauri && cargo test --all-targets
npx vitest run
npx tsc --noEmit
npm run lint:css
```

当前读数：Rust 498 passed / 1 ignored + 集成 3 passed；前端 197 passed；clippy 无新增。

## 目录结构

```text
src/                前端（组件、IPC 封装、状态 reducer、主题 token）
src-tauri/src/
  agent/            会话循环、上下文组装、摘要压缩、事件类型
  model/            OpenAI 兼容适配器（含 reasoning 流）
  tools/            内置工具与注册表（fetch_url、read_file）
  permission/       权限门与信任档
  mcp/              mcp.json 读取、工具命名、stdio 桥、进程树回收
  store/            SQLite 会话持久化
  image_prep.rs     图片归一化
docs/               设计、计划与验证记录
```

## 已知限制

- **仅 Windows x64**：进程树回收与凭据存储都用了 Win32。
- **HTTP 类型 MCP 未启用**：会明确提示「此构建未启用 HTTP 传输」。
- **常驻内存约 400 MB**：大头是 WebView2 的多进程运行时，不是泄漏。
- 内置联网抓取只抓公开网页，需要登录的页面抓不到。
- 思考过程的展示依赖模型把 reasoning 放进增量的 `reasoning_content`（或正文内联 `<think>`）。

## 安全说明

- API Key 只写 Windows 凭据管理器；不写 `settings.json`、不进日志、不出现在导出里。
- 工具参数逐段传 argv，不拼 shell 命令行；MCP 子进程设置 `CREATE_NO_WINDOW`。
- SSRF 判定在每一跳重定向上执行；内网与本机地址默认拒绝。
- 前端无 shell / 文件系统权限，进程执行全部在 Rust 侧。

## 许可

本项目采用 **MIT 许可证**（见根目录 `LICENSE`）。MIT 为宽松型开源许可证：允许复制、
修改、分发与商用，仅需在衍生作品中保留版权声明与许可证全文。
