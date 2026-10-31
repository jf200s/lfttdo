//! edit_file: one exact replacement in an existing UTF-8 text file.
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::tools::write_file::MAX_CONTENT_BYTES;
use async_trait::async_trait;
use serde_json::{json, Value};
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
                 且在文件中只出现一次；0 处或出现多次都会拒绝并说明。先用 read_file 看清原文。\
                 只接受绝对路径；roots 之外、经符号链接/junction 解析后落到 roots 之外的路径\
                 一律拒绝。new_string 上限 {MAX_CONTENT_BYTES} 字节且不能含 NUL；目标文件超过\
                 4 MiB、非 UTF-8（如 GBK）、疑似二进制或换行混用 LF/CRLF 时拒绝，编辑结果超过\
                 4 MiB 也会拒绝；不静默转码。"
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
                        "description": "要替换掉的原文片段，必须与文件内容完全一致且只出现一次"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "替换后的新片段；空字符串表示删除这段原文"
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
        let hits = text.matches(&old_lf).count();
        if hits == 0 {
            return Err(ToolError::Failed(format!(
                "在文件中没有找到 old_string（注意缩进、空白与行尾）；请先用 read_file 核对原文: {}",
                crate::tools::fs_write::display_path(&guarded)
            )));
        }
        if hits > 1 {
            return Err(ToolError::Failed(format!(
                "old_string 在文件中出现了 {hits} 处，必须唯一；请带上更长的上下文再试: {}",
                crate::tools::fs_write::display_path(&guarded)
            )));
        }

        let replaced = text.replacen(&old_lf, &new_lf, 1);
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
            "已编辑 {}（替换 1 处，共 {} 字节）",
            crate::tools::fs_write::display_path(&guarded),
            output.len()
        )))
    }
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
