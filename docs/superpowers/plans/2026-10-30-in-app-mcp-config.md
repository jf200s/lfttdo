# In-App MCP Configuration Editor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users edit `mcp.json` inside the Tauri app with strict validation, atomic save, and restart-to-apply semantics.

**Architecture:** `mcp/config.rs` keeps the pure parser (`parse_text`); a new `mcp/editor.rs` owns file read/status/strict validate/atomic write; `ipc.rs` exposes three commands. The frontend adds a raw JSON editor view inside `McpPanel`, with config diagnostics in the panel and no live bridge changes on save.

**Tech Stack:** Tauri v2, Rust (`serde`, `serde_json`), React 19, TypeScript, Vitest + Testing Library.

**Spec:** `docs/superpowers/specs/2026-10-30-in-app-mcp-config-design.md`

## Global Constraints

- Strict validation MUST reuse `config::parse_text`; no second rule set. Any file-level or entry-level problem rejects the whole save and writes nothing.
- `mcp_config_status` and `mcp_servers` MUST never return env/headers values. Only `mcp_config_read` may return the raw config text, and only when the editor is explicitly opened.
- Save errors may contain entry names and the loader's fixed reason phrases, but never secret values.
- Save only writes `mcp.json` (same-directory temp file → `sync_all` → `rename`). It must not touch `McpBridge`, `ToolRegistry`, `PermissionGate`, or `mcp_trust.json`. Changes apply on next app launch.
- The frontend has no shell/filesystem capability; all file IO stays in Rust. Do not add Tauri permissions.
- No native dialogs: `window.alert` / `confirm` / `prompt` throw in the test setup. Use inline panel UI.
- No new dependencies (runtime or dev).
- All UI copy is Chinese and reuses existing `panel-*` classes.
- The portable U-disk edition was removed in `b28e249`; do not reintroduce a launcher or `portable.txt`. Manual verification uses `npm run tauri dev` only; installer packaging is out of scope for this plan.
- No migration: existing `mcp.json` stays untouched until the user saves from the editor. Reverting the code leaves a plain JSON file that old versions still read.
- The user explicitly authorized local task commits on the dedicated feature branch for this execution. Commit each task's changes locally; never push; never merge into `main`. Do not commit in the main working tree.

---

### Task 1: Extract the pure parser from `config.rs`

**Files:**
- Modify: `src-tauri/src/mcp/config.rs:1-16` (header comment), `src-tauri/src/mcp/config.rs:285-386` (`load`), `src-tauri/src/mcp/config.rs:388-612` (tests)
- Test: `src-tauri/src/mcp/config.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: existing `RawFile`, `RawServer`, `McpConfig`, `ServerDef`, `Skipped`, `MCP_FILE`.
- Produces: `pub(crate) fn parse_text(content: &str) -> McpConfig`; `pub fn load(dir: &impl AsRef<Path>) -> McpConfig` keeps its current signature and behavior.

- [ ] **Step 1: Write the failing tests**

Add these two tests inside `mod tests` in `src-tauri/src/mcp/config.rs` (after `servers_follow_alphabetical_order_not_written_order` is fine):

```rust
    #[test]
    fn parse_text_reports_the_same_servers_and_skips_as_load() {
        let raw = r#"{ "mcpServers": {
            "ok": {"command":"npx"},
            "bad": {"args":"-y"},
            "dup": {"command":"a"},
            "dup": {"command":"b"}
        } }"#;
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(MCP_FILE), raw).unwrap();

        let from_file = load(&dir);
        let from_text = parse_text(raw);

        assert_eq!(
            from_file
                .servers
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>(),
            from_text
                .servers
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            from_file
                .skipped
                .iter()
                .map(|s| (s.name.as_str(), s.reason))
                .collect::<Vec<_>>(),
            from_text
                .skipped
                .iter()
                .map(|s| (s.name.as_str(), s.reason))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            from_file.parse_error.is_some(),
            from_text.parse_error.is_some()
        );
    }

    #[test]
    fn parse_text_keeps_the_file_level_error_separate_from_entry_skips() {
        let broken = parse_text("{ not json");
        assert!(broken.servers.is_empty());
        assert!(broken.skipped.is_empty());
        assert!(broken.parse_error.is_some());

        let bad_entry = parse_text(r#"{ "mcpServers": { "x": {} } }"#);
        assert!(bad_entry.servers.is_empty());
        assert_eq!(bad_entry.skipped.len(), 1);
        assert!(bad_entry.parse_error.is_none());
    }

    #[test]
    fn file_level_parse_errors_do_not_echo_the_input() {
        let c = parse_text("\"sk-REAL-SECRET\"");
        let e = c.parse_error.expect("根是字符串 ⇒ 文件级错误");
        assert!(!e.contains("sk-REAL-SECRET"), "解析错误泄密：{e}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp::config
```

Expected: compile error `cannot find function 'parse_text' in this scope`.

- [ ] **Step 3: Extract `parse_text` and make `load` call it**

In `src-tauri/src/mcp/config.rs`, move everything in `load` from `let raw = match serde_json::from_str::<RawFile>(&content) {` through the final `McpConfig { servers, skipped, parse_error: None }` into a new function. The resulting shape must be:

```rust
/// 把一份 JSON 文本解析成 server 列表 / 跳过台账 / 文件级错误。
/// `load` 与 `mcp/editor.rs` 的严格校验共用这一份判定，禁止另起一套规则。
pub(crate) fn parse_text(content: &str) -> McpConfig {
    let raw = match serde_json::from_str::<RawFile>(content) {
        Ok(raw) => raw,
        Err(e) => {
            return McpConfig {
                servers: Vec::new(),
                skipped: Vec::new(),
                // 只留位置：serde 的完整文案可能把输入里的字符串值（有时是密钥）带出来，
                // 而 `mcp_config_status` 会展示 parse_error。
                parse_error: Some(format!(
                    "JSON 解析失败：第 {} 行第 {} 列",
                    e.line(),
                    e.column()
                )),
            };
        }
    };
    let mut servers: Vec<ServerDef> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut entries = raw.servers;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, entry) in entries {
        if name.is_empty() {
            skipped.push(Skipped {
                name,
                reason: "名字为空",
            });
            continue;
        }
        let Ok(entry) = serde_json::from_value::<RawServer>(entry) else {
            skipped.push(Skipped {
                name,
                reason: "字段类型不对",
            });
            continue;
        };
        let def = if let Some(command) = entry.command {
            if command.is_empty() {
                skipped.push(Skipped {
                    name,
                    reason: "command 为空",
                });
                continue;
            }
            ServerDef::Stdio {
                name,
                command,
                args: entry.args.unwrap_or_default(),
                env: entry.env.unwrap_or_default().into_iter().collect(),
            }
        } else if let Some(url) = entry.url {
            ServerDef::Http {
                name,
                url,
                headers: entry.headers.unwrap_or_default().into_iter().collect(),
            }
        } else {
            skipped.push(Skipped {
                name,
                reason: "没有 command 也没有 url",
            });
            continue;
        };
        if servers.iter().any(|s| s.name() == def.name()) {
            skipped.push(Skipped {
                name: def.name().to_string(),
                reason: "同名重复",
            });
            continue;
        }
        servers.push(def);
    }
    McpConfig {
        servers,
        skipped,
        parse_error: None,
    }
}
```

`load` becomes:

```rust
pub fn load(dir: &impl AsRef<Path>) -> McpConfig {
    let content = match std::fs::read_to_string(dir.as_ref().join(MCP_FILE)) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return McpConfig {
                servers: Vec::new(),
                skipped: Vec::new(),
                parse_error: None,
            };
        }
        Err(e) => {
            return McpConfig {
                servers: Vec::new(),
                skipped: Vec::new(),
                parse_error: Some(format!("读取 {MCP_FILE} 失败: {e}")),
            };
        }
    };
    parse_text(&content)
}
```

Update the file header comment. Replace the sentence in `config.rs:4` that says the writer is T35 with:

```text
// 写入者是 `mcp/editor.rs`（应用内 JSON 编辑器）；本文件只做纯解析。
```

Do not change any existing test assertion or reason phrase. One stale test comment needs updating: `a_non_object_root_is_a_file_level_error_not_a_panic` says serde's `expecting()` text is included in the summary; after this change the summary is line/column only, so update that comment accordingly.

- [ ] **Step 4: Run the module tests to verify they pass**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp::config
```

Expected: PASS, including all pre-existing `config.rs` tests.

- [ ] **Step 5: Commit the task**

Run:

```bash
git add src-tauri/src/mcp/config.rs
git commit -m "refactor(mcp): extract pure config parser"
```

Report the test count and any deviations.

---

### Task 2: Add `mcp/editor.rs` read/status

**Files:**
- Create: `src-tauri/src/mcp/editor.rs`
- Modify: `src-tauri/src/mcp/mod.rs:1-16`
- Test: `src-tauri/src/mcp/editor.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `crate::mcp::config::{parse_text, McpConfig, Skipped, MCP_FILE}`.
- Produces:
  - `pub const EMPTY_CONFIG_TEXT: &str`
  - `pub struct Snapshot { pub path: PathBuf, pub exists: bool, pub text: String, pub parse_error: Option<String>, pub skipped: Vec<Skipped> }`
  - `pub struct Status { pub path: PathBuf, pub exists: bool, pub parse_error: Option<String>, pub skipped: Vec<Skipped> }`
  - `pub fn read(dir: &Path) -> Result<Snapshot, String>`
  - `pub fn status(dir: &Path) -> Status`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/mcp/editor.rs` with only the imports, the types/functions above as stubs that `todo!()`, and this test module:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp::editor
```

Expected: compile succeeds but tests panic at `todo!()`, or fail to compile because the function bodies are stubs. Either failure is acceptable at this step.

- [ ] **Step 3: Implement read/status and register the module**

In `src-tauri/src/mcp/mod.rs`, add `pub mod editor;` after `pub mod config;` and replace the final sentence of the module comment with:

```text
// HTTP 传输（T36）仍未启用；mcp.json 的唯一写入者是 `editor.rs`（应用内 JSON 编辑器），
// 它只做读取、严格校验与原子写入，不碰桥与工具表。
```

Implement `src-tauri/src/mcp/editor.rs`:

```rust
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
```

Do not derive `Debug` on `Snapshot` (it contains raw config text).

- [ ] **Step 4: Run the tests to verify they pass**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp::editor
```

Expected: 5 passed.

- [ ] **Step 5: Commit the task**

Run:

```bash
git add src-tauri/src/mcp/editor.rs src-tauri/src/mcp/mod.rs
git commit -m "feat(mcp): add config editor read and status"
```

Report the test count.

---

### Task 3: Add `editor::save` with strict validation and atomic write

**Files:**
- Modify: `src-tauri/src/mcp/editor.rs`
- Test: `src-tauri/src/mcp/editor.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `parse_text`, `McpConfig`, `MCP_FILE`, `TEMP_FILE`.
- Produces: `pub fn save(dir: &Path, text: &str) -> Result<(), String>`.

- [ ] **Step 1: Write the failing tests**

Append to the test module in `src-tauri/src/mcp/editor.rs`:

```rust
    #[test]
    fn save_writes_the_text_verbatim_and_load_reads_it_back() {
        let dir = tempdir().unwrap();
        let text = "{\n  \"mcpServers\": {\n    \"search\": {\n      \"command\": \"npx\",\n      \"args\": [\"-y\", \"@x/y\"],\n      \"custom\": true\n    }\n  }\n}\n";

        save(dir.path(), text).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(MCP_FILE)).unwrap(),
            text
        );
        let cfg = crate::mcp::config::load(dir.path());
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name(), "search");
        assert!(
            !dir.path().join(TEMP_FILE).exists(),
            "成功保存后不许留下 .tmp"
        );
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp::editor
```

Expected: compile error `cannot find function 'save'`.

- [ ] **Step 3: Implement `save`**

Add to `src-tauri/src/mcp/editor.rs`:

```rust
use std::io::Write;

/// 原子写的中转文件：与目标同目录，`rename` 才可能同卷原子。
const TEMP_FILE: &str = "mcp.json.tmp";

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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp::editor
```

Expected: 11 passed (5 from Task 2 + 6 new).

- [ ] **Step 5: Commit the task**

Run:

```bash
git add src-tauri/src/mcp/editor.rs
git commit -m "feat(mcp): validate and atomically save MCP config"
```

Report the test count.

---

### Task 4: Expose the three Tauri commands

**Files:**
- Modify: `src-tauri/src/ipc.rs` (imports; after `McpServerView`; after `mcp_disconnect`; `invoke_handler`)
- Modify: `src-tauri/capabilities/default.json:4`
- Test: `src-tauri/src/ipc.rs` (inline `#[cfg(test)]` module, near the existing MCP tests such as `mcp_servers_view_never_leaks_env_or_headers`)

**Interfaces:**
- Consumes: `crate::mcp::editor::{read, status, save}`.
- Produces:
  - `McpConfigIssue { name: String, reason: String }`
  - `McpConfigStatusView { path: String, exists: bool, parse_error: Option<String>, skipped: Vec<McpConfigIssue> }`
  - `McpConfigTextView { path: String, exists: bool, text: String, parse_error: Option<String>, skipped: Vec<McpConfigIssue> }` (no `Debug`)
  - `mcp_config_status_of(dir: &Path) -> Result<McpConfigStatusView, String>`
  - `mcp_config_read_of(dir: &Path) -> Result<McpConfigTextView, String>`
  - `mcp_config_save_of(dir: &Path, text: &str) -> Result<(), String>`
  - Commands: `mcp_config_status`, `mcp_config_read`, `mcp_config_save`.

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `mod tests` in `src-tauri/src/ipc.rs`:

```rust
    #[test]
    fn mcp_config_status_never_carries_secrets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "x": { "command": "npx", "env": { "TOKEN": "sk-REAL-SECRET" } } } }"#,
        )
        .unwrap();

        let v = mcp_config_status_of(dir.path()).unwrap();
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("sk-REAL-SECRET"), "{s}");
        assert!(s.contains("\"parseError\":null"), "{s}");
        assert!(s.contains("mcp.json"), "{s}");
    }

    #[test]
    fn mcp_config_read_carries_the_text_only_for_the_explicit_editor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "x": { "command": "npx", "env": { "TOKEN": "sk-REAL-SECRET" } } } }"#,
        )
        .unwrap();

        let v = mcp_config_read_of(dir.path()).unwrap();
        assert!(
            v.text.contains("sk-REAL-SECRET"),
            "编辑器必须能显示要编辑的原文"
        );
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains("\"text\""), "{s}");
    }

    #[test]
    fn mcp_config_save_of_rejects_invalid_text_and_writes_valid_text() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mcp_config_save_of(dir.path(), "{ nope").is_err());
        assert!(!dir.path().join("mcp.json").exists());

        mcp_config_save_of(dir.path(), r#"{ "mcpServers": {} }"#).unwrap();
        assert!(dir.path().join("mcp.json").exists());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp_config
```

Expected: compile error `cannot find function 'mcp_config_status_of'`.

- [ ] **Step 3: Add the wire types and functions**

Add `use crate::mcp::editor;` next to the existing `use crate::mcp::...` imports.

After `pub struct McpServerView { ... }`, add:

```rust
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpConfigIssue {
    pub name: String,
    pub reason: String,
}

/// 面板常规加载的配置状态：**不含正文与密钥**。
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpConfigStatusView {
    pub path: String,
    pub exists: bool,
    pub parse_error: Option<String>,
    pub skipped: Vec<McpConfigIssue>,
}

/// 编辑器打开时的快照：**只有这条命令**会把配置正文（含 env/headers）带到前端。
/// 故意不实现 `Debug`：避免正文误入日志。
#[derive(Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpConfigTextView {
    pub path: String,
    pub exists: bool,
    pub text: String,
    pub parse_error: Option<String>,
    pub skipped: Vec<McpConfigIssue>,
}
```

After `mcp_disconnect`, add:

```rust
fn config_issues(skipped: Vec<crate::mcp::config::Skipped>) -> Vec<McpConfigIssue> {
    skipped
        .into_iter()
        .map(|s| McpConfigIssue {
            name: s.name,
            reason: s.reason.to_string(),
        })
        .collect()
}

/// 可测核心：配置状态（不含正文）。
pub fn mcp_config_status_of(dir: &Path) -> Result<McpConfigStatusView, String> {
    let st = editor::status(dir);
    Ok(McpConfigStatusView {
        path: st.path.display().to_string(),
        exists: st.exists,
        parse_error: st.parse_error,
        skipped: config_issues(st.skipped),
    })
}

/// 可测核心：编辑器快照（含正文）。只有显式打开编辑器时才会被调用。
pub fn mcp_config_read_of(dir: &Path) -> Result<McpConfigTextView, String> {
    let snap = editor::read(dir)?;
    Ok(McpConfigTextView {
        path: snap.path.display().to_string(),
        exists: snap.exists,
        text: snap.text,
        parse_error: snap.parse_error,
        skipped: config_issues(snap.skipped),
    })
}

/// 可测核心：严格校验后原子写盘。
pub fn mcp_config_save_of(dir: &Path, text: &str) -> Result<(), String> {
    editor::save(dir, text)
}

#[tauri::command]
pub fn mcp_config_status(state: State<'_, AppState>) -> Result<McpConfigStatusView, String> {
    mcp_config_status_of(&state.mcp_dir)
}

#[tauri::command]
pub fn mcp_config_read(state: State<'_, AppState>) -> Result<McpConfigTextView, String> {
    mcp_config_read_of(&state.mcp_dir)
}

#[tauri::command]
pub fn mcp_config_save(state: State<'_, AppState>, text: String) -> Result<(), String> {
    mcp_config_save_of(&state.mcp_dir, &text)
}
```

Register the three commands in `invoke_handler`, immediately after `mcp_disconnect,`:

```rust
            mcp_config_status,
            mcp_config_read,
            mcp_config_save,
```

- [ ] **Step 4: Update the capability description**

In `src-tauri/capabilities/default.json`, replace the description with:

```json
  "description": "前端只允许事件与自定义命令；无 shell、无文件系统、无窗口尺寸/拖拽权限。MCP 的进程执行与 mcp.json 读写全在 Rust（mcp_* 自定义命令），前端拿不到任何 spawn/fs 能力",
```

Keep `"permissions": ["core:default", "core:event:default"]` unchanged.

- [ ] **Step 5: Run the tests to verify they pass**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml mcp_config
```

Expected: 3 passed. Then run the full Rust suite once:

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all existing tests still pass.

- [ ] **Step 6: Commit the task**

Run:

```bash
git add src-tauri/src/ipc.rs src-tauri/capabilities/default.json
git commit -m "feat(ipc): expose MCP config commands"
```

Report both test counts.

---

### Task 5: Add the frontend IPC layer and `McpConfigEditor`

**Files:**
- Modify: `src/types.ts:68-79` (after `McpServerView`)
- Modify: `src/ipc/commands.ts:169-188` (after `mcpDisconnect`)
- Create: `src/components/McpConfigEditor.tsx`
- Create: `src/components/McpConfigEditor.test.tsx`
- Modify: `src/styles.css:838` (after the panel block, before `/* ==== 滚动条 ==== */`)

**Interfaces:**
- Consumes: `invoke`, `McpConfigIssue`/`McpConfigStatus`/`McpConfigText`.
- Produces:
  - `mcpConfigStatus(): Promise<McpConfigStatus>`
  - `mcpConfigRead(): Promise<McpConfigText>`
  - `mcpConfigSave(text: string): Promise<void>`
  - `McpConfigEditor({ load?, save?, onDone }: McpConfigEditorProps)`.

- [ ] **Step 1: Write the failing component tests**

Create `src/components/McpConfigEditor.test.tsx`:

```tsx
// 编辑器组件的独立判据：载入、模板、保存失败保文本、脏文本二次确认、读失败重试。
// 所有 IPC 都注入假实现，组件本身不碰 Tauri。
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { McpConfigEditor } from "./McpConfigEditor";
import type { McpConfigText } from "../types";

const snapshot = (over: Partial<McpConfigText> = {}): McpConfigText => ({
  path: "C:\\data\\mcp.json",
  exists: true,
  text: '{\n  "mcpServers": {}\n}\n',
  parseError: null,
  skipped: [],
  ...over,
});

describe("McpConfigEditor", () => {
  it("loads the file into a textarea, shows the path and the stdio hint", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByLabelText("mcp.json 内容")).toHaveValue(
      snapshot().text,
    );
    expect(screen.getByText("C:\\data\\mcp.json")).toBeInTheDocument();
    expect(screen.getByText(/仅支持 stdio/)).toBeInTheDocument();
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("prefills the template and says the file will be created", async () => {
    const load = vi
      .fn()
      .mockResolvedValue(snapshot({ exists: false, text: '{\n  "mcpServers": {}\n}\n' }));
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByText(/保存后创建/)).toBeInTheDocument();
  });

  it("shows load-time parse and skipped issues", async () => {
    const load = vi.fn().mockResolvedValue(
      snapshot({
        parseError: "expected value at line 3 column 2",
        skipped: [{ name: "x", reason: "字段类型不对" }],
      }),
    );
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByText(/载入时无法解析/)).toBeInTheDocument();
    expect(screen.getByText(/x（字段类型不对）/)).toBeInTheDocument();
  });

  it("keeps the text and shows the backend error when saving is rejected", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const save = vi.fn().mockRejectedValue("mcp.json 未保存：bad：字段类型不对");
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} save={save} onDone={onDone} />);

    const box = await screen.findByLabelText("mcp.json 内容");
    fireEvent.change(box, { target: { value: "{ nope" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByTestId("mcp-config-error")).toHaveTextContent(
      "字段类型不对",
    );
    expect(box).toHaveValue("{ nope");
    expect(onDone).not.toHaveBeenCalled();
  });

  it("returns saved=true after a successful save", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const save = vi.fn().mockResolvedValue(undefined);
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} save={save} onDone={onDone} />);

    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(onDone).toHaveBeenCalledWith(true));
    expect(save).toHaveBeenCalledWith(snapshot().text);
  });

  it("asks before discarding edits and can keep editing", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} onDone={onDone} />);

    const box = await screen.findByLabelText("mcp.json 内容");
    fireEvent.change(box, { target: { value: "changed" } });
    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    expect(screen.getByText("放弃修改？")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "继续编辑" }));
    expect(screen.getByLabelText("mcp.json 内容")).toHaveValue("changed");

    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    fireEvent.click(screen.getByRole("button", { name: "放弃" }));
    expect(onDone).toHaveBeenCalledWith(false);
  });

  it("routes Escape through the dirty check", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    const box = await screen.findByLabelText("mcp.json 内容");
    fireEvent.change(box, { target: { value: "changed" } });
    fireEvent.keyDown(box, { key: "Escape" });

    expect(screen.getByText("放弃修改？")).toBeInTheDocument();
  });

  it("returns without a prompt when nothing changed", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} onDone={onDone} />);

    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    expect(onDone).toHaveBeenCalledWith(false);
    expect(screen.queryByText("放弃修改？")).toBeNull();
  });

  it("shows a retry path when the file cannot be read", async () => {
    const load = vi
      .fn()
      .mockRejectedValueOnce("读取 mcp.json 失败：拒绝访问")
      .mockResolvedValueOnce(snapshot());
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByText(/拒绝访问/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByLabelText("mcp.json 内容")).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
npx vitest run src/components/McpConfigEditor.test.tsx
```

Expected: FAIL because `./McpConfigEditor` does not exist.

- [ ] **Step 3: Add the TypeScript types**

In `src/types.ts`, after `McpServerView`, add:

```ts
/** 配置加载器的一条跳过记录：原因措辞来自 Rust 的固定五条。 */
export interface McpConfigIssue {
  name: string;
  reason: string;
}

/** `mcp_config_status` 的返回：**不含正文与 env/headers 值**，面板常规加载只调它。 */
export interface McpConfigStatus {
  path: string;
  exists: boolean;
  parseError: string | null;
  skipped: McpConfigIssue[];
}

/** `mcp_config_read` 的返回：**只有这条命令**会把配置原文（含密钥）带到前端。 */
export interface McpConfigText extends McpConfigStatus {
  text: string;
}
```

- [ ] **Step 4: Add the IPC wrappers**

In `src/ipc/commands.ts`, extend the type import with `McpConfigStatus` and `McpConfigText`, then add after `mcpDisconnect`:

```ts
/**
 * MCP 配置编辑器（应用内 JSON 编辑）。`mcpConfigStatus` 只给面板做提示，不含正文；
 * 只有 `mcpConfigRead` 会把含 env/headers 的原文带到前端，且只在用户点「编辑配置」时调用。
 * 保存的严格校验在 Rust；失败返回多行原因，不写盘。
 */
export function mcpConfigStatus(): Promise<McpConfigStatus> {
  return invoke<McpConfigStatus>("mcp_config_status");
}

export function mcpConfigRead(): Promise<McpConfigText> {
  return invoke<McpConfigText>("mcp_config_read");
}

export function mcpConfigSave(text: string): Promise<void> {
  return invoke<void>("mcp_config_save", { text });
}
```

- [ ] **Step 5: Create the editor component**

Create `src/components/McpConfigEditor.tsx`:

```tsx
import { useEffect, useState } from "react";
import { mcpConfigRead, mcpConfigSave } from "../ipc/commands";
import type { McpConfigText } from "../types";

interface McpConfigEditorProps {
  /** 测试注入；生产默认走 IPC。 */
  load?: () => Promise<McpConfigText>;
  save?: (text: string) => Promise<void>;
  /** saved = true 表示已写盘；面板据此提示「重启后生效」。 */
  onDone: (saved: boolean) => void;
}

/**
 * 应用内 mcp.json 编辑器。
 *
 * 只在挂载期间持有配置正文；关闭即丢弃，不写任何 store。
 * 保存成功由父组件负责关闭本视图并刷新列表，所以成功路径不在这里改父状态。
 */
export function McpConfigEditor({
  load = mcpConfigRead,
  save = mcpConfigSave,
  onDone,
}: McpConfigEditorProps) {
  const [snapshot, setSnapshot] = useState<McpConfigText | null>(null);
  const [text, setText] = useState("");
  const [original, setOriginal] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let alive = true;
    load()
      .then((c) => {
        if (!alive) return;
        setSnapshot(c);
        setText(c.text);
        setOriginal(c.text);
        setError(null);
      })
      .catch((e: unknown) => {
        if (alive) setError(String(e));
      });
    return () => {
      alive = false;
    };
  }, [load, reload]);

  const dirty = text !== original;

  const requestClose = () => {
    if (saving) return;
    if (dirty) {
      setConfirmDiscard(true);
      return;
    }
    onDone(false);
  };

  const submit = async () => {
    setSaving(true);
    try {
      await save(text);
    } catch (e) {
      setError(String(e));
      setSaving(false);
      return;
    }
    setSaving(false);
    onDone(true);
  };

  if (snapshot === null) {
    return (
      <div className="panel-group" data-testid="mcp-config-loading">
        {error === null ? (
          <p className="panel-note">正在读取 mcp.json…</p>
        ) : (
          <>
            <p className="panel-error" role="alert">
              {error}
            </p>
            <div className="panel-actions">
              <button
                type="button"
                onClick={() => {
                  setError(null);
                  setReload((n) => n + 1);
                }}
              >
                重试
              </button>
              <button type="button" onClick={() => onDone(false)}>
                返回
              </button>
            </div>
          </>
        )}
      </div>
    );
  }

  return (
    <div
      className="panel-group mcp-config-editor"
      data-testid="mcp-config-editor"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          requestClose();
        }
      }}
    >
      <p className="panel-readonly" data-testid="mcp-config-path">
        <span className="panel-hint">文件：</span>
        <code>{snapshot.path}</code>
      </p>
      <p className="panel-hint">
        配置中的 env/headers 会以明文显示。文件根必须是对象、mcpServers
        必须是对象。当前构建仅支持 stdio；HTTP 条目可保存但无法连接；disabled
        字段当前不识别。
      </p>
      {snapshot.exists ? null : (
        <p className="panel-hint">文件尚不存在，保存后创建。</p>
      )}
      {snapshot.parseError !== null ? (
        <p className="panel-error" role="alert">
          载入时无法解析：{snapshot.parseError}
        </p>
      ) : null}
      {snapshot.skipped.length > 0 ? (
        <div className="panel-note">
          <p>载入时有 {snapshot.skipped.length} 条配置被跳过：</p>
          <ul className="panel-issues">
            {snapshot.skipped.map((s, i) => (
              <li key={`${i}-${s.name}`}>
                {s.name}（{s.reason}）
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      <textarea
        className="mcp-config-text"
        aria-label="mcp.json 内容"
        autoFocus
        spellCheck={false}
        rows={16}
        readOnly={saving}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setError(null);
        }}
      />
      {error !== null ? (
        <p className="panel-error" role="alert" data-testid="mcp-config-error">
          {error}
        </p>
      ) : null}
      {confirmDiscard ? (
        <div className="panel-actions">
          <span className="panel-hint">放弃修改？</span>
          <button type="button" onClick={() => onDone(false)}>
            放弃
          </button>
          <button type="button" onClick={() => setConfirmDiscard(false)}>
            继续编辑
          </button>
        </div>
      ) : (
        <div className="panel-actions">
          <button
            type="button"
            className="panel-save"
            disabled={saving}
            onClick={() => void submit()}
          >
            保存
          </button>
          <button type="button" disabled={saving} onClick={requestClose}>
            返回
          </button>
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 6: Add the CSS**

In `src/styles.css`, insert after `.panel-save:hover:not(:disabled)` and before `/* ==== 滚动条 ==== */`:

```css
/* ==== MCP 配置编辑器 ==== */

/* 面板默认 460px 对整份 JSON 太窄；只在编辑视图加宽。 */
.panel-wide {
  width: 720px;
}

.mcp-config-text {
  width: 100%;
  min-height: 280px;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  background-color: var(--bg-input);
  font-family: var(--font-code);
  font-size: 0.85em;
  line-height: 1.5;
  tab-size: 2;
  resize: vertical;
}

/* 保存失败的后端错误是多行清单（每条一行），必须保留换行。 */
.mcp-config-editor .panel-error {
  white-space: pre-wrap;
}

.panel-note > p {
  margin: 0;
}

.panel-issues {
  margin: 4px 0 0;
  padding-left: 18px;
}
```

- [ ] **Step 7: Run the component tests, type check and CSS lint**

Run:

```bash
npx vitest run src/components/McpConfigEditor.test.tsx
npm run build
npm run lint:css
```

Expected: 9 editor tests pass; `tsc` + Vite build pass; stylelint passes.

- [ ] **Step 8: Commit the task**

Run:

```bash
git add src/types.ts src/ipc/commands.ts src/components/McpConfigEditor.tsx src/components/McpConfigEditor.test.tsx src/styles.css
git commit -m "feat(ui): add MCP config editor component"
```

Report the test count and any deviations.

---

### Task 6: Integrate the editor into `McpPanel`

**Files:**
- Modify: `src/components/McpPanel.tsx` (whole component)
- Modify: `src/components/McpPanel.test.tsx` (whole test file)
- Test: `src/components/McpPanel.test.tsx`

**Interfaces:**
- Consumes: `McpConfigEditor`, `mcpConfigStatus`, `mcpConfigRead`, `mcpConfigSave`, `McpConfigStatus`, `McpConfigText`.
- Produces: panel view switching, status banner, restart notice, backdrop behavior, new empty-state copy.

- [ ] **Step 1: Rewrite the panel tests (red first)**

Replace `src/components/McpPanel.test.tsx` with:

```tsx
// T35 的组件层判据：状态点、工具数、一键信任、失败原因就地显示（零弹窗）。
// 应用内配置编辑的判据：入口、状态横幅、保存后重启提示、编辑时遮罩不关闭。
// 命令层由 `ipc.rs` 的内联测试管，这里一律注入假实现，不碰 IPC。
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { McpPanel } from "./McpPanel";
import type { McpConfigStatus, McpServerView } from "../types";

const ready: McpServerView = {
  name: "search",
  status: "ready",
  tools: 4,
  trust: "ask",
  reason: null,
};

const cleanStatus = (): McpConfigStatus => ({
  path: "C:\\data\\mcp.json",
  exists: true,
  parseError: null,
  skipped: [],
});

const statusOf = () => vi.fn().mockResolvedValue(cleanStatus());

describe("McpPanel", () => {
  it("announces the tool count and offers one-click Auto on first connect", async () => {
    const setTrust = vi.fn().mockResolvedValue(undefined);
    const load = vi.fn().mockResolvedValue([ready]);
    render(
      <McpPanel
        initial={[ready]}
        onClose={() => {}}
        load={load}
        setTrust={setTrust}
        loadConfigStatus={statusOf()}
      />,
    );

    expect(screen.getByText("search 提供 4 个工具")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "本服务器全部允许" }));
    await waitFor(() => expect(setTrust).toHaveBeenCalledWith("search", "auto"));
  });

  it("shows the failure reason for a disconnected server without a popup", () => {
    const alertSpy = vi.spyOn(window, "alert").mockImplementation(() => {});
    render(
      <McpPanel
        initial={[
          {
            name: "dead",
            status: "disconnected",
            tools: 0,
            trust: "ask",
            reason: "spawn 失败：program not found",
          },
        ]}
        onClose={() => {}}
        loadConfigStatus={statusOf()}
      />,
    );
    expect(screen.getByText(/program not found/)).toBeInTheDocument();
    expect(alertSpy).not.toHaveBeenCalled();
    alertSpy.mockRestore();
  });

  it("re-reads the server list after a reconnect instead of keeping a stale count", async () => {
    const load = vi
      .fn()
      .mockResolvedValueOnce([{ ...ready, status: "disconnected", tools: 0 }])
      .mockResolvedValueOnce([{ ...ready, status: "ready", tools: 4 }]);
    const reconnect = vi.fn().mockResolvedValue(4);
    render(
      <McpPanel
        onClose={() => {}}
        load={load}
        reconnect={reconnect}
        loadConfigStatus={statusOf()}
      />,
    );
    await waitFor(() =>
      expect(screen.getByText("search 提供 0 个工具")).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("button", { name: "重连" }));
    await waitFor(() => expect(reconnect).toHaveBeenCalledWith("search"));
    await waitFor(() =>
      expect(screen.getByText("search 提供 4 个工具")).toBeInTheDocument(),
    );
  });

  it("grants the front end no process or filesystem capability", () => {
    const path = resolve(process.cwd(), "src-tauri/capabilities/default.json");
    const caps = JSON.parse(readFileSync(path, "utf8")) as {
      permissions: string[];
      description: string;
    };
    const flat = JSON.stringify(caps.permissions);
    expect(flat).not.toMatch(/shell:allow-spawn|fs:allow-read|fs:allow-write/);
    expect(flat).toMatch(/core:event/);
    expect(caps.description).toMatch(/mcp_\*/);
  });

  it("opens the raw config editor and shows the file text", async () => {
    const readConfig = vi.fn().mockResolvedValue({
      ...cleanStatus(),
      text: '{\n  "mcpServers": {}\n}\n',
    });
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        loadConfigStatus={statusOf()}
        readConfig={readConfig}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑配置" }));
    expect(await screen.findByLabelText("mcp.json 内容")).toHaveValue(
      '{\n  "mcpServers": {}\n}\n',
    );
    expect(readConfig).toHaveBeenCalledTimes(1);
  });

  it("shows the restart notice after a successful save and refreshes the list", async () => {
    const load = vi.fn().mockResolvedValue([]);
    const status = statusOf();
    const saveConfig = vi.fn().mockResolvedValue(undefined);
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        load={load}
        loadConfigStatus={status}
        readConfig={vi.fn().mockResolvedValue({ ...cleanStatus(), text: "{}" })}
        saveConfig={saveConfig}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑配置" }));
    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByTestId("mcp-config-saved")).toHaveTextContent(
      "重启应用后生效",
    );
    expect(saveConfig).toHaveBeenCalledWith("{}");
    await waitFor(() => expect(load).toHaveBeenCalled());
    await waitFor(() => expect(status).toHaveBeenCalledTimes(2));
  });

  it("banners a broken config and keeps the editor reachable", async () => {
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        loadConfigStatus={vi.fn().mockResolvedValue({
          ...cleanStatus(),
          parseError: "expected value at line 1 column 2",
        })}
      />,
    );

    expect(await screen.findByTestId("mcp-config-banner")).toHaveTextContent(
      "配置文件无法解析",
    );
    expect(screen.getByText("没有可用的服务器配置。")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "编辑配置" })).toBeInTheDocument();
  });

  it("lists skipped entries with their reasons", async () => {
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        loadConfigStatus={vi.fn().mockResolvedValue({
          ...cleanStatus(),
          skipped: [{ name: "bad", reason: "字段类型不对" }],
        })}
      />,
    );

    expect(await screen.findByTestId("mcp-config-banner")).toHaveTextContent(
      "bad（字段类型不对）",
    );
  });

  it("does not close the panel when the backdrop is clicked while editing", async () => {
    const onClose = vi.fn();
    const { container } = render(
      <McpPanel
        initial={[]}
        onClose={onClose}
        loadConfigStatus={statusOf()}
        readConfig={vi.fn().mockResolvedValue({ ...cleanStatus(), text: "{}" })}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑配置" }));
    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(container.querySelector(".panel-backdrop")!);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("points the empty state at the in-app editor", () => {
    render(
      <McpPanel initial={[]} onClose={() => {}} loadConfigStatus={statusOf()} />,
    );
    expect(
      screen.getByText(/点击「编辑配置」直接写 mcp.json/),
    ).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
npx vitest run src/components/McpPanel.test.tsx
```

Expected: FAIL — no button named `编辑配置`, no banner test id, and the old empty-state copy.

- [ ] **Step 3: Replace `McpPanel.tsx`**

Replace the whole file with:

```tsx
import { useEffect, useState } from "react";
import {
  mcpConfigRead,
  mcpConfigSave,
  mcpConfigStatus,
  mcpDisconnect,
  mcpReconnect,
  mcpServers,
  mcpSetTrust,
} from "../ipc/commands";
import type {
  McpConfigStatus,
  McpConfigText,
  McpServerView,
  McpTrust,
} from "../types";
import { McpConfigEditor } from "./McpConfigEditor";

interface McpPanelProps {
  /** 测试注入：省一次 IPC；生产不传，面板自己拉。 */
  initial?: McpServerView[];
  onClose: () => void;
  load?: () => Promise<McpServerView[]>;
  setTrust?: (name: string, trust: McpTrust) => Promise<void>;
  reconnect?: (name: string) => Promise<number>;
  disconnect?: (name: string) => Promise<void>;
  /** 配置状态（不含正文）；测试注入，生产走默认命令。 */
  loadConfigStatus?: () => Promise<McpConfigStatus>;
  /** 只在用户点开编辑器时调用，会把含密钥的原文带到前端。 */
  readConfig?: () => Promise<McpConfigText>;
  saveConfig?: (text: string) => Promise<void>;
}

const STATUS_TEXT: Record<McpServerView["status"], string> = {
  connecting: "连接中…",
  ready: "已连接",
  disconnected: "未连接",
};

const TRUST_TEXT: Record<McpTrust, string> = {
  auto: "全部允许",
  ask: "每次询问",
  block: "禁止",
};

/**
 * MCP 服务器面板（T35）。**视图即真相**：每次动作都重新拉一遍列表，
 * 不在前端维护第二份「连接状态」——Rust 的桥才是唯一的状态源（`ServerStatus`）。
 *
 * 应用内配置编辑（2026-10-30）：在同一面板里切出 JSON 编辑器。保存只写文件，
 * 重启后由 `mcp_bootstrap` 生效；当前会话的连接与工具不变。
 */
export function McpPanel({
  initial,
  onClose,
  load = mcpServers,
  setTrust = mcpSetTrust,
  reconnect = mcpReconnect,
  disconnect = mcpDisconnect,
  loadConfigStatus = mcpConfigStatus,
  readConfig = mcpConfigRead,
  saveConfig = mcpConfigSave,
}: McpPanelProps) {
  const [servers, setServers] = useState<McpServerView[] | null>(initial ?? null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [configStatus, setConfigStatus] = useState<McpConfigStatus | null>(null);
  const [editing, setEditing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setServers(await load());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const refreshStatus = async () => {
    try {
      setConfigStatus(await loadConfigStatus());
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    if (initial === undefined) {
      void refresh();
    }
    // 只在挂载时拉一次；之后的刷新由每个动作自己触发。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refreshStatus();
    // 状态只在挂载与保存成功后刷新。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = async (name: string, action: () => Promise<unknown>) => {
    setBusy(name);
    try {
      await action();
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      await refresh();
    }
  };

  const finishEditing = async (saved: boolean) => {
    setEditing(false);
    if (!saved) return;
    setNotice(
      "配置已保存，重启应用后生效。当前会话仍按启动时的连接与工具运行。",
    );
    await refresh();
    await refreshStatus();
  };

  return (
    <div className="panel-backdrop" onClick={editing ? undefined : onClose}>
      <section
        className={editing ? "panel panel-wide" : "panel"}
        aria-label={editing ? "编辑 MCP 配置" : "MCP 服务器"}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="panel-title">{editing ? "编辑 mcp.json" : "MCP 服务器"}</h2>
        {error !== null ? <p className="panel-note">{error}</p> : null}
        {editing ? (
          <McpConfigEditor
            load={readConfig}
            save={saveConfig}
            onDone={(saved) => void finishEditing(saved)}
          />
        ) : (
          <>
            {notice !== null ? (
              <p className="panel-note" data-testid="mcp-config-saved">
                {notice}
              </p>
            ) : null}
            {configStatus !== null &&
            (configStatus.parseError !== null ||
              configStatus.skipped.length > 0) ? (
              <div className="panel-note" data-testid="mcp-config-banner">
                {configStatus.parseError !== null ? (
                  <p>配置文件无法解析：{configStatus.parseError}</p>
                ) : null}
                {configStatus.skipped.length > 0 ? (
                  <>
                    <p>有 {configStatus.skipped.length} 条配置被跳过：</p>
                    <ul className="panel-issues">
                      {configStatus.skipped.map((s, i) => (
                        <li key={`${i}-${s.name}`}>
                          {s.name}（{s.reason}）
                        </li>
                      ))}
                    </ul>
                  </>
                ) : null}
                <p>点击「编辑配置」修复。</p>
              </div>
            ) : null}
            {servers === null ? (
              <p className="panel-note">正在读取 mcp.json…</p>
            ) : servers.length === 0 ? (
              <p className="panel-note">
                {configStatus !== null &&
                (configStatus.parseError !== null ||
                  configStatus.skipped.length > 0)
                  ? "没有可用的服务器配置。"
                  : "还没有配置服务器。点击「编辑配置」直接写 mcp.json。"}
              </p>
            ) : (
              servers.map((s) => (
                <fieldset className="panel-group" key={s.name}>
                  <legend>
                    {s.name} 提供 {s.tools} 个工具
                  </legend>
                  <p className="panel-row panel-readonly">
                    {STATUS_TEXT[s.status]} · 信任：{TRUST_TEXT[s.trust]}
                  </p>
                  {s.reason !== null ? (
                    <p className="panel-note">{s.reason}</p>
                  ) : null}
                  <p className="panel-row">
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(s.name, () => setTrust(s.name, "auto"))
                      }
                    >
                      本服务器全部允许
                    </button>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(s.name, () => setTrust(s.name, "ask"))
                      }
                    >
                      每次询问
                    </button>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(s.name, () => setTrust(s.name, "block"))
                      }
                    >
                      禁止
                    </button>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() => void run(s.name, () => reconnect(s.name))}
                    >
                      重连
                    </button>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() => void run(s.name, () => disconnect(s.name))}
                    >
                      断开
                    </button>
                  </p>
                </fieldset>
              ))
            )}
            <div className="panel-actions">
              <button
                type="button"
                onClick={() => {
                  setError(null);
                  setNotice(null);
                  setEditing(true);
                }}
              >
                编辑配置
              </button>
              <button type="button" onClick={onClose}>
                关闭
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
```

- [ ] **Step 4: Run the panel tests to verify they pass**

Run:

```bash
npx vitest run src/components/McpPanel.test.tsx
```

Expected: 10 passed.

- [ ] **Step 5: Run the full frontend suite and type check**

Run:

```bash
npm test
npm run build
```

Expected: all existing frontend tests pass; `tsc` + Vite build pass.

- [ ] **Step 6: Commit the task**

Run:

```bash
git add src/components/McpPanel.tsx src/components/McpPanel.test.tsx
git commit -m "feat(ui): integrate MCP config editor into panel"
```

Report the panel test count and full-suite result.

---

### Task 7: Documentation and end-to-end verification

**Files:**
- Modify: `README.md` (MCP feature bullet, directory line, security section)
- Test: manual app run

**Interfaces:**
- None (documentation and verification only).

- [ ] **Step 1: Update the README**

Change the MCP feature bullet that begins `- **MCP 客户端**：stdio 传输` to:

```markdown
- **MCP 客户端**：stdio 传输，启动后台连接、工具命名走 `mcp__{server}__{tool}`、每台 server 可设信任档（全部允许 / 每次询问 / 禁止），信任设置持久化；侧栏 MCP 面板可在应用内直接编辑 `mcp.json`（严格校验、重启生效）。
```

Change the directory line that begins `  mcp/` to:

```markdown
  mcp/              mcp.json 读取/编辑、工具命名、stdio 桥、进程树回收
```

Add this line to the security section, after the existing "前端无 shell / 文件系统权限…" line:

```markdown
- 应用内配置编辑器会把 `mcp.json` 原文（含 env/headers）显示在界面里；MCP 列表与状态命令始终不返回这些值。
```

Do not change the current test-count sentence (it is a historical reading, not a contract).

- [ ] **Step 2: Run the complete automated suites**

Run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml
npm test
npm run build
npm run lint:css
```

Expected: all four commands pass with no new warnings.

- [ ] **Step 3: Run the app and verify the golden path**

Run:

```bash
npm run tauri dev
```

Verify manually:

1. Open the MCP panel. If `mcp.json` is missing, the empty state says to click 「编辑配置」.
2. Click 「编辑配置」. The template is shown, the path is shown, and the stdio/HTTP/disabled hint is visible.
3. Save without edits. The panel returns to the list and shows the restart notice. The file `<data dir>/mcp.json` matches the editor text byte-for-byte (if it was missing, it is created with the template).
4. Reopen the editor. Break the JSON (delete a brace). Save. The editor stays open, shows a parse-position error, and the file on disk is unchanged.
5. Fix the JSON and add an entry with `"args": "-y"` (wrong type). Save. The editor names the entry and reason; the file is unchanged.
6. Fix it to a valid stdio server (use an existing local MCP command or `npx -y @modelcontextprotocol/server-memory` if the machine can download it). Save.
7. Confirm the list now shows the new server as 「未连接」 and the restart notice is visible. Confirm no env/header value appears in the list or banner.
8. Restart the app. The server connects (or shows its real failure reason) and its tools appear. Confirm the trust controls still work.
9. Open the editor for a config containing `env` / `headers`; confirm the values are visible only there.
10. Click the backdrop while editing; the panel must not close. Press Esc with unsaved text; the inline 「放弃修改？」 confirm must appear.
11. Start a reply streaming, open the MCP panel, save an edit, and confirm the streaming reply is unaffected (save only writes the file).
12. The portable U-disk edition is removed (`b28e249`); do not reintroduce a launcher or `portable.txt`. Installer packaging is out of scope for this plan.

If step 6 is not possible offline, record that fact; do not claim the server connected.

- [ ] **Step 4: Commit the docs and report**

Run:

```bash
git add README.md
git commit -m "docs: document in-app MCP config editing"
```

Report the automated results and the manual checklist results (including any skipped item and why). Manual GUI verification may be recorded by the controller after this task.

---

## Self-Review Notes

- Spec coverage: editor entry/flow (Task 6), read/status (Task 2), strict save/atomic write (Task 3), IPC contracts (Task 4), secret red lines (Tasks 2-4, 5, 7), failure handling (Tasks 3, 5), status banner/empty state (Task 6), tests (all tasks + Task 7), README/capability docs (Tasks 4, 7).
- No placeholders: every code/test step contains the actual code; no "similar to" references.
- Type consistency: `parse_text` → `editor::{read,status,save}` → `McpConfig*View` → `McpConfig*` TS → `McpConfigEditor` props → `McpPanel` props are named identically across tasks.
- Non-goals preserved: no form CRUD, no HTTP transport, no live apply, no trust migration, no new dependencies, no commits without user approval.
