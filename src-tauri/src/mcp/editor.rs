// 应用内配置编辑器的文件边界：mcp.json 的读取、状态、严格校验与原子写入。
//
// 与 config.rs 的分工：config.rs 只做纯解析（`parse_text`），本模块是唯一写入者。
// 不碰桥、工具表、信任表；保存只写文件，重启后由既有 mcp_bootstrap 生效。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::mcp::config::{McpConfig, MCP_FILE, Skipped, parse_text};

/// 文件不存在时编辑器预填的最小模板。保存它会创建一份空配置。
pub const EMPTY_CONFIG_TEXT: &str = "{\n  \"mcpServers\": {}\n}\n";

/// 原子写的中转文件：与目标同目录，`rename` 才可能同卷原子。
const TEMP_FILE: &str = "mcp.json.tmp";

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

fn rejection(cfg: &McpConfig) -> String {
    if let Some(err) = &cfg.parse_error {
        return format!("mcp.json 未保存：{err}");
    }
    let mut lines = Vec::with_capacity(cfg.skipped.len() + 1);
    lines.push(format!("mcp.json 未保存：{} 处条目问题", cfg.skipped.len()));
    for s in &cfg.skipped {
        lines.push(format!("- 「{}」：{}", s.name, s.reason));
    }
    lines.join("\n")
}

/// 严格校验后原子写入。任何文件级或条目级问题都整份拒绝；错误只含条目名与固定原因。
pub fn save(dir: &Path, text: &str) -> Result<(), String> {
    let cfg = parse_text(text);
    if cfg.parse_error.is_some() || !cfg.skipped.is_empty() {
        return Err(rejection(&cfg));
    }

    let temp = dir.join(TEMP_FILE);
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, dir.join(MCP_FILE))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|e| format!("写 {MCP_FILE} 失败：{e}"))
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

    #[test]
    fn save_writes_the_text_verbatim_and_load_reads_it_back() {
        let dir = tempdir().unwrap();
        let text = "{\n  \"mcpServers\": {\n    \"search\": {\n      \"command\": \"npx\",\n      \"args\": [\"-y\", \"@x/y\"],\n      \"custom\": true\n    }\n  }\n}\n";

        save(dir.path(), text).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(MCP_FILE)).unwrap(),
            text
        );
        // 简报此处写的是 `load(dir.path())`，但 Task 1 的签名是 `&impl AsRef<Path>`
        // （隐式 `Sized`，`Path` 不满足）⇒ 与 config.rs 测试同款传 `&dir`。
        let cfg = crate::mcp::config::load(&dir);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name(), "search");
        assert!(
            !dir.path().join(TEMP_FILE).exists(),
            "成功保存后不许留下 .tmp"
        );
    }

    // Windows 的 `rename` 覆盖已存在文件行为与 Unix 不同，必须在真实存在的
    // mcp.json 上跑一次，否则"编辑 → 保存 → 原子替换"这条主路径无人看守。
    #[test]
    fn save_replaces_an_existing_file_and_leaves_no_temp() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        let old_text = r#"{"mcpServers":{"old":{"command":"npx"}}}"#;
        std::fs::write(&path, old_text).unwrap();

        let new_text = "{\n  \"mcpServers\": {\n    \"new\": {\n      \"command\": \"node\"\n    }\n  }\n}\n";
        save(dir.path(), new_text).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), new_text);
        assert!(
            !dir.path().join(TEMP_FILE).exists(),
            "成功保存后不许留下 .tmp"
        );

        let cfg = crate::mcp::config::load(&dir);
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name(), "new");
    }

    #[test]
    fn a_file_level_error_leaves_an_existing_file_untouched() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(MCP_FILE);
        std::fs::write(&path, "{ \"mcpServers\": {} }").unwrap();

        let err = save(dir.path(), "{ nope").unwrap_err();

        assert!(err.starts_with("mcp.json 未保存："), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"mcpServers\": {} }"
        );
    }

    #[test]
    fn non_object_shapes_are_rejected_by_save() {
        for text in [
            r#"[ "mcpServers" ]"#,
            r#"{ "mcpServers": 5 }"#,
            r#"{ "mcpServers": null }"#,
            r#"{ "mcpServers": [ {"command":"x"} ] }"#,
        ] {
            let dir = tempdir().unwrap();
            let err = save(dir.path(), text).unwrap_err();
            assert!(
                err.starts_with("mcp.json 未保存："),
                "{text} ⇒ 要有文件级错误：{err}"
            );
            assert!(
                !dir.path().join(MCP_FILE).exists(),
                "{text} ⇒ 校验失败不许建文件"
            );
        }
    }

    #[test]
    fn every_entry_level_failure_blocks_the_write_and_names_the_entry() {
        let cases = [
            (r#"{ "mcpServers": { "": {"command":"x"} } }"#, "名字为空"),
            (r#"{ "mcpServers": { "bad": {"args":"-y"} } }"#, "字段类型不对"),
            (r#"{ "mcpServers": { "bad": {"command":""} } }"#, "command 为空"),
            (r#"{ "mcpServers": { "bad": {} } }"#, "没有 command 也没有 url"),
            (
                r#"{ "mcpServers": { "d": {"command":"a"}, "d": {"command":"b"} } }"#,
                "同名重复",
            ),
        ];

        for (text, reason) in cases {
            let dir = tempdir().unwrap();
            let err = save(dir.path(), text).unwrap_err();
            assert!(err.contains(reason), "`{reason}` 未出现在错误里：{err}");
            assert!(
                !dir.path().join(MCP_FILE).exists(),
                "校验失败不许建文件：{text}"
            );
        }
    }

    #[test]
    fn save_accepts_multiple_servers_and_load_reads_both() {
        let dir = tempdir().unwrap();
        let text = r#"{
  "mcpServers": {
    "comfyui-local": { "type": "streamable-http", "url": "http://127.0.0.1:9000/mcp" },
    "second": { "command": "npx", "args": ["-y", "@example/mcp"] }
  }
}
"#;
        save(dir.path(), text).unwrap();

        let cfg = crate::mcp::config::load(&dir);
        assert_eq!(cfg.servers.len(), 2, "两个 server 都必须保留");
        assert!(cfg.skipped.is_empty(), "{:?}", cfg.skipped);
        assert_eq!(
            cfg.servers.iter().map(|s| s.name()).collect::<Vec<_>>(),
            vec!["comfyui-local", "second"]
        );
    }

    #[test]
    fn save_rejects_a_server_outside_mcp_servers() {
        let dir = tempdir().unwrap();
        let err = save(
            dir.path(),
            r#"{ "mcpServers": { "ok": {"command":"x"} }, "second": {"url":"https://example.test/mcp"} }"#,
        )
        .unwrap_err();

        assert!(err.contains("second"), "要点名放错位置的条目：{err}");
        assert!(
            !dir.path().join(MCP_FILE).exists(),
            "校验失败不许留下 mcp.json"
        );
    }

    #[test]
    fn save_errors_never_echo_env_values() {
        let dir = tempdir().unwrap();
        let err = save(
            dir.path(),
            r#"{ "mcpServers": {
                "leaky": { "command": "npx", "env": { "TOKEN": "sk-REAL-SECRET" } },
                "bad": { "args": "-y" }
            } }"#,
        )
        .unwrap_err();

        assert!(!err.contains("sk-REAL-SECRET"), "{err}");
        assert!(err.contains("bad"), "要点名坏条目：{err}");
    }

    #[test]
    fn a_temp_write_failure_does_not_touch_the_target() {
        let dir = tempdir().unwrap();
        let target = dir.path().join(MCP_FILE);
        std::fs::write(&target, "{ \"mcpServers\": {} }").unwrap();
        std::fs::create_dir(dir.path().join(TEMP_FILE)).unwrap();

        let err = save(dir.path(), r#"{ "mcpServers": {} }"#).unwrap_err();

        assert!(err.contains("写 mcp.json 失败"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{ \"mcpServers\": {} }"
        );
        assert!(
            dir.path().join(TEMP_FILE).is_dir(),
            "临时路径是测试自己建的目录，不许被删"
        );
    }
}
