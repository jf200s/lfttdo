//! 打开外部链接的唯一出口。只允许 http(s)，交给系统默认浏览器
//! （Windows：绝对路径 rundll32；Unix：`xdg-open`）。
//!
//! 不用 shell：`rundll32 url.dll,FileProtocolHandler` 收的是 argv 里的一个 URL，
//! 不经过 `cmd /C`，所以 URL 里的 `&`/`|` 不会被解释成命令分隔符。
//! 协议白名单是这里的第一道闸：`file:`、`javascript:` 与自定义协议一律拒绝。

/// 协议白名单 + 长度上限。`javascript:` / `file:` / 自定义协议一律拒绝。
/// 返回解析后的 URL：打开动作必须用它，不能再用原始串（解析会剥掉首尾控制字符，
/// 「校验的对象」和「打开的对象」必须是同一个）。
pub fn validate_external_url(url: &str) -> Result<url::Url, String> {
    if url.len() > 2048 {
        return Err("链接过长（上限 2048 字符）".to_string());
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("链接格式无效：{e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(format!("只允许打开 http(s) 链接，收到 `{other}`")),
    }
}

/// 校验后交给系统默认浏览器。失败只返回文案，不弹任何窗口。
pub fn open_external(url: &str) -> Result<(), String> {
    let parsed = validate_external_url(url)?;
    #[cfg(windows)]
    {
        launcher(parsed.as_str())
            .spawn()
            .map_err(|e| format!("无法打开浏览器：{e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        launcher(parsed.as_str())
            .spawn()
            .map_err(|e| format!("无法打开浏览器：{e}"))?;
        Ok(())
    }
}

/// `rundll32` 必须用**绝对路径**：裸名会走 CreateProcess 的搜索序，便携目录
/// （可写）里的同名文件会先于 System32 被选中。argv 里只有一个 URL 元素，
/// 不经过 shell，`&`/`|` 不会被解释成命令分隔符。
#[cfg(windows)]
fn launcher(url: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(rundll32_path());
    cmd.args(["url.dll,FileProtocolHandler", url])
        .creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(windows)]
fn rundll32_path() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    std::path::PathBuf::from(root)
        .join("System32")
        .join("rundll32.exe")
}

/// `xdg-open` 是 freedesktop 的标准入口，走 PATH 解析（/usr/bin 不含当前目录，没有
/// CreateProcess 那条「便携目录先于 System32」的搜索序问题，无需绝对路径化）。
/// argv 里只有一个 URL 元素，不经 shell，`&`/`|` 不会被解释 —— 与 rundll32 同一纪律。
#[cfg(not(windows))]
fn launcher(url: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_allowed() {
        for ok in ["http://example.com", "https://example.com/a?b=c&d=e"] {
            assert!(validate_external_url(ok).is_ok(), "{ok} 应被允许");
        }
        for bad in [
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "mailto:a@b.c",
            "https://",
            "not a url",
        ] {
            assert!(validate_external_url(bad).is_err(), "{bad:?} 必须被拒");
        }
        let long = format!("https://example.com/{}", "a".repeat(2048));
        assert!(validate_external_url(&long).is_err(), "超长链接必须被拒");
    }

    /// 打开命令的形状：绝对路径的 System32 rundll32 + 一个 URL argv。
    /// 不真正 spawn（那会弹出浏览器），只检查构造出来的命令。
    #[cfg(windows)]
    #[test]
    fn the_launcher_is_an_absolute_rundll32_with_one_url_argument() {
        let path = rundll32_path();
        assert!(path.is_absolute(), "必须用绝对路径：{}", path.display());
        assert!(
            path.to_string_lossy()
                .to_lowercase()
                .ends_with("system32\\rundll32.exe"),
            "必须落在 System32：{}",
            path.display()
        );

        let cmd = launcher("https://example.com/a?b=1&c=2");
        assert!(
            cmd.get_program()
                .to_string_lossy()
                .ends_with("rundll32.exe")
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "url.dll,FileProtocolHandler".to_string(),
                "https://example.com/a?b=1&c=2".to_string()
            ],
            "URL 必须是单独一个 argv，不许拼进 shell 串"
        );
    }

    /// 打开命令的形状：xdg-open + 一个 URL argv。不真正 spawn（那会弹出浏览器），
    /// 只检查构造出来的命令 —— 与上面 rundll32 那条同一条验收。
    #[cfg(not(windows))]
    #[test]
    fn the_launcher_is_xdg_open_with_one_url_argument() {
        let cmd = launcher("https://example.com/a?b=1&c=2");
        assert_eq!(cmd.get_program(), "xdg-open");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["https://example.com/a?b=1&c=2".to_string()],
            "URL 必须是单独一个 argv，不许拼进 shell 串"
        );
    }
}
