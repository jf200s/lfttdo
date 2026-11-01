//! WebView **顶层导航**白名单。
//!
//! 前端已经拦了 Markdown 链接的点击/中键/拖拽，但 WebView 还有别的导航入口
//! （右键菜单、脚本、拖放等）。这一层是最后一道：只要目标不是应用自身来源或
//! 开发服务器，一律拒绝导航，页面留在应用里。
//!
//! 本模块不依赖 Tauri，纯函数可单测。

/// 允许的顶层导航目标：
/// - 生产：Windows 的 `http(s)://tauri.localhost`，其他平台的 `tauri://localhost`；
/// - 开发：Vite 的 `http://localhost:1420` / `http://127.0.0.1:1420` / `http://[::1]:1420`；
/// - `about:blank`（WebView 的空白页）。
///
/// 其余一律拒绝，包括 http(s) 的外部站点、`file:`、`data:`、`javascript:` 与自定义协议。
pub fn navigation_allowed(url: &url::Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "about" => url.path() == "blank",
        "http" | "https" => {
            let Some(host) = url.host_str() else {
                return false;
            };
            if host.eq_ignore_ascii_case("tauri.localhost") {
                return true;
            }
            // `url` 对 IPv6 的 host_str 会带方括号（`[::1]`），两种形态都认。
            matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") && url.port() == Some(1420)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn only_the_app_and_dev_origins_may_be_navigated_to() {
        for ok in [
            "tauri://localhost/index.html",
            "http://tauri.localhost/index.html",
            "https://tauri.localhost/index.html",
            "http://localhost:1420/",
            "http://127.0.0.1:1420/index.html",
            "http://[::1]:1420/",
            "about:blank",
        ] {
            assert!(
                navigation_allowed(&Url::parse(ok).unwrap()),
                "{ok} 应该被允许"
            );
        }
        for bad in [
            "https://example.com",
            "http://tauri.localhost.evil.com/",
            "http://localhost:3000/",
            "http://127.0.0.1/",
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "mailto:a@b.c",
        ] {
            assert!(
                !navigation_allowed(&Url::parse(bad).unwrap()),
                "{bad} 必须被拒绝"
            );
        }
    }
}
