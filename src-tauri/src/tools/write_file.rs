//! write_file: create or overwrite UTF-8 text inside the session output directory.
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const NAME: &str = "write_file";

pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    !roots.is_empty()
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: format!(
                "在用户配置的根目录（roots）内新建或整文件覆盖一个 UTF-8 文本文件。\
                 只接受绝对路径；roots 之外、经符号链接/junction 解析后落到 roots 之外的\
                 路径一律拒绝。改已有文件前先用 read_file 看清原文；只改一段内容时优先\
                 用 edit_file，不要整文件重写。内容上限 {MAX_CONTENT_BYTES} 字节且不能含 NUL；\
                 目标已存在时保持它原有的换行风格；非 UTF-8 文件（如 GBK）会被明确拒绝，\
                 不静默转码。"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "绝对路径，必须位于某个用户配置的 root 内（按符号链接/junction 解析后判定）"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的完整文件内容，UTF-8 文本"
                    }
                },
                "required": ["path", "content"]
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
                ToolError::BadInput("write_file 需要非空字符串参数 path（绝对路径）".to_string())
            })?;
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::BadInput("write_file 需要字符串参数 content（完整文件内容）".to_string())
            })?;
        if content.len() > MAX_CONTENT_BYTES {
            return Err(ToolError::BadInput(format!(
                "content 有 {} 字节，超过上限 {MAX_CONTENT_BYTES} 字节；请拆分文件或由用户手工处理",
                content.len()
            )));
        }
        if content.contains('\0') {
            return Err(ToolError::BadInput(
                "content 含 NUL 字节：写出的文件会被 read_file 判为二进制，等于把目标锁死；请去掉二进制内容"
                    .to_string(),
            ));
        }

        let guarded = ctx.resolve_within_roots(raw)?;
        ctx.ensure_within_write_root(&guarded)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let existing = crate::tools::fs_write::existing_text(&guarded)?;
        let new_text = match &existing {
            Some(old) if crate::tools::fs_write::is_crlf(old) => {
                crate::tools::fs_write::to_crlf(content)
            }
            _ => content.to_string(),
        };

        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        crate::tools::fs_write::atomic_write(&guarded, &new_text)?;
        let action = if existing.is_some() {
            "覆盖"
        } else {
            "新建"
        };
        Ok(ToolOutput::text(format!(
            "已写入 {}（{action}，{} 字节）",
            crate::tools::fs_write::display_path(&guarded),
            new_text.len()
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

    fn ctx_with_write_root(roots: Vec<PathBuf>, write_root: PathBuf) -> ToolCtx {
        let mut ctx = test_ctx(roots);
        ctx.write_root = Some(write_root);
        ctx
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
        std::fs::read_to_string(path).expect("读回写入结果失败")
    }

    fn cleanup(root: &Path) {
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn creates_a_new_file_inside_a_root() {
        let root = temp_root();
        let path = root.join("note.txt");
        let out = WriteFileTool
            .execute(
                json!({"path": p(&root, "note.txt"), "content": "你好\n第二行"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();

        assert_eq!(read(&path), "你好\n第二行");
        let ToolOutput::Text { text } = out else {
            panic!("write_file 只产出文本");
        };
        assert!(
            text.contains("note.txt") && text.contains("新建"),
            "结果要告诉模型写到了哪、是新建还是覆盖：{text}"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn overwrites_an_existing_file() {
        let root = temp_root();
        std::fs::write(root.join("note.txt"), "旧内容").unwrap();
        let out = WriteFileTool
            .execute(
                json!({"path": p(&root, "note.txt"), "content": "新内容"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();

        assert_eq!(read(&root.join("note.txt")), "新内容");
        let ToolOutput::Text { text } = out else {
            panic!("write_file 只产出文本");
        };
        assert!(text.contains("覆盖"), "覆盖要如实说，不能写成新建：{text}");
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejects_a_path_outside_the_roots() {
        let root = temp_root();
        let outside = temp_root();
        let target = outside.join("secret.txt");
        std::fs::write(&target, "别动我").unwrap();

        let err = WriteFileTool
            .execute(
                json!({"path": target.to_string_lossy(), "content": "被改了"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("越界")),
            "越界必须报越界，实得 {err:?}"
        );
        assert_eq!(read(&target), "别动我", "被拒的写入不许碰文件");
        cleanup(&root);
        cleanup(&outside);
    }

    #[tokio::test]
    async fn rejects_a_relative_path() {
        let root = temp_root();
        let err = WriteFileTool
            .execute(
                json!({"path": "note.txt", "content": "x"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("绝对路径")),
            "相对路径必须拒，实得 {err:?}"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejects_content_larger_than_the_cap() {
        let root = temp_root();
        let big = "x".repeat(MAX_CONTENT_BYTES + 1);
        let err = WriteFileTool
            .execute(
                json!({"path": p(&root, "big.txt"), "content": big}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("上限")),
            "超过内容上限必须报错而不是截断，实得 {err:?}"
        );
        assert!(!root.join("big.txt").exists(), "被拒的写入不许落盘");
        cleanup(&root);
    }

    #[tokio::test]
    async fn refuses_to_write_outside_the_session_output_dir() {
        let root = temp_root();
        let output = root.join("work");
        std::fs::create_dir(&output).unwrap();
        let ctx = ctx_with_write_root(vec![root.clone()], output.clone());
        let err = WriteFileTool
            .execute(
                json!({"path": p(&root, "outside.txt"), "content": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("工作文件夹")),
            "输出目录之外的写入必须拒绝：{err:?}"
        );
        assert!(!root.join("outside.txt").exists());

        WriteFileTool
            .execute(
                json!({"path": output.join("inside.txt").to_string_lossy(), "content": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.join("inside.txt").is_file());
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejects_content_containing_a_nul_byte() {
        let root = temp_root();
        let err = WriteFileTool
            .execute(
                json!({"path": p(&root, "nul.txt"), "content": "a\u{0}b"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("NUL")),
            "含 NUL 的内容会写出 read_file 再也读不了的二进制文件，必须拒绝：{err:?}"
        );
        assert!(!root.join("nul.txt").exists(), "被拒的写入不许落盘");
        cleanup(&root);
    }

    #[tokio::test]
    async fn refuses_to_overwrite_a_non_utf8_file() {
        let root = temp_root();
        std::fs::write(root.join("gbk.txt"), [0xC4, 0xE3, 0xBA, 0xC3]).unwrap();

        let err = WriteFileTool
            .execute(
                json!({"path": p(&root, "gbk.txt"), "content": "新内容"}),
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
            vec![0xC4, 0xE3, 0xBA, 0xC3],
            "拒绝时原文件必须一字不动"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn keeps_crlf_line_endings_when_overwriting_a_crlf_file() {
        let root = temp_root();
        std::fs::write(root.join("win.txt"), "a\r\nb\r\n").unwrap();
        WriteFileTool
            .execute(
                json!({"path": p(&root, "win.txt"), "content": "x\ny\n"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        assert_eq!(
            read(&root.join("win.txt")),
            "x\r\ny\r\n",
            "覆盖 CRLF 文件必须保持 CRLF，否则 .bat 之类会被写坏"
        );
        cleanup(&root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_result_names_the_path_without_the_verbatim_prefix() {
        let root = temp_root();
        let out = WriteFileTool
            .execute(
                json!({"path": p(&root, "note.txt"), "content": "x"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("write_file 只产出文本");
        };
        assert!(
            !text.contains(r"\\?\"),
            "工具结果要给人读，不能把 canonicalize 的 verbatim 前缀原样交出去：{text}"
        );
        assert!(text.contains("note.txt"), "{text}");
        cleanup(&root);
    }

    #[tokio::test]
    async fn leaves_a_new_file_untouched_when_cancelled() {
        let root = temp_root();
        let ctx = test_ctx(vec![root.clone()]);
        ctx.cancel.cancel();
        let err = WriteFileTool
            .execute(json!({"path": p(&root, "note.txt"), "content": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Cancelled), "实得 {err:?}");
        assert!(!root.join("note.txt").exists(), "取消后不许留下文件");
        cleanup(&root);
    }

    #[tokio::test]
    async fn rejects_a_directory_target() {
        let root = temp_root();
        std::fs::create_dir(root.join("dir")).unwrap();
        let err = WriteFileTool
            .execute(
                json!({"path": p(&root, "dir"), "content": "x"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("目录")),
            "目标是目录时必须说清楚，实得 {err:?}"
        );
        cleanup(&root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_targeting_outside_the_root_is_denied_before_any_write() {
        let root = temp_root();
        let outside = temp_root();
        std::fs::write(outside.join("secret.txt"), "别动我").unwrap();
        let out = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &root.join("peek").display().to_string(),
                &outside.display().to_string(),
            ])
            .output()
            .expect("启动 cmd.exe 建 junction 失败");
        assert!(
            out.status.success(),
            "建 junction 失败: {}",
            String::from_utf8_lossy(&out.stdout)
        );

        let err = WriteFileTool
            .execute(
                json!({"path": root.join("peek").join("secret.txt").to_string_lossy(), "content": "被改了"}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("越界")),
            "经 junction 写出 root 必须报越界，实得 {err:?}"
        );
        assert_eq!(read(&outside.join("secret.txt")), "别动我");
        cleanup(&root);
        cleanup(&outside);
    }
}
