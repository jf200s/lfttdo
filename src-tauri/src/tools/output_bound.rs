//! 工具输出限幅（G09）：超长输出截断为短预览，完整内容落盘 `.lfttdo/outputs/`，
//! 路径写回输出里给模型「再读全文」的入口。
//!
//! OpenCode 的 bash/edit 都这么处理大输出：界面与上下文只吃预览，全文在文件里。
//! lfttdo 的既有工具各有一个「硬上限」（grep 200 处、run_command 512 KiB），但
//! 上限之内的输出仍可能远超模型该吃的量 —— 一条 600 KB 的 minified 行就能把
//! 上下文顶爆。落盘用内容哈希命名：同一份输出重复产生时不翻倍占盘。
use crate::tools::registry::ToolCtx;
use crate::tools::todo::APP_STATE_DIR;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// 预览保留的字符数（按字符不按字节：字节切会碎在 UTF-8 序列中间）。
pub(crate) const OUTPUT_PREVIEW_CHARS: usize = 2000;

/// 限幅入口。短文本原样返回；超长文本：完整内容写 `<base>/.lfttdo/outputs/`，
/// 返回「前 N 字符 + 截断标记 + 完整输出路径」。base 取 write_root，缺省 roots[0]；
/// **没有落点时只能硬截断**（不给路径）—— 预览的职责（护住上下文）不依赖落盘成功。
pub(crate) fn bound_tool_output(text: String, ctx: &ToolCtx) -> String {
    if text.chars().count() <= OUTPUT_PREVIEW_CHARS {
        return text;
    }
    let total = text.chars().count();
    let preview: String = text.chars().take(OUTPUT_PREVIEW_CHARS).collect();
    // 标记放**最前**：模型第一眼就知道这是截断预览、全文在哪，而不是读完 2000 字符
    // 才发现还有更多（读前文时不知道后面有删节，会照着半截内容下结论）。
    match persist(&text, ctx) {
        Some(path) => format!(
            "[输出超长，共 {total} 字符，已截断；完整输出: {} 共 {total} 字符]\n{preview}",
            path.display()
        ),
        None => format!(
            "[输出超长，共 {total} 字符，已截断；没有可用的工作目录，完整输出未能落盘]\n{preview}"
        ),
    }
}

/// 完整输出落盘，返回绝对路径；写不了（无落点/磁盘错误）返回 None —— 截断照常生效。
fn persist(text: &str, ctx: &ToolCtx) -> Option<PathBuf> {
    let base = ctx
        .write_root
        .clone()
        .or_else(|| ctx.roots.first().cloned())?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let dir = base.join(APP_STATE_DIR).join("outputs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{:016x}.txt", hasher.finish()));
    std::fs::write(&path, text).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolCtx;
    use tokio_util::sync::CancellationToken;

    fn ctx(roots: Vec<PathBuf>, write_root: Option<PathBuf>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
            write_root,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: None,
            events: None,
        }
    }

    #[test]
    fn a_short_output_passes_through_untouched() {
        let c = ctx(vec![], None);
        assert_eq!(bound_tool_output("短输出".into(), &c), "短输出");
        let exact: String = "x".repeat(OUTPUT_PREVIEW_CHARS);
        assert_eq!(
            bound_tool_output(exact.clone(), &c),
            exact,
            "恰好上限不算超长"
        );
    }

    #[test]
    fn an_overlong_output_is_truncated_and_persisted_with_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(
            vec![tmp.path().to_path_buf()],
            Some(tmp.path().to_path_buf()),
        );
        let big = format!("needle {}", "字".repeat(100_000));
        let out = bound_tool_output(big.clone(), &c);
        assert!(out.chars().count() < 4000, "{}", out.chars().count());
        assert!(out.contains("已截断"), "{out}");
        // 路径在输出里、文件真实存在、内容完整。
        let base = tmp.path().to_str().unwrap();
        let start = out.find(base).unwrap();
        let end = out[start..].find(' ').unwrap_or(out.len() - start);
        let path = PathBuf::from(&out[start..start + end]);
        let full = std::fs::read_to_string(&path).unwrap();
        assert_eq!(full, big, "落盘的是完整输出");
        assert!(path.to_str().unwrap().contains(".lfttdo/outputs"));
    }

    #[test]
    fn the_same_content_persists_to_the_same_file() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(vec![], Some(tmp.path().to_path_buf()));
        let big = "y".repeat(50_000);
        let out1 = bound_tool_output(big.clone(), &c);
        let out2 = bound_tool_output(big, &c);
        let path_of = |o: &str| {
            let start = o.find(tmp.path().to_str().unwrap()).unwrap();
            let end = o[start..].find(' ').unwrap();
            PathBuf::from(&o[start..start + end])
        };
        assert_eq!(path_of(&out1), path_of(&out2), "同内容同名（内容哈希）");
    }

    #[test]
    fn without_a_base_dir_it_hard_truncates_and_says_so() {
        let c = ctx(vec![], None);
        let out = bound_tool_output("z".repeat(50_000), &c);
        assert!(out.chars().count() < 4000);
        assert!(out.contains("未能落盘"), "{out}");
    }

    /// 多字节字符不许被字节切碎：预览与落盘都必须按字符边界。
    #[test]
    fn truncation_happens_on_char_boundaries() {
        let c = ctx(
            vec![],
            Some(tempfile::tempdir().unwrap().path().to_path_buf()),
        );
        let big = "汉".repeat(OUTPUT_PREVIEW_CHARS + 500);
        let out = bound_tool_output(big, &c);
        assert!(out.contains("汉"), "{out}");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
