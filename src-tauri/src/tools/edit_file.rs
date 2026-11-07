//! edit_file: one exact replacement in an existing UTF-8 text file.
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::tools::write_file::MAX_CONTENT_BYTES;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;

pub const NAME: &str = "edit_file";

pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    !roots.is_empty()
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: format!(
                "在用户配置的根目录（roots）内编辑一个已有的 UTF-8 文本文件：把 old_string \
                 精确替换为 new_string。old_string 必须与文件中一段原文完全一致（含缩进与空行），\
                 且在文件中只出现一次；0 处或出现多次都会拒绝并说明，除非带 replace_all: true \
                 （把所有命中一次替换，回执说明替换处数）。结果带 unified diff（-/+ 行，\
                 前后各 2 行上下文）。先用 read_file 看清原文。只接受绝对路径；roots 之外、\
                 经符号链接/junction 解析后落到 roots 之外的路径一律拒绝。new_string 上限 \
                 {MAX_CONTENT_BYTES} 字节且不能含 NUL；目标文件超过 4 MiB、非 UTF-8（如 GBK）、\
                 疑似二进制或换行混用 LF/CRLF 时拒绝，编辑结果超过 4 MiB 也会拒绝；不静默转码。"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "绝对路径，必须位于某个用户配置的 root 内（按符号链接/junction 解析后判定）"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "要替换掉的原文片段，必须与文件内容完全一致且只出现一次（多命中时须带 replace_all）"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "替换后的新片段；空字符串表示删除这段原文"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "可选，true = 把 old_string 的所有命中一次替换（默认 false：多命中拒绝）"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            risk: Risk::Write,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let raw = input
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::BadInput("edit_file 需要非空字符串参数 path（绝对路径）".to_string())
            })?;
        let old = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::BadInput(
                    "edit_file 需要非空字符串参数 old_string（空模式会匹配每个位置）".to_string(),
                )
            })?;
        let new = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::BadInput(
                    "edit_file 需要字符串参数 new_string（删除这段原文时传空字符串）".to_string(),
                )
            })?;
        if new.len() > MAX_CONTENT_BYTES {
            return Err(ToolError::BadInput(format!(
                "new_string 有 {} 字节，超过上限 {MAX_CONTENT_BYTES} 字节",
                new.len()
            )));
        }
        if new.contains('\0') {
            return Err(ToolError::BadInput(
                "new_string 含 NUL 字节：写出的文件会被 read_file 判为二进制；请去掉二进制内容"
                    .to_string(),
            ));
        }

        let guarded = ctx.resolve_within_roots(raw)?;
        ctx.ensure_within_write_root(&guarded)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Some(existing) = crate::tools::fs_write::existing_text(&guarded)? else {
            return Err(ToolError::Failed(format!(
                "文件不存在（edit_file 不负责新建，请用 write_file）: {}",
                crate::tools::fs_write::display_path(&guarded)
            )));
        };
        if crate::tools::fs_write::has_mixed_eol(&existing) {
            return Err(ToolError::Failed(format!(
                "文件换行混用 LF 与 CRLF：只改命中片段会留下两种换行，整体转换又会改到未命中的行；\
                 请先统一换行，或用 write_file 整文件重写: {}",
                crate::tools::fs_write::display_path(&guarded)
            )));
        }

        let crlf = crate::tools::fs_write::is_crlf(&existing);
        let text = crate::tools::fs_write::to_lf(&existing);
        let old_lf = crate::tools::fs_write::to_lf(old);
        let new_lf = crate::tools::fs_write::to_lf(new);
        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hits = text.matches(&old_lf).count();
        if hits == 0 {
            return Err(ToolError::Failed(format!(
                "在文件中没有找到 old_string（注意缩进、空白与行尾）；请先用 read_file 核对原文: {}",
                crate::tools::fs_write::display_path(&guarded)
            )));
        }
        if hits > 1 && !replace_all {
            return Err(ToolError::Failed(format!(
                "old_string 在文件中出现了 {hits} 处，必须唯一；请带上更长的上下文再试，\
                 或确认要把所有命中都替换时带 replace_all: true: {}",
                crate::tools::fs_write::display_path(&guarded)
            )));
        }

        let replaced = if replace_all {
            text.replace(&old_lf, &new_lf)
        } else {
            text.replacen(&old_lf, &new_lf, 1)
        };
        let diff = unified_diff(&text, &replaced);
        let output = if crlf {
            crate::tools::fs_write::to_crlf(&replaced)
        } else {
            replaced
        };
        if output.len() as u64 > crate::tools::fs_write::MAX_EXISTING_BYTES {
            return Err(ToolError::Failed(format!(
                "编辑后的文件有 {} 字节，超过 {} 字节上限；写出它之后 edit_file 自己将无法再读: {}",
                output.len(),
                crate::tools::fs_write::MAX_EXISTING_BYTES,
                crate::tools::fs_write::display_path(&guarded)
            )));
        }
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        crate::tools::fs_write::atomic_write(&guarded, &output)?;
        Ok(ToolOutput::text(format!(
            "已编辑 {}（替换 {} 处，共 {} 字节）\n{}",
            crate::tools::fs_write::display_path(&guarded),
            hits,
            output.len(),
            if diff.is_empty() {
                String::new()
            } else {
                format!("```diff\n{diff}```")
            }
        )))
    }
}

/// 变更差异（有界 unified diff）。改动恒是「一段原文 → 一段新文」，用公共前缀/后缀行
/// 定位改动区间，前后各带 2 行上下文；对比在 **LF 归一后**的两份文本上进行 ——
/// 行尾差异（CRLF/LF）不是这次编辑的内容，不该出现在差异里。改动区超过
/// [`DIFF_LINES_CAP`] 行时截断并注明省略量（edit_file 的改动通常是个位数行，
/// 上限只为兜住「一次替换整个文件」的极端调用）。
fn unified_diff(old_text: &str, new_text: &str) -> String {
    const CTX: usize = 2;
    const DIFF_LINES_CAP: usize = 40;
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let removed = &old_lines[prefix..old_lines.len() - suffix];
    let added = &new_lines[prefix..new_lines.len() - suffix];
    if removed.is_empty() && added.is_empty() {
        return String::new();
    }
    let ctx_start = prefix.saturating_sub(CTX);
    let ctx_end = (old_lines.len() - suffix + CTX).min(old_lines.len());
    let mut out = format!("@@ -{} +{} @@\n", prefix + 1, prefix + 1);
    for line in &old_lines[ctx_start..prefix] {
        out.push_str(&format!(" {line}\n"));
    }
    let mut shown = 0usize;
    for line in removed {
        if shown >= DIFF_LINES_CAP {
            break;
        }
        out.push_str(&format!("-{line}\n"));
        shown += 1;
    }
    for line in added {
        if shown >= DIFF_LINES_CAP {
            break;
        }
        out.push_str(&format!("+{line}\n"));
        shown += 1;
    }
    if removed.len() + added.len() > shown {
        out.push_str(&format!(
            "…[差异过长，已省略 {} 行；用 read_file 查看完整结果]\n",
            removed.len() + added.len() - shown
        ));
    }
    for line in &old_lines[old_lines.len() - suffix..ctx_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolCtx;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use tokio_util::sync::CancellationToken;

    fn test_ctx(roots: Vec<PathBuf>) -> ToolCtx {
        ToolCtx {
            write_root: None,
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
            skills_dir: None,
            skills_disabled: Vec::new(),
        }
    }

    fn temp_root() -> PathBuf {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let root = dir.path().to_path_buf();
        std::mem::forget(dir);
        root
    }

    fn p(root: &Path, name: &str) -> String {
        root.join(name).to_string_lossy().to_string()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("读回编辑结果失败")
    }

    fn cleanup(root: &Path) {
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn replaces_a_unique_occurrence_and_keeps_the_rest() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "第一行\n旧值\n第三行\n").unwrap();
        let out = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "旧值",
                    "new_string": "新值"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();

        assert_eq!(read(&root.join("note.txt")), "第一行\n新值\n第三行\n");
        let ToolOutput::Text { text } = out else {
            panic!("edit_file 只产出文本");
        };
        assert!(
            text.contains("note.txt") && text.contains("1 处"),
            "结果要告诉模型改了哪、改了几处：{text}"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn fails_when_the_old_string_is_absent() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "只有这一行\n").unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "不存在的内容",
                    "new_string": "新值"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("没有找到")),
            "0 处匹配必须明确报没有找到，实得 {err:?}"
        );
        assert_eq!(
            read(&root.join("note.txt")),
            "只有这一行\n",
            "失败时文件不许变"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn fails_when_the_old_string_appears_more_than_once() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "值\n值\n").unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "值",
                    "new_string": "新值"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("2 处")),
            "多处匹配必须报出实际次数而不是静默改第一处，实得 {err:?}"
        );
        assert_eq!(read(&root.join("note.txt")), "值\n值\n");
        cleanup(&root);
    }

    /// G03：显式声明 `replace_all: true` 时多命中一次全部替换，回执报出替换处数。
    #[tokio::test]
    async fn replace_all_replaces_every_occurrence_and_reports_the_count() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "值\n值\n值\n").unwrap();
        let out = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "值",
                    "new_string": "新值",
                    "replace_all": true
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        assert_eq!(read(&root.join("note.txt")), "新值\n新值\n新值\n");
        let ToolOutput::Text { text } = out else {
            panic!("edit_file 只产出文本");
        };
        assert!(text.contains("替换 3 处"), "{text}");
        cleanup(&root);
    }

    /// G03 的另一半：单命中时带 replace_all 也只是普通替换，不放大成多份。
    #[tokio::test]
    async fn replace_all_with_a_single_hit_behaves_like_a_plain_replace() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "a b c\n").unwrap();
        let out = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "b",
                    "new_string": "d",
                    "replace_all": true
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        assert_eq!(read(&root.join("note.txt")), "a d c\n");
        let ToolOutput::Text { text } = out else {
            panic!("edit_file 只产出文本");
        };
        assert!(text.contains("替换 1 处"), "{text}");
        cleanup(&root);
    }

    /// G04：回执带 unified diff —— 被删行带 `-`、新增行带 `+`，上下文行带空格前缀。
    #[tokio::test]
    async fn the_receipt_carries_a_unified_diff() {
        let root = temp_root();
        std::fs::write(root.join("code.rs"), "line1\nline2\nline3\n").unwrap();
        let out = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "code.rs"),
                    "old_string": "line2",
                    "new_string": "line_two"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("edit_file 只产出文本");
        };
        assert!(text.contains("-line2"), "{text}");
        assert!(text.contains("+line_two"), "{text}");
        assert!(text.contains(" line1"), "上下文行要有空格前缀：{text}");
        assert!(text.contains("@@"), "要有 hunk 头：{text}");
        cleanup(&root);
    }

    /// 纯函数面：CRLF 差异不算变更（对比在 LF 归一后）；空差异返回空串。
    #[test]
    fn unified_diff_ignores_eol_only_changes_and_returns_empty_when_unchanged() {
        assert_eq!(unified_diff("a\nb\n", "a\nb\n"), "");
        assert_eq!(unified_diff("a\r\nb\r\n", "a\nb\n"), "", "行尾差异不是编辑内容");
        let diff = unified_diff("a\nb\nc\n", "a\nX\nc\n");
        assert!(diff.contains("-b"), "{diff}");
        assert!(diff.contains("+X"), "{diff}");
    }

    #[tokio::test]
    async fn rejects_an_empty_old_string() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "x").unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "",
                    "new_string": "y"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("old_string")),
            "空 old_string 必须拒绝（空模式会匹配每个位置），实得 {err:?}"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn matches_lf_old_string_against_a_crlf_file_and_preserves_crlf() {
        let root = temp_root();
        std::fs::write(root.join("win.txt"), "第一行\r\n旧值\r\n第三行\r\n").unwrap();
        EditFileTool
            .execute(
                json!({
                    "path": p(&root, "win.txt"),
                    "old_string": "第一行\n旧值",
                    "new_string": "第一行\n新值"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        assert_eq!(
            read(&root.join("win.txt")),
            "第一行\r\n新值\r\n第三行\r\n",
            "read_file 交给模型的是 LF，匹配要能跨过换行风格，写回仍保持 CRLF"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn an_empty_new_string_deletes_the_matched_snippet() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "保留\n删掉我\n保留\n").unwrap();
        EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "删掉我\n",
                    "new_string": ""
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        assert_eq!(read(&root.join("note.txt")), "保留\n保留\n");
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejects_a_new_string_containing_a_nul_byte() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "旧值").unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "旧值",
                    "new_string": "a\u{0}b"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("NUL")),
            "含 NUL 的替换内容必须拒绝：{err:?}"
        );
        assert_eq!(read(&root.join("note.txt")), "旧值");
        cleanup(&root);
    }

    #[tokio::test]
    async fn refuses_a_file_with_mixed_line_endings() {
        let root = temp_root();
        std::fs::write(root.join("mixed.txt"), "a\r\nb\nc\r\n").unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "mixed.txt"),
                    "old_string": "b",
                    "new_string": "B"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("混用")),
            "换行混用的文件无法只在命中片段内保持原样，必须拒绝而不是改写全文：{err:?}"
        );
        assert_eq!(read(&root.join("mixed.txt")), "a\r\nb\nc\r\n");
        cleanup(&root);
    }

    #[tokio::test]
    async fn refuses_an_edit_that_would_push_the_file_past_the_size_limit() {
        let root = temp_root();
        let max = crate::tools::fs_write::MAX_EXISTING_BYTES as usize;
        let seed = format!("{}旧", "x".repeat(max - "旧".len()));
        assert_eq!(seed.len(), max, "夹具前提：目标必须恰好在上限上");
        std::fs::write(root.join("big.txt"), &seed).unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "big.txt"),
                    "old_string": "旧",
                    "new_string": "x".repeat(1024)
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("超过")),
            "编辑后的结果越界必须拒绝，不能写出 edit_file 自己再也读不了的文件：{err:?}"
        );
        assert_eq!(
            std::fs::metadata(root.join("big.txt")).unwrap().len(),
            max as u64
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn refuses_a_non_utf8_file_without_touching_it() {
        let root = temp_root();
        std::fs::write(root.join("gbk.txt"), [0xC4, 0xE3, 0xBA, 0xC3]).unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "gbk.txt"),
                    "old_string": "旧",
                    "new_string": "新"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("UTF-8")),
            "非 UTF-8 目标必须明确拒绝，实得 {err:?}"
        );
        assert_eq!(
            std::fs::read(root.join("gbk.txt")).unwrap(),
            vec![0xC4, 0xE3, 0xBA, 0xC3]
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn refuses_a_missing_file_instead_of_creating_it() {
        let root = temp_root();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "missing.txt"),
                    "old_string": "旧",
                    "new_string": "新"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("不存在")),
            "edit_file 不负责新建，实得 {err:?}"
        );
        assert!(!root.join("missing.txt").exists());
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejects_a_path_outside_the_roots() {
        let root = temp_root();
        let outside = temp_root();
        std::fs::write(outside.join("secret.txt"), "别动我").unwrap();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&outside, "secret.txt"),
                    "old_string": "别动我",
                    "new_string": "被改了"
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("越界")),
            "越界必须报越界，实得 {err:?}"
        );
        assert_eq!(read(&outside.join("secret.txt")), "别动我");
        cleanup(&root);
        cleanup(&outside);
    }

    #[tokio::test]
    async fn rejects_a_new_string_larger_than_the_cap() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "旧值").unwrap();
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "旧值",
                    "new_string": big
                }),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("上限")),
            "new_string 超限必须拒绝，实得 {err:?}"
        );
        assert_eq!(read(&root.join("note.txt")), "旧值");
        cleanup(&root);
    }

    #[tokio::test]
    async fn leaves_the_file_untouched_when_cancelled() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "旧值").unwrap();
        let ctx = test_ctx(vec![root.clone()]);
        ctx.cancel.cancel();
        let err = EditFileTool
            .execute(
                json!({
                    "path": p(&root, "note.txt"),
                    "old_string": "旧值",
                    "new_string": "新值"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "实得 {err:?}");
        assert_eq!(read(&root.join("note.txt")), "旧值");
        cleanup(&root);
    }
}
