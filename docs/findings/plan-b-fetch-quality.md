# Plan B / T24：`fetch_url` 的抽取质量实测（spec 5.5 判定门，风险 7）

日期：2026-10-21。机器：本机（Windows，`cargo run --example fetch_probe -- --live`）。
被检对象：`src-tauri/src/tools/fetch.rs::render_markdown` 的 HTML 分支，即
`htmd::convert(&html)`（**裸配置，不带任何 skip_tags / 自定义 handler**）。
探针：`src-tauri/examples/fetch_probe.rs`（打的就是生产那一条链：
`RedirectWalker::new(SystemResolver, ReqwestTransport)` → `render_markdown`）。

判定门（逐字来自 spec 5.5 / brief Step 4）：

> 正文里导航/页脚噪声占比明显、或正文被截成碎片 ⇒ `htmd` 不达标，
> 换成 `readability` + `html2md` 并新开一个任务，不许「凑合用」。

## 结论（一句话）

**不达标。** 六个页面里两个透传分支（text/plain、application/json）完全可用；四个走
`htmd` 的页面里，**没有一个可用** —— 新闻页 59.9%、博客页 86.9%、SPA 页 96.0%、
门户页 5.0% 的「抽取字符」其实是 `<script>`/`<style>` 的**文本正文**，而剩下那部分
在门户/SPA 两类页面上仍然是一条条导航链接。第二个判据（碎片化）**过**：段落与代码块
的形状保住了。第一个判据不过，门就不过。

按 brief 的要求，替换方案写成提案、决策归用户：见文末《换方案》。**在换掉抽取器之前
不建议把 `fetch_url` 接进生产注册表（T27）**。

## 逐页读数

| # | 页面 | 抓取 ms | 抽取 ms | `render_markdown` 字符 | 对照（跳过 `script/style/noscript`）字符 | 脚本/样式占抽取字符 | 判定 |
|---|---|---|---|---|---|---|---|
| 1 | 新闻正文 `qbitai.com/2026/10/504734.html` | 249 | 11 | 31 639 | 12 697 | 59.9% | **不可用** |
| 2 | 博客 `overreacted.io/how-are-function-components-different-from-classes/` | 3282 | 20 | 149 764 | 19 673 | 86.9% | **不可用** |
| 3 | 门户 `ithome.com/` | 1287 | 30 | 64 691 | 61 469 | 5.0% | **不可用** |
| 4 | 纯文本 `rfc-editor.org/rfc/rfc1945.txt` | 1565 | 0（不过抽取器） | 137 582 | — | — | **可用** |
| 5 | JSON `api.github.com/repos/tokio-rs/tokio` | 763 | 0（不过抽取器） | 6 182 | — | — | **可用** |
| 6 | SPA 空壳 `36kr.com/` | 277 | 2 | 25 992 | 1 040 | 96.0% | **不可用** |
| 附 | 门户 `sina.com.cn/`（非 UTF-8 观察，不计入门） | 72 | 66 | 191 222 | 88 144 | 53.9% | **未实测**（这一页其实返回 UTF-8，没打到解码欠账） |

「对照」一列**不是**生产路径，只是为了把「噪声到底是脚本正文还是导航条」量出来，
好让换方案的提案有可对证的数。六页全部 0 跳、`换注册域: false`。

## 逐页摘录与判据

### 1 新闻正文（不可用）

69 377 字节 → 31 639 字符。抽取后前 400 字（逐字）：

```text
(function(html){html.className = html.className.replace(/\bno-js\b/,'js')})(document.documentElement);

多智能体协同重构医疗AI！清华AIR孵化：医生AI分身“坐诊”医院 – 量子位

img:is(\[sizes=auto i\],\[sizes^="auto," i\]){contain-intrinsic-size:3000px 1500px} /*# sourceURL=wp-img-auto-sizes-contain-inline-css */ 

:root{--wp-block-synced-color:#7a00df;--wp-block-synced-color--rgb:122,0,223;--wp-bound-block-color:var(--wp-bl
```

模型按默认分页（`max_chars = 8000`）读到的**第一页**里，开头 400 字是内联 JS 一行、
`<title>` 一行、WordPress 内联 CSS 两大段 —— **零行正文**。末尾 200 字同样是
「一段埋点 JS + 站点 slogan + 京ICP 备案号链接」。代理指标：链接行 89/489（18.2%，
占字符 23.0%）、长行字符占比 90.3%。
对照趟（跳过 `script/style/noscript`）后是 12 697 字符，开头变成
「标题 + `*   [首页](…) [资讯](…) …` 导航条」，正文段落仍然排在导航之后。

判据落点：第一条（噪声占比明显）——59.9% 的抽取字符是脚本与样式的文本，明显。

### 2 博客（不可用）

188 352 字节 → 149 764 字符（**抽取后几乎没变小**）。前 400 字（逐字）：

```text
How Are Function Components Different from Classes? — overreacted

document.querySelectorAll('body link\[rel="icon"\], body link\[rel="apple-touch-icon"\]').forEach(el => document.head.appendChild(el))

[overreacted](/)by [![Dan Abramov](/avi.jpg)](https://danabra.mov)

# How Are Function Components Different from Classes?

March 3, 2019

[Pay what you like](https://ko-fi.com/gaearon)

How do Reac
```

好消息在最末一行：`How do Reac…` —— 正文在第 400 字处已经开始，且段落连贯、
长行字符占比 98.1%，代码块形状保住（第二条判据过）。坏消息在尾部 200 字（逐字）：

```text
,\\"alt\\":\\"Demo of correct behavior\\"}\]\\n23:\[\\"$\\",\\"$L24\\",null,{\\"src\\":\\"/how-are-function-components-different-from-classes/./pikachu.gif\\",\\"alt\\":\\"Smiling Pikachu\\"}\]\\n"\])
```

这是 Next.js 的 RSC flight payload（`<script>` 里的转义 JSON）。对照趟给出
**19 673 字符 = 这篇文章的全部可读内容**，也就是说生产路径交给模型的内容里
**86.9% 是脚本正文**；按 8000 字一页翻，模型要翻过十几页转义 JSON 才知道正文已经结束，
而 `[已截断，原文 149764 字符]` 这句还会诱导它继续翻页。

### 3 带大量导航的门户（不可用）

144 934 字节 → 64 691 字符。前 400 字（逐字）：

```text
IT之家

(window.jQuery||document.write("\<script src='https:\\/\\/img.ruanmei.com\\/btd\\/cdn\\/expire-1-w\\/jquery\\/3.4.0\\/jquery.min.js'><\\/script>"));

[](https://www.ithome.com) [](https://img.ithome.com/app/songs/index.html)

[首页](https://www.ithome.com)

[IT圈](https://quan.ithome.com)

[最会买](https://www.zuihuimai.com)

[设置](###)

*   日夜间

    随系统

    浅色

    深色

*   主题色

    黑色

[投稿](https
```

这一页的数最关键：**脚本/样式只占抽取字符的 5.0%**，跳过它们还剩 61 469 字符 ——
噪声主体是**导航链接**（链接行 589/848 = 69.5% 的行、93.0% 的字符），
这正是判定门第一条点名的「导航/页脚噪声」形状，也不是配置能解决的那一类。

诚实记一条指标的失真：这页「长行字符占比 96.5%」看着像「正文成段」，其实是因为
Markdown 链接语法把行拉长（`[标题](https://www.ithome.com/0/823/017.htm)` 一行 60+ 字）。
**「长行占比」在链接为主的页面上不能当碎片化证据用**，人眼读摘录才算。

### 4 纯文本文件（可用）

`text/plain` 走透传：137 582 字节 → 137 582 字符，逐字节原样，空行与缩进都在
（前 400 字开头是 RFC 首页的那片空行，接着 `Network Working Group   T. Berners-Lee`）。
零抽取耗时。**这一条路与抽取器无关，判可用。**

### 5 JSON 接口（可用）

`application/json` 走透传：6 182 字节 → 6 182 字符，单行 JSON 原样（前 400 字起头
`{"id":67836789,"node_id":"…`）。零抽取耗时。**判可用。**

### 6 SPA 空壳（不可用，且撞风险 7）

25 361 字节 → 25 992 字符。前 400 字**整段是内联 CSS**：

```text
\* { box-sizing: border-box; } html, body { height: 100%; } body { margin: 0; background: #fff; color: #0c0d0e; font-family: -apple-system, BlinkMacSystemFont, "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", Arial, sans-serif; } .page { min-height: 100vh; padding: min(300px, 22vh) 20px 40px; display: flex; flex-direction: column; align-items: center; } .card { width: 432px; max-width: 100%; 
```

末尾 200 字（逐字）：

```text
RwtbvHhjqzqTvdbWrqzuR4zlB3uXX1zYzJn6hyLxhqgEuLW1dnpKXu/OGedKhb3VKdqwu+Jb4Z8aly77S3x6Iu3Njk1tVHW0zct7tbV+fWuMn9D1aO1PSeGdU4AAAAAElFTkSuQmCC) 火山引擎 

正在进行安全检测... 

为保障您的访问安全，系统正在检测当前网络环境，该过程通常需要几秒钟，请耐心等待
```

三重失败，逐条写清：

1. **`slice_page` 那句 `[正文为空…]` 没被打到** —— 这页抽出了 25 992 字符，
   其中 96.0% 是 CSS/脚本，可读的只有末尾那两句反爬提示。我们对「空正文」的判据是
   「抽出来 0 字符」，太窄：JS/CSS 为主的页面走的是「大正文 + 全是噪声」那条路。
2. **风险 7 的「不把验证码页喂给模型」在这一页没有兜住**：`36kr.com/` 返回的是
   火山引擎的安全检测页，我们把它当正文喂了 25 992 字符。而且它**没有换注册域**
   （0 跳、`cross_host = false`），所以 `[已跳转至 <host>]` 那句提示也不会出现 ——
   模型读到的是一个「看起来成功」的结果。
3. **内联 `data:` 图片能占掉半页字符**：`![](data:image/png;base64,iVBORw0KG…)` 这种
   一串就是几千字符，对模型零信息量。`htmd` 不剥它。（`readability`/`html2md`
   剥不剥：**未实测**。）

### 附加：非 UTF-8 门户（未实测，没打到）

`sina.com.cn/` 401 007 字节 → 191 222 字符（抽取 66 ms），对照 88 144 字符（53.9%）。
但 `content-type: text/html` 无 charset 参数、返回体本身是 UTF-8，抽取首行「新浪网」
正常 —— **这页没有触发 `from_utf8_lossy` 那条解码欠账**。所以「charset 参数不参与
解码」的代价至今**未实测**（见下《未实测清单》）。

## 根因（可复核，读了 htmd 0.5.6 源码）

`htmd = "0.5.6"` 的 `convert()` 用 `Options::default()`，其中
`translation_mode: TranslationMode::Pure`（`src/options.rs:35`）。在这个模式下：

- `script` / `style` / `noscript` 与一堆标签一起被挂到 `block_handler`
  （`src/element_handler/mod.rs:221` 起的列表），而 `block_handler` 在 Pure 模式下
  直接 `handlers.walk_children(...)` 把**子节点的文本**拼进输出
  （`src/element_handler/mod.rs:371`）—— 于是 JS/CSS 源码原样进 Markdown。
- `head` 走 `head_body_handler`，父节点是 `html` 时**照常用**（`src/element_handler/head_body.rs`），
  所以 `<head>` 里的 `<title>`、内联 `<style>`、内联 `<script>` 全部进输出 —— 这就是新闻页
  开头 400 字是「JS + 标题 + CSS」的顺序。
- 没有「正文边界」概念：`nav`/`footer`/`aside` 与 `<article>` 一视同仁 —— 这就是门户页
  那 93.0% 链接行的来源。

也就是说：判门挂掉的**不是** Markdown 语法转换质量（第二条判据过，标题/列表/代码块/段落
形状都对），而是**没有内容筛选**。这两件事的修法不同，换方案那节按这个区分来写。

## 体积账（Step 5，实测数字）

```text
cargo tree -e normal --manifest-path src-tauri/Cargo.toml | grep -c ''
  加 htmd 前：808 行        加 htmd 后：830 行      （+22 行依赖边）
git diff --numstat src-tauri/Cargo.lock
  58  2  src-tauri/Cargo.lock      （新增 58 行、改 2 行）
cargo tree -e normal … --invert htmd
  htmd v0.5.6  └── qwen-do v0.1.0   （只有本 crate 用它）
```

新增的**唯一版本包**恰好 5 个（对 `cargo tree -e normal` 前后取唯一 `name v version`
集合做差集，实测输出）：`htmd v0.5.6`、`html5ever v0.38.0`、`markup5ever v0.38.0`、
`markup5ever_rcdom v0.38.0+unofficial`、`xml5ever v0.38.0`。`phf`/`string_cache`/
`web_atoms`/`tendril`/`parking_lot` 等都是**树上早已有之**（打 `(*)` 复用），不是新增。

结论：**没有混进 `image`/`serde_yaml` 那一类不相关的重依赖** —— htmd 带来的就是
html5ever 那一族解析器，正是 brief 允许的那一类。按 brief 要求 `htmd = "0.5.6"`
保持原样一行，**没有做任何 feature 裁剪实验**。

**记为债务交发布体积任务（不在本任务动）**：`html5ever`/`markup5ever` 在 Cargo.lock 里
**同时存在 0.38 与 0.39 两个版本** —— 0.39 那一份本来就由
`dom_query v0.28.0 → tauri-utils v2.10.2 → tauri-codegen/tauri-macros` 带进来（构建期
proc-macro 侧，不进最终二进制），htmd 要的 0.38 是运行期新增。同一族解析器编两份，
要体积的人得实测一次安装包增量才能定，本任务只记不判。

## 未实测清单（不假装覆盖）

1. **非 UTF-8 页面的解码代价**：挑的 sina 首页实际返回 UTF-8，`from_utf8_lossy`
   那条路一次都没被真正打过。要测得挑一个确实返回 gb2312/gbk 的站点。
2. **换方案的质量**：`readability` + `html2md` 一次都没跑过（本任务不许在判门没过前
   改生产路径）。上面的对照趟证明「噪声里有多少是 script/style」，但**没有**证明
   readability 能把门户/SPA 两类救回来。
3. **重定向与错误状态在真网上的形态**：六页全 0 跳，`[已跳转至 <host>]` 与
   `[上游返回 4xx，内容可能受限]` 两句只有离线单测覆盖（`fetch.rs` 的
   `a_cross_host_landing_is_announced_in_the_output_text`、
   `an_upstream_403_or_429_is_announced_before_the_body`），真网上没验过。
4. **`cross_host` 本身的准确性**：T23 复评已实测到它在一个用例下**少报**，所以
   「已跳转至」只能当提示，不参与任何判决（`fetch.rs` 里 `cross_host` 上方有注）。
5. **大页面分页读感的真实体验**：探针只交首 400 字 + 末 200 字，没有真的把 18 页
   `fetch_url` 结果喂给模型看它会不会绕不出来 —— 那是 T28 切片 3 的活。
6. `data:` URI、`<svg>` 内文本在 `readability` 下的行为：未实测。

## 换方案（提案，决策归用户）

判门不过 ⇒ 按 brief 点名替换：`readability`（正文定界）+ `html2md`（DOM→Markdown），
**新开一个任务**做选型、体积账与同一套六页复测。理由与证据：

- 门挂在哪：四个 HTML 页面里，噪声主体两处 —— ① `<script>/<style>` 文本
  （新闻 59.9%、博客 86.9%、SPA 96.0%），② 导航/页脚链接（门户页 93.0% 字符，
  且跳过脚本样式后仍剩 61 469 字符）。①算配置可救，②必须靠「正文边界」，
  那正是 `readability` 的卖点。
- **中间方案（也实测了，写明代价）**：只给 `htmd` 加
  `skip_tags(["script","style","noscript"])` 就能把博客页 149 764 → 19 673、
  新闻页 31 639 → 12 697。但门户页 61 469 字符仍是链接 dump，SPA 页剩下的
  1 040 字符仍是 base64 + 反爬提示。这就是 brief 说的「凑合用」，
  **不建议**以它交付；写在这里是为了让决策有数，不是让下一个任务省掉。
- 换方案前 `fetch_url` 的处置建议：**不进生产 registry（T27）**，本任务的
  walker/分页/白名单/SSRF 那几层照旧交付并可复用，只把 HTML 那一支换掉。
  工具对模型可见却抽出来一半是 JS，比工具不可用更坏。

## 复跑

```bash
# 离线：只打印要打的页面清单，一个字节都不发
cargo run --manifest-path src-tauri/Cargo.toml --example fetch_probe
# 实测（本任务唯一允许联网的步骤）
cargo run --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live
# 复核单个页面
cargo run --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live <URL>
```

原始输出：`.superpowers/sdd/2026-10-21-plan-b-tools-mcp/t24-probe.txt`。

---

# T24b 复测：换成 `readability` + `html2md` 之后（2026-10-23）

上面那一节（T24）**保持原样**，这一节是同一台机器、同一批页面、同一个探针
（`src-tauri/examples/fetch_probe.rs`）在换掉抽取器之后的复跑。

被检对象：`render_markdown` 的 HTML 分支，现在是
`decode_body`（按声明字符集解码）→ `readability::extractor::extract`（正文定界）→
只取 `<body>` 内层 → `html2md::parse_html`。版本：`readability 0.3.0`
（`default-features = false`）、`html2md 0.2.17`、`encoding_rs 0.8.42`。
探针的**对照列**换了口径：T24 那一列是「htmd 跳过 script/style/noscript」，
这一节是「整页过一次 html2md、**不做**正文定界」—— 量的是定界这一步替模型挡掉多少字符。
原始输出：`.superpowers/sdd/2026-10-21-plan-b-tools-mcp/t24b-probe.txt`。

## 结论（一句话）

**收紧后的通过线：六页全过。** 新闻页与博客页开头 400 字就是正文句子、脚本样式归零；
门户页从 64 691 字符的链接 dump 掉到 186 字符、SPA 页从 25 992 字符的 CSS/JS 掉到 53 字符，
两条「不允许再交一堆链接或 JS 正文」的红线都没有被踩到。三条限制照实在下面写着，
不写成「完美」。

## 前后对照表（同一批页面）

| # | 页面 | T24（htmd）字符 | T24b 字符 | T24b 抽取 ms | 对照（整页不定界）字符 | 定界省掉 | 判定 |
|---|---|---|---|---|---|---|---|
| 1 | 新闻 `qbitai.com/2026/10/504734.html` | 31 639 | **6 201** | 4 | 33 199 | 81.3% | **可用** |
| 2 | 博客 `overreacted.io/how-are-function-components…` | 149 764 | **19 151** | 5 | 146 670 | 86.9% | **可用** |
| 3 | 门户 `ithome.com/` | 64 691 | **186** | 4 | 62 592 | 99.7% | **通过（有限制）** |
| 4 | 纯文本 `rfc-editor.org/rfc/rfc1945.txt` | 137 582 | 137 582 | 0（不过抽取器） | — | — | **可用** |
| 5 | JSON `api.github.com/repos/tokio-rs/tokio` | 6 182 | 6 182 | 0（不过抽取器） | — | — | **可用** |
| 6 | SPA 空壳 `36kr.com/` | 25 992 | **53** | 0 | 25 624 | 99.8% | **通过（有限制）** |
| 附 | 门户 `sina.com.cn/`（非 UTF-8 观察，不计入门） | 191 222 | **2 115** | 9 | 193 033 | 98.9% | 见下面「charset」 |

六页仍然全部 0 跳、`换注册域: false`，所以 `[已跳转至 …]` 与 `[上游返回 …]` 两句在真网上
**依旧没被打到**（T24 的未实测清单第 3 条照旧有效）。

## 逐页判据

1. **新闻（可用）**。前 400 字逐字开头是文章自己的引子块（`> 鹭羽 发自 凹非寺`、
   `> 量子位 | 公众号 QbitAI`），紧接着是正文句子「不会……只有我这样做吧？」「现在每次去
   医院看病前，我都会**让AI帮我跑一轮**。」—— 有正文句子这条过。T24 那一趟的开头是
   一行内联 JS + `<title>` + 两大段 WordPress 内联 CSS，现在**一行脚本都没有**
   （抽取 4 ms，比 htmd 的 11 ms 还快）。末尾是文章的结尾段落 + 官网链接 + 版权行，
   不是备案号 dump。限制：开头那两个 blockquote 是作者署名/公众号行，属于文章 DOM 内部的
   内容，不是我们从 `Product.title` 拼进去的（那条判据由
   `title_and_byline_are_not_spliced_into_the_body` 钉住）。
2. **博客（可用）**。前 400 字第一行就是 "How do React function components differ from
   React classes?"，第二段起是连贯正文；19 151 字符对上 T24 对照趟（htmd 跳过脚本样式）的
   19 673 —— 数量级一致，说明**正文定界一步就拿到了当年对照趟的水平**，而生产路径不再需要
   任何 skip_tags 配置。代码块与长段落形状保住（长行字符占比 97.1%）。T24 尾部那种
   Next.js RSC 转义 JSON 彻底消失。
3. **门户（通过，有限制）**。186 字符，内容是页面自己的 description 元信息一句
   （「IT之家，软媒旗下科技门户网站…」）+ 版权/举报页脚两行。**没有**再交 589 行导航链接
   （T24 是 64 691 字符、链接行占 69.5%）——「不允许再交一堆链接」这条过。
   限制照实写：门户页本来就没有单一正文，`readability` 的评分最后落在了页脚块上，
   交出来的既不是「抽不出正文」那句、也不是任何正文。要更严的话下一步该加的是
   「正文下限」（抽出字符数低于某个阈值就改说那一句），本任务没有擅自加。
4. **纯文本（可用）**。逐字节透传，137 582 → 137 582；现在这一支过 `decode_body`
   （声明的 `charset=utf-8` 参与解码），空行与缩进仍然一个不改。
5. **JSON（可用）**。6 182 → 6 182，单行原样。
6. **SPA 空壳（通过，有限制）**。53 字符 = 「正在进行安全检测…」+ 那句等待提示。
   T24 的三重失败里，第 1 条（把 25 992 字符 CSS/JS 当正文）与第 3 条（`data:` URI 占半页）
   现在**没了**：整页 25 624 字符被定界挡掉 99.8%，`![](data:image/png;base64,…)` 一条不剩。
   残留的是第 2 条：这一页 status=200、没换注册域，所以 `[上游返回 …]` 与 `[已跳转至 …]`
   两句都不会打，我们仍然把一块反爬提示当正文交了 53 字符 —— 比 T24 好两个数量级，
   但「识别验证码/反爬页」这件事本任务没做到，也不假装做到。

## charset 解码（R265）

- 三条判据都落了地，但**真网上依旧一次都没打到**：六页 + sina 附加页全部返回 UTF-8
  （sina 的 `content-type: text/html` 连 charset 参数都没有）。T24 未实测清单第 1 条
  在这一点上**仍然没关掉**，只是从「没有实现」变成「实现由单测钉住、真网未验证」：
  `a_gbk_html_page_decodes_instead_of_mojibake` 写死了真的 GBK 字节（`encoding_rs::GBK.encode`
  现场造夹具，并断言同一批字节走 `from_utf8_lossy` 一定产 U+FFFD），
  `an_unknown_charset_is_reported_not_lossy_decoded` 钉住未知标签报错。
- 探针里 sina 那一页的输出形状值得记一条：`readability` 定界后剩下 2 115 字符，
  内容是主页要闻列表的**模板文本**（`\<div class="uni-blk-bt clearfix"\>` 这种被 html2md
  转义过的标签串），中文本身没乱。也就是说门户类页面在 readability 下仍可能交出
  「嵌在 DOM 里的 HTML 源码」，不是链接 dump、但也不是散文。

## 本任务新查出的两件事（T24 的清单里没有）

1. **`html2md` 与 `panic = "abort"` 不兼容**（阻断级）。`html2md` 的
   `[lib] crate-type = ["rlib","dylib","staticlib"]`，**0.2.6 到 0.2.17 全是这样**（逐个版本
   实测过），dylib/staticlib 必须链 `panic_unwind`，在本仓库的 release profile 下报
   `error: the linked panic runtime panic_unwind is not compiled with this crate's panic strategy abort`
   ⇒ 本任务把 `[profile.release] panic` 改成了 `"unwind"`。体积代价见下表：**这是全部增量所在，
   换依赖本身净变化是负的**。要换回 abort，只能换一个不带 dylib 的 Markdown 转换器（决策归用户）。
2. **`readability` 需要一条基地址，而 `render_markdown` 的签名里没有 URL**。占位地址
   `https://fetch-url.invalid/` 被它用来改写相对链接，博客页那张 `pikachu.gif` 就变成了
   一个**不存在的绝对地址**交出去。修法：把占位前缀从结果里剥掉（`.invalid` 是 RFC 2606
   保留域，撞不上真链接），相对链接保持相对形状；
   测试 `the_placeholder_base_url_never_reaches_the_model` 钉住。
   正解是把真实 URL 传进 `render_markdown` —— 那是签名改动，brief 明令不动，留给下一个任务定。

## 体积账（Step 5，实测数字）

```text
cargo tree -e normal --manifest-path src-tauri/Cargo.toml | grep -c ''
  T24（带 htmd）：830 行        T24b：859 行        （+29 行依赖边）
  最初（没有 HTML 抽取器）：808 行
git diff --numstat src-tauri/Cargo.lock
  263  44  src-tauri/Cargo.lock
Cargo.lock 唯一 name+version 集合前后做差（实测）
  新增 26 个、移除 5 个（净 +21）：530 → 551
  新增：readability 0.3.0、html2md 0.2.17、encoding_rs 0.8.42、core_detect 1.0.0、
        html5ever 0.26.0、markup5ever 0.11.0、markup5ever_rcdom 0.2.0/0.39.0、
        xml5ever 0.17.0/0.39.0、futf、mac、tendril 0.4.3、string_cache(_codegen)、
        phf*、rand/rand_chacha/rand_core 0.8 一套、siphasher 0.3.11、multiversion_no_op
  移除：htmd 0.5.6 与它那一族 0.38.0（html5ever/markup5ever/markup5ever_rcdom/xml5ever）
release exe（`src-tauri/target/release/qwen-do.exe`，本机实测三趟）
  HEAD（htmd + panic=abort）            11 153 920 字节
  HEAD（htmd + panic=unwind，隔离用）   16 590 336 字节
  T24b（readability+html2md+encoding_rs + panic=unwind）  16 588 800 字节
  ⇒ profile 那一行 = +5 436 416 字节；**换依赖本身在同一 profile 下是 −1 536 字节**（几乎为零）
```

`encoding_rs` 不是传递依赖（`cargo tree -e all | grep encoding_rs` 在加它之前**空输出**，
实测：`readability 0.3.0`→html5ever 0.26 与 `html2md 0.2.17`→html5ever 0.39 两族都不带它），
所以按 brief 的规矩这一条属于「真新增包」，本任务**照实加了一行并在此提请裁定**，
没有静吞。安装包数没重打（本任务不动打包），验收线 <15 MiB 的压力集中在 profile 那一行。

## 未实测清单（T24b 之后的状态）

1. 非 UTF-8 页面的**真网**代价：仍然没打到（六页 + sina 全 UTF-8）。实现由单测钉住。
2. `readability` + `html2md` 的**质量**：这一节就是它的实测，从「一次都没跑过」变成
   「六页跑过、判门已下」。仍未测：`<svg>` 内文本、`data:` URI（SPA 那一页现在一条不剩，
   但那是这一页的形状，不是普适结论）、以及**同一站点多页的稳定性**（每页只打了一次）。
3. 重定向与错误状态在真网上的形态：六页全 0 跳，两句自述行仍只有离线覆盖。
4. 安装包体积（NSIS）在 `panic=unwind` 下的真实增量：没重打，只有 exe 数。
5. 门户/SPA 两类「抽出来是页脚/反爬提示」的形状要不要走「正文下限」阈值：本任务不加，
   等判门结果被复核后再定。

## 复跑（T24b）

```bash
cargo run --release --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live
cargo run --release --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live <URL>
```

