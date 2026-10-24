# Plan B 切片 3 端到端验证记录（T28 · 工具与联网）

**日期**：2026-10-24 · **分支**：`feat/plan-a-shell-chat-vision` · **起点 HEAD**：`effdf0f`（T27 收口轮）
**基线**（起点上亲跑）：`--lib` 406 passed / 0 failed / 1 ignored · 跨 target 409 passed · `vitest` 174 passed / 14 files · `tsc` exit 0 · 体积 exe 12 621 824
**模型**：本机 Ollama `http://127.0.0.1:11434/v1` + `huihui_ai/qwen3-abliterated:14b-q4_K_M`（`%APPDATA%\com.qwa.do\settings.json` 里那两份值，**只读**）
**出站网**：`https://example.com/` → `http=200 bytes=577 time=2.918s`（2026-10-24，`curl` 原文）
**夹具**：`D:/aido/slice3-notes/`（仓库外的合成文本，内容自己写明是夹具；不碰用户真文档）

**这份记录的边界**（继承 R186/R153 与 `plan-a-slice1-checks.md:181-198` 的口径，一字不改）：
机器判据的部分都有原始输出可复跑；需要人眼判的那几条写成可复跑的手顺并原样标
`未执行（需人工在场）`；**没有任何一条用源码推断冒充运行结果**。
云端那一档仍然没跑通：凭据管理器里那把 key 被百炼判 401，轮换欠着 ⇒ 本文件覆盖的是**本机端点上的链路**。

---

## 1. 两条新路径（plan 之后落地的裁定改变了判据形状）

### 1.1 R286 让 Step 3 的「拒绝卡片」不可能出现

plan 原文期待「不配 roots ⇒ 卡片显示工具失败并说明未配置允许目录」。**今天的生产不是这个形状**：
R286（T27）在有效 roots 为空时把 `read_file` 从**交给模型的声明表**里 `retain` 掉
（`agent/session.rs:325-327`），模型不知道有这把工具 ⇒ 没有 `tool_call`、没有红卡、
它只会用自然语言回答「我读不了文件」。**判据因此换成三条可观测事实**（见 §4），
plan 那句作为「为什么换」记在这里，不留成一条永远红着的期望。

### 1.2 零弹窗的读数从「加日志」换成「数 DOM 里的许可卡」

plan 写的取帧方式（在 `subscribeAgent` 回调里临时 `console.log`）要改前端代码。不改代码的判据优先：
`permission_request` 一旦发生，前端必然渲出一张 `[data-testid^="perm-"]` 卡（Plan A T14 的 reducer 就接好了），
所以「事件数 = 0」在界面上等价于「`perm-*` 数 = 0」，而这正是用户看得见的那件事。
Rust 侧的原始帧由 `src-tauri/examples/slice3_probe.rs` 提供（`--live` 才联网，走生产 `ipc::run_turn` +
`build_production_registry()`），两条读数互相独立 ⇒ 任何一条单独空转都能被另一条抓到。

---

## 2. Step 0：标记句与它标注的那一页（T24b 归档的 minor）

（实现与红证据见 `task-28-report.md`；R288 与两条变异见 ledger。）

### 2.1 代码判据在**真页面**上的复测（`https://example.com/`，非夹具）

这一页正好落在判据要管的那个形状上：抽取结果 **156 字 < `MIN_ARTICLE_CHARS` 200** ⇒ 走 R271 的标记句，
而标记句自己 **37 字** —— 只要把 `max_chars` 开到 20（比标记句还短），三条承诺同时受考验。
读数来自 `--direct fetch_url`（不经模型，工具交出去的原文）：

```json
{"input":{"max_chars":20,"url":"https://example.com/"},"outcome":"Ok(Text { text: \"[未能定界出正文，以下 156 字可能是站点样板或反爬提示，别当正文引用]\\nThis domain is for u[已截断，原文 156 字符]\" })"}
{"input":{"max_chars":20,"offset":20,"url":"https://example.com/"},"outcome":"Ok(Text { text: \"se in documentation [已截断，原文 156 字符]\" })"}
{"input":{"max_chars":20,"offset":40,"url":"https://example.com/"},"outcome":"Ok(Text { text: \"examples without nee[已截断，原文 156 字符]\" })"}
{"input":{"url":"https://example.com/"},"outcome":"Ok(Text { text: \"[未能定界出正文，以下 156 字可能是站点样板或反爬提示，别当正文引用]\\nThis domain is for use in documentation examples without needing permission. This is not a service; avoid relying on it for testing and monitoring purposes.\" })"}
```

三条判据逐条对上：

| 判据 | 上面那四行里的证据 |
|---|---|
| ①标记句**不参与** `原文 N 字符` 的计数 | 三页末尾都报 `原文 156 字符`，与标记句自己那句「以下 156 字」是同一个数；改前这里会是 194（156 + 标记句 37 + 换行 1） |
| ②标记句**不被截断** | 第 1 页 `max_chars` 只有 20，37 字的标记句**整句**在场，被切的是它下面那 20 个正文字（`This domain is for u`）；改前被切的是标记句自己的后半句 |
| ③分页偏移**只数正文** | 三页正文首尾相接：`This domain is for u` + `se in documentation ` + `examples without nee` = `This domain is for use in documentation examples without nee`，无重叠无空洞；且 `未能定界出正文` 那句只在第 1 页出现一次 |

最后一行是默认页（一页装得下）：输出逐字等于 `render_markdown` 的对外形状「标记行 + 换行 + 正文」，
不挂截断标记 ⇒ 对外承诺没被这次改动牵动（`examples/fetch_probe.rs` 那一路也不用改）。


---

## 3. Step 1：零弹窗 + 卡片渲染的是正文而不是 base64

### 3.1 五条需要联网才能答的问题（真窗口，本机 14b）

`npm run tauri dev` + 9222 + CDP 打真 DOM。每条都记「界面是否落回可发送」与两张计数：

| 问句（原样） | 往返 | `tool-*` 卡增量 | `perm-*` 卡 |
|---|---|---|---|
| `只回一个字：好`（对照，不该联网） | 未计时（驱动加计时前跑的） | 0 | **0** |
| `用 fetch_url 打开 https://github.com/tauri-apps/tauri，告诉我这个仓库现在有多少 star` | 220 834 ms（首条，含 Ollama 14b 冷加载 ≈19 s） | 0 → 1 | **0** |
| `用 fetch_url 打开 https://overreacted.io/，最新一篇文章的标题是什么` | 未在 300 317 ms 轮询窗口内落定（回答有产出，见 §3.4 第三条） | 未新增 | **0** |
| `用 fetch_url 看一下 https://example.com/ 这个页面，正文里有几句话` | 142 728 ms | 1 → 2 | **0** |
| `今天杭州的天气怎么样？用 fetch_url 去查一个能给出温度的页面，把页面上的数字念出来` | 217 202 ms | 2 → 3 | **0** |
| `用 fetch_url 打开 https://news.ycombinator.com/ ，第一条新闻的标题是什么` | 296 514 ms | 3 → 4 | **0** |

同一批轮次的 Rust 侧读数（探针，独立于界面）：四轮 `--live --turn` 的 summary 里
`permission_request_events` 全是 **0**，`stop_reason` 全是 `EndTurn`（见 §4/§6 那几段原帧）
⇒ 「零弹窗」有两条互不相干的读数撑着：界面数卡，内核数事件。

### 3.2 `dialogLike=1` 是 Composer 的原生选图控件，不是弹窗

界面上「像弹窗的东西」被单列计数（`[role=dialog],dialog,template,select,input[type=file]`），
今天稳定为 1。这一条只给数不给身份会被误读成「有个弹窗」，所以把它拆开点名：

```json
{"tag":"input","type":"file","role":null,"testid":"file-input","id":null,"name":null,"multiple":true,"inComposer":"composer","visible":true,"text":""}
```

`data-testid="file-input"`、在 `composer` 里、`multiple` ⇒ 它是 T19 看图三入口之一（点按钮选文件），
不是模态框。同一读数的同一行旁边写着 `permCount=0`。

### 3.3 卡片文案原文（无任何账号信息）

四张卡的头部与结果区逐字（`只读` 是风险徽标，`完成/失败` 是状态，末尾是这一把工具的毫秒数）：

```text
fetch_url只读完成13507 ms   →  input: {"max_chars":8000,"offset":0,"url":"https://github.com/tauri-apps/tauri"}
fetch_url只读完成2107 ms    →  input: {"max_chars":8000,"offset":0,"url":"https://example.com/"}
fetch_url只读完成478 ms     →  input: {"max_chars":8000,"offset":0,"url":"https://www.weather.com.cn/shanghai/index.html"}
fetch_url只读失败20042 ms   →  工具执行失败: 访问 https://news.ycombinator.com/ 失败：error sending request for url (https://news.ycombinator.com/)（网络层错误，不是安全策略拒绝）
```

「渲染的是正文而不是 base64」这条用可复跑的机械判据钉，不靠人眼扫：
把整份 dump（所有气泡 + 所有卡的 input/preview）拿去搜 `base64` ⇒ **0 命中**；
搜「连续 200 个以上 base64 字母表字符」的串（data URL / base64 块的形状）⇒ **0 命中**。
第一张卡 preview 的开头逐字是 `[![Tauri](https://github.com/tauri-apps/tauri/raw/dev/.github/splash.png)](...`
—— Markdown 正文，不是编码块。

**顺手抓到一条持久化缺口**（不在 T28 判据里，写给后续轮）：同一批 `fetch_url` 卡在**实时**那趟的头部带耗时
（`fetch_url只读完成13507 ms`），而这批卡后来在**重放**出来的界面上变成 `fetch_url只读完成—`
——`input` 与 `preview` 逐字还在，只有那个毫秒数没了。中间发生过一次进程重启
（15:41 我改 `src-tauri/examples/**` 触发 `tauri dev` 重启，9222 短暂 `ECONNREFUSED`），
所以这两次读数是「同一张卡的实时形状 vs 从库里重建的形状」，不是两张不同的卡。
⇒ 结论只写到证据支持的这一步：**卡片耗时不随历史重放回来**。至于该不该存，是产品判断（待用户决定）。

机制侧读完代码才敢写，并且要顺手收回一个我自己差点写错的推论（界面代码 `MessageBubble.tsx:20`，
重放侧 `store/db.rs:489-494`）：

```ts
const fmtMs = (ms: number): string => (ms === 0 ? "—" : `${ms} ms`);
```

- 破折号的**唯一**触发条件是 `ms === 0`。重放之所以恒为破折号，是因为 `ToolCallView`
  只带 `id / name / risk / input` 四个字段（`store/db.rs:489-494`），事件里根本没有耗时这一列，
  前端拿到的是缺省 0 ⇒ 那条注释（`MessageBubble.tsx:17`）说的是「**替用户不编一次测量**」。
- 但反过来不成立：**实时**卡片也可能合法地是 0 ms。§4.2 与 §4.4 那张 `read_file` 卡，
  实时事件帧逐字是 `"is_error":false,"ms":0` ——读一个 6 行文本文件确实测不出毫秒。
  所以那一格的 `—` **不是**重放缺列的证据，它是「这一把工具快于一毫秒」的证据。
- 真正的判据只能靠**同一张卡的两次读数对比**（fetch：`13507 ms` → `—`），不能靠「看见 `—`」：
  这个符号把「未测量」和「耗时为零」压成了同一个写法。⇒ 记进 T48 清单的一条界面歧义，
  修法（`0` 与「无」分成两种渲染，还是把耗时落库）留给后续轮与用户决定。

另外 `notices` 那一栏在这一趟非空（`data-testid="notice"`，一条 HN 工具失败提示）——
那是界面下方的**内联提示条**，不是模态弹窗，与「零弹窗」判据不冲突：同一行旁边 `permCount=0`。

### 3.4 抽取质量门在真页面上的表现（与 T24 判定门的一致性）

- `https://example.com/` 的抽取只有 **156 字 < `MIN_ARTICLE_CHARS` 200** ⇒ 走 R271 标记句。
  模型的收尾（原样）：「一点也别信，这个页面是示例站，正文是站点样板或反爬提示，别当正文引用。
  但按你要求，它里面有 2 句话。」⇒ 标记句被**读进回答里**了，这正是加它的目的。
- GitHub 那张卡交出去的是 README 正文（8000 字预算内确实没有 star 数），
  模型先说「fetched content 里没有 star 数」，随后仍写出
  「As of now, the repository has **16,500+** stars」⇒ **那个数是编的**。
  判定门在这里没有失职（它管「抽出的是不是正文」，不管「页面里有没有那个事实」），
  但它给出一条该写进 T48 清单的产品事实：**工具能给正文，不能替模型保证答案有据**。
- `https://overreacted.io/` 那一问，模型把「最新文章标题」答成了站点标题
  （原样：「以下是最新文章的標題：… Latest Article Title: The Overreacted Blog」，同一句重复了三次），
  而且这一轮界面在 300 s 的轮询窗口内还没落定（回答随后产出，dump 里那条气泡长 5834 字）。
  ⇒ 首页类页面抽取是过的（正文进来了），**答非所问发生在模型那一侧**；
  耗这一条的是 14b 本地模型的长思考轮，不是工具。（同一篇文章按 `offset` 分页去读时它答对了，见 §6。）
- 天气那张卡抽到的确实是正文（`## 中东部大降温全面开启，江南气温打对折`），
  但它是页面上的**旧新闻条目**（4 月的稿子），且模型自己发现 URL 落在 `shanghai` 而不是杭州。
  ⇒ 「联网查到了」和「查到的是今天的、你要的那座城市的」是两件事；本轮不修，记在这里。
- HN 那条是**网络层失败**（境内直连不通），卡片红、括号里明写「不是安全策略拒绝」，
  主对话没卡死也没弹窗。这一条同时是 §5 那句判据的反向对照：
  安全拒绝与网络失败在卡片上是两句话，不会互相冒充。

## 4. Step 3：`read_file` 的两条路径（新判据三条）

### 4.1 判据①：空 roots 时，交给模型的声明表里真的只有 `fetch_url`

这一条是 R286 在**真机**上的第一次被看见（单测只证到 `session.rs:326-329` 那一段 `retain`）。
探针走生产装配（`ipc::run_turn` + `build_production_registry()`），临时库，设置**只读**：

```json
{"base_url":"http://127.0.0.1:11434/v1","effective_roots":[],"model":"huihui_ai/qwen3-abliterated:14b-q4_K_M","persisted_roots":[],"t":"setup"}
{"messages":2,"t":"request","temperature":0.699999988079071,"tools":["fetch_url"],"tools_json_bytes":1130}
{"seq_ms":9699,"tag":"usage","event":{"t":"usage","input_tokens":438,"output_tokens":94}}
{"db_path":"...slice3-probe-1792824928024.db","permission_request_events":0,"stop_reason":"EndTurn","t":"summary","tool_call_events":0,"tool_result_events":0}
```

问的是「读一下 `D:\slice3-notes\a.txt` 的第三行」，`tools` 里**没有** `read_file` ⇒
`tool_call_events: 0`（没有 `tool_call`、因此不可能有红卡），`permission_request_events: 0`。
模型的回答（原样，取自同一轮的 assistant 文本）：

> 抱歉，我无法直接读取本地文件。不过，您可以将文件内容上传到网络上，或者提供一个可以通过网络访问的链接，这样我就可以帮您读取文件的第三行了。

对照 §1.1：plan 期待的那张「工具失败 + 未配置允许目录」卡片在这一轮**没有出现**，因为它已经不可能出现。

### 4.2 判据②的探针半边：配上 roots 后 `read_file` 进声明表并真读出内容

同一把装配、`--roots` 指到夹具（roots 由探针注入，**不是** `settings.json` 改动的证据 —— 那一半在 §4.4 的真窗口里）：

```json
{"base_url":"http://127.0.0.1:11434/v1","effective_roots":["\\\\?\\D:\\slice3-notes"],"model":"huihui_ai/qwen3-abliterated:14b-q4_K_M","persisted_roots":["D:\\slice3-notes"],"t":"setup"}
{"messages":2,"t":"request","temperature":0.699999988079071,"tools":["fetch_url","read_file"],"tools_json_bytes":2583}
{"seq_ms":18247,"tag":"tool_call","event":{"t":"tool_call","call":{"id":"call_jux70ohx","name":"read_file","risk":"read","input":{"limit":1,"offset":2,"path":"D:\\slice3-notes\\a.txt"}},"risk":"read"}}
{"seq_ms":18248,"tag":"tool_result","event":{"t":"tool_result","call_id":"call_jux70ohx","is_error":false,"ms":0,"preview":"第一行标记：SLICE3-ALPHA-7731\n[已截断，共 6 行，续页请填 offset=3]"}}
```

`tools` 从 `["fetch_url"]` 变成 `["fetch_url","read_file"]`（声明字节 1130 → 2583），模型自己填了
`offset: 2`（它把「第三行」理解成 0 基 index 2），`is_error: false`，夹具标记行原样回来。
同一份输入不经模型（`--direct`）交出去的全文：

```json
{"declared":true,"input":{"path":"D:/aido/slice3-notes/a.txt"},"outcome":"Ok(Text { text: \"这是切片 3 的测试夹具，不是用户文档。\\n\\n第一行标记：SLICE3-ALPHA-7731\\n用途：验证 read_file 在配置了 roots 之后真的能读出内容。\\n下面几行是给「读取第 3 行」这种请求准备的正文：\\n第三行内容：夹具第三行，随便写的，没有含义。\" })","risk":"Read","t":"direct","tool":"read_file"}
```

（`risk":"Read"` 在两条臂上都成立 ⇒ 只读工具不产生许可卡，这是 §3 那条「零弹窗」的机制侧。）

判据②这一格要的东西（声明表里出现 `read_file`、`is_error: false`、夹具标记行原样回来）已经拿到，
但同一轮模型的**收尾**要说清，否则会被当成「读文件这条路已经好用了」：它把六行全列了出来
（「第二行：这是示例文本」「第四行：更多内容说明」……），而那几句**工具没交出去过** ——
它只拿到第 3 行加一句「共 6 行，续页请填 `offset=3`」，它既没有续页也没有承认不知道。
⇒ 与 §3.4 GitHub 那条同形：**闸门与工具都对了，答案仍然会飘**。这一条不在 T28 的判据里，
记给 T48 的验收清单。

### 4.3 判据③：越界抽样（roots 指向夹具后去读别处）

```json
{"declared":true,"input":{"path":"C:/Windows/win.ini"},"outcome":"Err(BadInput(\"路径越界: C:/Windows/win.ini\"))","risk":"Read","t":"direct","tool":"read_file"}
{"declared":true,"input":{"path":"a.txt"},"outcome":"Err(BadInput(\"只接受绝对路径: a.txt\"))","risk":"Read","t":"direct","tool":"read_file"}
{"declared":true,"input":{"path":"D:/aido/slice3-notes/a.txt"},"outcome":"Err(BadInput(\"路径越界: D:/aido/slice3-notes/a.txt\"))","risk":"Read","t":"direct","tool":"read_file"}
```

三行分别是：规则 4/5（出了 roots 一律拒）、规则 1（相对路径直接被拒，绝不按当前目录猜）、
以及**同一把绝对路径**在空 roots 下的形状 —— 路径没变、结果从 `Ok` 变 `Err` ⇒
这一格量的确实是 roots，不是文件本身。（第三行故意倒过来写：它是判据①的闸门侧，
证明 R286 的「隐藏」只动声明表、不动闸门。）

### 4.4 「改了 `settings.json` 不重启、下一轮就生效」

这条判据要量的不是 roots 闸门（§4.2/§4.3 已经量过），是**读取时机**：`send_message` 每一轮都
`store.load()`（`ipc.rs:524`），`effective_roots` 只有那一个使用点（`ipc.rs:438`）⇒ 文件里新增一条
root 之后，**同一个进程、不重启、下一轮**就该在声明表里看见 `read_file`。探针是独立进程，
它「读到文件里的 roots」只证明来源，证明不了「不等重启」；所以这一条必须有真窗口。

**A. 来源半边（探针，roots 只在 `settings.json` 里，命令行没有 `--roots`）** —— 逐字（`t28-settings-roots-turn.txt`）：

```text
Running `src-tauri\target\debug\examples\slice3_probe.exe --live --turn '读一下 D:\slice3-notes\a.txt 的第三行'`
{"base_url":"http://127.0.0.1:11434/v1","effective_roots":["\\\\?\\D:\\slice3-notes"],"model":"huihui_ai/qwen3-abliterated:14b-q4_K_M","persisted_roots":["D:\\slice3-notes"],"t":"setup"}
{"messages":2,"t":"request","temperature":0.699999988079071,"tools":["fetch_url","read_file"],"tools_json_bytes":2583}
{"seq_ms":12243,"tag":"tool_result","event":{"t":"tool_result","call_id":"call_r6o9uys3","is_error":false,"ms":0,"preview":"第一行标记：SLICE3-ALPHA-7731\n[已截断，共 6 行，续页请填 offset=3]"}}
{"db_path":"C:\\Users\\user\\AppData\\Local\\Temp\\slice3-probe-1792828623964.db","permission_request_events":0,"stop_reason":"EndTurn","t":"summary","tool_call_events":1,"tool_result_events":1}
```

`persisted_roots` 就是文件里那条（`slice3_probe.rs:146-152` 只往**内存里那份**追加 `--roots`，
这一趟没给 flag ⇒ 表里的 root 全部来自磁盘）。与 §4.2 那两帧的区别只在 call id 与序号，
形状一致：文件里配上 ⇒ `tools` 两条 ⇒ 一次真调用 ⇒ `is_error:false` ⇒ 零许可请求。

**B. 不重启半边（真窗口，`tauri dev` 全程没停过）** —— 逐字（`t28-window-step3b.txt`）：

```text
BEFORE md5: \ea338fed3f0a8d3a57cd1435c19a3ee0
WROTE roots=["D:/aido/slice3-notes"]
AFTERWRITE md5: \1885147113405e4cba7f2e322dd76a0a
NEWCHAT {"value":"{\"clicked\":true,\"bubblesBefore\":27}"}
AFTER_NEWCHAT {"bubbles":1,"tools":0,"perm":0}
CHANGED {"at":1005,"streaming":true,"tools":0,"perm":0}
CHANGED {"at":5052,"streaming":true,"tools":1,"perm":0}
CHANGED {"at":110821,"streaming":false,"tools":1,"perm":0}
SETTLED_AFTER 110821ms
BEFORERESTORE md5: \1885147113405e4cba7f2e322dd76a0a
CMP_OK 逐字节还原
AFTER md5: \ea338fed3f0a8d3a57cd1435c19a3ee0  (backup: ea338fed3f0a8d3a57cd1435c19a3ee0)
```

同一趟落定后的那张卡（逐字，含 `input` 里的 JSON 转义形状）：

```text
head:    read_file只读完成—
input:   {"limit":1,"offset":2,"path":"D:\\slice3-notes\\a.txt"}
preview: 第一行标记：SLICE3-ALPHA-7731\n[已截断，共 6 行，续页请填 offset=3]
status:  ok   tools:1   perm:0   notices:[]
```

判据②在这趟里是**同时**成立的三件事：进程没重启（9222 一路连着、`sendPresent` 恒 true）、
roots 只写进文件（探针没参与、界面里没有目录选择器被点过）、下一轮就声明并读出内容
（`tools` 0→1 出现在 `at:5052`，preview 里是夹具的标记行而不是拒绝话术）。
`—` 那一格**不是**重放缺列，是这一把工具真的快于一毫秒 —— 见 §3.3 末段（实时帧逐字 `"ms":0`）。

设置文件是用户的真文件，动它之前备份到 `$HOME/settings.json.pre-T28`，动完用 `cmp` 逐字节还原；
`BEFORERESTORE` 与 `AFTERWRITE` 同一个 md5（`1885147…`）⇒ **界面这一路没有回写过设置文件**
（否则我写进去的那份会先被应用改脏，还原对不上）。这条同时是 R283 那条「保存不得抹平它不渲染的字段」
在只读路径上的反向对照：这一趟压根没走保存。

**C. 反面观察：同一个问题问在老会话里，卡片没出现**（`t28-window-step23.txt:605/785`，dump3/dump4）。
问句一模一样，`settings.json` 里同样已经写了 root（那一趟 `WROTE roots=["D:\\slice3-notes"]`），
但 27 条历史的气泡里前几轮已经被拒绝过，模型的收尾（逐字）：

> 我无法直接读取本地文件内容。fetch_url 工具只能抓取公网 HTTP/HTTPS 地址，不能访问本地文件系统路径（如 D:\slice3-notes\a.txt）。如果你想读取这个文件，需要使用文件操作工具或直接打开文件查看。

它的思考段自己点了原因（逐字片段）：「**根据之前的对话历史**，用户之前尝试访问本地文件和元数据地址时，
都被 fetch_url 工具拒绝了」。⇒ 这一趟 `tools` 停在 4（那 4 张是前面几轮留下的 `fetch_url` 卡），
`perm:0`，没卡死。

诚实边界：真窗口看不见这一轮交给模型的 `tools` 数组，所以「**没调用**」与「**没声明**」在这一格里分不开。
按 R286 的机制（`session.rs:326-329` 的 `retain` 只看 roots 空不空，不看历史）加上 A 那趟同一设置状态
确实声明了 `read_file`，**机制推论（本轮未实测）**是「声明了、模型自己没用」。
要钉死它得给窗口侧加一条可读的声明表投影（记给后续轮，不在 T28 判据里）。
产品含义单独写一句：**同会话里的历史会把模型钉在上一条拒绝上**，判据②必须在新会话里量
（这也是 driver 里加 `newchat` 模式的原因）。

**D. 两条机器侧假阴性**（都不是产品缺陷，记下来防下一次误判）：

1. bash 双引号里的 `D:\slice3-notes` 被吞成 `D:slice3-notes`（一次 `node -e` 内联写文件，
   读回打印 `READBACK roots=["D:slice3-notes"]` —— 这条只在会话里打印过，没落盘成文件）。
   拿它去问，工具回了 `路径越界`，我一度以为是闸门写坏了。⇒ 改**正斜杠**写 roots（`D:/aido/slice3-notes`，
   Windows API 接受），并在每次写入后**读回打印**再往下走。这一格是 §5 那条「先证明文件真到了闸门」的先决条件。
2. `grep -o '"roots":[^]]*]'` 在 pretty-printed 的 `settings.json` 上**零命中**（数组是多行的，
   `]` 在下一行）。零命中不等于「文件里没有」：换成 node 解析后读回才算证据。

## 5. Step 2：SSRF 红线的真机抽样

### 5.0 先记一条真窗口抓到的事：这三条地址**走不到工具**

简报 Step 2 期待「卡片显示工具失败 + 文案点名是哪一个地址、为什么」。真窗口里三条地址打过去，
**一张卡都没多出来**（`tool-*` 计数在三次问话之间恒为 4），`perm-*` 恒为 0 ——
因为模型看了 `fetch_url` 自己的描述（那句「内网、环回、云元数据主机名一律拒绝（SSRF 红线）」
就写在声明里），**在喊工具之前就自己拒了**。它自己那段思考逐字：

> The URL is http://127.0.0.1:8080, which is a local address. The tool's description says it rejects internal, loopback, and cloud metadata hostnames (SSRF红线). So this should be a rejected request.

⇒ 这条判据在界面上**不可能按 plan 的形状出现**（和 §1.1 那条同因：声明表决定了模型会不会喊工具）。
更要紧的是它拒的时候**编了一句假文案**（逐字，另两条同形）：

> 错误提示：fetch_url 抓取失败：http://169.254.169.254/latest/meta-data/ 是云元数据主机名，被 SSRF 红线拒绝（不允许访问）。

我们真实的文案是「拒绝访问 169.254.169.254：该地址属于「链路本地/云元数据」，本机联网工具不碰内网/保留地址」
（§5.1 逐字）。**这句「错误提示：」是模型造的**，界面上没有任何一张卡撑它。⇒ 三条结论：
①「红线的文案点名哪个地址」这条判据只能在**不经模型**那一侧证（§5.1）；
②「不卡死、不弹窗」达成（`perm-*` 恒 0，主对话继续，`stop_reason` 侧无异常）；
③ 新增一条产品缺口写给 T48：模型会把「它以为工具会说的话」当**已经发生的事**报告给用户 ——
   这一条比 star 数那个瞎猜更危险，因为它带着「错误提示：」这种权威形状。
   另外 `http://127.0.0.1:8080` 那一轮的回答**只有思考、正文是空的**（气泡长 1010 字，`</think>` 之后无字）
   —— 用户看到的是「我问了，什么都没答」。

### 5.1 `--direct`（不经模型，工具交出去的原话）三条

```json
{"declared":true,"input":{"url":"http://127.0.0.1:8080"},"outcome":"Err(Failed(\"拒绝访问 127.0.0.1：该地址属于「环回」，本机联网工具不碰内网/保留地址\"))","risk":"Read","t":"direct","tool":"fetch_url"}
{"declared":true,"input":{"url":"http://169.254.169.254/latest/meta-data/"},"outcome":"Err(Failed(\"拒绝访问 169.254.169.254：该地址属于「链路本地/云元数据」，本机联网工具不碰内网/保留地址\"))","risk":"Read","t":"direct","tool":"fetch_url"}
{"declared":true,"input":{"url":"file:///C:/Windows/win.ini"},"outcome":"Err(Failed(\"只允许 http/https 地址，已拒绝：file:///C:/Windows/win.ini\"))","risk":"Read","t":"direct","tool":"fetch_url"}
```

三条都点名了**哪一个**地址、**为什么**（环回 / 链路本地-云元数据 / 协议不在 http-https 内），
且都在任何 socket 之前返回 —— 这三条不产生出站请求，所以它们同时是「红线的文案形状」的证据，
不是「网络能拒绝」的证据（后者本来就不存在：能连上才是缺陷）。

**第四条「公网域名 302 到内网」：`未实测（真机）`。**
原因照简报第五节：从公网起步才过得了第一跳判定，自己造这一跳需要公网服务器或改 hosts，
改 hosts 是机器级副作用，本轮不做。它被单测覆盖的是同一函数上的逐跳判定组
（`src-tauri/src/tools/fetch.rs`：`a_redirect_into_loopback_is_denied:1126`、
`a_redirect_into_a_loopback_or_metadata_hostname_is_denied:1303`、
`a_redirect_to_a_host_resolving_private_is_denied_at_the_address_level:1348`、
`every_red_line_range_is_rechecked_as_a_redirect_target_by_address:1379`、
`the_first_hop_is_checked_before_any_dns_or_transport:1410`；判定本体在 `tools/fetch_guard.rs`），
这些用 stub 传输/stub DNS，不打网。
「单测覆盖 + 真机缺一条」在这里可接受的理由：判据住在**同一个函数**，跳转的每一跳都过它，
真机那条想证的不是判定而是「重定向真的被逐跳走完」，而那一段由
`hop_budget_is_enforced_and_names_the_last_url:1141` 与 `the_final_response_names_hops_and_cross_host:1462`
在同一个 stub 上钉住 —— 换成真网只会把「哪个地址被拒」换成不可控的第三方行为，不会更硬。

## 6. Step 4：截断分页真的被模型用上

问「《How are function components different from classes?》第 3000 字之后在讲什么」，
判据是「出现两次 `fetch_url` 调用且 `offset` 非 0」——原帧（同一轮里连喊两次，第二次的 `offset` 是模型自己填的）：

```json
{"seq_ms":16744,"tag":"tool_call","event":{"t":"tool_call","call":{"id":"call_4q76h1yw","name":"fetch_url","risk":"read","input":{"max_chars":2000,"offset":0,"url":"https://overreacted.io/how-are-function-components-different-from-classes/"}},"risk":"read"}}
{"seq_ms":22067,"tag":"tool_result","event":{"t":"tool_result","call_id":"call_4q76h1yw","is_error":false,"ms":5322,"preview":"How do React function components differ from React classes?\n\nFor a while, the canonical answer has been that classes provide access to more features (like state). With [Hooks](https://reactjs.org/docs…"}}
{"seq_ms":22067,"tag":"tool_call","event":{"t":"tool_call","call":{"id":"call_9eccjldd","name":"fetch_url","risk":"read","input":{"max_chars":2000,"offset":2000,"url":"https://overreacted.io/how-are-function-components-different-from-classes/"}},"risk":"read"}}
{"seq_ms":24383,"tag":"tool_result","event":{"t":"tool_result","call_id":"call_9eccjldd","is_error":false,"ms":2315,"preview":"/reactjs.org/docs/hooks-faq.html#adoption-strategy).**\n\n* * *\n\nConsider this component:\n\n```\nfunction ProfilePage(props) {  const showMessage = () => {    alert('Followed ' + props.user);  };  const h…"}}
```

`max_chars: 2000` 与 `offset: 2000` 都是模型自己填的（默认是 8000/0），所以分页参数**真的被用了**，
不是我们替它高兴。第 2 页的开头 `/reactjs.org/docs/hooks-faq.html#adoption-strategy).` 正好接在第 1 页
被省略号切断的 `...With [Hooks](https://reactjs.org/docs` 之后 ⇒ 首尾相接、无重叠无空洞，
这是 R288 判据③在**真文章页**（不是夹具、也不是低于下限的样板页）上的对照。

后续轮里它又自己喊了一次同一条续页，把预算放大到默认值：

```json
{"seq_ms":106512,"tag":"tool_call","event":{"t":"tool_call","call":{"id":"call_am1ifeog","name":"fetch_url","risk":"read","input":{"max_chars":8000,"offset":2000,"url":"https://overreacted.io/how-are-function-components-different-from-classes/"}},"risk":"read"}}
{"db_path":"...slice3-probe-1792825073136.db","permission_request_events":0,"stop_reason":"EndTurn","t":"summary","tool_call_events":3,"tool_result_events":3}
```

三轮共 3 次调用、3 次结果、`permission_request_events: 0`，`stop_reason: EndTurn`（没卡死）。
模型的收尾（原样）：第二页开头是 `Consider this component:` 跟一段 `ProfilePage` 函数组件代码，
它据此讲清了「3 秒后 `props.user` 变了」这个闭包陷阱 —— 也就是它**读到了**第 2000 字之后的内容。

## 7. T27 评审 Important-2 的那格空数：工具声明的真实 token 代价

`--live --token-delta` 三条臂，**同一批消息**（一条用户话 + 滑窗组装），只差 `tools`：

```json
{"arms":[{"arms":"no_tools","decl_json_bytes":0,"usage":{"input_tokens":73,"output_tokens":21}},{"arms":"fetch_only","decl_json_bytes":1130,"usage":{"input_tokens":441,"output_tokens":233}},{"arms":"with_tools","decl_json_bytes":2583,"usage":{"input_tokens":832,"output_tokens":20}}],"est_input_tokens_without_tools":95,"t":"token_delta"}
```

`usage` 是端点自己报的数，不是我们的估算。`decl_json_bytes` 用的是 `{"name","description","input_schema"}`
那个形状（和请求帧里的 `tools_json_bytes` 同一把尺，所以 §4 的 1130 / 2583 能直接对上）；
它是**代理计数**，线上体还要包一层 `{"type":"function","function":{…}}` ⇒ 真实字节只会更多。

| 量 | 数 | 怎么读 |
|---|---|---|
| 一条声明的边际代价 | `441 − 73 = `**+368 token**（`fetch_url`，1130 B） | 约 0.33 token/B ≈ 每 3 B 声明 1 token |
| 两条声明的合计 | `832 − 73 = `**+759 token** | `read_file` 自己 = `832 − 441 = +391`（1453 B，0.27 token/B） |
| `est_input_tokens`（不含声明） | 95 | `ContextBuilder::build(...).est_input_tokens`，与 `session.rs:317` 同一个纯函数、同一份输入 |
| 比值：不带工具 | `73 ÷ 95 = `**0.768** | 落在 `0.2..3.0` 内，**没被夹** ⇒ 今天唯一能当决策依据的比值 |
| 比值：带一条 | `441 ÷ 95 = `**4.64** | 超上界 ⇒ `set_calibration` 夹到 3.0 |
| 比值：带两条 | `832 ÷ 95 = `**8.76** | 同上，夹到 3.0 |

三个事实，一句结论：**只要这一轮声明了工具，校准系数就必然贴在天花板**。
`session.rs:170-172` 拿 `input_tokens ÷ last_est_input` 喂 `set_calibration`，
而分母（`context.rs:61` 的 `est_input_tokens`）**不含声明表**（`session.rs:326` 的 `registry.defs()`
走的是 `ChatRequest.tools`，从没进过 est）⇒ 分子含、分母不含，比值与声明体积成正比，
2026-10-24 的内置两把工具就已经把它推到 8.76；T29–T37 每接一个 MCP server、
每多一把工具，这个数只会更大。这不是「估算不准」，是**同一轮里两套口径**：
`context.rs:85-89` 的夹紧把 8.76 变成 3.0，看起来安全，实际上把「被夹过」这件事本身抹平了
（T27 收口时台账写的正是这句：被夹过的比值不能当决策依据）。

**机制推论（本轮未实测，留给修的那一轮用测试证）**：系数被钉在 3.0 之后，
`est = raw × calibration`（`context.rs:155` / `:352`）会把后续轮的 est **抬高约 3 倍**，
于是压缩与滑窗比实际需要更早触发 —— 也就是说这条缺陷的方向不是「撑爆上下文」，
而是「白白提前丢历史」。方向和幅度都要靠测试钉，本轮不动 est（简报第六节明写）。

**裁定 R289（写进台账，修在后续轮）**：选「把工具声明计入 est」，不选「不计入但写明」。
理由三条，按这次拿到的数说话：
① 分母必须和分子同一口径，否则校准那条比值天生就是一次系统性高估，夹紧只是把错误挪了个位置；
② 759 token 在 `budget_tokens` 里不是零头（两把内置工具就吃掉这个数，MCP 之后是它的若干倍）；
③ 「写明」这条治不了 est 的第二个消费者 —— `plan_compaction` 也读同一个数，
    它拿到的是预算判据，不是展示文案。
不在 T28 里做的理由：它要同时改 `context.rs` 的 est 定义与 `session.rs` 的分母来源，
而 T28 交付的是**验证记录**；混着改会让本文件里那些「基线 406→409」的读数失去对照。

## 8. 留给下一轮与待用户决定的

### 8.1 产品缺口：空 roots 时没有人告诉用户「你还没配置可读目录」

简报第三节点名要记的那条。今天的事实形状是：`effective_roots` 为空 ⇒ `read_file` 不进声明表
（§4.1），模型不知道有这把工具，于是它只会说「我无法直接读取本地文件」——**这句话里没有
「因为还没配置允许目录」这个原因**，而原因是可以一句话说清的。而这一片还没有 roots 界面（R282）。
⇒ 归 T35（server/设置管理界面那一轮）或待用户决定，不在 T28 里做。
待用户定的那一问还是同一个：默认允许哪几个目录（HOME？Documents？还是坚持一律空）。

### 8.2 R289（est 把工具声明算进去）修的那一轮要带的判据

见 §7。修的时候三件事一起做，否则等于没修：
① `est_input_tokens` 的定义加声明表；② `set_calibration` 的分母跟着换（同一口径）；
③ 被夹过要**留得下来**——今天夹紧是静默的，台账只能靠事后重算发现。
测试要钉的形状建议：「同一份历史，声明表从 0 把到 N 把，`est_input_tokens` 单调上升且
上升量与 `tools_json_bytes` 同向」+「比值超出上界时发一条可见事件而不是悄悄夹」。

### 8.3 MCP 那几轮（T29–T37）能直接从本节拿走的数

一条内置声明 ≈ **368–391 token**（≈0.3 token/B，§7）。接一个 MCP server 时，
「工具数 × 各自 schema 体积」就是这个数量级的加法，而它**同时**打在
①每轮请求体、②校准比值（今天会被夹）、③`budget_tokens` 的余量上。
所以 64 字符命名（T30）之外，还该有一条「声明体积超预算时先报数再截什么」的判断——
本轮不动它，只把系数交出去。

### 8.4 未实测清单（谁欠着的）

| 条目 | 状态 | 原因 |
|---|---|---|
| 公网域名 302 到内网（真机） | `未实测（真机）` | 需要公网服务器或改 hosts（机器级副作用），本轮不做；单测覆盖见 §5 |
| 云端那一档（百炼 Qwen / VL） | `未执行` | 凭据管理器里那把 key 被百炼判 401，**轮换欠着**（R164：本文件不出现 key） |
| T48 人工验收 | `未执行（需人工在场）` | 界面观感、<150 MB 常驻（实测 394.4 MB）、≤10 renders/s（实测 26–27）等都等用户拍板 |
| 系数跨轮持久化 | 未做 | 计划里没有这一条，等 8.2 一起裁 |

### 8.5 副作用与撤销（可复跑，也可撤销）

- `settings.json`：改动前后各取一次 md5，跑完从 `$HOME/settings.json.pre-T28` 还原并 `cmp`。
  开工前的实测是**逐字节相同**（两边都 `ea338fed3f0a8d3a57cd1435c19a3ee0`，且里面**没有** `roots` 键），
  所以本轮窗口腿第一次问「读文件」时，界面正处在「未配置 roots」的真实状态——不是我们造出来的。
  还原读数：见 §4.4 那一段的最后一行。
- SQLite：探针一律开**临时库**（`%TEMP%\slice3-probe-*.db`），用户那份 `qwen-do.db` 探针一行没写；
  **但**界面腿是真窗口真会话，`npm run tauri dev` 会把这几轮写进用户那份真历史。
  ⇒ 要不要清掉这几条测试会话，**等用户决定**，本轮不擅自删（那是他的历史）。
- 夹具：`D:/aido/slice3-notes/`（仓库外、内容自己写明是夹具）。用完可删，等收口再删。
- 9222：随 `tauri dev` 进程生灭，没写进 `tauri.conf.json`，也没写进任何 npm 脚本。

### 8.6 驱动器本轮学到的两条（写给下一次用同一套手顺的人）

- **上一轮还在流式时，textarea 与「发送」都是禁用的**，`Input.insertText` 会静默失败
  ——草稿留在框里、句子根本没出去，而日志上只看得到 `SEND "no send button"`。
  第一趟里两条 ask（天气、HN）就是这么丢的。修法不是放宽判据，是**先等空闲再打字**
  （驱动加了 idle 前置 + 「草稿仍在框里 = 这句没发出去」的快速失败），第二趟全部落定。
- **改 `src-tauri/**`（含 `examples/`）会让 `tauri dev` 重启应用**，9222 短暂
  `ECONNREFUSED`，正在跑的窗口腿会断在第一句话上。⇒ 窗口腿进行期间不改 Rust 侧文件；
  要改就先跑完。本轮那条 loopback SSRF 的窗口 ask 就是这么断的，已重跑（见 §5 末段）。
