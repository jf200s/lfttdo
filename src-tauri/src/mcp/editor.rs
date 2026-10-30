// 应用内配置编辑器的文件边界：mcp.json 的读取、状态、严格校验与原子写入。
//
// 与 config.rs 的分工：config.rs 只做纯解析（`parse_text`），本模块是唯一写入者。
// 不碰桥、工具表、信任表；保存只写文件，重启后由既有 mcp_bootstrap 生效。

use std::path::{Path, PathBuf};

use crate::mcp::config::{McpConfig, MCP_FILE, Skipped, parse_text};

/// 文件不存在时编辑器预填的最小模板。保存它会创建一份空配置。
pub const EMPTY_CONFIG_TEXT: &str = "{\n  \"mcpServers\": {}\n}\n";

/// 编辑器打开时的一次完整快照（含正文）。只在用户显式打开编辑器时产生。
pub struct Snapshot {
    pub path: PathBuf,
    pub exists: bool,
    pub text: String,
    pub parse_error: Option<String>,
    pub skipped: Vec<Skipped>,
}

/// 面板常规加载的配置状态：**不含正文与密钥**。
pub struct Status {
    pub path: PathBuf,
    pub exists: bool,
    pub parse_error: Option<String>,
    pub skipped: Vec<Skipped>,
}

fn path_of(dir: &Path) -> PathBuf {
    dir.join(MCP_FILE)
}

fn status_of(path: PathBuf, exists: bool, cfg: McpConfig) -> Status {
    Status {
        path,
        exists,
        parse_error: cfg.parse_error,
        skipped: cfg.skipped,
    }
}

/// 读配置状态。文件不存在 ⇒ `exists = false`；读失败收进 `parse_error`，不 panic。
pub fn status(dir: &Path) -> Status {
    let path = path_of(dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => status_of(path, true, parse_text(&content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Status {
            path,
            exists: false,
            parse_error: None,
            skipped: Vec::new(),
        },
        Err(e) => Status {
            path,
            exists: true,
            parse_error: Some(format!("读取 {MCP_FILE} 失败: {e}")),
            skipped: Vec::new(),
        },
    }
}

/// 读编辑器快照。文件不存在 ⇒ 模板 + `exists = false`；其他读失败 ⇒ Err。
pub fn read(dir: &Path) -> Result<Snapshot, String> {
    let path = path_of(dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = parse_text(EMPTY_CONFIG_TEXT);
            return Ok(Snapshot {
                path,
                exists: false,
                text: EMPTY_CONFIG_TEXT.to_string(),
                parse_error: cfg.parse_error,
                skipped: cfg.skipped,
            });
        }
        Err(e) => return Err(format!("读取 {MCP_FILE} 失败: {e}")),
    };
    let cfg = parse_text(&content);
    Ok(Snapshot {
        path,
        exists: true,
        text: content,
        parse_error: cfg.parse_error,
        skipped: cfg.skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_reads_as_the_template() {
        let dir = tempdir().unwrap();
        let snap = read(dir.path()).unwrap();
        assert!(!snap.exists);
        assert_eq!(snap.text, EMPTY_CONFIG_TEXT);
        assert!(snap.parse_error.is_none());
        assert!(snap.skipped.is_empty());

        let st = status(dir.path());
        assert!(!st.exists);
        assert!(st.parse_error.is_none());
        assert!(st.skipped.is_empty());
    }

    #[test]
    fn a_broken_file_is_read_verbatim_and_reported() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(MCP_FILE), "{ not json").unwrap();

        let st = status(dir.path());
        assert!(st.exists);
        assert!(st.parse_error.is_some());

        let snap = read(dir.path()).unwrap();
        assert_eq!(snap.text, "{ not json");
        assert!(snap.parse_error.is_some());
    }

    #[test]
    fn status_lists_skipped_entries_by_name_and_reason() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(MCP_FILE),
            r#"{ "mcpServers": { "bad": { "args": "-y" } } }"#,
        )
        .unwrap();

        let st = status(dir.path());
        assert_eq!(st.skipped.len(), 1);
        assert_eq!(st.skipped[0].name, "bad");
        assert_eq!(st.skipped[0].reason, "字段类型不对");
    }

    #[test]
    fn a_directory_named_mcp_json_errors_on_read_but_status_survives() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(MCP_FILE)).unwrap();

        assert!(read(dir.path()).is_err());

        let st = status(dir.path());
        assert!(st.exists);
        assert!(st.parse_error.is_some());
    }

    #[test]
    fn status_parse_error_does_not_echo_config_text() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(MCP_FILE), "\"sk-REAL-SECRET\"").unwrap();

        let st = status(dir.path());
        let e = st.parse_error.expect("根是字符串 ⇒ 文件级错误");
        assert!(!e.contains("sk-REAL-SECRET"), "状态命令泄密：{e}");
    }
}
