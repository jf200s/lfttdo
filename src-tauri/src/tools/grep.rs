//! grep：在 roots 内按正则做内容搜索（对齐 OpenCode 的同名工具）。
//!
//! 与 `read_file` 同一条 containment 取向：**路径防线归闸门**（`resolve_within_roots`），
//! 本文件打开的就是闸门给的那份 `PathBuf`，不留第二把尺子。目录级搜索是自己遍历
//! （不引 ripgrep 这类外部进程）：遍历有界（深度/数量/单文件体积），跳过 `.git`、
//! `node_modules`、`target` 三类纯噪音目录；二进制与非 UTF-8 文件按「跳过 + 末尾报数」
//! 处理而不是让整次搜索报错 —— 搜索一棵目录树时，一个 GBK 文件把整次搜索打死，
//! 模型只会原地重试。
//!
//! 输出上限 [`MAX_MATCHES`]：搜索的意义是「找到再精读」，200 处命中足够定位；
//! 真正的大输出限幅（截断 + 落盘）是 G09 的交付，落在输出组装那一层。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const NAME: &str = "grep";

pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    !roots.is_empty()
}

/// 命中条数上限。到顶就停（不再继续读后续文件），末尾说明收窄手段。
const MAX_MATCHES: usize = 200;
/// 单行展示上限（字符）：超长行（压缩 JS、minified）截断，避免一条命中占满输出。
const LINE_DISPLAY_CAP: usize = 500;
/// 遍历深度与文件数上限：防符号链接环之外的失控树（幽灵目录、挂载点）。
const MAX_DEPTH: usize = 32;
pub(crate) const MAX_FILES: usize = 20_000;
/// 二进制嗅探窗口：前 8 KiB 含 NUL 即按二进制跳过（与 read_file 的判据同一形状）。
const BINARY_SNIFF: usize = 8 * 1024;
/// 跳过的目录名（grep 与 glob 共用）：纯构建产物/依赖树，搜索只会被它们淹没。glob 参数可显式点名例外吗？
/// 不能 —— 这是硬规则，理由与「模型喊越界路径照样被闸门拒」相同：省一轮注定淹没的调用。
pub(crate) const SKIPPED_DIRS: [&str; 3] = [".git", "node_modules", "target"];

pub struct GrepTool;

/// 最小通配匹配：`*` 任意段、`?` 单字符，其余按字面。文件名匹配（不含目录），
/// 只服务 grep/glob 的 `glob` 过滤参数；完整的 glob 语义（`**`、字符类）不在本工具的面上。
fn wildcard_match(name: &str, pattern: &str) -> bool {
    // 经典双指针回溯：p 指模式，s 指名字。'*' 记位可回退，'?' 吃一个字符。
    let s: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut si, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 一处命中的展示行。行内容按字符截到 [`LINE_DISPLAY_CAP`]（字节切会碎在 UTF-8 中间）。
fn match_line(path: &Path, line_no: usize, line: &str) -> String {
    let display: String = if line.chars().count() > LINE_DISPLAY_CAP {
        let truncated: String = line.chars().take(LINE_DISPLAY_CAP).collect();
        format!("{truncated}…")
    } else {
        line.to_string()
    };
    format!("{}:{}: {}", path.display(), line_no, display.trim_end())
}

/// 递归收集待搜文件。返回 (文件表, 被跳过的非文本文件数)。
pub(crate) fn collect_files(root: &Path, files: &mut Vec<PathBuf>, skipped: &mut usize) {
    fn walk(dir: &Path, depth: usize, files: &mut Vec<PathBuf>, skipped: &mut usize) {
        if depth > MAX_DEPTH || files.len() >= MAX_FILES {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_FILES {
                return;
            }
            let path = entry.path();
            // symlink_metadata 不跟随链接：目录链接不深入（防环），文件链接按原样搜
            // （它 canonicalize 后若越界，根本进不了闸门给我们的 root 子树）。
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if SKIPPED_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                walk(&path, depth + 1, files, skipped);
            } else if meta.is_file() {
                files.push(path);
            }
        }
    }
    walk(root, 0, files, skipped);
}

/// 文件是不是文本：前 8 KiB 含 NUL ⇒ 二进制。读不动 ⇒ 按「跳过」计（报数，不报错）。
fn is_probably_text(path: &Path) -> Option<bool> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; BINARY_SNIFF];
    let n = f.read(&mut buf).ok()?;
    Some(!buf[..n].contains(&0))
}

/// 对文件内容跑正则，交出命中行（最多把 [`MAX_MATCHES`] 填满为止）。
/// 读取失败（非 UTF-8、权限等）由调用方计为「跳过」，不让整次搜索报错。
fn search_lines(path: &Path, content: &str, re: &regex::Regex, out: &mut Vec<String>) {
    for (i, line) in content.lines().enumerate() {
        if out.len() >= MAX_MATCHES {
            return;
        }
        if re.is_match(line) {
            // 行号 1 起始（ripgrep 惯例；read_file 的 offset=0 是「分页下标」，两种单位）。
            out.push(match_line(path, i + 1, line));
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: format!(
                "在用户配置的根目录（roots）内按正则搜索文件内容，只读、只接受绝对路径；\
                 roots 之外、经符号链接/junction 解析后落到 roots 之外的路径一律拒绝。\
                 返回「文件路径:行号: 内容」逐条列出，末尾带命中统计。`path` 可以是文件\
                 （只搜它）或目录（递归整棵子树），缺省为搜索全部 roots。`.git`、\
                 `node_modules`、`target` 目录跳过；二进制/非 UTF-8 文件跳过并在末尾报数。\
                 最多返回 {MAX_MATCHES} 处命中，超出会提示收窄 pattern 或加 `glob` 过滤\
                 （如 \"*.rs\"，匹配文件名，支持 * 与 ?）。`ignore_case: true` 忽略大小写。\
                 找到目标后通常应再用 read_file 精读上下文。"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Rust regex 语法的搜索模式（按行匹配）"
                    },
                    "path": {
                        "type": "string",
                        "description": "绝对路径（文件或目录），必须位于某个用户配置的 root 内；缺省 = 全部 roots"
                    },
                    "glob": {
                        "type": "string",
                        "description": "可选，按文件名过滤，如 \"*.rs\"（支持 * 与 ?）"
                    },
                    "ignore_case": {
                        "type": "boolean",
                        "description": "可选，true = 忽略大小写"
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
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::BadInput("grep 需要非空字符串参数 pattern（正则）".to_string())
            })?;
        let re = RegexBuilder::new(pattern)
            .case_insensitive(input.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false))
            .build()
            .map_err(|e| ToolError::BadInput(format!("pattern 不是合法正则: {e}")))?;
        let glob = input
            .get("glob")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty());

        // 搜索目标：闸门只认「绝对路径 → roots 内」。缺省路径 = 逐个 root 搜索，
        // roots 本身就是闸门 canonicalize 过的产物，不必再验一次。
        let mut targets: Vec<(PathBuf, bool)> = match input.get("path").and_then(|v| v.as_str()) {
            Some(raw) if !raw.trim().is_empty() => {
                let resolved = ctx.resolve_within_roots(raw)?;
                let meta = std::fs::symlink_metadata(&resolved).map_err(|e| {
                    ToolError::BadInput(format!("路径无法访问: {}: {e}", resolved.display()))
                })?;
                vec![(resolved, meta.is_dir())]
            }
            _ => ctx
                .roots
                .iter()
                .map(|r| {
                    let meta = std::fs::symlink_metadata(r).map_err(|e| {
                        ToolError::BadInput(format!("root 无法访问: {}: {e}", r.display()))
                    })?;
                    Ok((r.clone(), meta.is_dir()))
                })
                .collect::<Result<Vec<_>, ToolError>>()?,
        };
        targets.sort();
        targets.dedup();

        let mut files: Vec<PathBuf> = Vec::new();
        let mut skipped = 0usize;
        for (path, is_dir) in targets {
            if is_dir {
                collect_files(&path, &mut files, &mut skipped);
            } else {
                files.push(path);
            }
        }
        if let Some(glob) = glob {
            files.retain(|f| {
                f.file_name()
                    .is_some_and(|n| wildcard_match(&n.to_string_lossy(), glob))
            });
        }

        let mut out: Vec<String> = Vec::new();
        let mut matched_files: Vec<PathBuf> = Vec::new();
        let truncated = files.len();
        for file in &files {
            if out.len() >= MAX_MATCHES {
                break;
            }
            match is_probably_text(file) {
                Some(true) => {}
                // 嗅探失败（读不动）与二进制同样按「跳过」计，不报错。
                _ => {
                    skipped += 1;
                    continue;
                }
            }
            let before = out.len();
            match std::fs::read_to_string(file) {
                Ok(content) => search_lines(file, &content, &re, &mut out),
                // 非 UTF-8（GBK 等）在这里才暴露：跳过并计数，搜索继续。
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            }
            if out.len() > before {
                matched_files.push(file.clone());
            }
        }
        if out.is_empty() {
            let hint = if let Some(glob) = glob {
                format!("（glob 过滤 {glob}）")
            } else {
                String::new()
            };
            return Ok(ToolOutput::text(format!(
                "没有找到匹配 {pattern} 的内容{hint}；共扫描 {} 个文本文件{}。",
                files.len(),
                if skipped > 0 {
                    format!("，跳过 {skipped} 个二进制/非 UTF-8 文件")
                } else {
                    String::new()
                }
            )));
        }
        let mut text = out.join("\n");
        if out.len() >= MAX_MATCHES {
            text.push_str(&format!(
                "\n[已达 {MAX_MATCHES} 处命中上限，结果被截断；请收窄 pattern 或加 glob 过滤]"
            ));
        }
        if truncated > files.len() {
            text.push_str(&format!(
                "\n[文件数达到扫描上限，仅扫描前 {} 个文件]",
                files.len()
            ));
        }
        if skipped > 0 {
            text.push_str(&format!("\n[跳过 {skipped} 个二进制/非 UTF-8 文件]"));
        }
        text.push_str(&format!(
            "\n共 {} 处命中（{} 个文件）",
            out.len(),
            matched_files.len()
        ));
        Ok(ToolOutput::Text { text })
    }
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
        }
    }

    async fn run(input: Value, roots: Vec<PathBuf>) -> Result<ToolOutput, ToolError> {
        GrepTool.execute(input, &ctx(roots)).await
    }

    #[tokio::test]
    async fn a_single_file_match_reports_path_line_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\nlet x = find_me();\n").unwrap();
        let out = run(
            json!({ "pattern": "find_me", "path": file }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        };
        assert!(text.contains("a.rs:2: let x = find_me();"), "{text}");
        assert!(text.contains("共 1 处命中"), "{text}");
    }

    #[tokio::test]
    async fn a_directory_walk_finds_nested_matches_and_skips_noise_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/hidden.rs"), "find_me_in_git\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("nested")).unwrap();
        std::fs::write(tmp.path().join("nested/b.rs"), "let y = find_me();\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "nothing\n").unwrap();
        let out = run(
            json!({ "pattern": "find_me", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        };
        assert!(text.contains("nested/b.rs:1"), "{text}");
        assert!(!text.contains(".git"), "跳过的目录不许出现：{text}");
        assert!(!text.contains("c.txt"), "未命中的文件不该出现：{text}");
    }

    #[tokio::test]
    async fn an_invalid_regex_is_bad_input_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(
            json!({ "pattern": "([" }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_path_outside_the_roots_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let err = run(
            json!({ "pattern": "x", "path": outside.path().join("f.txt") }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn the_glob_filter_narrows_by_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "find_me\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "find_me\n").unwrap();
        let out = run(
            json!({ "pattern": "find_me", "path": tmp.path(), "glob": "*.rs" }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        };
        assert!(text.contains("a.rs"), "{text}");
        assert!(!text.contains("b.txt"), "{text}");
    }

    #[tokio::test]
    async fn binary_and_non_utf8_files_are_skipped_with_a_counted_note() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), b"fi\x00nd_me".to_vec()).unwrap();
        std::fs::write(tmp.path().join("gbk.txt"), [0xd5, 0xd2, 0x0a]).unwrap(); // GBK「找」+ 换行
        std::fs::write(tmp.path().join("ok.txt"), "find_me\n").unwrap();
        let out = run(
            json!({ "pattern": "find_me", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        };
        assert!(text.contains("ok.txt"), "{text}");
        assert!(text.contains("跳过 2 个二进制/非 UTF-8 文件"), "{text}");
    }

    #[tokio::test]
    async fn the_match_cap_stops_the_search_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let line = "hit\n".repeat(300);
        std::fs::write(tmp.path().join("many.txt"), line).unwrap();
        let out = run(
            json!({ "pattern": "hit", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        };
        assert!(text.contains(&format!("[已达 {MAX_MATCHES} 处命中上限")), "{text}");
    }

    /// 超长行按字符截断：不 panic（字节切 UTF-8 会碎），不交出 500 字以上的行。
    #[tokio::test]
    async fn an_overlong_line_is_truncated_by_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let line = format!("{find}{long}\n", find = "find_me", long = "字".repeat(2000));
        std::fs::write(tmp.path().join("long.txt"), line).unwrap();
        let out = run(
            json!({ "pattern": "find_me", "path": tmp.path() }),
            vec![tmp.path().to_path_buf()],
        )
        .await
        .unwrap();
        let text = match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        };
        let line_out = text.lines().next().unwrap();
        // 展示格式是「{path}:{行号}: {内容}」：按「冒号+空格」切，第 2 段起是内容
        // （第 1 段吃掉了「路径:行号」整体 —— 路径里的 `C:` 这类盘符不带空格，不会被误切）。
        let content = line_out.split(": ").nth(1).unwrap();
        assert!(content.ends_with('…'), "截断要挂省略号：{content}");
        assert!(content.chars().count() <= LINE_DISPLAY_CAP + 1, "{content}");
    }

    /// 注册名与声明过滤判据共用同一份常量（与 read_file 的 NAME 纪律一致）。
    #[test]
    fn name_constant_is_the_registered_name() {
        assert_eq!(GrepTool.meta().name, NAME);
        assert_eq!(NAME, "grep");
        assert!(usable_with_roots(&[PathBuf::from("/tmp")]));
        assert!(!usable_with_roots(&[]));
    }

    /// wildcard_match 的边界：纯字面、前缀星号、问号、多星。
    #[test]
    fn wildcard_match_covers_star_question_and_literal() {
        assert!(wildcard_match("a.rs", "*.rs"));
        assert!(wildcard_match("a.rs", "a.*"));
        assert!(wildcard_match("a", "?"), "? 匹配单个字符");
        assert!(!wildcard_match("a.rs", "?"), "? 只吃一个字符，不吃四个");
        assert!(wildcard_match("a.rs", "a.rs"));
        assert!(!wildcard_match("a.rs", "*.txt"));
        assert!(!wildcard_match("a.rs", "b*"));
        assert!(wildcard_match("a.b.rs", "*.rs"));
        assert!(!wildcard_match("a.rs", ""));
        assert!(wildcard_match("", "*"));
    }

    /// Arc<GrepTool> 能作为 dyn Tool 注册（注册表路径的形状检查）。
    #[test]
    fn grep_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(GrepTool));
        assert!(registry.get(NAME).is_some());
        assert_eq!(registry.risk_of(NAME), Risk::Read);
    }
}
