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

/// 唯一分类处：`Some(理由)` 即禁止连接，`None` 即公网可达、允许。
/// IPv4 映射/兼容的 IPv6 地址在这里**递归**回 IPv4 判定——不递归就等于留洞。
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
    None
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

/// URL 形态层第一道闸：方案白名单 + 主机存在性 + **字面 IP 直接分类**。
///
/// 十进制/十六进制/八进制简写主机（`http://2130706433/`、`http://0x7f.0.0.1/`）
/// 由 `url` crate 按 WHATWG 标准归一成 `Host::Ipv4`，走的就是下面的字面 IP
/// 分支；实测确认后此处不另设第二份解析（见 task 22 报告 findings）。
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
        Some(Host::Domain(_)) => Ok(url),
        None => Err(GuardError::NoHost),
    }
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
}
