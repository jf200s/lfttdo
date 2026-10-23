// Plan B 的网络层（task 23）：逐跳重定向遍历 + 地址钉死 + 体积闸门。
//
// 本文件**只做**三件事：走跳、每跳复验、把体积按上限边收边卡。
// HTML→Markdown、内容类型白名单、分页都是 task 24 的活（plan:317）。
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
use url::{Host, Url};

use crate::tools::fetch_guard::{GuardError, Resolver, Target, check_url, resolve_target};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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
}
