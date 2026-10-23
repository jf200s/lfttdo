// Plan B 的 SSRF 判定层：纯函数，不依赖网络、不依赖 reqwest。
//
// 本模块只回答一个问题：「这个地址能不能连」。逐跳重定向复验（task 23）、
// fetch_url（task 24）、MCP HTTP URL（task 36）都必须回到 `deny_reason` 这
// **唯一分类处**，任何地方不得自带第二份 IP 判定表。
//
// R249：`resolve_target` 是交付接口，不是测试脚手架：
// - `Host::Ipv4/Ipv6` 直接分类，**不做二次解析**；
// - `Host::Domain` 调 `Resolver` 后用 `assert_all_public` 整集复验；
// - 空地址集必须是 `Err(Resolution(..))`，绝不放行。
//
// `Host::Domain` 的不变式（必须连着上一句读）：`check_url` 对域名只做**名称级**
// 红线（环回/云元数据主机名，见 `deny_domain`），它挡住的是「名字本身就危险」的
// 输入；一个 Domain 主机只有在 `resolve_target` 也跑过之后才算被完整守住——
// 名字无害但解析到内网的情况，只有解析结果复验能兜住。
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::OnceLock;

use ipnet::Ipv4Net;
use url::{Host, Url};

/// 判定层的全部拒绝理由。`Display` 文案直接给终端用户看（spec 风险 5：
/// 凡能回显 URL/地址的一律回显），`Debug` 留给日志与测试断言。
///
/// R248：`TooLargeBody { limit }` 的 `Display` 打印裸数字（例如
/// 「响应体超过 2097152 字节上限」），task 23 的断言依赖这串数字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// 方案不在 http/https 白名单内（含连方案都解析不出来的输入）。
    SchemeDenied(String),
    /// URL 合法但没有主机名。
    NoHost,
    /// 字面 IP，或分类命中的地址。
    Denied { addr: IpAddr, reason: &'static str },
    /// 主机名本身就撞红线（`localhost`/`*.localhost`/云元数据名）——还没有地址可点名。
    DeniedHostName { host: String, reason: &'static str },
    /// DNS 返回的地址集里混进了私网/保留地址——整集拒绝。
    PrivateInDnsSet { addr: IpAddr },
    /// 主机解析失败或解析结果为空：(主机名, 人话原因)。
    Resolution(String, String),
    /// 重定向遍历超上限，回显最终停下的 URL。
    TooManyHops(String),
    /// 响应体超过 `limit` 字节上限。
    TooLargeBody { limit: usize },
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemeDenied(url) => write!(f, "只允许 http/https 地址，已拒绝：{url}"),
            Self::NoHost => write!(f, "地址里缺少主机名，无法判断是否允许访问"),
            Self::Denied { addr, reason } => {
                write!(
                    f,
                    "拒绝访问 {addr}：该地址属于「{reason}」，本机联网工具不碰内网/保留地址"
                )
            }
            Self::PrivateInDnsSet { addr } => write!(
                f,
                "域名解析结果里混有「{addr}」这类内网/保留地址，整个解析结果已拒绝（防 DNS 混载绕过）"
            ),
            Self::DeniedHostName { host, reason } => write!(
                f,
                "拒绝访问主机名「{host}」：该名称属于「{reason}」，本机联网工具不碰内网/元数据地址"
            ),
            Self::Resolution(host, msg) => write!(f, "解析主机 {host} 失败：{msg}"),
            Self::TooManyHops(url) => write!(f, "重定向次数超过上限，已停止在：{url}"),
            Self::TooLargeBody { limit } => write!(f, "响应体超过 {limit} 字节上限"),
        }
    }
}

impl std::error::Error for GuardError {}

/// 通过全部判定后交给网络层的最终目标。
#[derive(Debug, Clone)]
pub struct Target {
    pub url: Url,
    pub host: String,
    pub addrs: Vec<IpAddr>,
}

/// 地址解析抽象：生产实现用 `tokio::net::lookup_host`（由后续任务接线），
/// 测试用脚本化实现——「每一跳复验」和「DNS rebinding」用例只能在注入下测。
pub trait Resolver {
    fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, GuardError>;
}

// —— 分类表：以下常量的字面内容就是对外承诺的「人话」，测试按子串点名 ——
const R_V4_LOOPBACK: &str = "环回";
const R_V4_PRIVATE: &str = "私网";
const R_V4_LINK_LOCAL: &str = "链路本地/云元数据";
const R_V4_UNSPECIFIED: &str = "本网络";
const R_V4_MULTICAST: &str = "组播";
const R_V4_RESERVED: &str = "保留";
const R_V4_CGNAT: &str = "运营商级 NAT";
const R_V4_V4COMPAT: &str = "IPv4 兼容地址";
const R_V6_LOOPBACK: &str = "IPv6 环回";
const R_V6_UNSPECIFIED: &str = "IPv6 未指定";
const R_V6_LINK_LOCAL: &str = "IPv6 链路本地";
const R_V6_MULTICAST: &str = "IPv6 组播";
const R_V6_UNIQUE_LOCAL: &str = "IPv6 唯一本地地址";
const R_V6_SITE_LOCAL: &str = "IPv6 站点本地地址（已废弃）";
const R_NAME_LOOPBACK: &str = "本机环回主机名";
const R_NAME_METADATA: &str = "云元数据主机名";

/// 唯一分类处：`Some(理由)` 即禁止连接，`None` 即公网可达、允许。
/// IPv4 映射/兼容，以及 NAT64/6to4/Teredo/站点本地这些**内嵌 IPv4 的 IPv6 族**，
/// 都要在这里把内层 v4 抽出来**递归**回 IPv4 判定——不递归就等于留洞。
pub fn deny_reason(addr: IpAddr) -> Option<&'static str> {
    match addr {
        IpAddr::V4(v4) => deny_v4(v4),
        IpAddr::V6(v6) => deny_v6(v6),
    }
}

fn deny_v4(v4: Ipv4Addr) -> Option<&'static str> {
    if v4.is_loopback() {
        return Some(R_V4_LOOPBACK);
    }
    if v4.is_private() {
        return Some(R_V4_PRIVATE);
    }
    if v4.is_link_local() {
        return Some(R_V4_LINK_LOCAL);
    }
    if v4.is_unspecified() {
        return Some(R_V4_UNSPECIFIED);
    }
    if v4.is_multicast() {
        return Some(R_V4_MULTICAST);
    }
    if v4.is_broadcast() {
        return Some(R_V4_RESERVED);
    }
    // IANA 特殊段里 std API 没覆盖的，全部在这里显式点名（顺序即优先级）。
    v4_reserved_networks()
        .iter()
        .find(|(net, _)| net.contains(&v4))
        .map(|(_, reason)| *reason)
}

/// 用 `ipnet` 显式列出的 IPv4 特殊段；`OnceLock` 只解析一次。
/// 注意 `Ipv4Addr::is_private` 已覆盖 10/8、172.16/12、192.168/16，这里不重复。
fn v4_reserved_networks() -> &'static [(Ipv4Net, &'static str)] {
    static NETWORKS: OnceLock<Vec<(Ipv4Net, &'static str)>> = OnceLock::new();
    NETWORKS
        .get_or_init(|| {
            [
                ("0.0.0.0/8", "本网络"), // is_unspecified 只管 0.0.0.0 本身，0.x 整段同样不可达公网
                ("100.64.0.0/10", R_V4_CGNAT),
                ("198.18.0.0/15", "设备基准测试保留"),
                ("192.0.2.0/24", "文档保留网段"),
                ("198.51.100.0/24", "文档保留网段"),
                ("203.0.113.0/24", "文档保留网段"),
                ("192.0.0.0/24", "保留（IETF 协议分配）"),
                ("192.88.99.0/24", "6to4 中继任播（已废弃保留）"),
                ("240.0.0.0/4", R_V4_RESERVED),
            ]
            .iter()
            .map(|(s, reason)| {
                (
                    s.parse::<Ipv4Net>()
                        .unwrap_or_else(|e| panic!("内置网段 {s} 非法：{e}")),
                    *reason,
                )
            })
            .collect()
        })
        .as_slice()
}

fn deny_v6(v6: Ipv6Addr) -> Option<&'static str> {
    // ::1 与 :: 必须先看本体语义（`to_ipv4` 会把它们折成 0.0.0.1/0.0.0.0，
    // 先折叠就会把「IPv6 环回」错报成「IPv4 映射…」）。
    if v6.is_loopback() {
        return Some(R_V6_LOOPBACK);
    }
    if v6.is_unspecified() {
        return Some(R_V6_UNSPECIFIED);
    }
    if v6.is_multicast() {
        return Some(R_V6_MULTICAST);
    }
    if let Some(v4) = v6.to_ipv4_mapped() {
        return deny_v4(v4).map(v4_mapped_reason);
    }
    let seg = v6.segments();
    if seg[..6] == [0; 6] {
        // ::/96 的 IPv4 兼容形式（已废弃，如 [::127.0.0.1]）：本体不可路由，整段拒。
        return Some(R_V4_V4COMPAT);
    }
    if seg[0] & 0xffc0 == 0xfe80 {
        return Some(R_V6_LINK_LOCAL);
    }
    if seg[0] & 0xfe00 == 0xfc00 {
        return Some(R_V6_UNIQUE_LOCAL);
    }
    if seg[0] == 0x100 {
        return Some("IPv6 丢弃保留段"); // 100::/64
    }
    if seg[0] == 0x2001 && seg[1] == 0xdb8 {
        return Some("IPv6 文档保留网段"); // 2001:db8::/32
    }
    // —— IPv6→IPv4 内嵌族 —— 外层前缀在 IANA 表里是「可全球路由」的，光看前缀
    // 一律放行就等于留洞：`http://[64:ff9b::a9fe:a9fe]/` 不经任何解析就能打到
    // 169.254.169.254（云元数据红线）。下面每一族都把内层 IPv4 抽出来递归回
    // `deny_v4`，理由拼成「<族名> 内嵌 IPv4 <内层类别>」，让调用方能指出是哪个
    // 地址、因为哪个族被拒。
    let bits = v6.to_bits();
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0; 4] {
        // NAT64 well-known prefix 64:ff9b::/96（RFC 6052）：低 32 位即内嵌 IPv4。
        return embedded_v4_denial(bits as u32, EmbeddedV4Family::Nat64);
    }
    if seg[0] == 0x2002 {
        // 6to4 2002::/16（RFC 3056）：内嵌 IPv4 在第 16~47 位。
        return embedded_v4_denial(
            ((bits >> 80) & 0xffff_ffff) as u32,
            EmbeddedV4Family::SixToFour,
        );
    }
    if seg[0] == 0x2001 && seg[1] == 0x0000 {
        // Teredo 2001::/32（RFC 4364）：低 32 位存的是**按位取反**的客户端 IPv4。
        // 两种读法都要验——只看字面尾段会漏 `2001::80ff:fffe`（取反后是 127.0.0.1），
        // 只看取反值会漏 `2001::1`（尾段本体是 0.0.0.1）。取反优先，那才是本体的语义。
        let low = bits as u32;
        return embedded_v4_denial(!low, EmbeddedV4Family::Teredo)
            .or_else(|| embedded_v4_denial(low, EmbeddedV4Family::TeredoRaw));
    }
    if seg[0] & 0xffc0 == 0xfec0 {
        // 站点本地 fec0::/10（RFC 3879 已废弃）：整段本就不公网路由，尾部又常被当
        // 内嵌 IPv4 用，所以先报内嵌族、兜底整段拒。NAT64/6to4 不能这样兜底：它们
        // 内嵌公网地址时是合法的公网目标，整段拒就是误杀。
        return embedded_v4_denial(bits as u32, EmbeddedV4Family::SiteLocal)
            .or(Some(R_V6_SITE_LOCAL));
    }
    None
}

/// 内嵌 IPv4 的 IPv6 过渡/隧道族；族名进拒绝理由，方便定位是哪一族、哪个内层地址。
enum EmbeddedV4Family {
    /// 64:ff9b::/96（RFC 6052 well-known prefix）。
    Nat64,
    /// 2002::/16（RFC 3056）。
    SixToFour,
    /// 2001::/32 低 32 位按 RFC 4364 取反还原出的客户端地址。
    Teredo,
    /// 2001::/32 低 32 位的**字面**尾段（未按 RFC 取反的写法）。
    TeredoRaw,
    /// fec0::/10（RFC 3879，已废弃的站点本地）。
    SiteLocal,
}

/// 理由必须是 `&'static str`（`Denied.reason` 的签名），所以族名用 `concat!`
/// 在编译期拼接，不引入运行时格式化。
macro_rules! embedded_v4_reason_str {
    ($family:literal, $inner:expr) => {
        match $inner {
            R_V4_LOOPBACK => concat!($family, " 内嵌 IPv4 环回"),
            R_V4_PRIVATE => concat!($family, " 内嵌 IPv4 私网"),
            R_V4_LINK_LOCAL => concat!($family, " 内嵌 IPv4 链路本地/云元数据"),
            R_V4_UNSPECIFIED => concat!($family, " 内嵌 IPv4 本网络"),
            R_V4_MULTICAST => concat!($family, " 内嵌 IPv4 组播"),
            R_V4_CGNAT => concat!($family, " 内嵌 IPv4 运营商级 NAT"),
            _ => concat!($family, " 内嵌 IPv4 保留地址"),
        }
    };
}

fn embedded_v4_reason(family: EmbeddedV4Family, inner: &'static str) -> &'static str {
    match family {
        EmbeddedV4Family::Nat64 => embedded_v4_reason_str!("NAT64", inner),
        EmbeddedV4Family::SixToFour => embedded_v4_reason_str!("6to4", inner),
        EmbeddedV4Family::Teredo => embedded_v4_reason_str!("Teredo", inner),
        EmbeddedV4Family::TeredoRaw => embedded_v4_reason_str!("Teredo 未取反尾部", inner),
        EmbeddedV4Family::SiteLocal => embedded_v4_reason_str!("站点本地", inner),
    }
}

/// 把内嵌的 32 位还原成 IPv4 并**递归**回 `deny_v4`；`None` 表示内嵌的是公网地址，
/// 即这个过渡地址本身是合法目标（族检查因此不会整段误杀）。
fn embedded_v4_denial(raw: u32, family: EmbeddedV4Family) -> Option<&'static str> {
    deny_v4(Ipv4Addr::from(raw)).map(|inner| embedded_v4_reason(family, inner))
}

/// 递归结果的「IPv4 映射」前缀映射——保持 &'static str 签名不退化。
fn v4_mapped_reason(inner: &'static str) -> &'static str {
    match inner {
        R_V4_LOOPBACK => "IPv4 映射环回",
        R_V4_PRIVATE => "IPv4 映射私网",
        R_V4_LINK_LOCAL => "IPv4 映射链路本地/云元数据",
        R_V4_UNSPECIFIED => "IPv4 映射本网络",
        R_V4_MULTICAST => "IPv4 映射组播",
        R_V4_CGNAT => "IPv4 映射运营商级 NAT",
        R_V4_RESERVED | R_V4_V4COMPAT => "IPv4 映射保留地址",
        _ => "IPv4 映射保留地址",
    }
}

/// URL 形态层第一道闸：方案白名单 + 主机存在性 + **字面 IP 直接分类** + 名称级红线。
///
/// 十进制/十六进制/八进制简写主机（`http://2130706433/`、`http://0x7f.0.0.1/`）
/// 由 `url` crate 按 WHATWG 标准归一成 `Host::Ipv4`，走的就是下面的字面 IP
/// 分支；实测确认后此处不另设第二份解析（见 task 22 报告 findings）。
///
/// `Host::Domain` 只做 `deny_domain` 的名称级红线；完整的地址级防护仍要求
/// `resolve_target` 也跑过（见模块开头不变式）。
pub fn check_url(raw: &str) -> Result<Url, GuardError> {
    let url = Url::parse(raw).map_err(|_| GuardError::SchemeDenied(raw.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(GuardError::SchemeDenied(url.as_str().to_string()));
    }
    let denied = |addr| GuardError::Denied {
        addr,
        reason: deny_reason(addr).unwrap_or("保留"),
    };
    match url.host() {
        Some(Host::Ipv4(a)) => {
            let addr = IpAddr::V4(a);
            if deny_reason(addr).is_some() {
                Err(denied(addr))
            } else {
                Ok(url)
            }
        }
        Some(Host::Ipv6(a)) => {
            let addr = IpAddr::V6(a);
            if deny_reason(addr).is_some() {
                Err(denied(addr))
            } else {
                Ok(url)
            }
        }
        Some(Host::Domain(name)) => {
            if let Some(reason) = deny_domain(name) {
                return Err(GuardError::DeniedHostName {
                    host: name.to_string(),
                    reason,
                });
            }
            Ok(url)
        }
        None => Err(GuardError::NoHost),
    }
}

/// 名称级红线：不查 DNS 也能确定危险的主机名。
///
/// 只做**精确**匹配——`localhost`、任意 `*.localhost` 后缀、`metadata.google.internal`
/// （大小写不敏感、去掉根点）。刻意**不**加 `.local`/`.internal` 这类通配后缀规则：
/// 那是误杀面，而计划红线点名的就是这两个形态。缺了这道闸，只调 `check_url` 的
/// 调用方（T36 的用户自配 MCP URL）会放过 `http://localhost/`。
fn deny_domain(name: &str) -> Option<&'static str> {
    // `url` crate 已按 WHATWG 小写化主机名，但这里不依赖那个实现细节。
    let h = name.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return Some(R_NAME_LOOPBACK);
    }
    if h == "metadata.google.internal" {
        return Some(R_NAME_METADATA);
    }
    None
}

/// 解析结果整集复验：任何一个地址命中分类表就拒绝整个集合（DNS 混载绕过面）。
/// 空集合同样拒绝——「没查到私网地址」不等于「可以连」。
pub fn assert_all_public(addrs: &[IpAddr]) -> Result<(), GuardError> {
    if addrs.is_empty() {
        return Err(GuardError::Resolution(
            "(空解析结果)".to_string(),
            "地址集为空，无法证明目标可公网访问".to_string(),
        ));
    }
    for &addr in addrs {
        if let Some(reason) = deny_reason(addr) {
            return Err(GuardError::Denied { addr, reason });
        }
    }
    Ok(())
}

/// 第二道闸：URL 已过 `check_url` 后，按主机形态拿最终地址集。
/// `Host::Ipv4/Ipv6` 直接分类且**不做二次解析**；`Host::Domain` 才走 `Resolver`。
pub fn resolve_target(url: &Url, resolver: &dyn Resolver) -> Result<Target, GuardError> {
    let host = url.host_str().unwrap_or("").to_string();
    let target = |addrs: Vec<IpAddr>| Target {
        url: url.clone(),
        host: host.clone(),
        addrs,
    };
    match url.host() {
        Some(Host::Ipv4(a)) => {
            let addr = IpAddr::V4(a);
            match deny_reason(addr) {
                Some(reason) => Err(GuardError::Denied { addr, reason }),
                None => Ok(target(vec![addr])),
            }
        }
        Some(Host::Ipv6(a)) => {
            let addr = IpAddr::V6(a);
            match deny_reason(addr) {
                Some(reason) => Err(GuardError::Denied { addr, reason }),
                None => Ok(target(vec![addr])),
            }
        }
        Some(Host::Domain(_)) => {
            let addrs = resolver
                .lookup(&host)
                .map_err(|e| GuardError::Resolution(host.clone(), e.to_string()))?;
            if addrs.is_empty() {
                return Err(GuardError::Resolution(
                    host,
                    "解析结果为空，拒绝按「无私网地址」放行".to_string(),
                ));
            }
            // DNS 混载：整集复验由 assert_all_public 执行；命中私网时换成
            // 集合专属变体（测试与 task 23 的逐跳复验都点名 PrivateInDnsSet）。
            assert_all_public(&addrs).map_err(|e| match e {
                GuardError::Denied { addr, .. } => GuardError::PrivateInDnsSet { addr },
                other => other,
            })?;
            Ok(target(addrs))
        }
        None => Err(GuardError::NoHost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_reason_covers_every_private_class() {
        let cases: &[(&str, &str)] = &[
            ("127.0.0.1", "环回"),
            ("127.2.3.4", "环回"),
            ("10.0.0.7", "私网"),
            ("172.16.5.5", "私网"),
            ("192.168.1.1", "私网"),
            ("169.254.169.254", "链路本地/云元数据"),
            ("100.64.0.1", "运营商级 NAT"),
            ("0.0.0.0", "本网络"),
            ("224.0.0.1", "组播"),
            ("255.255.255.255", "保留"),
            ("::1", "IPv6 环回"),
            ("fe80::1", "IPv6 链路本地"),
            ("fd00::1234", "IPv6 唯一本地地址"),
            ("ff02::1", "IPv6 组播"),
            ("::", "IPv6 未指定"),
            ("::ffff:127.0.0.1", "IPv4 映射环回"),
            ("::ffff:169.254.169.254", "IPv4 映射链路本地"),
        ];
        for (s, want) in cases {
            let a: IpAddr = s
                .parse()
                .unwrap_or_else(|_| panic!("{s} 不是合法 IP，用例写错了"));
            let got = deny_reason(a).unwrap_or_else(|| panic!("{a} 必须被拒，理由应为「{want}」"));
            assert!(got.contains(want), "{a} 的理由不对：{got}");
        }
        for ok in ["93.184.216.34", "1.1.1.1", "8.8.8.8", "2606:4700::1111"] {
            let a: IpAddr = ok.parse().unwrap();
            assert_eq!(
                deny_reason(a),
                None,
                "{a} 是公网，不许误杀（误杀会让联网功能整体不可用）"
            );
        }
    }

    #[test]
    fn check_url_denies_non_http_and_literal_private_hosts() {
        for bad in [
            "file:///C:/secrets.txt",
            "gopher://127.0.0.1:6379/",
            "ftp://example.com/x",
        ] {
            assert!(
                matches!(check_url(bad), Err(GuardError::SchemeDenied(_))),
                "{bad} 必须按方案拒绝"
            );
        }
        // WHATWG URL 会把十进制/十六进制/八进制简写解析成 IPv4，这些是经典绕过面
        assert!(matches!(
            check_url("http://2130706433/"),
            Err(GuardError::Denied { .. })
        ));
        assert!(matches!(
            check_url("http://0x7f.0.0.1/"),
            Err(GuardError::Denied { .. })
        ));
        assert!(matches!(
            check_url("http://[::1]/"),
            Err(GuardError::Denied { .. })
        ));
        assert!(matches!(
            check_url("http://169.254.169.254/latest/meta-data/"),
            Err(GuardError::Denied { .. })
        ));
        // 用户没写方案时 Url::parse 会失败；不许替它猜 http://，要报明确错误
        assert!(check_url("example.com").is_err());
        assert!(check_url("https://example.com/a?b=1").is_ok());
    }

    #[test]
    fn a_dns_answer_containing_one_private_address_is_denied() {
        struct Scripted(Vec<IpAddr>);
        impl Resolver for Scripted {
            fn lookup(&self, _: &str) -> Result<Vec<IpAddr>, GuardError> {
                Ok(self.0.clone())
            }
        }
        let mix = Scripted(vec![
            "93.184.216.34".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ]);
        let url = check_url("https://evil.example/").unwrap();
        match resolve_target(&url, &mix) {
            Err(GuardError::PrivateInDnsSet { .. }) => {}
            other => panic!("A 记录里混一个环回就必须整集拒绝，实得 {other:?}"),
        }
        // 空解析结果不能当成「没有私网地址」放行
        assert!(matches!(
            resolve_target(&url, &Scripted(vec![])),
            Err(GuardError::Resolution(..))
        ));
    }

    /// Finding 3：`::ffff:/96` 与 `::/96` 两段递归本身。删掉 `deny_v6` 里的
    /// `seg[..6] == [0; 6]` 分支（brief 称「这一步是整个表的要点」）必须让本测试变红。
    #[test]
    fn deny_reason_recurses_through_mapped_and_v4_compatible_forms() {
        let mapped: &[(&str, &str)] = &[
            ("::ffff:127.0.0.1", "IPv4 映射环回"),
            ("::ffff:10.0.0.7", "IPv4 映射私网"),
            ("::ffff:169.254.169.254", "IPv4 映射链路本地/云元数据"),
            ("::ffff:100.64.0.1", "IPv4 映射运营商级 NAT"),
        ];
        for (s, want) in mapped {
            let a: IpAddr = s
                .parse()
                .unwrap_or_else(|_| panic!("{s} 不是合法 IP，用例写错了"));
            let got = deny_reason(a).unwrap_or_else(|| panic!("{a} 必须被拒，理由应为「{want}」"));
            assert!(got.contains(want), "{a} 的理由不对：实得「{got}」");
        }
        // 是「递归回 v4 判定」而不是「见到 ::ffff: 一律拒」：公网映射地址必须放行。
        assert_eq!(
            deny_reason("::ffff:93.184.216.34".parse().unwrap()),
            None,
            "::ffff: 段整段拒绝会误杀 NAT64/双栈真实站点"
        );

        // 已废弃的 `::/96` IPv4 兼容形式（尾部是 hextet，不走 to_ipv4_mapped）。
        for s in ["::7f00:1", "::a00:1", "::a9fe:a9fe"] {
            let a: IpAddr = s.parse().unwrap();
            let got = deny_reason(a).unwrap_or_else(|| panic!("{a} 的 IPv4 兼容形式必须被拒"));
            assert!(
                got.contains("IPv4 兼容地址"),
                "{a} 的理由不对：实得「{got}」"
            );
        }
    }

    /// Finding 1：所有 IPv6→IPv4 内嵌族都必须把内层 v4 抽出来递归回 `deny_v4`。
    #[test]
    fn deny_reason_recurses_through_every_ipv4_embedded_family() {
        let cases: &[(&str, &str)] = &[
            // NAT64 64:ff9b::/96：低 32 位就是内嵌 IPv4。
            ("64:ff9b::7f00:1", "NAT64 内嵌 IPv4 环回"),
            ("64:ff9b::a00:1", "NAT64 内嵌 IPv4 私网"),
            ("64:ff9b::a9fe:a9fe", "NAT64 内嵌 IPv4 链路本地/云元数据"),
            ("64:ff9b::6440:1", "NAT64 内嵌 IPv4 运营商级 NAT"),
            // 6to4 2002::/16：第 16~47 位是内嵌 IPv4。
            ("2002:7f00:1::", "6to4 内嵌 IPv4 环回"),
            ("2002:a9fe:a9fe::", "6to4 内嵌 IPv4 链路本地/云元数据"),
            // Teredo 2001::/32：RFC 4364 的低 32 位是**按位取反**后的客户端 IPv4。
            ("2001::80ff:fffe", "Teredo 内嵌 IPv4 环回"),
            (
                "2001::a9fe:a9fe",
                "Teredo 未取反尾部 内嵌 IPv4 链路本地/云元数据",
            ),
            ("2001::1", "Teredo"),
            // 站点本地 fec0::/10（RFC 3879 已废弃，整段不可公网路由）。
            ("fec0::a9fe:a9fe", "站点本地 内嵌 IPv4 链路本地/云元数据"),
            ("fec0::1", "站点本地"),
            // 文档段 2001:db8::/32 与 Teredo 2001::/32 在 /32 上并不重叠，
            // 但表里文档段先判：这里钉住它不被错报成 Teredo。
            ("2001:db8::7f00:1", "文档"),
        ];
        for (s, want) in cases {
            let a: IpAddr = s
                .parse()
                .unwrap_or_else(|_| panic!("{s} 不是合法 IP，用例写错了"));
            let got =
                deny_reason(a).unwrap_or_else(|| panic!("{a} 必须被拒，理由应点名「{want}」"));
            assert!(
                got.contains(want),
                "{a} 的理由要点名是哪个地址、哪个族：期望含「{want}」，实得「{got}」"
            );
        }
        // 反向不许误杀：内嵌**公网** IPv4 的合法过渡地址必须放行，否则等于关掉 IPv6 联网。
        for ok in ["64:ff9b::5d58:d822", "2002:5d58:d822::", "2606:4700::1111"] {
            let a: IpAddr = ok.parse().unwrap();
            assert_eq!(deny_reason(a), None, "{a} 是可达公网的目标，不许误杀");
        }
        // 逐字钉住给用户看的文案：族名 + 内层类别，调用方能直接展示。
        assert_eq!(
            deny_reason("64:ff9b::a9fe:a9fe".parse().unwrap()),
            Some("NAT64 内嵌 IPv4 链路本地/云元数据")
        );
        assert_eq!(
            deny_reason("2002:7f00:1::".parse().unwrap()),
            Some("6to4 内嵌 IPv4 环回")
        );
        assert_eq!(
            deny_reason("2001::80ff:fffe".parse().unwrap()),
            Some("Teredo 内嵌 IPv4 环回")
        );
        assert_eq!(
            deny_reason("2001::a9fe:a9fe".parse().unwrap()),
            Some("Teredo 未取反尾部 内嵌 IPv4 链路本地/云元数据")
        );
        assert_eq!(
            deny_reason("fec0::a9fe:a9fe".parse().unwrap()),
            Some("站点本地 内嵌 IPv4 链路本地/云元数据")
        );
        assert_eq!(
            deny_reason("fec0::1.1.1.1".parse().unwrap()),
            Some("IPv6 站点本地地址（已废弃）")
        );
    }

    /// Finding 1 的真实可达形态：不带任何解析器，一个 URL 字面量就够。
    #[test]
    fn check_url_denies_literal_urls_of_the_embedded_ipv4_families() {
        for url in [
            "http://[64:ff9b::a9fe:a9fe]/",     // ⇒ 169.254.169.254：云元数据红线
            "http://[64:ff9b::7f00:1]/",        // ⇒ 127.0.0.1
            "http://[2002:7f00:1::]/",          // ⇒ 127.0.0.1
            "http://[2001::1]/",                // Teredo
            "http://[2001::80ff:fffe]/",        // Teredo：取反还原后是 127.0.0.1
            "http://[fec0::a9fe:a9fe]/",        // 站点本地
            "http://[::ffff:169.254.169.254]/", // IPv4 映射
            "http://[::7f00:1]/",               // IPv4 兼容
        ] {
            let got = check_url(url);
            assert!(
                matches!(got, Err(GuardError::Denied { .. })),
                "{url} 必须按字面 IP 拒绝，实得 {got:?}"
            );
        }
        // 云元数据那条红线要一路点名到「族 + 内层类别」，供 task 36 直接给用户看。
        let e = check_url("http://[64:ff9b::a9fe:a9fe]/").unwrap_err();
        assert!(
            matches!(&e, GuardError::Denied { addr, reason }
                if addr.to_string() == "64:ff9b::a9fe:a9fe"
                && reason.contains("NAT64")
                && reason.contains("链路本地/云元数据")),
            "实得 {e:?}"
        );
        // 反向：内嵌公网 IPv4 的 NAT64 地址与真公网 IPv6 仍是合法目标。
        for ok in ["http://[64:ff9b::5d58:d822]/", "http://[2606:4700::1111]/"] {
            assert!(
                check_url(ok).is_ok(),
                "{ok} 是公网目标，误杀等于砍掉 IPv6 站点"
            );
        }
    }

    /// Finding 2：计划红线点名的主机名字面形式（`localhost` 与云元数据名）。
    #[test]
    fn check_url_denies_loopback_and_metadata_hostnames() {
        for name in [
            "http://localhost/",
            "https://localhost:8443/x",
            "http://LOCALHOST./",
            "http://LocalHost/",
            "http://api.localhost/",
            "http://nested.service.localhost.",
            "http://metadata.google.internal/",
            "http://Metadata.Google.Internal./latest",
        ] {
            assert!(
                check_url(name).is_err(),
                "{name} 是红线点名的环回/元数据主机名，必须拒绝"
            );
        }
        // 拒绝必须点名：是哪个名称、撞了哪条红线（调用方要能对用户说清楚）。
        let e = check_url("http://localhost/").unwrap_err();
        assert!(
            matches!(&e, GuardError::DeniedHostName { host, reason }
                if host == "localhost" && *reason == R_NAME_LOOPBACK),
            "实得 {e:?}"
        );
        assert!(
            e.to_string().to_ascii_lowercase().contains("localhost"),
            "文案要点名是哪个地址：{e}"
        );
        let meta = check_url("http://Metadata.Google.Internal./latest").unwrap_err();
        assert!(
            matches!(&meta, GuardError::DeniedHostName { reason, .. } if *reason == R_NAME_METADATA),
            "实得 {meta:?}"
        );
        assert!(
            meta.to_string()
                .to_ascii_lowercase()
                .contains("metadata.google.internal"),
            "文案要点名是哪个地址：{meta}"
        );
        // 过头收紧同样是缺陷：这些都不是 `localhost`，也不是那个元数据主机名。
        for ok in [
            "http://notlocalhost.example.com/",
            "http://mylocalhost/",
            "http://localhost.evil.example/",
            "http://evil.localhost.evil.example/",
            "http://metadata.google.internal.example.com/",
            "http://notmetadata.google.internal/",
            "http://intranet.local/",
            "http://api.internal/",
        ] {
            assert!(check_url(ok).is_ok(), "{ok} 不该被名称闸误杀");
        }
    }

    /// Finding 3：`resolve_target` 的字面 IP 分支绝不回落到解析器。
    #[test]
    fn resolve_target_never_re_resolves_a_literal_ip() {
        struct Exploding;
        impl Resolver for Exploding {
            fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, GuardError> {
                panic!("字面 IP 不许二次解析（这正是 DNS rebinding 的入口），却被要求解析 {host}");
            }
        }
        let url = check_url("http://93.184.216.34/").unwrap();
        let t = resolve_target(&url, &Exploding).unwrap();
        assert_eq!(t.addrs, vec!["93.184.216.34".parse::<IpAddr>().unwrap()]);
        assert_eq!(t.host, "93.184.216.34");

        let url6 = check_url("http://[2606:4700::1111]/").unwrap();
        assert_eq!(
            resolve_target(&url6, &Exploding).unwrap().addrs,
            vec!["2606:4700::1111".parse::<IpAddr>().unwrap()]
        );

        // 私网字面量同样在「解析之前」就拒，且同样不碰解析器。
        let bad = Url::parse("http://127.0.0.1/").unwrap();
        assert!(matches!(
            resolve_target(&bad, &Exploding),
            Err(GuardError::Denied { .. })
        ));
    }

    /// Finding 3：`assert_all_public` 作为公开 API 被直接调用（task 23 逐跳复验的用法）。
    #[test]
    fn assert_all_public_rejects_any_private_member_of_the_set() {
        let mixed: Vec<IpAddr> = ["93.184.216.34", "100.64.0.1"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let got = assert_all_public(&mixed).unwrap_err();
        assert!(
            matches!(&got, GuardError::Denied { addr, reason }
                if addr.to_string() == "100.64.0.1" && reason.contains("运营商级 NAT")),
            "整集复验必须点名是哪个地址命中，实得 {got:?}"
        );
        // 全公网集合（含 IPv6）放行。
        let all_public: Vec<IpAddr> = ["93.184.216.34", "2606:4700::1111"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_all_public(&all_public).unwrap();
        // 新加的 IPv6 内嵌族同样经由 assert_all_public 生效（DNS 返回 NAT64 地址时）。
        let nat64: Vec<IpAddr> = vec!["64:ff9b::a9fe:a9fe".parse().unwrap()];
        assert!(matches!(
            assert_all_public(&nat64),
            Err(GuardError::Denied { .. })
        ));
        // 空集合不是「没有私网地址」。
        assert!(matches!(
            assert_all_public(&[]),
            Err(GuardError::Resolution(..))
        ));
    }

    /// Finding 3 + R248：容量错误的 `Display` 打印裸数字串，task 23 的断言依赖它。
    #[test]
    fn too_large_body_display_prints_the_bare_digit_run() {
        let e = GuardError::TooLargeBody { limit: 2_097_152 };
        assert_eq!(e.to_string(), "响应体超过 2097152 字节上限");
        assert!(e.to_string().contains("2097152"), "{e}");
        assert!(format!("{e:?}").contains("2097152"), "{e:?}");
    }
}
