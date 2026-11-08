//! glob：在 roots 内按路径模式匹配文件（对齐 OpenCode 的同名工具）。
//!
//! 与 `grep` 的分工：grep 找「内容」，glob 找「路径」。遍历器**复用** `grep.rs` 的
//! `collect_files`（同一份目录跳过表与有界遍历，R77：不留两份）；匹配器自己实现
//! （不引 `glob` crate）：把模式编译成路径感知的正则 —— `*`/`?` 不越过 `/`，
//! `**` 跨段，其余字符按字面转义。结果按字典序（`BTreeSet`），与注册表「名字序可复现」
//! 的同一取向；OpenCode 按修改时间排序，但那会让测试与输出随文件系统时间戳漂移。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::grep::collect_files;
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const NAME: &str = "glob";

pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    !roots.is_empty()
}

/// 结果条数上限：glob 的意义是「列出来再挑」，1000 条足够定位；
/// 超出的应对手段（收窄模式）写在提示里。
const MAX_RESULTS: usize = 1000;

/// 把路径 glob 模式编译成正则。`**/` = 零或多段目录，`**` = 任意串，
/// `*` = 不含 `/` 的任意段内字符，`?` = 单个非 `/` 字符，其余按字面转义。
fn path_glob_regex(pattern: &str) -> Result<regex::Regex, ToolError> {
    if pattern.trim().is_empty() {
        return Err(ToolError::BadInput(
            "glob 需要非空字符串参数 pattern".to_string(),
        ));
    }
    let mut re = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                // `**/` 吃掉自身后面的斜杠：匹配「零段或多段目录」。
                if i + 2 < chars.len() && chars[i + 2] == '/' {
                    re.push_str("(?:[^/]*/)*");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                for esc in ".*+?()|[]{}^$\\".chars() {
                    if c == esc {
                        re.push('\\');
                        break;
                    }
                }
                re.push(c);
                i += 1;
            }
        }
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| ToolError::BadInput(format!("pattern 无法编译: {e}")))
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: format!(
                "在用户配置的根目录（roots）内按路径模式匹配文件，只读、只接受绝对路径；\
                 roots 之外、经符号链接/junction 解析后落到 roots 之外的路径一律拒绝。\
                 模式语法：`**/*.rs` 跨目录匹配所有 Rust 文件，`*` 不越过 `/`，`?` 单字符，\
                 其余按字面。匹配的是**相对搜索起点的路径**，返回绝对路径清单（字典序），\
                 末尾带数量统计。`path` 缺省为全部 roots；`.git`、`node_modules`、`target` \
                 目录跳过。最多返回 {MAX_RESULTS} 条，超出会提示收窄模式。\
                 找到目标后通常应再用 read_file 精读。"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "路径 glob 模式，如 \"**/*.rs\"、\"src/*.ts\"、\"Cargo.toml\""
                    },
                    "path": {
                        "type": "string",
                        "description": "绝对路径（应为目录），必须位于某个用户配置的 root 内；缺省 = 全部 roots"
                    }
                },
                "required": ["pattern"]
            }),
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::BadInput("glob 需要字符串参数 pattern（如 \"**/*.rs\"）".to_string())
            })?;
        let re = path_glob_regex(pattern)?;

        // 搜索起点：与 grep 同一条闸门路径（resolve_within_roots / 全 roots）。
        let mut roots: Vec<PathBuf> = match input.get("path").and_then(|v| v.as_str()) {
            Some(raw) if !raw.trim().is_empty() => {
                let resolved = ctx.resolve_within_roots(raw)?;
                let meta = std::fs::symlink_metadata(&resolved).map_err(|e| {
                    ToolError::BadInput(format!("路径无法访问: {}: {e}", resolved.display()))
                })?;
                if !meta.is_dir() {
                    return Err(ToolError::BadInput(format!(
                        "path 应当是目录（glob 找的是文件路径，不是内容）: {}",
                        resolved.display()
                    )));
                }
                vec![resolved]
            }
            _ => ctx
                .roots
                .iter()
                .map(|r| {
                    let meta = std::fs::symlink_metadata(r).map_err(|e| {
                        ToolError::BadInput(format!("root 无法访问: {}: {e}", r.display()))
                    })?;
                    if !meta.is_dir() {
                        return Err(ToolError::BadInput(format!(
                            "root 应当是目录: {}",
                            r.display()
                        )));
                    }
                    Ok(r.clone())
                })
                .collect::<Result<Vec<_>, ToolError>>()?,
        };
        roots.sort();
        roots.dedup();

        let mut files: Vec<PathBuf> = Vec::new();
        let mut skipped = 0usize; // collect_files 的签名带它（grep 用于二进制报数）；glob 不消费
        for root in &roots {
            collect_files(root, &mut files, &mut skipped);
        }
        // 模式匹配的是「相对搜索起点的路径」：`**/*.rs` 从 root 出发才算得对。
        let mut hits: BTreeSet<PathBuf> = BTreeSet::new();
        let mut truncated_by_cap = false;
        for file in &files {
            let Ok(rel) = file.strip_prefix(root_of(file, &roots)) else {
                continue;
            };
            let rel = rel.to_string_lossy();
            if !re.is_match(&rel) {
                continue;
            }
            if hits.len() >= MAX_RESULTS {
                truncated_by_cap = true;
                break;
            }
            hits.insert(file.clone());
        }
        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "没有匹配模式 {pattern} 的文件；共扫描 {} 个文件。试试放宽模式（如 \"**/{pattern}\"）。",
                files.len()
            )));
        }
        let mut text: String = hits
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // 汇总与提示放最前（与 grep 同一布局：预览截断时提示必须还在）。
        let mut head = format!("共 {} 个文件", hits.len());
        if truncated_by_cap {
            head.push_str("；[已达上限，结果被截断，请收窄 pattern]");
        }
        text = format!("{head}\n{text}");
        // G09：超长输出截断 + 完整内容落盘（.lfttdo/outputs/）。
        let text = crate::tools::output_bound::bound_tool_output(text, ctx);
        Ok(ToolOutput::Text { text })
    }
}

/// 文件所属的搜索起点（strip_prefix 用）。collect_files 只会推入 roots 子树内的路径，
/// 找不到归属时退回整个文件名（等价于相对路径 = 文件名，不应发生，只为不 panic）。
fn root_of<'a>(file: &'a Path, roots: &'a [PathBuf]) -> &'a Path {
    roots
        .iter()
        .find(|r| file.starts_with(r))
        .map(|r| r.as_path())
        .unwrap_or(file.parent().unwrap_or(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx(roots: Vec<PathBuf>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: roots.clone(),
            write_root: None,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: None,
            events: None,
        }
    }

    async fn run(input: Value, roots: Vec<PathBuf>) -> Result<ToolOutput, ToolError> {
        GlobTool.execute(input, &ctx(roots)).await
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn double_star_pattern_matches_top_level_and_nested() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("nested/deep")).unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(tmp.path().join("nested/c.rs"), "fn c() {}\n").unwrap();
        std::fs::write(tmp.path().join("nested/deep/d.rs"), "fn d() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x\n").unwrap();
        let out = run(
            json!({ "pattern": "**/*.rs", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("a.rs"), "{text}");
        assert!(text.contains("nested/c.rs"), "{text}");
        assert!(text.contains("nested/deep/d.rs"), "{text}");
        assert!(!text.contains("b.txt"), "{text}");
        assert!(text.contains("共 3 个文件"), "{text}");
    }

    #[tokio::test]
    async fn a_literal_pattern_matches_only_that_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("other.toml"), "").unwrap();
        let out = run(
            json!({ "pattern": "Cargo.toml", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("Cargo.toml"), "{text}");
        assert!(!text.contains("other.toml"), "{text}");
    }

    #[tokio::test]
    async fn star_does_not_cross_directory_separators() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d/b.rs"), "").unwrap();
        let out = run(
            json!({ "pattern": "*.rs", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.rs"), "{text}");
    }

    #[tokio::test]
    async fn skipped_dirs_are_not_searched() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join("target/artifact.rs"), "").unwrap();
        std::fs::write(tmp.path().join("real.rs"), "").unwrap();
        let out = run(
            json!({ "pattern": "**/*.rs", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("real.rs"), "{text}");
        assert!(!text.contains("artifact.rs"), "{text}");
    }

    #[tokio::test]
    async fn an_empty_result_is_a_message_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        let out = run(
            json!({ "pattern": "**/*.zig", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(text.contains("没有匹配"), "{text}");
    }

    #[tokio::test]
    async fn a_path_outside_the_roots_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let err = run(
            json!({ "pattern": "**/*", "path": outside.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_file_path_target_is_rejected_with_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        std::fs::write(&file, "").unwrap();
        let err = run(
            json!({ "pattern": "**/*", "path": file }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    /// path_glob_regex 的纯函数面：转义、跨段、零段。
    #[test]
    fn path_glob_regex_covers_escape_doublestar_and_zero_segments() {
        let re = |p: &str| path_glob_regex(p).unwrap();
        assert!(re("**/*.rs").is_match("a.rs"), "**/ 要能匹配零段目录");
        assert!(re("**/*.rs").is_match("x/y/z.rs"));
        assert!(!re("**/*.rs").is_match("a.txt"));
        assert!(re("src/*.ts").is_match("src/a.ts"));
        assert!(!re("src/*.ts").is_match("src/x/a.ts"), "* 不越段");
        assert!(re("a?c").is_match("abc"));
        assert!(!re("a?c").is_match("ac"));
        assert!(re("a+b").is_match("a+b"), "+ 按字面");
        assert!(!re("a+b").is_match("aab"));
        assert!(re("**").is_match("x/y"));
        assert!(path_glob_regex("").is_err());
    }

    #[test]
    fn name_constant_is_the_registered_name() {
        assert_eq!(GlobTool.meta().name, NAME);
        assert_eq!(NAME, "glob");
        assert!(usable_with_roots(&[PathBuf::from("/tmp")]));
        assert!(!usable_with_roots(&[]));
    }

    #[test]
    fn glob_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(GlobTool));
        assert!(registry.get(NAME).is_some());
        assert_eq!(registry.risk_of(NAME), Risk::Read);
    }
}
