//! websearch（G21）：公网 Web 搜索工具 —— DuckDuckGo 的非 JS HTML 端点 + 手写解析器。
//!
//! # 形状
//!
//! HTTP 与解析**分离**：[`DdgProvider`] 只管取回（状态码 + 响应体），解析是纯函数
//! [`parse_ddg_html`]，可以离线单测；「200/非 200」的防御语义收在纯函数
//! [`ddg_respond`] 里，同样离线可测。测试注入走 [`SearchProvider`] 接缝 ——
//! 生产只有 DdgProvider 一个实现。
//!
//! # 已知脆弱点（诚实标注：这些是「降级形态」的承诺，不是「永远可用」的承诺）
//!
//! 1. **类名不是 API**：`result__a` / `result__snippet`（`/html/` 端点）与
//!    `result-link` / `result-snippet`（lite 表格端点）都是站点私有类名，DDG 改版
//!    即失效。失效形态是「解析出 0 条」→ 工具回报零命中 —— 这是**降级**不是报错：
//!    HTTP 200 的页面上「验证码/反爬页」与「真零结果」无法区分，宁可如实报零命中，
//!    也不编造一个失败原因。CI 无网络，结构漂移**只能在运行时暴露**；单测钉的是
//!    解析器对已知结构的理解，不是站点的行为。
//! 2. **跳板链接**：结果 href 多为 `//duckduckgo.com/l/?uddg=<百分号编码的目标>&…`，
//!    解码见 [`resolve_result_url`]；跳板形态变化时该条结果被**丢弃**（不是编一个
//!    猜测的 URL 交出去）。
//! 3. **网络行为不进 CI**：[`DdgProvider`] 的请求、状态码与真实页面解析无法在 CI
//!    验证（与 fetch 的传输层同一条纪律）—— CI 钉纯函数，网络路径靠人工验收。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub const NAME: &str = "websearch";

/// 交给模型的结果条数上限。这条上限是**工具的对外承诺**（写在 description 里），
/// 所以收口在 `execute`（任何 provider 的产出都过这道闸），解析器本身不截断 ——
/// 截断只该有一处实现、一个口径。
const MAX_RESULTS: usize = 10;
/// query 长度上限（字符）。搜索词不是论文，超长的 query 多半是把要搜的正文整段贴了
/// 进来，端点也会拒绝或降质 —— 早一步给出明确报错比让模型读到空结果好猜。
const MAX_QUERY_CHARS: usize = 300;
/// 摘要展示的字符数（按字符不按字节：中文 3 字节，字节切会碎在 UTF-8 序列中间）。
const SNIPPET_PREVIEW_CHARS: usize = 120;

/// 单次搜索请求超时。与 fetch.rs 的单跳超时同量级：搜索是一问一答，用户按停止后
/// 这条请求最多再活 20 秒。
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// 搜索端点。用 DDG 的**非 JS HTML** 端点（服务端渲染，无需浏览器）：固定常量、
/// 不接受配置 —— 「只出站到固定端点」是本工具 Risk::Read 的前提，端点一旦可配，
/// 这个风险等级就不成立了。
const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// 一条搜索结果。
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 搜索能力接缝：HTTP 取回 + 解析的合体。独立成 trait 是为了测试注入 ——
/// 真实 provider 要发网络请求（CI 无网络），替身给固定命中就能把工具层的
/// 编号/截断/零命中/透传行为全部钉住。
#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, query: &str) -> Result<Vec<SearchHit>, ToolError>;
}

/// 生产 provider：DuckDuckGo HTML 端点。
///
/// **网络行为不进 CI**（诚实标注）：本实现的请求路径（DNS/TLS/状态码/真实页面）
/// 只有带网络时才能验证；CI 里被钉住的是它调用的两个纯函数
/// （[`ddg_respond`] 与 [`parse_ddg_html`]）。
pub struct DdgProvider {
    http: reqwest::Client,
}

impl DdgProvider {
    pub fn new() -> Self {
        // UA 与 fetch.rs 同一句自我标识：对外承诺只有一份（改动要在计划层面被看见）。
        // 带超时：provider 自己兜底，不依赖调用方给 client 配置。
        let http = reqwest::Client::builder()
            .user_agent(crate::tools::fetch::USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .expect("reqwest client 构造失败（TLS 后端初始化失败，进程级不可恢复）");
        Self { http }
    }
}

impl Default for DdgProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SearchProvider for DdgProvider {
    async fn search(&self, query: &str) -> Result<Vec<SearchHit>, ToolError> {
        // q 的 URL 编码交给 url crate 的 form 编码（空格→+、中文→%XX），
        // 自己拼字符串必然在「& ? # 中文」上出错。
        let url = Url::parse_with_params(DDG_ENDPOINT, &[("q", query)]).map_err(|e| {
            ToolError::Failed(format!(
                "构造搜索地址失败（端点是常量，这一支不该走到）：{e}"
            ))
        })?;
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("搜索请求失败：{e}")))?;
        let status = resp.status().as_u16();
        // 响应体不做体积闸：端点固定，返回的是服务端渲染的列表页（百 KB 量级）；
        // 交给模型的文本量另有 output_bound 在出口兜底。
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("读取搜索结果失败：{e}")))?;
        ddg_respond(status, &body)
    }
}

/// 「状态码 + 响应体」→ 结果或错误的**纯函数**（DdgProvider 的网络路径只是搬运）。
/// 拆出来是因为防御语义必须在无网络的 CI 里钉住：
/// - **非 200 = [`ToolError::Failed`]**：反爬/限流/宕机时 DDG 常回 403/5xx 的异常页，
///   把异常页喂给解析器只会得到「0 条命中」这个假话 —— 非成功状态一律报错。
/// - **200 + 解析出 0 条 = 零命中 [`Ok(vec![])`]**：不与错误混淆。200 说明端点正常
///   应答了，0 条就是 0 条（可能真没有，也可能是第 1 条脆弱点说的结构漂移/验证码页
///   —— 那是「降级」，报错反而编造了失败原因）。
fn ddg_respond(status: u16, body: &str) -> Result<Vec<SearchHit>, ToolError> {
    if status != 200 {
        return Err(ToolError::Failed(format!(
            "搜索端点返回 HTTP {status}（非 200 一律按失败处理，不解析异常页内容）"
        )));
    }
    Ok(parse_ddg_html(body))
}

// ────────────────────────── 解析器（纯函数区） ──────────────────────────

/// 一个还没配摘要的结果锚（href + 标题内文 + 在页面里的位置）。
struct RawAnchor {
    /// 本锚 `<a` 的起始下标（给下一条结果定摘要窗口的左界）。
    start: usize,
    /// 本锚 `</a>` 之后的下标（摘要从这里向后找）。
    end: usize,
    href: String,
    title: String,
}

/// 解析 DDG 结果页 HTML，抓出「链接 / 标题 / 摘要」。
///
/// 纯函数：不联网、不看时钟，同一输入永远同一输出 —— 单测在下面钉着。实现是
/// 手写的字节扫描（fetch.rs 的 meta_charset / article_body_html 同一取向），不引
/// DOM 树：要抓的只有两种元素（结果锚 + 摘要元素），拉一棵 DOM 换不来正确性。
///
/// 两种已知标记形态都认（见文件头脆弱点 1）：
/// - `/html/` 端点：`<a class="result__a">` + `<a class="result__snippet">`
/// - lite 表格端点：`<a class="result-link">` + `<td class="result-snippet">`
///
/// 结构对不上（类名改版、验证码页）→ 返回空 `Vec`，**不报错** —— 报错归
/// [`ddg_respond`] 按状态码判。
fn parse_ddg_html(html: &str) -> Vec<SearchHit> {
    // 小写镜像与原文**逐字节等长**（to_ascii_lowercase 只改 ASCII 字母、每字节 1:1），
    // 所以在小写镜像里找到的下标可以直接用来切原文。镜像用来认标签与类名
    // （大小写不敏感），原文用来取 href 的**值**（URL 大小写敏感，小写镜像里的
    // 值是坏的 —— result_anchors 里两个标签串各取所需）。
    let lower = html.to_ascii_lowercase();
    let anchors = result_anchors(&lower, html);
    let mut hits = Vec::new();
    for (i, a) in anchors.iter().enumerate() {
        // URL 解不出来的条目整条丢弃：交一个猜测的/跳板的 URL 给模型，
        // 模型会拿它去 fetch —— 比少一条结果更糟。
        let Some(url) = resolve_result_url(&a.href) else {
            continue;
        };
        // 摘要配对窗口：本锚结束 → 下一个结果锚开始。越过窗口配对会张冠李戴
        // （把下一条结果的摘要接到本条上）。
        let window_end = anchors.get(i + 1).map_or(lower.len(), |n| n.start);
        let snippet = snippet_between(&lower, html, a.end, window_end);
        hits.push(SearchHit {
            title: a.title.clone(),
            url,
            snippet,
        });
    }
    hits
}

/// 收集页面里全部「结果锚」：带结果类名的 `<a>`（href 原文 + 标题内文）。
fn result_anchors(lower: &str, html: &str) -> Vec<RawAnchor> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some((open, close)) = next_anchor_tag(lower, from) {
        from = open + 2; // 只从 '<a' 之后继续找，保证循环前进
        let tag_lower = &lower[open..=close];
        let class_ok = tag_attr_span(tag_lower, "class").is_some_and(|(s, e)| {
            let v = &tag_lower[s..e];
            v.contains("result__a") || v.contains("result-link")
        });
        if !class_ok {
            continue;
        }
        // href 的值从**原文**里取（URL 大小写敏感），位置从镜像里算。
        let tag_orig = &html[open..=close];
        let Some((s, e)) = tag_attr_span(tag_lower, "href") else {
            continue;
        };
        let href = tag_orig[s..e].to_string();
        // 标题内文：到本锚自己的 `</a` 为止（真实 HTML 里标题不可能内含 </a）。
        let inner_start = close + 1;
        let Some(close_rel) = lower[inner_start..].find("</a") else {
            continue;
        };
        let inner_end = inner_start + close_rel;
        out.push(RawAnchor {
            start: open,
            // +4 = "</a>" 的长度：摘要窗口的起点越过闭合标签。
            end: inner_end + 4,
            href,
            title: visible_text(&html[inner_start..inner_end]),
        });
    }
    out
}

/// 下一个 `<a …>` 标签的 (开, 闭) 下标（open 指向 `<`，close 指向 `>`）。
/// `<a` 必须后跟空白或 `>`：`<abbr`、`<audio` 不算锚。
fn next_anchor_tag(lower: &str, from: usize) -> Option<(usize, usize)> {
    let mut search = from;
    while let Some(rel) = lower[search..].find("<a") {
        let open = search + rel;
        let is_anchor = match lower.as_bytes().get(open + 2) {
            Some(b) => b.is_ascii_whitespace() || *b == b'>',
            None => false,
        };
        if is_anchor {
            // 标签体到第一个 `>`：真实属性值里不会出现裸 `>`（DDG 的页面如此）；
            // 真出现也只是把标签切短，后面的 attr 提取找不到目标、按不匹配跳过。
            let close = open + lower[open..].find('>')?;
            return Some((open, close));
        }
        search = open + 2;
    }
    None
}

/// 在（小写镜像的）标签串里定位某属性值的字节区间 [start, end)。
///
/// 返回区间而不是值：调用方要用同一区间去切**原文** —— 属性值的大小写是 URL 的
/// 一部分（镜像里的值已被小写化），不能直接交镜像切片。
fn tag_attr_span(tag_lower: &str, name: &str) -> Option<(usize, usize)> {
    let mut from = 0usize;
    loop {
        let rel = tag_lower[from..].find(name)?;
        let at = from + rel;
        from = at + name.len();
        // 名字前必须紧跟空白：排除 data-href= 误命中 href（真正的分隔判据在
        // 下面的 `=`：hreflang= 会过这一关，但过不了 `=` 那关）。
        if !tag_lower[..at].ends_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(after) = tag_lower[from..].strip_prefix('=') else {
            continue;
        };
        // 属性名与 = 之间不允许空白（DDG 的服务端渲染页面如此；要支持就得再拆状态机）。
        let value_start = from + 1 + (after.len() - after.trim_start().len());
        let after_trimmed = after.trim_start();
        let quote = after_trimmed.chars().next()?;
        // 带引号形态双/单都认（/html/ 端点用双引号，lite 端点用单引号）——
        // 开引号本身不属于值，起点要跳过它。
        let (vstart, vend) = if quote == '"' || quote == '\'' {
            let inner = &after_trimmed[1..];
            let qpos = inner.find(quote)?;
            (value_start + 1, value_start + 1 + qpos)
        } else {
            // 无引号形态：读到空白或标签结束。
            (
                value_start,
                value_start + after_trimmed.find(|c: char| c.is_ascii_whitespace() || c == '>')?,
            )
        };
        return Some((vstart, vend));
    }
}

/// 在 (from, to) 窗口里找摘要元素（`result__snippet` / `result-snippet` 两种形态），
/// 交出它的内文。找不到 = 没有摘要（空串），不算失败。
fn snippet_between(lower: &str, html: &str, from: usize, to: usize) -> String {
    let to = to.min(lower.len());
    if from >= to {
        return String::new();
    }
    let window = &lower[from..to];
    let Some(cls) = window
        .find("result__snippet")
        .or_else(|| window.find("result-snippet"))
    else {
        return String::new();
    };
    // 从类名位置向前找所属标签的开 `<`、向后找 `>` —— 内文从标签结束起、
    // 到同名闭合标签为止。（类名若出现在前文文本里，这里会认错标签 —— 保守
    // 代价：那条摘要可能取空，不会张冠李戴到别的元素上。）
    let Some(tag_open) = window[..cls].rfind('<') else {
        return String::new();
    };
    let Some(gt) = window[tag_open..].find('>') else {
        return String::new();
    };
    let tag_name = tag_name_of(&window[tag_open..=tag_open + gt]);
    let inner_start = tag_open + gt + 1;
    let close_pat = format!("</{tag_name}");
    let Some(close_rel) = window[inner_start..].find(&close_pat) else {
        return String::new();
    };
    // 所有下标都来自 ASCII 模式的匹配，落在字符边界上，可以安全切原文。
    visible_text(&html[from + inner_start..from + inner_start + close_rel])
}

/// `<td class='result-snippet'>` → "td"：取标签名（在小写镜像上做）。
fn tag_name_of(tag: &str) -> String {
    tag[1..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// 把结果 href 变成模型能直接用的目标 URL。
///
/// 已知形态三支（优先级从高到低）：跳板链接（从 `uddg` 参数解出真实目标）、
/// 协议相对（补 https）、绝对 http(s)。都不是 → `None`，调用方丢弃该条。
fn resolve_result_url(raw: &str) -> Option<String> {
    // href 属性值里的 `&` 在 HTML 里写成 `&amp;`，先解实体再拆参数。
    let decoded = decode_entities(raw);
    let decoded = decoded.trim();
    // 跳板：`…/l/?uddg=<百分号编码的目标>&rut=…`。取第一个 uddg= 之后、下一个 & 之前
    // 的段 —— 目标自身若含 uddg= 必然已被百分号编码（%3D…），不会抢先被匹配。
    if let Some(rest) = decoded.split("uddg=").nth(1) {
        let target = rest.split('&').next().unwrap_or("");
        let target = percent_decode(target);
        if !target.is_empty() {
            return Some(target);
        }
    }
    if let Some(rest) = decoded.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    if decoded.starts_with("http://") || decoded.starts_with("https://") {
        return Some(decoded.to_string());
    }
    None
}

/// 百分号解码（只解 %XX）。`+` **不**折成空格：uddg 里是 DDG 编码过的完整 URL
/// （空格已是 %20），目标 URL 自己的 query 里完全可能有字面 `+`，按 form 语义
/// 折成空格就把链接改坏了。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = b.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(hex).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 元素内文 → 可见文本：去标签 → 解实体 → 折叠空白。
///
/// 顺序是有意的：先去标签再解实体，`&lt;b&gt;` 解出来的是字面 `<b>` 文本，
/// 不会被第二步误当标签吞掉。去标签遇到未配对的 `<` 会吞到下一个 `>` 为止 ——
/// 那是畸形 HTML，保守吞掉比把半截标签交出去好。
fn visible_text(raw: &str) -> String {
    let mut stripped = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => stripped.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 实体解码：一张小表（搜索结果文本里实际会出现的几个）+ 数字实体。
/// 认不出的按原样保留 —— 宁可交回 `&unknown;` 也不猜一个错字。
/// 单遍从左到右扫：`&amp;lt;` 解成 `&lt;`（文本）而不是 `<`，不会二次解。
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'&' {
            if let Some((rep, len)) = named_entity(&s[i..]) {
                out.push_str(rep);
                i += len;
                continue;
            }
            if let Some((ch, len)) = numeric_entity(&s[i..]) {
                out.push(ch);
                i += len;
                continue;
            }
        }
        // 非 ASCII 字节按字符推进（String 切片必须落在字符边界上）。
        let step = utf8_char_len(b[i]);
        out.push_str(&s[i..i + step]);
        i += step;
    }
    out
}

/// 命名实体小表：(实体名带分号, 替换文本, 字节长)。实体全是 ASCII，长度是常量。
fn named_entity(tail: &str) -> Option<(&'static str, usize)> {
    const TABLE: [(&str, &str, usize); 6] = [
        ("&amp;", "&", 5),
        ("&lt;", "<", 4),
        ("&gt;", ">", 4),
        ("&quot;", "\"", 6),
        ("&apos;", "'", 6),
        // nbsp 折成普通空格：紧跟 split_whitespace 的折叠，视觉等价且少一个特殊字符。
        ("&nbsp;", " ", 6),
    ];
    TABLE
        .iter()
        .find(|(name, _, _)| tail.starts_with(name))
        .map(|(_, rep, len)| (*rep, *len))
}

/// 数字实体 `&#NNN;` / `&#xHH…;`（x 大小写都认；结尾分号缺失也认 —— HTML5 的
/// 宽容形态，少数页面真的会省）。返回 (字符, 消耗字节数)。
fn numeric_entity(tail: &str) -> Option<(char, usize)> {
    let rest = tail.strip_prefix("&#")?;
    let hex_mode = rest.starts_with('x') || rest.starts_with('X');
    let digits_start = if hex_mode { 3 } else { 2 }; // '&' '#' ['x']
    let digits = &tail[digits_start..];
    let take = digits
        .chars()
        .take_while(|c| {
            if hex_mode {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        })
        .count();
    if take == 0 {
        return None;
    }
    // 数字全是 ASCII：take（字符数）就是字节数，切片安全。
    let n = u32::from_str_radix(&digits[..take], if hex_mode { 16 } else { 10 }).ok()?;
    let mut consumed = digits_start + take;
    if tail[consumed..].starts_with(';') {
        consumed += 1;
    }
    Some((char::from_u32(n).unwrap_or('\u{FFFD}'), consumed))
}

/// UTF-8 首字节 → 该字符的字节长度（输入是合法 UTF-8 的 String，必然落在 1/2/3/4）。
fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

// ────────────────────────── 工具 ──────────────────────────

pub struct WebSearchTool {
    provider: Arc<dyn SearchProvider>,
}

impl WebSearchTool {
    /// 生产接线：DuckDuckGo HTML 端点 provider（构造本身不联网，请求到 execute 才发）。
    pub fn new() -> Self {
        Self {
            provider: Arc::new(DdgProvider::new()),
        }
    }

    /// 测试注入缝：换掉 provider（离线替身给固定命中/固定错误）。
    pub fn with_provider(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description:
                "用 DuckDuckGo 搜索公网 Web，返回编号清单「N. 标题 — URL（摘要前 120 字）」，\
                          最多 10 条；零命中会明确说明而不是返回空串。query 必填、最长 300 字符。\
                          只读：仅向固定的 DuckDuckGo 端点出站，不访问其他地址。需要某个页面的\
                          完整内容时，拿到 URL 后再用 fetch_url 抓取。"
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索关键词，最长 300 字符"
                    }
                },
                "required": ["query"]
            }),
            // Read：唯一出站目标是写死的搜索端点（不可配置），与 fetch_url 同档 ⇒ 零弹窗。
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // 入参校验全部排在发请求之前：schema 的 required 只是给模型的提示，
        // 真正的防线在这里（missing / 空串 / 纯空白 / 类型不对都算 BadInput）。
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::BadInput("websearch 需要非空字符串参数 query（搜索关键词）".to_string())
            })?;
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(ToolError::BadInput(format!(
                "query 最长 {MAX_QUERY_CHARS} 字符，实得 {} 字符；请精简关键词",
                query.chars().count()
            )));
        }

        let hits = self.provider.search(query).await?;
        if hits.is_empty() {
            // 零命中 = 端点正常应答但解析不出条目（真没有 / 结构漂移 / 验证码页）。
            // 这三种在 200 页面上无法区分，如实并列说给模型，让它自己决定重试策略。
            return Ok(ToolOutput::text(format!(
                "没有搜到与「{query}」相关的结果（搜索端点正常应答，但没有可解析出的条目：\
                 可能是关键词太冷门，也可能是站点改版或触发了反爬验证；换一组更常见的\
                 关键词重试通常更有效）。"
            )));
        }
        let mut lines = Vec::new();
        for (i, hit) in hits.into_iter().take(MAX_RESULTS).enumerate() {
            // 摘要按字符截（不按字节：中文 3 字节，字节切会碎在 UTF-8 序列中间）。
            let snippet: String = hit.snippet.chars().take(SNIPPET_PREVIEW_CHARS).collect();
            if snippet.is_empty() {
                // 没有摘要就不挂空括号——「（ ）」是噪音不是信息。
                lines.push(format!("{}. {} — {}", i + 1, hit.title, hit.url));
            } else {
                lines.push(format!(
                    "{}. {} — {}（{snippet}）",
                    i + 1,
                    hit.title,
                    hit.url
                ));
            }
        }
        // G09：超长输出截断 + 完整内容落盘（.lfttdo/outputs/）。
        let text = crate::tools::output_bound::bound_tool_output(lines.join("\n"), ctx);
        Ok(ToolOutput::Text { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: vec![],
            write_root: None,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: None,
            events: None,
        }
    }

    /// 测试替身：固定命中表 / 固定错误信息。
    struct StubProvider {
        hits: Vec<SearchHit>,
        error: Option<String>,
    }

    impl StubProvider {
        fn hits(hits: Vec<SearchHit>) -> Self {
            Self { hits, error: None }
        }

        fn failing(msg: &str) -> Self {
            Self {
                hits: vec![],
                error: Some(msg.to_string()),
            }
        }
    }

    #[async_trait]
    impl SearchProvider for StubProvider {
        async fn search(&self, _query: &str) -> Result<Vec<SearchHit>, ToolError> {
            if let Some(msg) = &self.error {
                return Err(ToolError::Failed(msg.clone()));
            }
            Ok(self.hits.clone())
        }
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    // ———————— 固定 HTML 样本：真实结构简版 ————————
    // 两个已知标记形态各占一条：/html/ 端点的 result__a + result__snippet（链接是
    // DDG 跳板、值带 &amp; 实体），lite 表格的 result-link + result-snippet
    // （单引号属性、直链、&nbsp; 与数字实体都在）。

    const SAMPLE: &str = r#"<!DOCTYPE html>
<html><head><title>rust at DuckDuckGo</title></head>
<body>
<div class="result results_links results_links_deep web-result">
  <h2 class="result__title">
    <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=fa3b62">Rust Programming Language</a>
  </h2>
  <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=fa3b62">A language empowering everyone to build reliable <b>&amp;</b> efficient software.</a>
</div>
<table>
  <tr>
    <td>2.&nbsp;</td>
    <td>
      <a rel="nofollow" href="https://GitHub.com/rust-lang/rust" class='result-link'>Rust&nbsp;by&nbsp;Example</a>
    </td>
  </tr>
  <tr>
    <td class='result-snippet'>Rust by Example (RBE) is a collection of runnable examples with quotes &#x27; and amps &amp;.</td>
  </tr>
</table>
</body></html>"#;

    /// 结构对不上的页面（Google 形状）：类名一个都不认识 ⇒ 空 vec。
    const UNKNOWN_STRUCTURE: &str = r#"<html><body>
<div class="g"><h3 class="title"><a href="/url?q=https://example.com">Google-shaped</a></h3>
<span class="st">snippet</span></div>
</body></html>"#;

    #[test]
    fn parse_ddg_html_reads_both_known_markups() {
        let hits = parse_ddg_html(SAMPLE);
        assert_eq!(hits.len(), 2, "{hits:?}");
        // 第 1 条（/html/ 形态）：跳板链接解出真实目标；标题、摘要里的实体都解开。
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            hits[0].snippet,
            "A language empowering everyone to build reliable & efficient software.",
            "内嵌 <b> 去掉、&amp; 解开"
        );
        // 第 2 条（lite 表格形态）：直链原样保留**原文大小写**（小写镜像只用来认
        // 标签，值必须来自原文）；&nbsp; 折成空格；单引号属性与数字实体都解开。
        assert_eq!(hits[1].title, "Rust by Example");
        assert_eq!(hits[1].url, "https://GitHub.com/rust-lang/rust");
        assert_eq!(
            hits[1].snippet,
            "Rust by Example (RBE) is a collection of runnable examples with quotes ' and amps &."
        );
    }

    #[test]
    fn parse_ddg_html_returns_empty_on_a_structure_it_does_not_know() {
        // 空串也是「结构对不上」：解析器对无输入交空结果，不 panic。
        assert!(parse_ddg_html("").is_empty());
        assert!(parse_ddg_html(UNKNOWN_STRUCTURE).is_empty());
    }

    #[test]
    fn an_anchor_without_a_usable_http_url_is_dropped_not_fabricated() {
        let html = r#"<a class="result-link" href="javascript:void(0)">JS 伪协议</a>
<a class="result-link" href="https://ok.example/">正常</a>"#;
        let hits = parse_ddg_html(html);
        assert_eq!(hits.len(), 1, "伪协议链接整条丢弃：{hits:?}");
        assert_eq!(hits[0].url, "https://ok.example/");
    }

    // ———————— ddg_respond：防御语义（离线钉住） ————————

    #[test]
    fn http_200_with_zero_parsed_hits_is_ok_empty_not_an_error() {
        // 200 + 结构对不上 ⇒ 零命中。这一支与错误**不混淆**：报错归非 200 管。
        let hits = ddg_respond(200, UNKNOWN_STRUCTURE).unwrap();
        assert!(hits.is_empty(), "200 + 0 条 = Ok(vec![])，实得 {hits:?}");
        // 正面：200 + 已知结构 ⇒ 命中照常出来。
        assert_eq!(ddg_respond(200, SAMPLE).unwrap().len(), 2);
    }

    #[test]
    fn http_non_200_is_failed_with_the_status_named() {
        for status in [403, 429, 503] {
            let err = ddg_respond(status, "<html>anomaly page</html>").unwrap_err();
            assert!(
                matches!(&err, ToolError::Failed(m) if m.contains(&status.to_string())),
                "{status} 必须报 Failed 并点名状态码，实得 {err:?}"
            );
        }
    }

    // ———————— 工具层（stub provider 注入） ————————

    #[tokio::test]
    async fn two_hits_render_as_a_numbered_list_with_title_and_url() {
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(vec![
            SearchHit {
                title: "Rust 程序设计语言".into(),
                url: "https://kaisery.github.io/trpl-zh-cn/".into(),
                snippet: "Rust 官方中文教程".into(),
            },
            SearchHit {
                title: "空摘要的一条".into(),
                url: "https://example.com/no-snippet".into(),
                snippet: String::new(),
            },
        ])));
        let out = tool
            .execute(json!({ "query": "rust 教程" }), &ctx())
            .await
            .unwrap();
        let text = text_of(out);
        assert!(
            text.contains("1. Rust 程序设计语言 — https://kaisery.github.io/trpl-zh-cn/"),
            "{text}"
        );
        assert!(text.contains("（Rust 官方中文教程）"), "{text}");
        assert!(
            text.contains("2. 空摘要的一条 — https://example.com/no-snippet"),
            "{text}"
        );
        assert!(!text.contains("（）"), "空摘要不挂空括号：{text}");
    }

    #[tokio::test]
    async fn an_empty_result_says_so_instead_of_returning_an_empty_string() {
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(vec![])));
        let out = tool
            .execute(json!({ "query": "冷门词" }), &ctx())
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.contains("没有搜到"), "零命中要说明：{text}");
        assert!(text.contains("冷门词"), "说明里回显查询词：{text}");
    }

    #[tokio::test]
    async fn a_provider_error_is_passed_through_untouched() {
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::failing(
            "搜索端点返回 HTTP 403（测试注入）",
        )));
        let err = tool
            .execute(json!({ "query": "x" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("测试注入")),
            "provider 的错误原样透传，不包装不吞掉：{err:?}"
        );
    }

    #[tokio::test]
    async fn a_snippet_longer_than_120_chars_is_cut_by_chars_not_bytes() {
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(vec![SearchHit {
            title: "标题".into(),
            url: "https://example.com/".into(),
            snippet: "字".repeat(150),
        }])));
        let out = tool.execute(json!({ "query": "x" }), &ctx()).await.unwrap();
        let text = text_of(out);
        assert!(text.contains(&"字".repeat(120)), "{text}");
        assert!(
            !text.contains(&"字".repeat(121)),
            "第 121 字起不许出现：{text}"
        );
    }

    #[tokio::test]
    async fn ten_long_hits_trigger_the_output_bound() {
        // 10 条 × ~300 字符 > output_bound 的 2000 字符预览线 ⇒ 必须截断 + 落盘。
        let tmp = tempfile::tempdir().unwrap();
        let mut c = ctx();
        c.write_root = Some(tmp.path().to_path_buf());
        let hits = (0..10)
            .map(|i| SearchHit {
                title: format!("第 {i} 条 {}", "标题很长".repeat(25)),
                url: format!("https://example.com/{i}"),
                snippet: "摘要".repeat(60),
            })
            .collect();
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(hits)));
        let out = tool
            .execute(json!({ "query": "长输出" }), &c)
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.starts_with("[输出超长"), "截断标记在最前：{text}");
        // 完整清单落盘可读（10 条都在，含最后一条）。
        let base = tmp.path().to_str().unwrap();
        let start = text.find(base).unwrap();
        let end = text[start..].find(' ').unwrap();
        let full = std::fs::read_to_string(&text[start..start + end]).unwrap();
        assert!(
            full.contains("https://example.com/9"),
            "落盘的是完整清单：{full}"
        );
    }

    #[tokio::test]
    async fn more_than_ten_hits_are_capped_at_ten() {
        let hits = (0..12)
            .map(|i| SearchHit {
                title: format!("T{i}"),
                url: format!("https://e.com/{i}"),
                snippet: String::new(),
            })
            .collect();
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(hits)));
        let out = tool.execute(json!({ "query": "x" }), &ctx()).await.unwrap();
        let text = text_of(out);
        assert!(text.contains("10. T9"), "{text}");
        assert!(!text.contains("11."), "对外承诺最多 10 条：{text}");
    }

    #[tokio::test]
    async fn a_query_over_300_chars_is_bad_input_and_exactly_300_passes() {
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(vec![])));
        let err = tool
            .execute(json!({ "query": "字".repeat(301) }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
        // 边界：恰好 300 字符放行 —— stub 返回空 ⇒ 走到零命中文本，证明校验没拦它。
        let out = tool
            .execute(json!({ "query": "字".repeat(300) }), &ctx())
            .await
            .unwrap();
        assert!(text_of(out).contains("没有搜到"));
    }

    #[tokio::test]
    async fn a_missing_or_blank_or_non_string_query_is_bad_input() {
        let tool = WebSearchTool::with_provider(Arc::new(StubProvider::hits(vec![])));
        for input in [
            json!({}),
            json!({ "query": "" }),
            json!({ "query": "   " }),
            json!({ "query": 42 }),
        ] {
            let err = tool.execute(input, &ctx()).await.unwrap_err();
            assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
        }
    }

    // ———————— 注册形状 ————————

    #[test]
    fn meta_pins_name_risk_and_schema() {
        let m = WebSearchTool::new().meta();
        assert_eq!(m.name, NAME);
        assert_eq!(NAME, "websearch");
        assert_eq!(m.risk, Risk::Read, "只出站到固定端点 ⇒ 只读档，零弹窗");
        assert_eq!(
            m.input_schema,
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词，最长 300 字符" }
                },
                "required": ["query"]
            }),
            "发给模型的 schema 必须是声明的那一份"
        );
        assert!(m.description.contains("DuckDuckGo"));
    }

    /// Arc<WebSearchTool> 能作为 dyn Tool 注册（trait object 路径的形状检查）。
    #[test]
    fn websearch_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(WebSearchTool::with_provider(Arc::new(
            StubProvider::hits(vec![]),
        ))));
        assert!(registry.get(NAME).is_some());
        assert_eq!(registry.risk_of(NAME), Risk::Read);
    }
}
