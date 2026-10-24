// Plan B 的网络层：task 23 的逐跳重定向遍历 + 地址钉死 + 体积闸门，
// 和 task 24 的 `fetch_url` 工具（内容类型白名单 + HTML→Markdown 抽取 + 按字符分页）。
//
// T23 那一半**只做**三件事：走跳、每跳复验、把体积按上限边收边卡。
// 「取回来的字节怎么变成交给模型的文本」全部住在文件后半段的 T24 那一节（plan:317），
// 两节的分界线就是 `Fetched` 这个类型：前半段造它，后半段消费它。
//
// # 为什么自己走跳，而不是让 reqwest 跟
//
// `reqwest` 跟重定向时**每一跳都会重新解析 DNS**，于是「我校验过的那个 IP」和
// 「我实际连上去的那个 IP」不是同一件事——这正是 DNS rebinding 的入口。这里把
// 重定向交给 `RedirectWalker`：每一跳先 `check_url`（形态/方案/字面 IP/主机名红线），
// 再 `resolve_target`（地址集整集复验），最后用 `resolve_to_addrs` 把**已校验的这组
// 地址**钉进 client。库自己一条重定向都不跟（`Policy::none()`）。
//
// # 每跳复验这条不变式住在哪儿
//
// T22 的 `resolve_target` 在 `Host::Domain` 一支**不重做名称级判定**（task 22 报告
// deferred (2)），名称红线只在 `check_url` 里。所以「每一跳、`resolve_target` 之前、
// 先跑 `check_url`」这条只能由本文件的 `RedirectWalker::validate` 守住，包括对
// `Url::join` 之后的 `Location`（`validate_joined`）。少跑一次就等于只校验了首跳。
//
// # 四处按现场调整过的接口（台账裁定，理由写在对应符号上）
//
// - R262：`RedirectWalker` 持 `Arc<dyn Resolver>` + `Arc<dyn FetchTransport>`，不按
//   plan 原文泛型化 —— 同步 `Resolver` 只有在 `spawn_blocking` 里跑才不占 worker。
// - R259：`SystemResolver` 由本任务交付，且是生产解析器。
// - `FetchTransport::get` 多带一个 `max_bytes`：容量错误（R248 的 `TooLargeBody`）
//   只能在**逐块读**的那个循环里构造，而逐块读只发生在 transport 内。brief 的三参
//   签名会把闸门挤到「收完再判」，那是本任务明确要避免的形状。
// - `Fetched` 多带一个 `location`：关掉了库跟跳，`Location` 就得由 transport 如实
//   上报（原始值，未归一），否则 `Step.location`（plan 共享辅助表）无处可去。
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use url::{Host, Url};

use crate::agent::types::{Risk, ToolOutput};
use crate::text::head_chars;
use crate::tools::fetch_guard::{GuardError, Resolver, Target, check_url, resolve_target};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};

/// spec 5.4「明确 UA」：写死的常量，`ReqwestTransport` 的每一条请求都带着它。
/// 改动这个值等同于改动对外承诺，要在计划层面被看见，所以它是 `const` 而不是配置项。
pub const USER_AGENT: &str = "qwen-do/0.1 (desktop agent; read-only fetch)";

/// 重定向跳数上限（spec 5.4「重定向 ≤5 跳」）。`0` 是合法值：只取首跳、一条都不跟。
pub const DEFAULT_MAX_HOPS: usize = 5;

/// 响应体上限（2 MiB）。task 24 若需要更大页面，用 `with_max_bytes` 覆盖；
/// `GuardError::TooLargeBody { limit }` 报给用户的就是这个数（R248）。
pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 单跳总超时（连接 + 发送 + 读完 body）。
const HOP_TIMEOUT: Duration = Duration::from_secs(20);

/// `DeniedHostName.reason` 用的类别名（和 T22 的表同款「名词短语」形状，
/// 由 `GuardError` 的 `Display` 拼成给用户看的整句）。
const R_USERINFO: &str = "URL 凭据段伪装主机";

/// 过完全部复验、拿到最终一跳时的结果。
#[derive(Debug, Clone)]
pub struct Fetched {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// 实际取回内容的那一跳（由 walker 覆盖，transport 里填的只是占位值）。
    pub final_url: String,
    /// 跟掉了几个 `3xx`（首跳就 200 ⇒ 0）。
    pub hops: usize,
    /// 首跳主机与最终主机的**注册域**是否不同（判据见 `cross_host`）。
    pub cross_host: bool,
    /// `Location` 头的**原始**值（未归一）。归一与复验是 walker 的活（`validate_joined`），
    /// transport 只负责如实上报——`weird_locations_are_rejected_not_fetched` 测的就是这条分工。
    pub location: Option<String>,
}

/// 取一跳。实现方**必须**：不跟重定向、逐块读 body 并按 `max_bytes` 边收边判、
/// 只用传进来的 `addrs` 连接（R250：端口已由 walker 定死，transport 不猜端口）。
#[async_trait]
pub trait FetchTransport: Send + Sync {
    async fn get(
        &self,
        url: &Url,
        addrs: &[SocketAddr],
        max_bytes: usize,
    ) -> Result<Fetched, GuardError>;
}

/// 逐跳遍历器（R262：`dyn` 而非泛型——见文件头）。
pub struct RedirectWalker {
    resolver: Arc<dyn Resolver + Send + Sync>,
    transport: Arc<dyn FetchTransport>,
    max_hops: usize,
    max_bytes: usize,
}

impl RedirectWalker {
    pub fn new(
        resolver: Arc<dyn Resolver + Send + Sync>,
        transport: Arc<dyn FetchTransport>,
    ) -> Self {
        Self {
            resolver,
            transport,
            max_hops: DEFAULT_MAX_HOPS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn with_max_hops(mut self, max_hops: usize) -> Self {
        self.max_hops = max_hops;
        self
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// 走完整条重定向链取回一跳的内容：每跳复验 ⇒ 解析复验 ⇒ 钉地址取回 ⇒ 体积闸门。
    pub async fn fetch(&self, raw: &str) -> Result<Fetched, GuardError> {
        // 首跳也走同一个复验函数：「每一跳都验」不含「首跳默认可信」这种例外。
        let mut current = self.validate(raw)?;
        let start_host = host_of(&current);
        let mut hops = 0usize;

        loop {
            // 到这一步 `current` 一定已过 `check_url`；解析结果整集复验由
            // `resolve_target` 负责（`Host::Domain` 才查 DNS，字面 IP 绝不二次解析）。
            // 这条 `resolve` **必须留在循环里**（每跳重新解析 + 整集复验）：
            // 把它提升到循环外，`a_redirect_to_a_host_resolving_private_is_denied_at_the_address_level`
            // 与 `every_red_line_range_is_rechecked_as_a_redirect_target_by_address` 必红（已实测）。
            let target = self.resolve(&current).await?;
            let addrs = socket_pins(&target.url, &target.addrs)?;
            let mut res = self
                .transport
                .get(&target.url, &addrs, self.max_bytes)
                .await?;
            if res.body.len() > self.max_bytes {
                // 主闸门在 transport 的逐块读里（R248）；这一条只兜「自带缓冲、
                // 不认 `max_bytes`」的实现，不是把闸门挪到收完之后。
                return Err(GuardError::TooLargeBody {
                    limit: self.max_bytes,
                });
            }

            let following = (300..=399).contains(&res.status);
            if following {
                if let Some(loc) = res.location.as_deref().map(str::trim) {
                    // 归一 + 复验：畸形/跨方案/内网/凭据段伪装全部在这里断掉，
                    // **不会**走到下一跳的 `resolve` 或 `transport`。
                    let next = self.validate_joined(&current, loc)?;
                    if hops + 1 > self.max_hops {
                        // 回显的是「实际停下的那一跳」（已过复验的地址），不是我们
                        // 拒绝去的那个 Location——把一个没校验过的字符串回显给用户，
                        // 本身就是在造新的注入面。
                        return Err(GuardError::TooManyHops(current.as_str().to_string()));
                    }
                    hops += 1;
                    current = next;
                    continue;
                }
            }

            // 最终响应（非 3xx，或 3xx 但没给 Location ⇒ 无处可去）。
            // hops / final_url / cross_host 只有 walker 知道，覆盖 transport 的占位值。
            res.final_url = current.as_str().to_string();
            res.hops = hops;
            res.cross_host = cross_host(&start_host, &host_of(&current));
            return Ok(res);
        }
    }

    /// 单跳复验：`check_url`（方案 + 主机存在性 + 字面 IP + 名称级红线）→ 再过凭据段。
    fn validate(&self, raw: &str) -> Result<Url, GuardError> {
        let url = check_url(raw)?;
        deny_userinfo(url)
    }

    /// `Location` 一律先 `Url::join` 归一（协议相对、相对路径、绝对 URL 同一个入口），
    /// 归一完**立刻**复验。`join` 本身失败的（补不出一个 http/https 地址）按
    /// `SchemeDenied` 拒——和 `check_url` 对「解析不出来的输入」的处理同一个口径，
    /// 绝不能降级成 `Resolution`（那是「看不懂就当没危险」的口子）。
    fn validate_joined(&self, base: &Url, location: &str) -> Result<Url, GuardError> {
        let joined = base
            .join(location)
            .map_err(|_| GuardError::SchemeDenied(location.to_string()))?;
        self.validate(joined.as_str())
    }

    /// R262：同步 `Resolver` 只在 `spawn_blocking` 里跑。OS 解析慢（企业 DNS、
    /// IPv6 重试）时既不占 tokio worker，await 侧也随时能被上层放弃。
    /// 传进去的 `url` 已由 `validate` 复验过，`resolve_target` 只做地址级判定。
    async fn resolve(&self, url: &Url) -> Result<Target, GuardError> {
        let resolver = Arc::clone(&self.resolver);
        let blocking_url = url.clone();
        let host_for_error = host_of(url);
        tokio::task::spawn_blocking(move || resolve_target(&blocking_url, resolver.as_ref()))
            .await
            .map_err(|e| GuardError::Transport {
                url: host_for_error,
                reason: format!("地址解析任务被中断（不是策略拒绝）：{e}"),
            })?
    }
}

/// R250：把「已过复验的 `IpAddr` 集」配成 `SocketAddr` 交给 transport。
/// 端口**只从 URL 取**（`port_or_known_default`）：`check_url` 只放过 http/https，
/// 两者都有已知默认端口，取不到端口说明上游不变式被破坏 ⇒ fail closed，不猜。
fn socket_pins(url: &Url, addrs: &[IpAddr]) -> Result<Vec<SocketAddr>, GuardError> {
    let port = url.port_or_known_default().ok_or(GuardError::NoHost)?;
    Ok(addrs.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
}

/// 体积闸门的**唯一一处**：一块到手时判，越线就拒且**一个字节都不进缓冲区**（R248）。
///
/// 抽成自由函数有两个理由，都不是为了测试而抽：
/// 1. `ReqwestTransport` 的 `chunk()` 循环与脚本替身必须走同一条闸，否则「边收边判」
///    只在生产路径为真、在测试路径为假，那条容量测试就成了摆设；
/// 2. `chunk()` 不联网测不到，而这条能（见 `guarded_push_...` 测试）——「全收完再判」
///    的实现会在这里露出来，因为缓冲区长度就是证据。
fn guarded_push(body: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> Result<(), GuardError> {
    if body.len() + chunk.len() > max_bytes {
        return Err(GuardError::TooLargeBody { limit: max_bytes });
    }
    body.extend_from_slice(chunk);
    Ok(())
}

/// 凭据段（`user:pass@host`）拒绝。`https://ok.example@evil.example/p` 在浏览器与
/// 日志里都会被读成「去 ok.example」，实际连的是 `evil.example`——这是实测存在的
/// 绕过面，且**与地址无关**（对面是公网主机也一样拒），所以放在解析之前。
///
/// 实测（task 23 报告的探针原始输出 + 测试
/// `an_empty_userinfo_location_normalizes_to_a_plain_cross_host_hop`）：`url` crate 按
/// WHATWG 序列化时会**抹掉空 userinfo**，`https://@evil.example/p` 与
/// `https://:@evil.example/p` 都归一成 `https://evil.example/p`（`username()` 空、
/// `password()` 为 `None`）。那种写法归一后主机没有歧义，所以**不是**伪装面、按普通
/// 一跳跟随（跨主提示由 `cross_host` 负责）；真正要拦的是 `username()`/`password()`
/// 非空的形态。
fn deny_userinfo(url: Url) -> Result<Url, GuardError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(GuardError::DeniedHostName {
            host: host_of(&url),
            reason: R_USERINFO,
        });
    }
    Ok(url)
}

/// `Fetched.cross_host` 的判据：**注册域**不同才算跨主（`registrable` 见其文档）。
/// `https://a.example/x` → `https://www.a.example/y` 不算（同一注册域），
/// → `https://login.b.example/` 算。供 task 24 生成「已跳转至 <host>」提示用，
/// 本任务**不做**验证码页识别。
///
/// T24 消费侧的定位（写在这里而不是只写在 T24，因为判据的强度是这条线的属性）：
/// 那个提示是给模型的 **HINT**，**任何地方都不许拿它 gate** —— 不许因为它是假就放行、
/// 也不许因为它是真就丢掉正文。理由：`registrable` 取末两段，实测有一种形状会
/// **少报**（`foo.co.uk` 与 `bar.co.uk` 被折成同一个 `co.uk`，见
/// `registrable_is_the_last_two_labels_with_the_documented_imprecision`），
/// 而它本来就只看主机名不看内容。少报的后果是一句提示没出现，不是安全判定变松
/// （SSRF 一条地址都没少验，那是逐跳复验的职责）。
fn cross_host(from: &str, to: &str) -> bool {
    registrable(from) != registrable(to)
}

/// 「注册域」的近似取法：末两段。
///
/// **故意不处理公共后缀**（`a.co.uk`、`foo.blogspot.com` 这类 PSL 多段后缀），也
/// 故意不引 `psl` 依赖：安装包 <15MB 是验收线，而 PSL 数据表本身就几百 KB。
/// 误差方向要说清楚（这是有意的精度取舍，不是漏）：末两段会把 `foo.co.uk` 与
/// `bar.co.uk` 都折成 `co.uk`，于是**少报**一次跨主提示（多一段的公共后缀才会这样，
/// 常见 `.com/.org/.net/.io` 一律准确）。少报只影响 T24 那句提示的出现与否，
/// 不影响 SSRF 判定——安全侧一条地址都没少验。
fn registrable(host: &str) -> String {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() <= 2 {
        return h;
    }
    labels[labels.len() - 2..].join(".")
}

/// 主机身份的唯一归一处（brief Step 3 钉的取法）：`Host::Domain` 去尾点 + 小写，
/// 字面 IP 用规范形式（`Host::Ipv6` 的 `to_string()` 不带方括号）。
fn host_of(url: &Url) -> String {
    match url.host() {
        Some(Host::Domain(name)) => name.trim_end_matches('.').to_ascii_lowercase(),
        Some(Host::Ipv4(a)) => a.to_string(),
        Some(Host::Ipv6(a)) => a.to_string(),
        None => String::new(),
    }
}

/// R259：生产解析器（T22 故意留给本任务交付的那一块）。
///
/// **桥接形状**（这里是 `std::net::ToSocketAddrs` 而不是 plan 头部猜的
/// `tokio::net::lookup_host`，三条理由）：
/// 1. T22 把 `Resolver::lookup` 定成**同步**且 object-safe（`resolve_target` 收
///    `&dyn Resolver`）；在同步 trait 方法里做异步解析只能在已有 runtime 上
///    `block_on`，嵌套 `block_on` 会直接 panic。
/// 2. `lookup_host` 要端口才能拿 `SocketAddr`，而端口在这里根本不该参与解析
///    （R250：端口在 walker 侧由 URL 决定）。
/// 3. 于是异步性由**调用方**提供：`RedirectWalker::resolve` 把整次查询放进
///    `spawn_blocking`（R262），慢 DNS 既不占 worker 也不卡取消。
///
/// `ToSocketAddrs` 的 `port = 0` 只是 `getaddrinfo` 的 service 占位；取的是 `.ip()`，
/// 真正的端口由 `socket_pins` 配。空结果集**原样返回**：让 `resolve_target` 统一按
/// 「无私网地址 ≠ 可放行」归因成 `Resolution`，不在这里造第二份口径。
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, GuardError> {
        // `url` crate 交下来的域名不带方括号；这里仍防御性地归一一次，因为
        // `SystemResolver` 是 `pub` 的，别的调用方可能传 `[::1]` 或尾点形式。
        let normalized = host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let addrs = (normalized.as_str(), 0u16)
            .to_socket_addrs()
            .map_err(|e| GuardError::Resolution(host.to_string(), e.to_string()))?;
        Ok(addrs.map(|socket| socket.ip()).collect())
    }
}

/// 生产 transport：一跳一个 client，`resolve_to_addrs` 把连接地址钉死在
/// **已过复验的那组地址**上（DNS rebinding 的真防线），且一条重定向都不跟。
pub struct ReqwestTransport;

impl ReqwestTransport {
    /// 每跳新建：`resolve_to_addrs` 是按域名键控的 client 级配置，链上换了主机就
    /// 必须换 client——复用同一个 client 等于把上一跳的钉定关系带进这一跳。
    fn pinned_client(host: &str, addrs: &[SocketAddr]) -> Result<reqwest::Client, GuardError> {
        reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HOP_TIMEOUT)
            .user_agent(USER_AGENT)
            .resolve_to_addrs(host, addrs)
            .build()
            .map_err(|e| GuardError::Transport {
                url: host.to_string(),
                reason: e.to_string(),
            })
    }

    /// 生产请求的唯一构造点。UA 在 `ClientBuilder::user_agent` 与这里各写一次是
    /// 故意的：前者满足 spec 5.4，后者让「请求确实带着它」在**不联网**的测试里可读
    /// （`Client` 不暴露默认头，默认头要到 `execute_request` 才并进请求）。
    /// 请求走 `Client::execute`，所以测试读到的就是发出去的那一条。
    fn request(client: &reqwest::Client, url: &Url) -> Result<reqwest::Request, GuardError> {
        client
            .get(url.clone())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .build()
            .map_err(|e| transport_err(url, e))
    }
}

#[async_trait]
impl FetchTransport for ReqwestTransport {
    async fn get(
        &self,
        url: &Url,
        addrs: &[SocketAddr],
        max_bytes: usize,
    ) -> Result<Fetched, GuardError> {
        let host = url.host_str().ok_or(GuardError::NoHost)?;
        let client = Self::pinned_client(host, addrs)?;
        let req = Self::request(&client, url)?;
        let mut res = client
            .execute(req)
            .await
            .map_err(|e| transport_err(url, e))?;

        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        // 原样上报，不归一：畸形 `Location` 由 walker 复验（`validate_joined`）。
        // 非 ASCII 的 Location 头读成 `None` ⇒ 这一跳按最终响应处理，宁可少跟
        // 一条重定向也不去猜一个看不懂的目标。
        let location = res
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string());

        // 体积闸门：先看声明长度（能省一趟字节），再逐块累加。
        // 注意这里**不许**用 declared 长度去 `Vec::with_capacity`——那是让远端
        // 决定本机分配多少内存。
        if let Some(declared) = res.content_length() {
            if declared > max_bytes as u64 {
                return Err(GuardError::TooLargeBody { limit: max_bytes });
            }
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| transport_err(url, e))? {
            // 收够上限就断流（R248：`TooLargeBody` 只能在逐块读的循环里构造）。
            guarded_push(&mut body, &chunk, max_bytes)?;
        }

        Ok(Fetched {
            status,
            content_type,
            body,
            final_url: url.as_str().to_string(),
            hops: 0,
            cross_host: false,
            location,
        })
    }
}

/// reqwest 的错误一律归 `Transport`：**不是**策略拒绝，也**不是** DNS 故障
/// （超时/TLS/连接被断都长这样）。回显 URL，spec 风险 5。
fn transport_err(url: &Url, e: reqwest::Error) -> GuardError {
    GuardError::Transport {
        url: url.as_str().to_string(),
        reason: e.to_string(),
    }
}

// ══════════════════════════ T24：`fetch_url` ══════════════════════════
//
// 到这里为止，前面那一层只回答「这个地址能不能连、连上去取到了什么」。
// 本段回答「取到的东西怎么交给模型」：内容类型白名单、HTML→Markdown、按字符分页，
// 以及三条自述信息 —— 两条挂在正文之前（换了注册域、上游返回错误状态码），
// 一条挂在正文末尾（被截断时自报原文字符总数）。
//
// # 为什么这里有两个 HTTP client（R251 的落点，**不是**遗漏）
//
// `ToolCtx.http` 那个共享 client 本工具**一条请求都不发**，这是故意的分工：
//
// - `ctx.http` 是**进程级共享**的那一个：一条连接池、一套默认头，给别的工具
//   （以及任何不需要逐跳定制的调用点）用。共享的前提是「所有调用点接受同一份
//   client 级配置」。
// - `RedirectWalker` 要的两条恰好**做不到**「一份配置共享」：
//   `redirect(Policy::none())`（库自己一条重定向都不许跟，见文件头那条 DNS rebinding
//   的理由），以及 `resolve_to_addrs(host, addrs)` —— 而 `resolve_to_addrs` 是按**主机**
//   键控的 client 级配置，链上每一跳的主机都在换，所以 `ReqwestTransport` 只能
//   **每一跳新建一个 client**（`pinned_client`）。
//
// 一个共享 client 装不下「每跳一个主机一套钉定地址」，硬塞的结果是把上一跳的钉定关系
// 带进这一跳 —— 那正是这条防线要防的东西。所以两个 client 各干各的：本工具用
// `self.walker`，`ctx.http` 留给不需要逐跳复验的用户。
// （本段用的 `Risk`/`ToolOutput`/`Tool*`/`json!`/`head_chars` 都在文件顶部那组 `use` 里，
// 与 T23 的导入同处，不在这里重复导入。）

/// 每页默认交出去的字符数（模型一次读一页的直觉上限；`slice_page` 按**字符**不按字节）。
pub const DEFAULT_MAX_CHARS: usize = 8_000;

/// `max_chars` 的硬上限。没有这一条，模型只要写 `max_chars: 10000000` 就能把整个
/// 2 MiB 正文一次塞进上下文 —— 体积闸（`DEFAULT_MAX_BYTES`）管的是「收多少字节」，
/// 管不了「一次交多少字符」，两条是不同问题的两道闸。
pub const MAX_CHARS_CAP: usize = 24_000;

/// 内容类型白名单（spec 5.4「只处理文本形态」的正面清单）。
///
/// 三类各走各的路：`html` 过抽取器，`text/plain` 与 `application/json` 交原文
/// —— 见 `html_becomes_markdown_while_plain_text_and_json_pass_through`。
/// 清单之外一律**报错**而不是返回空文本：空文本会被模型读成「这个页面是空的」，
/// 而真相是「我们不去碰这种格式」，两者对模型的下一步完全不同（Step 1 的判据）。
///
/// **T24b 的三处改动**（判门与裁定见 `docs/findings/plan-b-fetch-quality.md`）：
/// - HTML 那一支不再用「整页过一次 Markdown 转换器」（R264：判门实测不过，正文里没有
///   「正文边界」这个概念），改成 `readability` 定界 + 把**裁剪后的片段**转 Markdown
///   （修复轮 1 的 R267：那一步用 `htmd`，理由写在 `Cargo.toml` 的 `htmd` 那行注释上）。
/// - `charset` 参数从「只用来认类型」升级为「真的参与解码」（R265）：三支共用
///   `decode_body`，全链路**没有** `from_utf8_lossy` 了 —— 解不了就报错，
///   绝不把 U+FFFD 交给抽取器或模型。
/// - R270：多收一条 `base_url` —— 就是**实际取回内容那一跳的地址**（`Fetched::final_url`）。
///   `readability` 要一条基地址才能把正文里的相对链接改写成可再取一次的绝对地址；
///   首轮这里塞的是一条占位假地址（外加「把假地址剥回去」的补丁），两条都在本轮删掉了：
///   假数据会留在交给模型的文本里，而相对链接保持相对形状时模型根本没法照着它翻页。
///   透传那两支（`text/plain`/`application/json`）**不碰** `base_url`，原文逐字照旧。
/// - R271（修复轮 2）：HTML 臂抽出**非空但低于 `MIN_ARTICLE_CHARS`** 的 Markdown 时，前面
///   挂一行「未能定界出正文……」的标记再交原文。定界失败那一支交的是降级剩下的样板，
///   不标记就等于让模型把站点 slogan/反爬提示当正文引用；标记而不是丢掉/报错的理由在
///   `MIN_ARTICLE_CHARS` 的注释上。空串那一支仍交 `NO_ARTICLE_TEXT`（另一句真话，两支没合并）。
///   透传那两支**不受这条影响**，一条标记都不许加。
///
/// 本函数是**对外那一串**（有标记时 = 标记行 + 换行 + 正文），三条臂的实际代码在
/// `render_markdown_with_label`；`FetchTool::execute` 直接走那一个，原因写在那一个的注释上。
pub fn render_markdown(
    content_type: &str,
    body: &[u8],
    base_url: &str,
) -> Result<String, ToolError> {
    let (label, markdown) = render_markdown_with_label(content_type, body, base_url)?;
    if label.is_empty() {
        Ok(markdown)
    } else {
        Ok(format!("{label}\n{markdown}"))
    }
}

/// 与 `render_markdown` 同一条链，但把「R271 的标记句」和「可分页的正文」**分开交**：
/// 第一个元素是标记句，空串 = 这一支没有标记（正文达标 / 透传两支 / 抽不出正文那一支）。
///
/// 为什么要多这一个入口，而不是让 `execute` 继续切 `render_markdown` 那一串：黏在一起之后，
/// 标记句就占进了分页的预算，于是三条对外承诺会一起漏 ——
/// ①`max_chars` 比标记句还短时，被切掉的正是「别当正文引用」那半句，交出去的是半句真话；
/// ②`slice_page` 的 `total` 把标记句自己的字数也算进「原文 N 字符」，同篇正文报出两个 N；
/// ③第 1 页替标记句吃掉的字符不算在正文里 ⇒ 模型按工具描述那句
/// 「续页就填 offset = 上次 offset + max_chars」续页时读到重叠内容。
/// 判据是「标记句不参与计数、不被截断、偏移只数正文」，只有**分页之前**分开才做得到。
fn render_markdown_with_label(
    content_type: &str,
    body: &[u8],
    base_url: &str,
) -> Result<(String, String), ToolError> {
    let mt = media_type_of(content_type);
    match mt.as_str() {
        "text/html" | "application/xhtml+xml" => {
            let html = decode_body(content_type, body, true)?;
            let markdown = extract_article_markdown(&html, &mt, base_url)?;
            // 空串是假话（「页面是空的」），整页转换是真话但更坏（一堆链接与 JS）。
            // 这一句和 `slice_page` 的「正文为空」同一取向：把失败原因交给模型。
            if markdown.trim().is_empty() {
                Ok((String::new(), NO_ARTICLE_TEXT.to_string()))
            } else {
                // R271：非空但**低于正文下限** ⇒ 定界这一步其实没成功，交出去的是降级
                // 剩下的站点样板/反爬提示。这一支**不丢文本**（模型还得自己判），只在前面
                // 挂一行说明「这不是定界出来的正文」——评审点名的失败形状是模型会照原话答
                // 「这一页讲的是软媒旗下科技门户网站」，缺的就是这一句。
                // 选「标记」而不是「报错/丢掉」的理由写在 `MIN_ARTICLE_CHARS` 上。
                let n = markdown.chars().count();
                if n < MIN_ARTICLE_CHARS {
                    Ok((sub_floor_label(n), markdown))
                } else {
                    Ok((String::new(), markdown))
                }
            }
        }
        // 原文透传：纯文本的空行与缩进、JSON 的结构本身就是内容，过一次 markdown
        // 转换器只会把它们变成装饰或丢掉。**解码**仍共用同一个辅助（判据 3）。
        "text/plain" | "application/json" => {
            Ok((String::new(), decode_body(content_type, body, false)?))
        }
        other => Err(ToolError::Failed(format!(
            "content-type「{other}」不在白名单内：fetch_url 只取 text/html、text/plain、application/json 三种格式"
        ))),
    }
}

/// `readability` 定界失败（抽不出正文）时交给模型的那一句。
const NO_ARTICLE_TEXT: &str =
    "[这一页抽不出正文（可能是脚本渲染的页面），可以试 text/plain 的原始来源或换一个 URL]";

/// R271：正文下限（**字符**数，不是字节数）。HTML 臂抽出的 Markdown 非空但低于这条线时，
/// 前面挂一行「这不是定界出来的正文」的标记，**文本照样交出去**。
///
/// 为什么阈值这一侧选「标记」而不是「报错/丢掉」：**标记的假阴性没有代价**（一页真短文
/// 只是多了一行说明，内容一个字没少），**丢掉的假阴性有代价**（一页真短文被吞掉，模型和
/// 用户都无从发现它被吞了）。而 `readability 0.3.0` 的 `Product` 只有 `{title, content}`、
/// **没有置信度**可用，判不出「有没有正文」，只能判「少不少」—— 少的时候说实话，
/// 不假装它不存在。
///
/// 200 这个数是照着真网六页定的：qbitai 6 136、overreacted 19 138 远高于线（不标记），
/// ithome 187、36kr 52 低于线（标记）—— 被标记的正好是评审点名的那两页样板/反爬提示。
/// **只作用于 HTML 臂**：`text/plain` 与 `application/json` 是逐字透传，一条标记都不许加
/// （`text_plain_below_the_floor_gets_no_label_at_all` 钉住）。
pub const MIN_ARTICLE_CHARS: usize = 200;

/// 低于 `MIN_ARTICLE_CHARS` 时挂在正文前面那一行（`{n}` = 后面交出去的字符数）。
/// 句子是 R271 定的逐字文案，改它等于改对外承诺。
fn sub_floor_label(n: usize) -> String {
    format!("[未能定界出正文，以下 {n} 字可能是站点样板或反爬提示，别当正文引用]")
}

/// 按**声明**的字符集把字节解成文本；解不了就报错，绝不 lossy（R265 的三条判据）。
///
/// 1. HTTP `Content-Type` 里有 `charset=` ⇒ 按声明解（`encoding_rs` 的 LABEL 表，
///    gb2312/gbk/big5/shift_jis/windows-1252/… 都在里面）。
/// 2. 声明缺失、或声明与内容矛盾时：HTML 页**再读一次** `<meta charset>`（同 LABEL 表）。
///    认不出的标签 ⇒ 明确报错「这个页面声明的字符集 X 我不解」，**不**走 `from_utf8_lossy`：
///    静默乱码比报错坏，模型会把乱码当内容引用。
/// 3. 非 HTML 两支（`text/plain`/`application/json`）没有 `<meta>` 可读，缺声明时先试
///    严格 UTF-8，再按 WHATWG 给 `text/*` 的缺省 `windows-1252` 解 —— 那一趟逐字节都有出处、
///    **不产 U+FFFD**，这是它和 lossy 的区别；给了声明却对不上 ⇒ 报错（同上）。
fn decode_body(content_type: &str, body: &[u8], is_html: bool) -> Result<String, ToolError> {
    let declared = charset_param(content_type);
    if let Some(label) = declared.as_deref() {
        let enc = encoding_for(label)?;
        if let Some(text) = enc.decode_without_bom_handling_and_without_replacement(body) {
            return Ok(text.into_owned());
        }
        if !is_html {
            return Err(ToolError::Failed(format!(
                "这个页面声明的字符集「{label}」和取回的字节对不上，我不猜：硬解出来的字符没有出处。"
            )));
        }
    } else if !is_html {
        return match std::str::from_utf8(body) {
            Ok(text) => Ok(text.to_string()),
            // 没有声明、又不是合法 UTF-8 ⇒ 按 WHATWG 给 `text/*` 的缺省 windows-1252 解。
            // 它**不**是 lossy：表里那 5 个未定义码位（0x81/0x8D/0x8F/0x90/0x9D）在这里
            // 直接报错，而不是换成 U+FFFD 交出去。
            Err(_) => encoding_rs::WINDOWS_1252
                .decode_without_bom_handling_and_without_replacement(body)
                .map(|text| text.into_owned())
                .ok_or_else(|| {
                    ToolError::Failed(
                    "这一页没有字符集声明、也不是合法 UTF-8，按缺省的 windows-1252 也解不出来，\
                     我不猜：猜出来的乱码会被当正文引用。"
                        .to_string(),
                )
                }),
        };
    }

    // HTML：没有声明、或声明与内容矛盾 ⇒ `<meta charset>` 再试一次（判据 2）。
    if let Some(label) = meta_charset(body) {
        let enc = encoding_for(&label)?;
        if let Some(text) = enc.decode_without_bom_handling_and_without_replacement(body) {
            return Ok(text.into_owned());
        }
    }
    match std::str::from_utf8(body) {
        Ok(text) => Ok(text.to_string()),
        Err(_) => Err(ToolError::Failed(
            "这一页既没有可用的字符集声明、也不是合法 UTF-8，我不猜：猜出来的乱码会被当正文引用。"
                .to_string(),
        )),
    }
}

/// LABEL → 解码器。认不出来就是**错误**，不是「换个编码再试试」。
fn encoding_for(label: &str) -> Result<&'static encoding_rs::Encoding, ToolError> {
    encoding_rs::Encoding::for_label(label.as_bytes()).ok_or_else(|| {
        ToolError::Failed(format!(
            "这个页面声明的字符集「{label}」我不解：它不在 encoding_rs 的 LABEL 表里。"
        ))
    })
}

/// `Content-Type` 里的 `charset=` 参数（小写、去引号）。`media_type_of` 把它丢掉是
/// 白名单的口径（只认类型），这里单独把它读回来 —— 两条互不影响。
fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("charset") {
            let value = value.trim().trim_matches(['\'', '"']);
            (!value.is_empty()).then(|| value.to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// 从前 2 KiB 里读 `<meta … charset=X …>`（判据 2 的那「一次」）。
/// 只在字节层扫 ASCII 标签名 —— 任何字符集下 `<meta` 和 `charset=` 都是 ASCII；
/// UTF-16 之类的页扫不到 `<meta`，那就走「没有声明」那支。
fn meta_charset(body: &[u8]) -> Option<String> {
    let lower = body[..body.len().min(2048)].to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(meta) = find_window(&lower, b"<meta", from) {
        // 标签正文到第一个 `>` 为止（没闭合就到窗口末尾，浏览器也这么宽容）。
        let end = find_window(&lower[meta..], b">", 0)
            .map(|i| meta + i)
            .unwrap_or(lower.len());
        if let Some(cs) = find_window(&lower[meta..end], b"charset=", 0) {
            let start = meta + cs + b"charset=".len();
            let label: Vec<u8> = lower[start..end]
                .iter()
                .copied()
                .take_while(|b| {
                    !matches!(
                        b,
                        b'"' | b'\'' | b';' | b'=' | b',' | b' ' | b'\t' | b'\r' | b'\n'
                    )
                })
                .collect();
            // 标签本来就是 ASCII：解不出来（畸形字节）就当没有声明，而不是硬造一个名字。
            if let Ok(label) = String::from_utf8(label)
                && !label.is_empty()
            {
                return Some(label);
            }
        }
        from = meta + b"<meta".len();
    }
    None
}

/// `haystack` 里 `needle` 第一次出现的位置（从 `from` 起）。
fn find_window(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// `readability` 定界 + 把**裁剪后的正文片段**转 Markdown；只用正文那一段。
fn extract_article_markdown(html: &str, mt: &str, base_url: &str) -> Result<String, ToolError> {
    // R270：基地址是**真实**的那一条（调用方给 `Fetched::final_url`），不是首轮的占位假地址。
    // 解析不出来就报错，**不**降级成假基地址：相对链接按一个不存在的站点展开，交出去的是
    // 一条模型会照着去取的错链接，比这一页失败更难发现。
    let base = Url::parse(base_url).map_err(|e| ToolError::Failed(format!(
        "content-type 是 {mt}，但这一页的地址「{base_url}」解析不出来（{e}），我不拿一条假地址去改写相对链接"
    )))?;
    let mut reader: &[u8] = html.as_bytes();
    let product = readability::extractor::extract(&mut reader, &base)
        .map_err(|e| ToolError::Failed(format!("content-type 是 {mt}，但正文抽取失败：{e}")))?;
    // 这个 crate 的 `extract` 不返回 `Option`（它总交回一个 `Product`）：「没有可辨识正文」
    // 在它这儿表现为**降级成整份文档**，`<head>`（title、站点名、内联样式）会留在
    // `content` 里。所以这里只取 `<body>` 的内层，`Product.title` 一个字都不用 ——
    // 拼进去模型就当页面内容引用了（brief Step 1 判据 2）。
    let body_html = article_body_html(&product.content);
    // `htmd::convert` 返回 `Result`，所以这一步的失败**上报**而不是交回一个空串：
    // 空串会走到 `render_markdown` 的「抽不出正文」那一句，那是假话 —— 片段是有内容的，
    // 坏的是转换这一步。
    htmd::convert(&body_html).map_err(|e| {
        ToolError::Failed(format!(
            "content-type 是 {mt}，正文已定界，但转成 Markdown 失败：{e}"
        ))
    })
}

/// 取序列化结果里 `<body>…</body>` 的内层；没有 body 标签（选中了某个 div 当正文）时
/// 原样交回 —— 那种情况下 `content` 本来就是正文片段。
fn article_body_html(content: &str) -> String {
    let lower = content.to_ascii_lowercase();
    let (Some(open), Some(close)) = (lower.find("<body"), lower.rfind("</body")) else {
        return content.to_string();
    };
    let Some(start) = lower[open..].find('>').map(|i| open + i + 1) else {
        return content.to_string();
    };
    if start >= close {
        return String::new();
    }
    // `start`/`close` 都落在 ASCII 标签名的字节上，切片不会切在字符中间。
    content[start..close].to_string()
}

/// 媒体类型归一：取 `;` 前那段、去空白、小写。
/// 参数（`charset=…`）在这里被丢掉是**有意的**——白名单只认类型，不认参数；
/// 参数由 `charset_param` 单独读回来参与**解码**（R265），两件事各有入口、互不影响。
fn media_type_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// 按**字符**切一页，并把「还有没有下文」写进结果里。
///
/// 三条口径都是测试钉死的对外承诺（Step 2）：
/// 1. 单位是字符不是字节：中日韩一个字 3 字节，按字节切既不符合模型对「第 8000 字」
///    的直觉，又必然落在 UTF-8 边界上 panic（`string[..n]` 的经典事故），而 panic 恰好
///    发生在「内容太大所以要切」这支 —— 最不该 panic 的时刻。
/// 2. 截断标记**自报原文字符总数**（`total_note=true`）：不带总数的「已截断」让模型无法
///    决定还要不要翻页、翻几次。`total_note=false` 留给正文前已经说过总数的调用方。
/// 3. 取到末尾就不挂标记（否则模型以为还有）；offset 越界单独说一句并带上总数
///    （交空串等于让模型自己猜是取空了还是越界了）；正文整体为空也单独一句，
///    因为对 offset 0 说「超出范围」是假话 —— SPA 空壳抽不出文本时就是这个形状。
pub fn slice_page(text: &str, offset: usize, max_chars: usize, total_note: bool) -> String {
    let total = text.chars().count();
    if total == 0 {
        return "[正文为空：上游文档里没有可交给模型的文本（SPA 空壳、纯脚本页、空 body 都走到这里）]".to_string();
    }
    if offset >= total {
        return format!("[offset {offset} 超出范围：原文共 {total} 字符]");
    }
    // 复用 `crate::text::head_chars`：全仓库唯一的「按字符边界切 + 报丢弃多少」。
    // 代价是先把剩余部分 collect 出来 —— 在 2 MiB 字节闸之下这次分配无所谓，
    // 而在本文件再写一份 `chars().skip().take()` 就造出了第二个切法（R77 的反面）。
    let rest: String = text.chars().skip(offset).collect();
    let (head, dropped) = head_chars(&rest, max_chars);
    if dropped == 0 {
        head
    } else if total_note {
        format!("{head}[已截断，原文 {total} 字符]")
    } else {
        format!("{head}[已截断]")
    }
}

/// 正文前那几行提示（最多两条），以及它们的出现条件。
///
/// 同域跳转**不产生任何行**：`Fetched.cross_host` 为假时正文就是正文，前面不挂噪声
/// （brief Step 3 那句）。判据在测试里写成「输出与正文逐字相等」。
fn prefix_lines(f: &Fetched) -> Vec<String> {
    let mut lines = Vec::new();
    // `cross_host` 在前：同域那一跳根本不必去解析主机名。解析不出主机就不加这一行
    // （见 `landing_host`），所以这里是「且」而不是嵌套 `if`。
    if f.cross_host
        && let Some(host) = landing_host(&f.final_url)
    {
        // 反爬/登录页场景的判据是「明示最终主机」，不是「识别验证码页」——
        // 后者我们做不到，假装做到就是在给用户假信息（T23 已写明）。
        lines.push(format!("[已跳转至 {host}]"));
    }
    // brief 点名 403/429；实现按**整个错误状态类**（≥400）而不是按这两个码：
    // 404/500 的 error page 不带状态行喂进去，模型无从和真正文区分。
    // 状态码写在行里，所以它也必然出现在 `preview`（前 200 字，`agent::types`）里 ——
    // 卡片上看不见的状态等于没告诉模型「这是被拦了」。
    if f.status >= 400 {
        lines.push(format!("[上游返回 {}，内容可能受限]", f.status));
    }
    lines
}

/// 提示行里的主机名：复用 `host_of`（全文件唯一的主机归一处，去尾点 + 小写）。
/// 解析不出或没有主机 ⇒ 不加这一行：宁可少一句提示，也不在正文前挂一个乱码主机。
fn landing_host(final_url: &str) -> Option<String> {
    Url::parse(final_url)
        .ok()
        .map(|u| host_of(&u))
        .filter(|h| !h.is_empty())
}

/// 可选的非负整数入参。缺省与显式 `null` 都按「没给」处理（模型常拿 `null` 填默认值）；
/// 给了但形状不对（负数、字符串、小数）一律 `BadInput` 而不是静默按默认值跑 ——
/// 静默之下模型以为自己的 `offset` 生效了，下一页它会读到重复内容。
fn optional_uint(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(n as usize)),
            None => Err(ToolError::BadInput(format!(
                "{key} 必须是非负整数（按字符计），实得 {v}"
            ))),
        },
    }
}

/// `fetch_url`：只读地取一个公网页面，交给模型 Markdown/原文。
///
/// R262：本类型**没有**泛型参数（plan 与早期台账猜的
/// `FetchTool<R = SystemResolver, T = ReqwestTransport>` 形状作废）。可注入性由
/// `RedirectWalker` 内部的 `Arc<dyn Resolver>` + `Arc<dyn FetchTransport>` 提供，
/// 测试替身在这一层注入：本文件的内联 `mod tests` 与生产同模块，可以直接构造私有字段
/// （R251：生产类型上**不开** `pub fn with_transport`）。
pub struct FetchTool {
    walker: RedirectWalker,
}

impl FetchTool {
    /// 生产接线：`SystemResolver`（R259，同步解析 + `spawn_blocking`）+
    /// `ReqwestTransport`（每跳新建 pinned client、一条重定向都不跟）。
    /// 构造本身不联网：`Client` 只建配置，请求要到 `execute` 才发。
    pub fn new() -> Self {
        Self {
            walker: RedirectWalker::new(Arc::new(SystemResolver), Arc::new(ReqwestTransport)),
        }
    }
}

/// 与 `new()` 同一条生产接线（`clippy::new_without_default`）。注册表那边（T27）拿
/// `FetchTool::default()` 与 `FetchTool::new()` 是同一个东西，不留两种写法。
impl Default for FetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FetchTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "fetch_url".to_string(),
            // 分页单位必须写在描述里：`read_file`（T25）按**行**分页，本工具按**字符**，
            // 而模型只看得到 description（plan Task 25 那条「两处各自写清单位」的要求）。
            description: "抓取一个公网网页/文本并交给模型。只接受 http/https，内网、环回、\
                          云元数据主机名一律拒绝（SSRF 红线）。分页按【字符】计，不是行、\
                          不是字节：`offset` 是起始字符下标（从 0 开始），`max_chars` 是这一页\
                          最多交多少个字符（默认 8000，上限 24000）；正文被截断时末尾会带\
                          「[已截断，原文 N 字符]」，续页就填 offset = 上次 offset + max_chars。\
                          只处理 text/html（转成 Markdown）、text/plain 与 application/json（原文）；\
                          图片、PDF 等格式返回一条明确的拒绝消息。换了注册域或上游返回错误状态码时，\
                          正文前会有一行提示。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "完整地址，必须带 http:// 或 https:// 方案"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "从第几个字符开始交（按字符，不是行不是字节），默认 0"
                    },
                    "max_chars": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "这一页最多交多少个字符，默认 8000，上限 24000"
                    }
                },
                "required": ["url"]
            }),
            // Read：spec 5.1「内置工具全为 Read ⇒ 全程零弹窗」的那一条就落在这个返回值上。
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // 注册表也查一次取消（R53）；这里是工具侧那一次，为了在**进 walker 之前**停下。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // 入参校验全部排在发请求之前：`a_missing_or_non_string_url_is_bad_input_before_any_request`
        // 用的脚本是 0 步的，真去敲 transport 就会 panic 在「吐完了还请求」。
        let raw = input
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::BadInput("fetch_url 需要非空字符串参数 url".to_string()))?;
        let offset = optional_uint(&input, "offset")?.unwrap_or(0);
        let max_chars = optional_uint(&input, "max_chars")?
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_CHARS_CAP);

        // 安全判决原样上抛：`GuardError` 的 `Display` 就是给用户看的那句人话（回显地址/
        // 主机名，spec 风险 5），包装时不许换成 "fetch failed" 那种糊话。
        let f = self
            .walker
            .fetch(raw)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;

        // 取消臂只补「取回之后」这一段：抽取 + 分页是纯 CPU，用户按了停止就没必要再算
        // 一遍并把它塞进 history。DNS/请求在途的取消仍不在本任务范围（T23 台账：
        // 注册表的 execute 前守卫 + 每跳 20s 超时已覆盖用户可见路径）。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // R270：基地址用 `final_url`（实际取回内容那一跳），**不是**用户请求里那条：
        // 正文里的相对链接是相对于** Serving 这个内容的那个地址**的，按请求地址展开
        // 会指到另一台机器上的同类路径。
        // 标记句与正文**分开取**（为什么要有第二个入口，写在 `render_markdown_with_label`
        // 的注释上）：分页只吃正文。标记句挂在 `slice_page` 之外、且只随第 1 页出现 ——
        // 「以下 N 字」说的是正文的开头，续页再挂它就成了假话（那一页以下的字数与 N 无关）。
        let (label, text) = render_markdown_with_label(&f.content_type, &f.body, &f.final_url)?;
        let mut out = String::new();
        for line in prefix_lines(&f) {
            out.push_str(&line);
            out.push('\n');
        }
        if offset == 0 && !label.is_empty() {
            out.push_str(&label);
            out.push('\n');
        }
        out.push_str(&slice_page(&text, offset, max_chars, true));
        Ok(ToolOutput::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    // ———————— 测试替身（plan 共享辅助表逐个签名钉死；只住本模块，不进生产） ————————

    /// 一次脚本化的响应。`chunks` 逐块吐，用来测「边收边判容量」。
    #[derive(Debug, Clone)]
    struct Step {
        status: u16,
        content_type: String,
        chunks: Vec<Vec<u8>>,
        location: Option<String>,
    }

    /// `url`/`from` 形参按共享辅助表的原签名保留，但**刻意不用**：脚本是按调用
    /// 次序吐 `Step` 的，「第几跳在哪个地址」由 walker 决定，替身不许自己记账。
    fn ok200(url: &str, ct: &str, body: &[u8]) -> Step {
        let _ = url;
        Step {
            status: 200,
            content_type: ct.to_string(),
            chunks: vec![body.to_vec()],
            location: None,
        }
    }

    fn ok302(from: &str, location: &str) -> Step {
        let _ = from;
        Step {
            status: 302,
            content_type: "text/html".to_string(),
            chunks: vec![],
            location: Some(location.to_string()),
        }
    }

    /// 原样吐 `Location`，**不做**任何归一或转义 —— 畸形与跨方案的绕过面就靠这个形状测。
    fn redirect_with_raw_location(from: &str, raw_location: &str) -> Step {
        let _ = from;
        Step {
            status: 302,
            content_type: "text/html".to_string(),
            chunks: vec![],
            location: Some(raw_location.to_string()),
        }
    }

    /// 声明总量 `total_bytes`、按 64 KiB 一块块吐：实现若是「全收完再判长度」，
    /// 这条替身会真的把 3 MiB 堆进内存（brief Step 1 第 3 条注释说的就是这个）。
    fn chunked_ok200(url: &str, ct: &str, total_bytes: usize) -> Step {
        let _ = url;
        const CHUNK: usize = 64 * 1024;
        let mut chunks = Vec::new();
        let mut left = total_bytes;
        while left > 0 {
            let n = CHUNK.min(left);
            chunks.push(vec![b'x'; n]);
            left -= n;
        }
        Step {
            status: 200,
            content_type: ct.to_string(),
            chunks,
            location: None,
        }
    }

    struct ScriptedTransport {
        steps: Vec<Step>,
        next: Mutex<usize>,
    }

    impl ScriptedTransport {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps,
                next: Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl FetchTransport for ScriptedTransport {
        async fn get(
            &self,
            url: &Url,
            addrs: &[SocketAddr],
            max_bytes: usize,
        ) -> Result<Fetched, GuardError> {
            let idx = {
                // 只在取号时持锁，别把锁带进 await。
                let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
                let i = *guard;
                *guard += 1;
                i
            };
            let step = self.steps.get(idx).unwrap_or_else(|| {
                panic!(
                    "吐完了还请求：第 {} 次调用，脚本只有 {} 步",
                    idx + 1,
                    self.steps.len()
                )
            });
            assert!(
                !addrs.is_empty(),
                "R250：walker 必须把**已过复验**的地址成对交给 transport，transport 不猜端口"
            );
            // 容量闸门与生产路径**同一条**（`guarded_push`）：替身不许自己造一份
            // 「收完再判」的形状，否则那条容量测试就是在测替身。
            let mut body = Vec::new();
            for chunk in &step.chunks {
                guarded_push(&mut body, chunk, max_bytes)?;
            }
            Ok(Fetched {
                status: step.status,
                content_type: step.content_type.clone(),
                body,
                // hops / final_url / cross_host 只有 walker 知道，transport 只填占位值。
                final_url: url.as_str().to_string(),
                hops: 0,
                cross_host: false,
                location: step.location.clone(),
            })
        }
    }

    /// 任何主机一律返回同一个公网地址（共享辅助表钉死的值）。
    struct PublicResolver;

    impl Resolver for PublicResolver {
        fn lookup(&self, _host: &str) -> Result<Vec<IpAddr>, GuardError> {
            Ok(vec!["93.184.216.34".parse().unwrap()])
        }
    }

    /// 能**按主机给出不同答案**的解析器：`ok.*` 给公网地址，其余主机给 `private`。
    /// `PublicResolver` 表达不出的形状就靠它钉住「hop ≥ 2 的地址级复验真的在跑」：
    /// 若把 walker 循环里的 `self.resolve` 提升到循环外（每跳复用首跳的地址集），
    /// 全公网替身下所有测试照绿，只有按主机差异作答的解析器能让这种实现变红。
    struct Selective {
        private: IpAddr,
    }

    impl Resolver for Selective {
        fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, GuardError> {
            let h = host.trim_end_matches('.').to_ascii_lowercase();
            if h.starts_with("ok.") {
                Ok(vec!["93.184.216.34".parse().unwrap()])
            } else {
                Ok(vec![self.private])
            }
        }
    }

    // ————————————————————— Step 1：每跳复验 —————————————————————

    #[tokio::test]
    async fn a_redirect_into_loopback_is_denied() {
        // 302 链：public → 127.0.0.1。老写法（让 reqwest 自己跟）会直接把内网内容取回来。
        let t =
            ScriptedTransport::new(vec![ok302("https://ok.example/", "http://127.0.0.1/admin")]);
        let w = RedirectWalker::new(Arc::new(PublicResolver), Arc::new(t))
            .with_max_hops(5)
            .with_max_bytes(2 * 1024 * 1024);
        let e = w.fetch("https://ok.example/").await.unwrap_err();
        assert!(
            matches!(e, GuardError::Denied { addr, .. } if addr.to_string() == "127.0.0.1"),
            "重定向后的内网目标必须拒绝并点名，实得 {e:?}"
        );
    }

    #[tokio::test]
    async fn hop_budget_is_enforced_and_names_the_last_url() {
        let mut chain = vec![ok302("https://a.test/1", "https://b.test/2"); 6];
        chain.extend(vec![ok200("https://b.test/2", "text/html", b"never")]);
        let w = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(chain)),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        let e = w.fetch("https://a.test/1").await.unwrap_err();
        assert!(
            matches!(&e, GuardError::TooManyHops(u) if u == "https://b.test/2"),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn oversize_body_is_stopped_at_the_cap_not_buffered_then_checked() {
        // 先声明超长再分块给：实现若是「全收完再判长度」，这条会 OOM 而不是报容量
        let w = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(vec![chunked_ok200(
                "https://big.test/",
                "text/plain",
                3 * 1024 * 1024,
            )])),
        )
        .with_max_hops(5)
        .with_max_bytes(2 * 1024 * 1024);
        let e = w.fetch("https://big.test/").await.unwrap_err();
        assert!(
            format!("{e:?}").contains("2097152"),
            "容量错误要报出上限，实得 {e:?}"
        );
    }

    /// 上一条只能在**单跳**形状上证明「上限被传下去并报了数」；「边收边判」这件事
    /// 本身由这条钉（`chunk()` 不联网测不到，`guarded_push` 能）：越线的那一块**不进
    /// 缓冲区**，所以「全收完再判」的实现在这里必红（缓冲区长度就是证据）。
    #[test]
    fn guarded_push_refuses_the_chunk_that_crosses_the_cap_without_growing_the_buffer() {
        const CAP: usize = 2 * 1024 * 1024; // = 2_097_152，与容量测试那条上限同一个数
        // **刚好收满上限**必须放行（判据写成 `>=` 就是误杀合法页面）。
        let mut full: Vec<u8> = vec![b'a'; CAP - 2];
        guarded_push(&mut full, b"ok", CAP).unwrap();
        assert_eq!(
            full.len(),
            CAP,
            "刚好到上限必须放行（否则就是误杀合法页面）"
        );

        // 再多一个字节：整块被拒，且缓冲区**一个字节都没长** —— 「收完再判」在这里露形。
        let mut empty: Vec<u8> = Vec::new();
        let big = vec![b'x'; 3 * 1024 * 1024];
        let e = guarded_push(&mut empty, &big, CAP).unwrap_err();
        assert!(
            matches!(e, GuardError::TooLargeBody { limit } if limit == CAP),
            "越线的那一块必须整块被拒并报出上限，实得 {e:?}"
        );
        assert!(empty.is_empty(), "超上限的字节一个都不许进缓冲区");

        let mut at_cap: Vec<u8> = vec![b'a'; CAP];
        let e2 = guarded_push(&mut at_cap, b"x", CAP).unwrap_err();
        assert!(
            matches!(e2, GuardError::TooLargeBody { limit } if limit == CAP),
            "{e2:?}"
        );
        assert_eq!(at_cap.len(), CAP, "越线后长度必须还停在上限，不是上限+1");
        assert!(format!("{e2:?}").contains("2097152"), "{e2:?}");
    }

    // ————————————————————— Step 2：畸形 Location —————————————————————

    #[tokio::test]
    async fn weird_locations_are_rejected_not_fetched() {
        for (loc, why) in [
            ("file:///C:/windows/win.ini", "跨方案"),
            ("gopher://x/", "跨方案"),
            ("//127.0.0.1/x", "协议相对也要补出方案再判"),
            ("http://127.0.0.1\\@ok.example/", "user-info 伪装主机"),
        ] {
            let w = RedirectWalker::new(
                Arc::new(PublicResolver),
                Arc::new(ScriptedTransport::new(vec![redirect_with_raw_location(
                    "https://ok.example/",
                    loc,
                )])),
            )
            .with_max_hops(5)
            .with_max_bytes(1024);
            let e = w.fetch("https://ok.example/").await.unwrap_err();
            assert!(
                !matches!(e, GuardError::Resolution(..)),
                "{why}：不许因为解析不了就退化成放行，实得 {e:?}"
            );
        }
    }

    /// Step 2 的补强：凭据段伪装主机是**实测存在**的绕过面，必须在解析之前按名字拒。
    /// 主机本身是公网名（`evil.example` + `PublicResolver`），所以只有凭据段这一条闸能用。
    /// 脚本只有 1 步：真去跟第二跳就会 panic 在「吐完了还请求」。
    #[tokio::test]
    async fn a_location_carrying_userinfo_is_denied_by_name() {
        let w = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(vec![redirect_with_raw_location(
                "https://ok.example/",
                "https://ok.example@evil.example/p",
            )])),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        let e = w.fetch("https://ok.example/").await.unwrap_err();
        assert!(
            matches!(&e, GuardError::DeniedHostName { host, .. } if host == "evil.example"),
            "凭据段必须按「真正要去连的那个主机」点名拒绝，实得 {e:?}"
        );
        // 同一道闸也管首跳本身（首跳不是「免检区」）。
        let first = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(vec![])),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        assert!(matches!(
            first.fetch("https://ok.example@1.1.1.1/").await,
            Err(GuardError::DeniedHostName { .. })
        ));
    }

    /// 实测：WHATWG 归一会**抹掉空 userinfo**（`https://@h/p` ⇒ `https://h/p`），
    /// 归一后主机没有歧义 ⇒ 不是伪装面，按普通一跳跟随并报 `cross_host`。
    /// 这条钉住「闸拦的是有歧义的凭据段，不是所有带 `@` 的字面量」，防止日后
    /// 有人把它收紧成「见 @ 就拒」而误杀合法站点。
    #[tokio::test]
    async fn an_empty_userinfo_location_normalizes_to_a_plain_cross_host_hop() {
        for loc in ["https://@evil.example/p", "https://:@evil.example/p"] {
            let w = RedirectWalker::new(
                Arc::new(PublicResolver),
                Arc::new(ScriptedTransport::new(vec![
                    redirect_with_raw_location("https://ok.example/", loc),
                    ok200("https://evil.example/p", "text/html", b"served"),
                ])),
            )
            .with_max_hops(5)
            .with_max_bytes(1024);
            let f = w.fetch("https://ok.example/").await.unwrap();
            assert_eq!(
                f.final_url, "https://evil.example/p",
                "{loc} 归一后的形状不对"
            );
            assert!(f.cross_host, "空 userinfo 归一后仍是换注册域，必须报跨主");
            assert_eq!(f.hops, 1);
        }
    }

    /// 硬要求的正面判据：`resolve_target` 的 `Host::Domain` 一支**不重做名称级判定**
    /// （task 22 报告 deferred (2)），所以「每一跳、解析之前先 `check_url`」只能靠 walker
    /// 守。`http://localhost/` 这类**名字**危险、解析结果无害（替身给的是公网 IP）的目标，
    /// 只有这条闸能拦 —— 把 `validate_joined` 里的 `check_url` 摘掉，本测试必红（已实测，
    /// 见 task 23 报告的变异核验）。
    #[tokio::test]
    async fn a_redirect_into_a_loopback_or_metadata_hostname_is_denied() {
        for (loc, want) in [
            ("http://localhost/admin", "localhost"),
            ("http://api.localhost/x", "api.localhost"),
            (
                "http://metadata.google.internal/latest/meta-data/",
                "metadata.google.internal",
            ),
            // 尾点 + 大写：归一必须发生在名称闸之前，否则 `LOCALHOST.` 就溜过去了。
            ("http://LOCALHOST./x", "localhost"),
        ] {
            let w = RedirectWalker::new(
                Arc::new(PublicResolver),
                Arc::new(ScriptedTransport::new(vec![redirect_with_raw_location(
                    "https://ok.example/",
                    loc,
                )])),
            )
            .with_max_hops(5)
            .with_max_bytes(1024);
            let e = w.fetch("https://ok.example/").await.unwrap_err();
            assert!(
                matches!(&e, GuardError::DeniedHostName { host, .. }
                    if host.trim_end_matches('.') == want),
                "{loc} 必须在解析之前按主机名点名拒绝（期望点名 {want}），实得 {e:?}"
            );
            // 尾点写法照样被拒，但 `GuardError` 回显的是**归一前**的那个名字
            // （`DeniedHostName { host: "localhost." }`）—— T36 的 UI 文案若要做
            // 等值比较，得自己 `trim_end_matches('.')`。本任务不动 T22 的判定形状。
            if loc.contains("LOCALHOST.") {
                assert!(
                    matches!(&e, GuardError::DeniedHostName { host, .. } if host == "localhost."),
                    "尾点回显形状变了，T36 的消费口径要跟着改：{e:?}"
                );
            }
        }
    }

    /// 审查发现补强：第二跳的**名字无害**（`check_url` 放行），只有「对重解析结果做
    /// 地址级复验」这一道闸能拦住它 —— `PublicResolver` 恒答公网，表达不出这个形状。
    /// 把 `self.resolve` 从 walker 循环里提升到循环外（只在首跳解析一次），本测试必红：
    /// walker 会带着**过期的首跳目标**去敲 transport 的第 2 次调用（脚本只有 1 步，
    /// panic 即证据），永远等不到 `PrivateInDnsSet`。transport 恰好只被调 1 次是
    /// 「私网第二 hop 死在解析复验、请求没发出去」的判据。
    #[tokio::test]
    async fn a_redirect_to_a_host_resolving_private_is_denied_at_the_address_level() {
        let t = Arc::new(ScriptedTransport::new(vec![ok302(
            "https://ok.example/",
            "https://rebind.example/x",
        )]));
        let w = RedirectWalker::new(
            Arc::new(Selective {
                private: "169.254.169.254".parse().unwrap(),
            }),
            t.clone(),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        let e = w.fetch("https://ok.example/").await.unwrap_err();
        assert!(
            matches!(&e, GuardError::PrivateInDnsSet { addr }
                if addr.to_string() == "169.254.169.254"),
            "第 2 跳的 DNS 结果混入链路本地/元数据地址必须整集拒绝并点名，实得 {e:?}"
        );
        assert_eq!(
            *t.next.lock().unwrap_or_else(|e| e.into_inner()),
            1,
            "地址复验必须在 transport 之前拦住第 2 跳：transport 只许被调用一次"
        );
    }

    /// 红线网段作为**重定向目标**（经 DNS 到达，名字闸放行的无害主机名）的全部复验：
    /// 10/8、172.16/12、192.168/16 与 169.254/16。既有测试在 hop 上只覆盖过
    /// 字面 127.0.0.1 和 `localhost` 系名称；这里的每一条都带「transport 恰好 1 次」
    /// 判据，提升到循环外的 `resolve` 无法在这些测试下保持绿色。
    #[tokio::test]
    async fn every_red_line_range_is_rechecked_as_a_redirect_target_by_address() {
        for private in [
            "10.1.2.3",        // 10/8
            "172.16.0.1",      // 172.16/12
            "192.168.7.7",     // 192.168/16
            "169.254.169.254", // 链路本地/云元数据
        ] {
            let addr: IpAddr = private.parse().unwrap();
            let t = Arc::new(ScriptedTransport::new(vec![ok302(
                "https://ok.example/",
                "https://later.example/x",
            )]));
            let w = RedirectWalker::new(Arc::new(Selective { private: addr }), t.clone())
                .with_max_hops(5)
                .with_max_bytes(1024);
            let e = w.fetch("https://ok.example/").await.unwrap_err();
            assert!(
                matches!(&e, GuardError::PrivateInDnsSet { addr: a } if *a == addr),
                "{private} 作为重定向目标必须被第 2 跳的地址级复验点名拒绝，实得 {e:?}"
            );
            assert_eq!(
                *t.next.lock().unwrap_or_else(|e| e.into_inner()),
                1,
                "{private}：复验拒绝后不许再敲 transport，第 2 跳的请求一个都没发出去"
            );
        }
    }

    /// 「每一跳复验」不包括「首跳免检」：第一跳的方案/地址/名字就不过，
    /// 解析与取回都不许发生（脚本是 0 步，真去要就 panic 在这条测试里）。
    #[tokio::test]
    async fn the_first_hop_is_checked_before_any_dns_or_transport() {
        let w = || {
            RedirectWalker::new(
                Arc::new(PublicResolver),
                Arc::new(ScriptedTransport::new(vec![])),
            )
            .with_max_hops(5)
            .with_max_bytes(1024)
        };
        assert!(matches!(
            w().fetch("file:///C:/windows/win.ini").await,
            Err(GuardError::SchemeDenied(_))
        ));
        assert!(matches!(
            w().fetch("http://169.254.169.254/latest/meta-data/").await,
            Err(GuardError::Denied { .. })
        ));
        assert!(matches!(
            w().fetch("http://localhost/").await,
            Err(GuardError::DeniedHostName { .. })
        ));
        assert!(matches!(
            w().fetch("https://creds@ok.example/").await,
            Err(GuardError::DeniedHostName { .. })
        ));
    }

    // ————————————————————— Step 3：cross_host 判据 —————————————————————

    #[test]
    fn cross_host_compares_registrable_suffix_not_whole_host() {
        assert!(!cross_host("www.a.example", "a.example"));
        assert!(cross_host("a.example", "b.example"));
        assert!(cross_host("a.example.com", "a.example.org")); // 后缀不同必须算跨主
    }

    /// `registrable` 的取法与它的**误差方向**一起钉住：末两段，公共后缀不处理。
    #[test]
    fn registrable_is_the_last_two_labels_with_the_documented_imprecision() {
        assert_eq!(registrable("WWW.A.Example."), "a.example");
        assert_eq!(registrable("login.a.example"), "a.example");
        assert_eq!(registrable("a.example.org"), "example.org");
        // 单标签主机（含裸 `localhost`）没有「末两段」，原样返回。
        assert_eq!(registrable("myhost"), "myhost");
        // 已知误差：`.co.uk` 这类多段公共后缀会被当成注册域，于是 `foo.co.uk` 与
        // `bar.co.uk` 判为同域。这是**不引 `psl`** 的代价，写在函数文档里。
        assert_eq!(registrable("foo.co.uk"), "co.uk");
        assert!(!cross_host("foo.co.uk", "bar.co.uk"));
    }

    /// walker 侧的 `cross_host`/`hops`/`final_url`：T24 的「已跳转至 <host>」提示就靠这三个。
    #[tokio::test]
    async fn the_final_response_names_hops_and_cross_host() {
        let same = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(vec![
                ok302("https://a.example/x", "https://www.a.example/y"),
                ok302("https://www.a.example/y", "https://login.a.example/z"),
                ok200("https://login.a.example/z", "text/html", b"hi"),
            ])),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        let f = same.fetch("https://a.example/x").await.unwrap();
        assert_eq!(f.status, 200);
        assert_eq!(f.hops, 2, "hops 数的是**跟掉的 3xx 条数**，不是请求条数");
        assert_eq!(f.final_url, "https://login.a.example/z");
        assert!(!f.cross_host, "同一注册域不算跨主：{f:?}");
        assert_eq!(f.body, b"hi".to_vec());

        let cross = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(vec![
                ok302("https://a.example/x", "https://login.b.example/"),
                ok200("https://login.b.example/", "text/html", b"x"),
            ])),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        let f = cross.fetch("https://a.example/x").await.unwrap();
        assert!(f.cross_host, "换注册域必须报跨主：{f:?}");
        assert_eq!(f.hops, 1);
    }

    /// 3xx 但没有 `Location`：没地方可去，这一跳就是最终响应 —— 不猜、不静默重发。
    #[tokio::test]
    async fn a_redirect_without_location_is_the_final_response() {
        let w = RedirectWalker::new(
            Arc::new(PublicResolver),
            Arc::new(ScriptedTransport::new(vec![Step {
                status: 304,
                content_type: "text/html".to_string(),
                chunks: vec![b"nothing to see".to_vec()],
                location: None,
            }])),
        )
        .with_max_hops(5)
        .with_max_bytes(1024);
        let f = w.fetch("https://ok.example/").await.unwrap();
        assert_eq!(f.status, 304);
        assert_eq!(f.hops, 0);
        assert_eq!(f.final_url, "https://ok.example/");
    }

    // ————————————————————— 生产接线（不联网）—————————————————————

    /// spec 5.4「明确 UA」：常量写死，且**请求真的带着它**。
    /// `Client` 不暴露默认头（默认头是 `execute_request` 才并进去的），所以这条
    /// 只能读生产请求构造点 `ReqwestTransport::request` 的产物 —— 不额外开测试缝。
    #[test]
    fn the_request_carries_the_pinned_user_agent() {
        let addrs: Vec<SocketAddr> = vec!["93.184.216.34:443".parse().unwrap()];
        let client = ReqwestTransport::pinned_client("ok.example", &addrs).unwrap();
        let url = Url::parse("https://ok.example/").unwrap();
        let req = ReqwestTransport::request(&client, &url).unwrap();
        assert_eq!(req.method(), reqwest::Method::GET);
        assert_eq!(req.url().as_str(), "https://ok.example/");
        assert_eq!(
            req.headers()
                .get(reqwest::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            Some(USER_AGENT),
            "UA 常量必须真的出现在请求头上"
        );
    }

    /// R259 + R262 的形状判据：生产解析器与生产 transport 都要能塞进
    /// `Arc<dyn ...>` 交给 `RedirectWalker`（object-safe + `Send + Sync`），全程不碰网络。
    /// 默认上限直接读**私有字段**（同模块可见），不为测试在生产类型上开 getter。
    #[test]
    fn the_production_resolver_and_transport_wire_into_the_walker() {
        let w = RedirectWalker::new(Arc::new(SystemResolver), Arc::new(ReqwestTransport));
        assert_eq!(w.max_hops, DEFAULT_MAX_HOPS, "spec 5.4：重定向 ≤5 跳");
        assert_eq!(
            w.max_bytes, DEFAULT_MAX_BYTES,
            "默认体积闸门要和容量测试同一条线"
        );
    }

    /// 本任务为 `GuardError` 新加的 `Transport` 变体：文案要回显 URL（spec 风险 5），
    /// 并且**明说它不是策略拒绝**——上层把它归成「被安全闸拦了」就是在给用户假信息
    /// （同 R260 那条理由的反方向）。
    #[test]
    fn the_transport_error_echoes_the_url_and_says_it_is_not_a_policy_block() {
        let e = GuardError::Transport {
            url: "https://ok.example/".to_string(),
            reason: "connection timed out".to_string(),
        };
        assert!(e.to_string().contains("https://ok.example/"), "{e}");
        assert!(e.to_string().contains("不是安全策略拒绝"), "{e}");
    }

    // ══════════════════════════ T24：`fetch_url` 工具 ══════════════════════════
    //
    // Step 1–3 的判据**逐字**来自 brief（那三段字符串是测试钉死的对外承诺）。
    // R251：`fetch_with_transport` 吃**已构造好的** `ScriptedTransport`、交回 Text
    // payload —— brief 共享辅助表那句「吃 `Vec<Step>`、返回 `GuardError`」与 Step 3
    // 的测试现场互相矛盾，以测试现场为准。
    // R262：`FetchTool` 不带泛型参数，替身靠「同模块内可直接写私有字段」注入，
    // 生产类型上**不开** `pub fn with_transport`。

    fn fetch_tool_with(transport: ScriptedTransport) -> FetchTool {
        FetchTool {
            walker: RedirectWalker::new(Arc::new(PublicResolver), Arc::new(transport)),
        }
    }

    /// `FetchTool` 用的是**自己的** `RedirectWalker`（文件尾部的注释写明为什么和
    /// `ctx.http` 是两个 client），所以这里的 ctx 只是 `execute` 签名要的壳；
    /// `reqwest::Client::new()` 不联网。
    fn test_ctx() -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: vec![],
        }
    }

    async fn fetch_with_transport(transport: ScriptedTransport) -> Result<String, ToolError> {
        let out = fetch_tool_with(transport)
            .execute(json!({"url": "https://news.example/"}), &test_ctx())
            .await?;
        match out {
            ToolOutput::Text { text } => Ok(text),
            other => Err(ToolError::Failed(format!(
                "fetch_url 只许产出文本，实得 {other:?}"
            ))),
        }
    }

    // ————————————————————— Step 1：内容类型白名单 —————————————————————

    /// R270 之后 `render_markdown` 多收一条基地址。测试一律用**真实形状**的地址
    /// （首轮的占位假地址已删）：相对链接要真能被它展开，否则那几条判据什么都没钉住。
    /// 和 `fetch_with_transport` 请求的那条同一个，好让「直接调 `render_markdown`」与
    /// 「走 `execute`」两条路的输出可以逐字比对（见 `a_same_domain_landing_…`）。
    const PAGE: &str = "https://news.example/";

    #[tokio::test]
    async fn only_html_text_and_json_are_accepted() {
        for ct in ["text/html; charset=utf-8", "text/plain", "application/json"] {
            assert!(render_markdown(ct, b"hello", PAGE).is_ok(), "{ct} 必须接受");
        }
        for ct in [
            "image/png",
            "application/pdf",
            "application/octet-stream",
            "text/css",
        ] {
            let e = render_markdown(ct, b"x", PAGE).unwrap_err();
            assert!(
                e.to_string().contains("content-type"),
                "{ct} 必须给明确错误而不是空文本，实得 {e}"
            );
        }
    }

    /// 白名单**之内**的三类各走自己那条路：html 过抽取器 + Markdown 转换器，纯文本与 JSON 交原文。
    /// 「JSON 也拿去喂 markdown 转换器」会把 `{"a": 1}` 变成装饰文本，
    /// 「纯文本也过一遍」会吃掉日志/源码里的手工缩进 —— 两条都在这里钉住。
    ///
    /// R269：夹具原来是 `<h1>Hello</h1>`。换抽取器后 `readability` 把文档级 `<h1>` 当页面
    /// 标题样板剥掉（这条新语义由 `a_document_level_h1_is_stripped_…` 显式钉住），而这条测试
    /// 的真实意图是「HTML 走抽取、纯文本与 JSON 原样透传」，不是 `<h1>` 的形状 ⇒ 样本换成
    /// `<h2>`，三条承诺一个字没改。
    ///
    /// R273（修复轮 2，只改这句话不说谎的说明，**断言本身没放宽也没删**）：`contains("## Hello")`
    /// 证的是「HTML 那一臂过了 Markdown 转换、出来的是 ATX 标题形状」——它在 T24 判过不合格的
    /// **整页 htmd** 下同样会绿，所以它**不**能拿来证明抽取器被调用。证明那件事的覆盖在
    /// `script_and_style_text_never_reaches_the_markdown_step` 与
    /// `a_document_level_h1_is_stripped_while_h2_stays_a_heading` 两条上，二者都由变异核验过
    /// （整页形状下当场红）。
    #[tokio::test]
    async fn html_becomes_markdown_while_plain_text_and_json_pass_through() {
        let md =
            render_markdown("text/html", "<h2>Hello</h2><p>世界</p>".as_bytes(), PAGE).unwrap();
        assert!(
            md.contains("## Hello"),
            "html 必须真的过了 Markdown 转换这一步、拿到的是 ATX 标题形状（setext 不算），\
             实得 {md}"
        );
        // 中文正文必须活着走到 Markdown 里（抽取器丢中文是很现实的失效模式）。
        assert!(md.contains("世界"), "中文正文不许被丢掉：{md}");
        assert_eq!(
            render_markdown("text/plain", b"a\n\n\nb", PAGE).unwrap(),
            "a\n\n\nb",
            "纯文本一个字节都不许改写（缩进与空行就是它的语义）"
        );
        assert_eq!(
            render_markdown("application/json", b"{\"a\":1}", PAGE).unwrap(),
            "{\"a\":1}",
            "JSON 交原文，不重排版"
        );
        // 大小写与参数都不许影响判定（`check_url` 那边也是这个口径）。
        assert!(render_markdown("TEXT/HTML; charset=UTF-8", b"<i>x</i>", PAGE).is_ok());
        // 非 UTF-8 字节流不许 panic，也不许静默丢内容之外的东西：解码走 `encoding_rs`
        // （R265/R268），这一支缺声明时按严格 UTF-8 → windows-1252 两趟，这里只钉「不 panic」。
        assert!(render_markdown("text/plain", b"\xff\xfe bad bytes", PAGE).is_ok());
    }

    // ————————————————————— Step 3（T24b）：抽取器与字符集 ————————————————
    //
    // 这几条钉的是 T24b 换抽取器（`readability` 定界 + 裁剪片段转 Markdown，R267/R269）
    // 与补 charset 解码（R265/R268）的四件事：按声明解、解不了就报错、抽不出正文明说、
    // 只用正文那一段；修复轮 1 再加两条：基地址是真的（R270）、脚本样式进不来（R267 的理由）。

    /// 组装一段**真的**GBK 字节（不靠本机 codepage，和 T25 的 gbk.txt 同一取向）。
    fn gbk_html(sentence: &str) -> Vec<u8> {
        let html = format!(
            "<html><head><meta charset=\"gb2312\"><title>旧中文站的标题</title></head><body>\
             <div class=\"article\"><p>{sentence}这一段够长，逗号也够多，readability 才会把它当正文评分。</p>\
             <p>第二句还是正文，继续加长，免得候选节点选不中这一段。</p></div>\
             <script>var noise = \"这一串是脚本正文，不该出现在结果里\";</script></body></html>"
        );
        let gbk = encoding_rs::GBK.encode(&html).0;
        assert!(
            std::str::from_utf8(&gbk).is_err(),
            "夹具必须真的不是 UTF-8，否则这条测试什么都没钉住"
        );
        gbk.into_owned()
    }

    #[tokio::test]
    async fn a_gbk_html_page_decodes_instead_of_mojibake() {
        let sentence = "上海今天多云转晴，气温二十度，出行记得带伞。";
        let body = gbk_html(sentence);
        // 同一份字节按 UTF-8 解必然产 U+FFFD —— 那正是旧代码 `from_utf8_lossy` 的行为。
        assert!(
            String::from_utf8_lossy(&body).contains('\u{FFFD}'),
            "这条测试只在「lossy 会坏」的前提下才有意义"
        );
        let md = render_markdown("text/html; charset=gb2312", &body, PAGE).unwrap();
        assert!(
            md.contains(sentence),
            "按声明的 gb2312 解出真中文，实得：{md}"
        );
        assert!(!md.contains('\u{FFFD}'), "不许把 U+FFFD 喂给抽取器：{md}");
        assert!(!md.contains("这一串是脚本正文"), "脚本正文不许进结果：{md}");
    }

    /// 三支共用同一个解码辅助，所以三种白名单类型都得报错（判据 2 + 判据 3）。
    #[tokio::test]
    async fn an_unknown_charset_is_reported_not_lossy_decoded() {
        for ct in [
            "text/html; charset=not-a-real-label",
            "text/plain; charset=not-a-real-label",
            "application/json; charset=not-a-real-label",
        ] {
            let e = render_markdown(ct, "正文".as_bytes(), PAGE).unwrap_err();
            let msg = e.to_string();
            assert!(
                msg.contains("字符集") && msg.contains("not-a-real-label"),
                "{ct} 必须报错并点名那个标签，实得：{msg}"
            );
            assert!(
                !msg.contains('\u{FFFD}'),
                "错误文案自己也不许带替换符：{msg}"
            );
        }
    }

    /// SPA 空壳（root 是空 div、内容全在 script 里）：明确一句「抽不出正文」，
    /// 不许交空串、不许交整页链接、也不许交 JS 正文。
    #[tokio::test]
    async fn readability_failing_to_find_article_is_said_explicitly() {
        let spa = "<!doctype html><html><head><title>某单页应用</title>\
                   <style>html,body{height:100%;margin:0}</style></head>\
                   <body><div id=\"root\"></div>\
                   <script>window.__INITIAL_STATE__={\"bundle\":\"abcdef\"};</script>\
                   <noscript>请启用 JavaScript</noscript></body></html>";
        let out = render_markdown("text/html; charset=utf-8", spa.as_bytes(), PAGE).unwrap();
        assert!(
            out.contains("抽不出正文"),
            "必须明说抽不出正文，实得：{out}"
        );
        assert!(!out.trim().is_empty(), "不许交空串：{out}");
        assert!(!out.contains("__INITIAL_STATE__"), "不许交 JS 正文：{out}");
        assert!(
            !out.contains("(http") && !out.contains("](/"),
            "不许退化成一堆链接：{out}"
        );
    }

    /// `readability` 会把 `<title>`、站点名、byline 一起交出来（`Product.title` 是独立字段，
    /// 降级时 `<head>` 整段还会留在 `content` 里）——只用正文那一段，一个字都不许拼进来：
    /// 模型会把它们当页面内容引用。
    #[tokio::test]
    async fn title_and_byline_are_not_spliced_into_the_body() {
        let html = "<html><head><title>站点名：这篇报道的标题</title></head><body>\
                    <div class=\"byline\">作者：某某某｜责任编辑：某某某</div>\
                    <div class=\"article\"><p>正文第一句，讲的是本地生活，读起来像个段落。</p>\
                    <p>正文第二句，把段落写长一点，好让候选评分选中它。</p></div>\
                    </body></html>";
        let md = render_markdown("text/html; charset=utf-8", html.as_bytes(), PAGE).unwrap();
        assert!(md.contains("正文第一句"), "正文必须在，实得：{md}");
        assert!(!md.contains("这篇报道的标题"), "title 不许拼进正文：{md}");
        assert!(!md.contains("站点名"), "站点名不许拼进正文：{md}");
        assert!(!md.contains("责任编辑"), "byline 不许拼进正文：{md}");
    }

    /// 真网复测打到的缺陷（findings 第 2 页 overreacted 的那张 gif）：`readability` 拿
    /// **占位**基地址改写相对链接，会造出一个**不存在的 host** 交进正文。
    /// R270 之后占位地址本身已经没了：`render_markdown` 收真实 URL，相对链接就按真实
    /// 页面地址展开。两条判据一起钉 —— 既要把展开后的**绝对地址**（连 Markdown 形状）钉在
    /// 占位地址原来该在的位置上，也不许把相对链接留在相对形状
    /// （留在相对形状 = 基地址没用上，模型无法照着链接再取一次）。
    #[tokio::test]
    async fn relative_links_are_resolved_against_the_real_page_url() {
        let html = "<html><body><article><p>正文第一句，带一张相对路径的图，字数要够多才不会被降权。\
                    <img src=\"/img/pic.gif\" alt=\"配图\"><a href=\"/next\">下一页</a></p>\
                    <p>第二段仍然属于正文，继续加长以通过 readability 的候选评分。</p></article></body></html>";
        let md = render_markdown("text/html", html.as_bytes(), PAGE).unwrap();
        assert!(
            md.contains("![配图](https://news.example/img/pic.gif)"),
            "图片链接要按真实页面地址展开成 Markdown 形状（修复轮 1 里那条 `fetch-url.invalid` \
             负断言比较的字符串在代码里已经不存在 ⇒ 永远绿；换成这条：占位地址原来该在的\
             位置上必须出现展开后的绝对地址）：{md}"
        );
        assert!(
            !md.contains("![配图](/img/pic.gif)") && !md.contains("[下一页](/next)"),
            "相对形状留在原位 = 基地址根本没生效，模型照着它翻不了页：{md}"
        );
        assert!(
            md.contains("https://news.example/img/pic.gif"),
            "相对链接要按真实页面地址展开：{md}"
        );
        assert!(
            md.contains("https://news.example/next"),
            "锚链接同样按真实页面地址展开：{md}"
        );
    }

    /// R269：换抽取器后**有意接受**的语义，写显式断言钉住，不留成"漏了一条测试"。
    /// `readability 0.3.0` 的 `scorer::clean`（`src/scorer.rs:332`）把 `h1` 和
    /// script/style/header/footer/aside 一起列进样板清单 —— 文档级 `<h1>` 一律删掉，
    /// 因为 arc90 算法把它当**页面标题**（站点名）而不是文章小标题。
    /// 这不是遗漏：h2–h6 全部保留，正是下面那条断言的另一半。
    #[tokio::test]
    async fn a_document_level_h1_is_stripped_while_h2_stays_a_heading() {
        let html = "<html><body><article>\
                    <h1>整站大标题（readability 当页面标题剥掉）</h1>\
                    <h2>这一节讲的是什么</h2>\
                    <p>正文第一句，段落要够长才不会被候选评分降权。</p>\
                    <p>第二段仍然是正文，继续加长以保证候选节点选中这个 article。</p>\
                    </article></body></html>";
        let md = render_markdown("text/html", html.as_bytes(), PAGE).unwrap();
        assert!(
            !md.contains("整站大标题"),
            "`<h1>` 会被 readability 剥掉 —— 这是换抽取器后有意接受的语义，不是遗漏：{md}"
        );
        assert!(
            md.contains("## 这一节讲的是什么"),
            "h2 必须活着走到 Markdown 里，且是 ATX 形状（`##` 前缀）：{md}"
        );
    }

    /// R267 的理由钉在这条：同一个 Markdown 转换器，喂**整页**不合格（T24 判门），
    /// 喂 **readability 已裁剪的正文片段**合格 —— 脚本与样式的文本在定界那一步就已经
    /// 不在片段里了（`scorer::clean` 的样板清单）。所以这条测的是「片段里没有的东西
    /// 不会从 Markdown 那一步漏回来」。
    /// 变异核验（见报告）：把交给 Markdown 那一步的输入从 `article_body_html(...)` 换成
    /// **整页 HTML**，这条立刻变红 —— 判据不是空转的。
    #[tokio::test]
    async fn script_and_style_text_never_reaches_the_markdown_step() {
        let html = "<html><head><style>body{color:#111 /* 内联样式正文 */}</style></head><body>\
                    <article><p>正文第一句，段落要够长才不会被候选评分降权。</p>\
                    <script>var hidden = \"脚本正文不该出现在结果里\";</script>\
                    <p>第二段仍然是正文，继续加长以保证候选节点选中这个 article。</p></article>\
                    </body></html>";
        let md = render_markdown("text/html", html.as_bytes(), PAGE).unwrap();
        assert!(md.contains("正文第一句"), "正文必须在：{md}");
        assert!(
            !md.contains("脚本正文不该出现在结果里"),
            "脚本正文不许进结果：{md}"
        );
        assert!(!md.contains("内联样式正文"), "样式正文不许进结果：{md}");
        assert!(!md.contains("<script"), "连标签壳都不许残留：{md}");
    }

    /// R270 的调用点判据：`FetchTool::execute` 交出去的是 **`final_url`**（实际取回内容
    /// 的那一跳），不是用户请求里那条。重定向后还在按请求地址展开链接，等于把正文里的
    /// 链接指向另一个站点的同类路径 —— 那是一条模型看不见的错。
    #[tokio::test]
    async fn after_a_redirect_links_resolve_against_the_final_url() {
        let html = "<html><body><article><p>正文第一句，带一个指向下一页的相对链接，字数要够多才不会被降权。\
                    <a href=\"/next\">下一页</a></p>\
                    <p>第二段仍然属于正文，继续加长以通过 readability 的候选评分。</p></article></body></html>";
        let out = fetch_with_transport(ScriptedTransport::new(vec![
            ok302(
                "https://news.example/",
                "https://files.other.example/article/1",
            ),
            ok200(
                "https://files.other.example/article/1",
                "text/html",
                html.as_bytes(),
            ),
        ]))
        .await
        .unwrap();
        assert!(
            out.contains("https://files.other.example/next"),
            "相对链接要按**最终落地地址**展开，实得：{out}"
        );
        assert!(
            !out.contains("news.example/next"),
            "不许按请求里的地址展开（那是另一台机器上的同类路径）：{out}"
        );
        assert!(
            out.contains("[下一页](https://files.other.example/next)"),
            "占位地址原来该在的那个位置上，必须是一条**展开后的绝对地址**（连 Markdown 链接形状\
             一起钉，光钉裸 URL 的话 `[下一页](/next)` 这种「基地址没生效」的形状抓不住）：{out}"
        );
    }

    // ————————————————————— 修复轮 2（R271）：正文下限的标记句 ————————————————

    /// R271 的失败形状（评审亲测：ithome 187 字 = 站点 slogan + 版权页脚、36kr 52 字 = 反爬提示）：
    /// `readability` **定界失败**那一支不返回空，而是降级成「整份文档剩下的样板」，于是这些
    /// 样板被当正文静静交出去，模型会照原话答「这一页讲的是软媒旗下科技门户网站」。
    /// 这条钉住修法：低于下限**不丢文本**，改成「一行标记 + 原文」两段，且**不**与空串那一支
    /// （`NO_ARTICLE_TEXT`）合并成一句 —— 那是两句不同的真话。
    #[tokio::test]
    async fn a_sub_floor_extraction_is_labelled_instead_of_dropped() {
        // 一份 readability 定界不出来的门户页：正文位置只剩站点 slogan、站点简介
        // 与版权/举报/备案页脚，合计一百来字 —— 真网上打到的是 ithome 的 187 字。
        // 同一份夹具被下面「标记句 × 分页」那三条测试复用（夹具一改，四条判据一起动）。
        let html = SUB_FLOOR_PORTAL_HTML;
        let out = render_markdown("text/html", html.as_bytes(), PAGE).unwrap();
        let (label_line, body) = out
            .split_once('\n')
            .unwrap_or_else(|| panic!("标记句必须单独占一行、原文跟在它下面，实得：{out}"));
        assert_eq!(
            label_line,
            format!(
                "[未能定界出正文，以下 {} 字可能是站点样板或反爬提示，别当正文引用]",
                out.chars().count() - label_line.chars().count() - 1
            ),
            "标记句要逐字是那句，且句里的 {{n}} 必须等于它下面交出去的字符数（报假数比不报更坏）：{out}"
        );
        // **没丢掉**：那几十上百字样板本体必须在结果里。
        assert!(
            body.contains("软媒旗下科技门户网站，速享科技每一刻。")
                && body.contains("违法和不良信息举报电话"),
            "低于下限不许把文本吞掉，样板本体必须照样交给模型：{out}"
        );
        assert!(
            body.chars().count() < MIN_ARTICLE_CHARS && !body.trim().is_empty(),
            "夹具必须真的落在「非空但低于下限」那一格里，否则这条什么都没钉住：{} 字",
            body.chars().count()
        );
        // **没合并**：空串那一支的另一句真话不许串到这里。
        assert!(
            !out.contains(NO_ARTICLE_TEXT),
            "非空但低于下限走的是标记句，不是「抽不出正文」那句（两支没合并）：{out}"
        );
    }

    /// 透传那两臂**不受阈值影响**（rfc1945 与 JSON 逐字透传是它们判「过」的理由）：
    /// 一段低于下限的 `text/plain` 必须逐字等于输入，一条标记都不许加。
    #[tokio::test]
    async fn text_plain_below_the_floor_gets_no_label_at_all() {
        let short = "第 1 行\n第 2 行留着缩进\n\t第 3 行\ttab\n";
        assert!(
            short.chars().count() < MIN_ARTICLE_CHARS,
            "夹具必须真的低于下限，否则这条守的是「长文本不标记」——那不是本条的判据"
        );
        assert_eq!(
            render_markdown("text/plain", short.as_bytes(), PAGE).unwrap(),
            short,
            "text/plain 逐字透传的承诺不许被 R271 的阈值污染"
        );
        assert_eq!(
            render_markdown("application/json", br#"{"a":1}"#, PAGE).unwrap(),
            "{\"a\":1}",
            "application/json 同样逐字透传"
        );
    }

    /// 评审判断 2 的缺口：`extract_article_markdown` 在 base 解析不出来时**硬报错、不降级**，
    /// 这个决定之前零测试。降级成假基地址交出去的是「模型会照着去取的错链接」，
    /// 比这一页失败更难发现 —— 所以这里钉的是「报错」这个形状，不许改成兜底。
    #[tokio::test]
    async fn an_unparseable_base_url_is_an_error_not_a_fallback() {
        let html = "<html><body><article><p>正文第一句。</p></article></body></html>";
        let e = render_markdown("text/html", html.as_bytes(), "not a url").unwrap_err();
        assert!(
            matches!(e, ToolError::Failed(_)),
            "解析不出基地址必须是 Failed，实得 {e:?}"
        );
        let msg = e.to_string();
        assert!(
            msg.contains("假地址") && msg.contains("not a url"),
            "错误里要同时有「我不拿假地址改写链接」这句原话和那条坏输入，实得：{msg}"
        );
    }

    // ————————————————————— Step 2：截断与分页 —————————————————————

    #[test]
    fn truncation_is_explicit_and_offset_pages_forward() {
        let long = "字".repeat(9_000); // 每字 3 字节，同时测「按字符不按字节」
        let note = "[已截断，原文 9000 字符]";
        let first = slice_page(&long, 0, 8_000, true);
        assert!(first.ends_with(note), "尾部标记不对：{first}");
        assert_eq!(
            first.chars().count(),
            8_000 + note.chars().count(),
            "正文必须正好 8000 字符，标记另计"
        );
        let second = slice_page(&long, 8_000, 8_000, true);
        assert_eq!(second.chars().count(), 1_000, "第二段只剩 1000 字");
        assert!(
            !second.contains("已截断"),
            "取到末尾时不许再挂截断标记，否则模型会以为还有内容"
        );
        assert!(
            slice_page(&long, 99_000, 100, true).contains("超出范围"),
            "offset 越界要说明，不能返回空串让模型猜"
        );
    }

    /// `total_note` 这个参数必须有实际含义，否则它就是签名里的装饰品。
    #[test]
    fn the_note_can_drop_the_total_when_the_caller_already_said_it() {
        let long = "字".repeat(9_000);
        let p = slice_page(&long, 0, 8_000, false);
        assert!(p.ends_with("[已截断]"), "实得尾部：{p}");
        assert_eq!(p.chars().count(), 8_000 + "[已截断]".chars().count());
        assert!(!p.contains("原文"), "total_note=false 就不许报原文长度");
    }

    /// 越界那句必须带上「原文共多少字符」：模型拿到「超出范围」而无原文长度，
    /// 唯一能做的就是再猜一个 offset —— 那是在花一次往返换一句废话。
    #[test]
    fn the_out_of_range_page_names_the_total_so_the_model_can_back_off() {
        let long = "字".repeat(9_000);
        assert_eq!(
            slice_page(&long, 99_000, 100, true),
            "[offset 99000 超出范围：原文共 9000 字符]"
        );
        // 恰好落在末尾：那里没有字符可交，按越界同一口径说清楚（不返回空串）。
        assert_eq!(
            slice_page(&long, 9_000, 100, true),
            "[offset 9000 超出范围：原文共 9000 字符]"
        );
        // 抽取结果为空（SPA 空壳就是这个形状）：对 offset 0 说「超出范围」是假话，
        // 交空串又让模型自己猜，所以单独一句。
        assert!(
            slice_page("", 0, 8_000, true).starts_with("[正文为空"),
            "空正文的说法不对：{}",
            slice_page("", 0, 8_000, true)
        );
    }

    // ——————— R271 的标记句 × 分页：标记句不在被切的那一串里 ———————
    //
    // 缺陷形状（T28 Step 0 派发）：标记句和正文黏成一串后 `execute` 对整串分页，于是
    // ①页开得比标记句还小时「别当正文引用」那半句被切掉、②`slice_page` 的 `total`
    // 把标记句自己的字数也算进「原文 N 字符」（同一个 n 在两句里报出两个数）、
    // ③第 1 页替标记句吃掉了字符 ⇒ 模型按「offset = 上次 offset + max_chars」续页会读到重叠。

    /// 低于 `MIN_ARTICLE_CHARS`、但**长到能被 20 字的页切成好几页**的门户页夹具
    /// （ithome 那 187 字样板的同一种形状）：只有「页比标记句短」这个形状才看得见上面三条，
    /// 所以这里刻意把夹具做成「非空 + 低于下限 + 多页」。
    const SUB_FLOOR_PORTAL_HTML: &str = "<!doctype html><html><head><title>软媒网｜某科技门户</title>\
                    <meta name=\"description\" content=\"软媒旗下科技门户网站\"></head>\
                    <body><div class=\"wrap\">\
                    <p>软媒旗下科技门户网站，速享科技每一刻。</p>\
                    <p>本站收录前沿科技新闻、新品资讯、手机评测与智能设备导购，是数码爱好者每天要逛的科技站。</p>\
                    <p>© 2026 软媒 版权所有｜违法和不良信息举报电话 010-00000000｜京公网安备 11010802035028 号</p>\
                    </div></body></html>";

    /// 正文基线：从 `render_markdown` 那句**对外形状不变**的「标记行 + 换行 + 正文」里剥出正文。
    /// 不写死字数，是为了让「`原文 N` 数的到底是正文还是正文+标记句」这条判据在夹具被改动后
    /// 依然成立（写死的数字只会跟着缺陷一起被改错）。
    fn sub_floor_body() -> String {
        let glued = render_markdown("text/html", SUB_FLOOR_PORTAL_HTML.as_bytes(), PAGE).unwrap();
        let (label_line, body) = glued.split_once('\n').unwrap_or_else(|| {
            panic!("夹具必须走「非空但低于下限」那一支（第一行是标记句），实得：{glued}")
        });
        assert!(
            label_line.starts_with("[未能定界出正文，以下 ")
                && label_line.ends_with("，别当正文引用]"),
            "夹具的标记行形状不对，后面几条断言都失去意义：{label_line}"
        );
        let n = body.chars().count();
        // 夹具的两条前置，各自对应一条判据；写成裸魔数（原来那个 `n > 60`）看不出谁在承重：
        // ①页必须比标记句**短**（`20 < label`），否则「页切到标记句」这个形状根本不出现；
        // ②正文必须容得下 `offset=20` 再加一整页（`n > 40`），否则第二条测试的左右两边
        // 同为「超出范围」那一串 ⇒ 等式在比两个相同的错误文案，判据 ③ 空转。
        let label_chars = label_line.chars().count();
        assert!(
            20 < label_chars && n > 40 && n < MIN_ARTICLE_CHARS,
            "夹具不成立（标记句 {label_chars} 字 / 正文 {n} 字）：需要页 20 字**短于**标记句、\
             正文**长过** 40 字（offset=20 之后还有一整页）、且正文低于下限 {MIN_ARTICLE_CHARS} 字，\
             否则那三条断言各自失去对象"
        );
        body.to_string()
    }

    /// 判据 ①+②：`max_chars` 小于标记句长度时，标记句必须**整句**留在第 1 页，
    /// 且它下面那一页必须逐字等于「只切正文」的 `slice_page` 结果 —— 这一条等式同时钉住
    /// 「N 只数正文」（`total` 里没混进标记句）和「第 1 页给满 max_chars 个正文字」。
    #[tokio::test]
    async fn a_page_smaller_than_the_label_still_carries_the_whole_label() {
        let body = sub_floor_body();
        let out = fetch_tool_with(ScriptedTransport::new(vec![ok200(
            PAGE,
            "text/html",
            SUB_FLOOR_PORTAL_HTML.as_bytes(),
        )]))
        .execute(json!({"url": PAGE, "max_chars": 20}), &test_ctx())
        .await
        .unwrap();
        let ToolOutput::Text { text: first } = out else {
            panic!("fetch_url 只交文本");
        };
        let (label, page) = first.split_once('\n').unwrap_or_else(|| {
            panic!("标记句要独立成一行、正文跟在它下面；整行都凑不齐说明它被切了：{first}")
        });
        assert_eq!(
            label,
            sub_floor_label(body.chars().count()),
            "第 1 页的标记句必须逐字完整（页再小也不能切它），实得：{first}"
        );
        assert_eq!(
            page,
            slice_page(&body, 0, 20, true),
            "标记句之外的那一页必须等于「只切正文」的结果：黏在一起切时正文少给 20 个标记句\
             的字符，末尾那句「原文 N 字符」也多算了标记句自己的字数"
        );
    }

    /// 判据 ③：按工具描述教的「offset = 上次 offset + max_chars」续页时，续页必须是**正文的
    /// 下一段**（不重叠），而「以下 N 字」那句只属于第 1 页 —— 续页再挂它就是假话（那一页以下
    /// 的字数和 N 没关系）。
    #[tokio::test]
    async fn the_next_page_starts_where_the_body_page_stopped_and_repeats_no_label() {
        let body = sub_floor_body();
        let steps = || vec![ok200(PAGE, "text/html", SUB_FLOOR_PORTAL_HTML.as_bytes())];
        let second = fetch_tool_with(ScriptedTransport::new(steps()))
            .execute(
                json!({"url": PAGE, "offset": 20, "max_chars": 20}),
                &test_ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text { text: second } = second else {
            panic!("fetch_url 只交文本");
        };
        assert_eq!(
            second,
            slice_page(&body, 20, 20, true),
            "第 2 页必须从正文第 20 字接着切（第 1 页没替标记句吃字符 ⇒ 不重叠）"
        );
        assert!(
            !second.contains("未能定界出正文"),
            "标记句只在第 1 页出现一次，续页重复挂它等于对同一篇正文报两遍：{second}"
        );
    }

    /// 判据 ②的另一半 + 不回归：默认页（一页装得整篇）下标记句**只出现一次**，
    /// 输出逐字等于 `render_markdown` 的对外形状，也不挂截断标记。
    #[tokio::test]
    async fn the_default_page_carries_the_label_exactly_once() {
        let body = sub_floor_body();
        let glued = render_markdown("text/html", SUB_FLOOR_PORTAL_HTML.as_bytes(), PAGE).unwrap();
        let out = fetch_with_transport(ScriptedTransport::new(vec![ok200(
            PAGE,
            "text/html",
            SUB_FLOOR_PORTAL_HTML.as_bytes(),
        )]))
        .await
        .unwrap();
        assert!(
            body.chars().count() < DEFAULT_MAX_CHARS,
            "夹具必须一页装得下，否则这条测的不是默认页"
        );
        assert_eq!(
            out, glued,
            "默认页（一页装得下）的输出必须还是 `render_markdown` 那一串"
        );
        assert_eq!(
            out.matches("别当正文引用").count(),
            1,
            "标记句只能出现一次：{out}"
        );
        assert!(!out.contains("已截断"), "一页装得下就不许挂截断标记：{out}");
    }

    // ————————————————————— Step 3：跨主提示与状态提示 —————————————————————

    #[tokio::test]
    async fn a_cross_host_landing_is_announced_in_the_output_text() {
        // 反爬/登录页场景：spec 要求「不把验证码页喂给模型」。判据实现为「明示最终主机」，
        // 而不是假装能识别验证码。
        let out = fetch_with_transport(ScriptedTransport::new(vec![
            ok302(
                "https://news.example/",
                "https://passport.other.example/login",
            ),
            ok200(
                "https://passport.other.example/login",
                "text/html",
                "<h1>登录</h1>".as_bytes(),
            ),
        ]))
        .await
        .unwrap();
        assert!(
            out.starts_with("[已跳转至 passport.other.example]"),
            "实得 {out}"
        );
    }

    /// 同域跳转**不许**加提示行（brief Step 3 正文那句）。判据写成「输出与正文逐字相等」：
    /// 只断 `!contains("已跳转至")` 的话，挂上任何别的头部行都还是绿的。
    #[tokio::test]
    async fn a_same_domain_landing_adds_no_prefix_line_at_all() {
        let body: &[u8] = "<p>正文</p>".as_bytes();
        let out = fetch_with_transport(ScriptedTransport::new(vec![
            ok302("https://news.example/x", "https://www.news.example/y"),
            ok200("https://www.news.example/y", "text/html", body),
        ]))
        .await
        .unwrap();
        assert_eq!(
            out,
            // 基地址这里是**落地那一跳**（`final_url`），和 `execute` 交给 `render_markdown`
            // 的那一条同一个值（R270）——「输出与正文逐字相等」这句判据才有基地址可言。
            render_markdown("text/html", body, "https://www.news.example/y").unwrap(),
            "同域跳转的输出必须就是正文本身"
        );
    }

    /// 403/429 前置一行，且**状态码必须在 preview 里**：事件流卡片只交前 200 个字符
    /// （`agent::types::PREVIEW_CHARS`），看不见状态码就等于没告诉模型「这是被拦了不是正文」。
    #[tokio::test]
    async fn an_upstream_403_or_429_is_announced_before_the_body() {
        for code in [403u16, 429] {
            let out = fetch_with_transport(ScriptedTransport::new(vec![Step {
                status: code,
                content_type: "text/html".to_string(),
                chunks: vec![b"<p>are you a robot?</p>".to_vec()],
                location: None,
            }]))
            .await
            .unwrap();
            assert!(
                out.starts_with(format!("[上游返回 {code}，内容可能受限]").as_str()),
                "实得 {out}"
            );
            assert!(out.contains("are you a robot?"), "正文不许被丢掉：{out}");
            assert!(
                ToolOutput::text(out.clone()).preview().contains("上游返回"),
                "状态行必须进 preview（前 200 字），实得 {out}"
            );
        }
    }

    /// 同一句模板对**全部** 4xx/5xx 生效（brief 点名 403/429，实现按状态类而不是按码）：
    /// 404/500 的 error page 不带状态行喂进去，模型无法和真正文区分。2xx/3xx 不许挂。
    #[tokio::test]
    async fn every_error_status_gets_the_line_and_2xx_does_not() {
        for code in [404u16, 409, 451, 500, 503] {
            let out = fetch_with_transport(ScriptedTransport::new(vec![Step {
                status: code,
                content_type: "text/plain".to_string(),
                chunks: vec![b"nope".to_vec()],
                location: None,
            }]))
            .await
            .unwrap();
            assert!(
                out.starts_with(format!("[上游返回 {code}，内容可能受限]").as_str()),
                "实得 {out}"
            );
        }
        for code in [200u16, 204, 304] {
            let out = fetch_with_transport(ScriptedTransport::new(vec![Step {
                status: code,
                content_type: "text/plain".to_string(),
                chunks: vec![b"ok".to_vec()],
                location: None,
            }]))
            .await
            .unwrap();
            assert_eq!(out, "ok", "{code} 不许挂状态行，实得 {out}");
        }
    }

    // ————————————————————— 工具声明与入参 —————————————————————

    /// `meta()` 的三项对外承诺：名字、风险、schema。风险必须是 Read —— spec 5.1
    /// 「内置工具全为 Read ⇒ 全程零弹窗」的前提就落在这个返回值上。
    #[test]
    fn the_tool_meta_is_fetch_url_read_with_the_pinned_schema() {
        let m = FetchTool::new().meta();
        assert_eq!(m.name, "fetch_url");
        assert_eq!(m.risk, Risk::Read);
        assert!(
            m.description.contains("字符"),
            "分页单位必须写进描述（`read_file` 按行、本工具按字符，模型只看描述）：{}",
            m.description
        );
        let s = m.input_schema;
        assert_eq!(s["type"], json!("object"));
        assert_eq!(s["required"], json!(["url"]), "只有 url 是必填");
        let props = s["properties"].as_object().expect("properties 必须是对象");
        assert_eq!(props.len(), 3, "只声明 url/offset/max_chars：{props:?}");
        assert!(
            props.contains_key("url")
                && props.contains_key("offset")
                && props.contains_key("max_chars")
        );
        assert_eq!(props["url"]["type"], json!("string"));
        assert_eq!(props["offset"]["type"], json!("integer"));
        assert_eq!(props["max_chars"]["type"], json!("integer"));
    }

    /// R262 的生产接线 + 默认上限：`FetchTool::new()` 必须组装出
    /// `RedirectWalker::new(SystemResolver, ReqwestTransport)`，且**不联网**
    /// （`Client::new()` 只建连接池配置，一条请求都不发）。
    #[test]
    fn the_production_constructor_keeps_the_walker_defaults() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FetchTool>();
        let t = FetchTool::new();
        assert_eq!(t.walker.max_hops, DEFAULT_MAX_HOPS);
        assert_eq!(t.walker.max_bytes, DEFAULT_MAX_BYTES);
    }

    #[tokio::test]
    async fn a_missing_or_non_string_url_is_bad_input_before_any_request() {
        // 脚本 0 步：真去敲 transport 就会 panic 在「吐完了还请求」，所以这条同时是
        // 「入参校验发生在发请求之前」的判据。
        for input in [
            json!({}),
            json!({"url": 42}),
            json!({"url": ""}),
            json!({"url": "   "}),
        ] {
            let e = fetch_tool_with(ScriptedTransport::new(vec![]))
                .execute(input.clone(), &test_ctx())
                .await
                .unwrap_err();
            assert!(
                matches!(e, ToolError::BadInput(_)),
                "{input} 必须 BadInput，实得 {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_non_integer_offset_or_max_chars_is_bad_input_not_silently_defaulted() {
        // 静默按默认值跑 = 模型以为自己的 offset 生效了，下一页它会读到重复内容。
        for input in [
            json!({"url": "https://news.example/", "offset": -1}),
            json!({"url": "https://news.example/", "offset": "8000"}),
            json!({"url": "https://news.example/", "max_chars": 1.5}),
        ] {
            let e = fetch_tool_with(ScriptedTransport::new(vec![]))
                .execute(input.clone(), &test_ctx())
                .await
                .unwrap_err();
            assert!(
                matches!(&e, ToolError::BadInput(m) if m.contains("整数")),
                "{input} 必须报「必须是非负整数」，实得 {e:?}"
            );
        }
        // 显式 null 交给可选参数按「没给」处理（模型常这么填默认值）。
        assert!(
            fetch_tool_with(ScriptedTransport::new(vec![ok200(
                "https://news.example/",
                "text/plain",
                b"ok",
            )]))
            .execute(
                json!({"url": "https://news.example/", "offset": null}),
                &test_ctx()
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_cancelled_ctx_stops_before_the_walker_is_touched() {
        let ctx = test_ctx();
        ctx.cancel.cancel();
        let e = fetch_tool_with(ScriptedTransport::new(vec![]))
            .execute(json!({"url": "https://news.example/"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(e, ToolError::Cancelled), "{e:?}");
    }

    /// 安全闸的判决必须原样到人眼前（spec 风险 5：能回显地址的一律回显），
    /// 而不是被 `Failed("fetch failed")` 这种糊话吃掉。
    #[tokio::test]
    async fn the_guard_decision_reaches_the_caller_with_the_address_in_it() {
        let e = fetch_tool_with(ScriptedTransport::new(vec![]))
            .execute(json!({"url": "http://127.0.0.1/secret"}), &test_ctx())
            .await
            .unwrap_err();
        assert!(matches!(e, ToolError::Failed(_)), "{e:?}");
        assert!(
            e.to_string().contains("127.0.0.1"),
            "拒绝文案必须回显地址，实得 {e}"
        );
    }

    /// 端到端的按字符分页：默认页 8000 字 + 标记，第二次调用接着往下走且不重复。
    #[tokio::test]
    async fn a_long_page_pages_by_characters_across_two_calls() {
        let body = "字".repeat(9_000); // 27000 字节，远在 2 MiB 闸门之下
        let steps = || {
            vec![ok200(
                "https://news.example/",
                "text/plain",
                body.as_bytes(),
            )]
        };
        let first = fetch_with_transport(ScriptedTransport::new(steps()))
            .await
            .unwrap();
        assert_eq!(
            first.chars().count(),
            DEFAULT_MAX_CHARS + "[已截断，原文 9000 字符]".chars().count()
        );
        assert!(first.ends_with("[已截断，原文 9000 字符]"), "实得 {first}");

        let tool = fetch_tool_with(ScriptedTransport::new(steps()));
        let out = tool
            .execute(
                json!({"url": "https://news.example/", "offset": DEFAULT_MAX_CHARS}),
                &test_ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text { text: second } = out else {
            panic!("fetch_url 只交文本");
        };
        assert_eq!(
            second,
            body.chars().skip(DEFAULT_MAX_CHARS).collect::<String>(),
            "第二页必须正好接在第一页后面"
        );
    }

    /// `max_chars` 有上限：不设的话模型可以一次把 2 MiB 正文整个塞进上下文。
    #[tokio::test]
    async fn max_chars_is_clamped_to_the_cap() {
        let body = "字".repeat(60_000);
        let tool = fetch_tool_with(ScriptedTransport::new(vec![ok200(
            "https://news.example/",
            "text/plain",
            body.as_bytes(),
        )]));
        let out = tool
            .execute(
                json!({"url": "https://news.example/", "max_chars": 10_000_000}),
                &test_ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("fetch_url 只交文本");
        };
        let got = text.chars().count();
        assert!(
            got <= MAX_CHARS_CAP + "[已截断，原文 60000 字符]".chars().count(),
            "一次调用交出去 {got} 个字符，说明上限没生效"
        );
        assert!(
            text.ends_with("[已截断，原文 60000 字符]"),
            "实得尾部 {text}"
        );
    }
}
