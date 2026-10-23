// Plan B 的第二把生产工具：`read_file` —— 只在用户配置的 roots 内做只读文本读取。
//
// 本任务**不注册进生产**（`ipc.rs` 那张空 `ToolRegistry` 一字不动，注册与 UI 卡片
// 是 Task 27 的活）；交付的是工具本体 + 测试。
//
// # 为什么要「二次 canonicalize」这一整段
//
// `ToolCtx::resolve_within_roots`（registry.rs）判的是**父目录**的 canonical 形态，
// 最后一段原样接回去 —— 那是 `write_file` 的语义（新建文件时末段还不存在）。于是
// 「最后一段自己就是符号链接/junction」不归它管。读文件要求文件**已存在**，可以放心
// 把整条路径再 canonicalize 一次、交回**同一道闸**复验（复用而不是另造比较，R77 的
// 口径）：链接指向 root 外时在 open 之前就报「越界」。本机建不了文件级符号链接
// （非开发者模式，实测报「没有足够的权限」），同形状的目录 junction 由
// `a_junction_as_the_final_component_is_denied_before_any_read` 钉住这一步 ——
// 把这段摘掉该条当场变红（变异核验见 task-25 报告）。
//
// # 分页单位：按**行**
//
// 与 `fetch_url` 的按字符分页刻意不同（读日志/源码的日常直觉是「第几行」）。两处各自
// 在 `description` 里写清单位，防止模型混用（plan Task 25 的原话）。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::io::Read;

/// 单次交付的字节上限：超过就必须分页，而不是把整个文件吸进内存。
pub const MAX_BYTES: usize = 512 * 1024;

/// `limit` 缺省值（按**行**）。512 KiB 封顶之下，一次交 200 行既贴合「先看一屏日志」
/// 的直觉，也不会让模型在第一页就吞下整份文件。
pub const DEFAULT_LINE_LIMIT: usize = 200;

/// 二进制判定的头部窗口：NUL 一旦出现在二进制里必然挤在开头几 KiB（编辑器与 `file`
/// 命令都是这条启发式），窗口之外的字节不值得为它多扫一遍。
const BINARY_HEAD_WINDOW: usize = 8 * 1024;

/// NUL 字节启发式。**只认 NUL**：中文的 UTF-8 字节大量落在 ASCII 之外，若按「非 ASCII
/// 即二进制」判，正经的中文文本会被误杀（`looks_binary_flags_nul_and_passes_utf8_chinese`
/// 钉住这条前提，否则 Step 3 的 GBK 分支根本走不到解码那一步）。
pub fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// 可选的非负整数入参（**按行计**）。缺省与显式 `null` 都按「没给」处理（模型常拿
/// `null` 填默认值）；给了但形状不对一律 `BadInput` 而不是静默按默认值跑 —— 静默之下
/// 模型以为自己的 offset 生效了，下一页它会读到重复内容（与 fetch.rs 同一口径，
/// 单位与文案是本工具自己的）。
fn optional_line(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(n as usize)),
            None => Err(ToolError::BadInput(format!(
                "{key} 必须是非负整数（按行计），实得 {v}"
            ))),
        },
    }
}

/// `read_file`：只读 roots 内的 UTF-8 文本文件。`Risk::Read` —— spec 5.1「内置工具
/// 全为 Read ⇒ 全程零弹窗」对这把工具同样成立的前提是 containment 门真的关得住，
/// 所以上面那段二次 canonicalize 不是装饰。
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "read_file".to_string(),
            description: "读取用户配置的根目录（roots）内的 UTF-8 文本文件，只读、只接受绝对\
                          路径；roots 之外、经符号链接/junction 解析后落到 roots 之外的路径一律\
                          拒绝。分页按【行】计，不是字符、不是字节：`offset` 是起始行号（从 0 \
                          开始），`limit` 是这一页最多交多少行（默认 200）；被截断时末尾带\
                          「[已截断，共 N 行，续页请填 offset=M]」，续页就填那个 M（与 `fetch_url` \
                          的按字符分页是两种单位，别混用）。含 NUL 的二进制文件、非 UTF-8 文本、\
                          超过 524288 字节的文件都会返回一条说明原因的错误，而不是乱码。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "绝对路径，必须位于某个用户配置的 root 内（按符号链接/junction 解析后判定）"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "从第几行开始交（按行、从 0 起计），默认 0"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "这一页最多交多少行（按行计），默认 200"
                    }
                },
                "required": ["path"]
            }),
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // 注册表也查一次取消（R53）；这里是工具侧那一次，为了在**碰文件系统之前**停下。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let raw = input
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::BadInput("read_file 需要非空字符串参数 path（绝对路径）".to_string())
            })?;
        let offset = optional_line(&input, "offset")?.unwrap_or(0);
        // limit=0 交不出任何行，与其报错不如按最小可用值走（模型真想要 0 行时，
        // 「空页 + 截断标记」也仍然是真话）。
        let limit = optional_line(&input, "limit")?
            .unwrap_or(DEFAULT_LINE_LIMIT)
            .max(1);

        let resolved = ctx.resolve_within_roots(raw)?;

        // 二次 canonicalize + 交回同一道闸复验：理由见文件头。**顺序是判据的一部分** ——
        // 这一步必须发生在任何 open/read 之前，否则「最后一段是链接」的逃逸就靠
        // open 失败来兜底，而那是个随文件系统心情变化的错误。
        let real = resolved.canonicalize().map_err(|e| {
            ToolError::Failed(format!(
                "文件不存在或路径无法解析: {}: {e}",
                resolved.display()
            ))
        })?;
        let guarded = ctx.resolve_within_roots(&real.to_string_lossy())?;

        let meta = std::fs::metadata(&guarded)
            .map_err(|e| ToolError::Failed(format!("stat 失败: {}: {e}", guarded.display())))?;
        if !meta.is_file() {
            return Err(ToolError::Failed(format!(
                "不是普通文件（目录或特殊文件）: {}",
                guarded.display()
            )));
        }
        if meta.len() > MAX_BYTES as u64 {
            return Err(ToolError::Failed(format!(
                "文件超大：{} 字节，超过上限 {MAX_BYTES} 字节。用 offset/limit 按行分页读取，或换一个更小的文件。",
                meta.len()
            )));
        }

        // 体积闸的兜底臂：stat 与 read 之间文件还在长（不是原子快照），所以**边读边卡**
        // —— take 到 MAX_BYTES+1，真多出那一个字节就按超大拒，缓冲区无论 raced 成什么样
        // 都封顶在 512 KiB + 1（与 fetch.rs 的 `guarded_push` 同一个「收多少卡多少」取向）。
        let mut bytes: Vec<u8> = Vec::new();
        std::fs::File::open(&guarded)
            .map_err(|e| ToolError::Failed(format!("打开失败: {}: {e}", guarded.display())))?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ToolError::Failed(format!("读取失败: {}: {e}", guarded.display())))?;
        if bytes.len() > MAX_BYTES {
            return Err(ToolError::Failed(format!(
                "文件超大：读到的字节超过了上限 {MAX_BYTES} 字节（stat 之后还在长）。用 offset/limit 按行分页读取。"
            )));
        }

        if looks_binary(&bytes[..bytes.len().min(BINARY_HEAD_WINDOW)]) {
            return Err(ToolError::Failed(format!(
                "文件疑似二进制（含 NUL 字节），不作为乱码交给模型: {}",
                guarded.display()
            )));
        }
        // 不做 from_utf8_lossy：U+FFFD 会被模型当真实内容引用（fetch.rs R265 同一条理由），
        // 报错让人去转码，比交一堆替换符诚实。
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::Failed(format!(
                "文件不是合法 UTF-8 文本，不做静默替换（模型会把 U+FFFD 当真实内容读）: {}；先转成 UTF-8（如 iconv -f GBK -t UTF-8）再来读。",
                guarded.display()
            ))
        })?;

        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if total == 0 {
            // 交空串等于让模型自己猜「是空文件还是没读到」，单独一句（同 slice_page 口径）。
            return Ok(ToolOutput::text("[文件为空，没有可读文本]"));
        }
        if offset >= total {
            return Ok(ToolOutput::text(format!(
                "[offset {offset} 超出范围：文件共 {total} 行]"
            )));
        }
        let end = (offset + limit).min(total);
        let page = lines[offset..end].join("\n");
        if end < total {
            // 标记里直接给出续页的 offset：让模型自己算 = 多一次算错就白烧一个往返。
            Ok(ToolOutput::text(format!(
                "{page}\n[已截断，共 {total} 行，续页请填 offset={end}]"
            )))
        } else {
            // 取到末尾不挂标记，否则模型以为还有下文。
            Ok(ToolOutput::text(page))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolCtx;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use tokio_util::sync::CancellationToken;

    // `ToolCtx` 字段全 pub、registry.rs:33 自己就是字面量构造；registry 的
    // `ctx_with_roots` 住在那个模块的私有 tests 里，本文件够不着 ⇒ 这里自搭一份，
    // 不为了测试把私有辅助改成 pub。`reqwest::Client::new()` 不联网。
    fn test_ctx(roots: Vec<PathBuf>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
        }
    }

    /// 临时 root。`std::mem::forget` 是为了让目录活到测试收尾：TempDir 的 Drop 若
    /// 提前清扫，断言里那条路径在盘上已经不存在了。目录在每条测试末尾
    /// `remove_dir_all(&dir).ok()` 显式删（registry 测试同一口径）。
    fn temp_root() -> PathBuf {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let root = dir.path().to_path_buf();
        std::mem::forget(dir);
        root
    }

    fn temp_root_with(name: &str, bytes: &[u8]) -> PathBuf {
        let root = temp_root();
        std::fs::write(root.join(name), bytes).unwrap();
        root
    }

    fn write(root: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(root.join(name), bytes).unwrap();
    }

    fn p(root: &Path, name: &str) -> String {
        root.join(name).to_string_lossy().to_string()
    }

    /// R275：Windows **文件**符号链接要开发者模式（本机实测 `CreateSymbolicLink`
    /// 报「没有足够的权限」），但目录 junction（`mklink /J`）标准用户就能建，
    /// 且同为 reparse point —— canonicalize 对它走同一条展开路径，所以逃逸测试
    /// 用 junction 顶上，不拿「建不了链接」当通过。
    /// `mklink` 是 cmd 内建：参数走数组、绝不拼命令串（路径里的空格与 `&`
    /// 拼进字符串就是注入面）。
    #[cfg(windows)]
    fn make_junction(link: &Path, target_dir: &Path) {
        let out = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.display().to_string(),
                &target_dir.display().to_string(),
            ])
            .output()
            .expect("启动 cmd.exe 建 junction 失败");
        assert!(
            out.status.success(),
            "建 junction 失败 {} -> {}: {}",
            link.display(),
            target_dir.display(),
            String::from_utf8_lossy(&out.stdout)
        );
    }

    // ———————— Step 1：链接逃逸必须拒、root 本身是链接必须放 ————————

    #[cfg(windows)]
    #[tokio::test]
    async fn a_symlink_pointing_outside_the_root_is_denied() {
        // brief 原夹具是 symlink_file（文件级链接），本机权限建不了（见 make_junction
        // 注释的实测结果）⇒ 按 R275 用 junction 重测：secret 放 root 外目录里，
        // root 内的链接指向该目录。canonicalize 展开的正是这种 reparse point。
        let dir = temp_root();
        let root = dir.join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        write(&outside, "secret.txt", b"do-not-read");
        make_junction(&root.join("link_dir"), &outside);

        let ctx = test_ctx(vec![root.clone()]);
        let e = ReadFileTool
            .execute(
                json!({"path": p(&root.join("link_dir"), "secret.txt")}),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("root") || e.contains("越界"),
            "符号链接逃逸必须被拒，实得 {e}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_as_the_final_component_is_denied_before_any_read() {
        // `resolve_within_roots` 只展开**父目录**里的链接，最后一段自己若是
        // reparse point 它管不到。判据是「必须报越界」而不是「读它总会失败的
        // 文件系统错误」——后者说明闸门放行了、只是靠 open 出错兜底，
        // 换成能读的形态（文件级链接）就真的读走了 root 外的内容。
        let dir = temp_root();
        let root = dir.join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        write(&outside, "secret.txt", b"do-not-read");
        make_junction(&root.join("peek"), &outside);

        let ctx = test_ctx(vec![root.clone()]);
        let e = ReadFileTool
            .execute(json!({"path": p(&root, "peek")}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("越界"),
            "最后一段是 junction 时必须在读之前就被 containment 门拦下并报越界，实得 {e}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_root_that_is_itself_a_junction_still_reads_its_files() {
        // 判据的另一头（brief Step 1「要两头都钉住」）：root 自己是指向别处的
        // junction 时，root「里面」的正常文件必须能读 —— 整棵树被拒是过度收紧。
        let dir = temp_root();
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        write(&target, "hello.txt", "链接根里的内容".as_bytes());
        let jroot = dir.join("jroot");
        make_junction(&jroot, &target);

        let ctx = test_ctx(vec![jroot.clone()]);
        let out = ReadFileTool
            .execute(json!({"path": p(&jroot, "hello.txt")}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        assert!(
            text.contains("链接根里的内容"),
            "root 是 junction 时正常读取必须放行，实得 {text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ———————— Step 2：跨盘符与 `..` ————————

    #[tokio::test]
    async fn absolute_paths_on_other_drives_and_dotdot_are_denied() {
        let root = temp_root_with("a.txt", b"ok");
        let ctx = test_ctx(vec![root.clone()]);
        for path in [
            "C:\\Windows\\win.ini",
            "\\\\nas\\share\\x",
            "D:\\other\\a.txt",
        ] {
            assert!(
                ReadFileTool
                    .execute(json!({"path": path}), &ctx)
                    .await
                    .is_err(),
                "{path} 必须被拒"
            );
        }
        // `..` 这条依赖 canonicalize 而不是字符串检查：`a/../../b` 会跳出 root。
        let tricky = root.join("sub").join("..").join("..").join("escape.txt");
        assert!(
            ReadFileTool
                .execute(json!({"path": tricky.to_string_lossy()}), &ctx)
                .await
                .is_err(),
            "带 .. 的穿越路径必须被拒"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ———————— Step 3：二进制 / 超大 / 非 UTF-8 都要说为什么 ————————

    #[tokio::test]
    async fn binary_oversize_and_non_utf8_all_say_why() {
        let root = temp_root();
        write(&root, "blob.bin", &[0u8, 1, 2, 255, 0]);
        write(&root, "big.txt", &vec![b'a'; MAX_BYTES + 1]);
        // 「中文」的 GBK 字节，写死而不依赖本机 codepage（brief 原样；
        // 它的 UTF-8 表示同样能过 looks_binary，测的就是 from_utf8 那一步）。
        write(&root, "gbk.txt", &[0xD6, 0xD0, 0xCE, 0xC4]);
        let ctx = test_ctx(vec![root.clone()]);

        let e1 = ReadFileTool
            .execute(json!({"path": p(&root, "blob.bin")}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(e1.contains("二进制"), "{e1}");

        let e2 = ReadFileTool
            .execute(json!({"path": p(&root, "big.txt")}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e2.contains("524288") && e2.contains("offset"),
            "超大必须报上限并指出可用 offset 分页，实得 {e2}"
        );

        let e3 = ReadFileTool
            .execute(json!({"path": p(&root, "gbk.txt")}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e3.contains("UTF-8"),
            "非 UTF-8 必须明说而不是静默替换成 U+FFFD（模型会把 replacement 字符当内容解读）：{e3}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ———————— 正常路径与分页单位（Step 4：offset/limit 按**行**） ————————

    #[tokio::test]
    async fn a_plain_text_file_inside_the_root_is_returned() {
        let root = temp_root();
        write(&root, "note.txt", "第一行\n第二行\n第三行\n".as_bytes());
        let out = ReadFileTool
            .execute(
                json!({"path": p(&root, "note.txt")}),
                &test_ctx(vec![root.clone()]),
            )
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        // 逐字相等而不是 contains：头部装饰、行号、截断标记都不在承诺里。
        assert_eq!(text, "第一行\n第二行\n第三行", "读全时输出必须就是行本身");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn offset_and_limit_page_by_lines_and_say_the_total() {
        let root = temp_root();
        let body: String = (1..=5).map(|i| format!("line-{i}\n")).collect();
        write(&root, "five.txt", body.as_bytes());
        write(&root, "empty.txt", b"");
        let ctx = test_ctx(vec![root.clone()]);

        let out = ReadFileTool
            .execute(
                json!({"path": p(&root, "five.txt"), "offset": 1, "limit": 2}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        assert_eq!(
            text, "line-2\nline-3\n[已截断，共 5 行，续页请填 offset=3]",
            "截断标记必须自带续页 offset，模型不必自己算"
        );

        // 取到末尾不挂标记（否则模型以为还有）。
        let out = ReadFileTool
            .execute(json!({"path": p(&root, "five.txt"), "offset": 3}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        assert_eq!(text, "line-4\nline-5", "末页逐字：{text}");

        // offset 越界单独一句并带上总行数；空文件也要说实话。
        let out = ReadFileTool
            .execute(json!({"path": p(&root, "five.txt"), "offset": 9}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        assert_eq!(
            text, "[offset 9 超出范围：文件共 5 行]",
            "越界句不对：{text}"
        );

        let out = ReadFileTool
            .execute(json!({"path": p(&root, "empty.txt")}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        assert_eq!(text, "[文件为空，没有可读文本]", "空文件不能悄悄交空串");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn looks_binary_flags_nul_and_passes_utf8_chinese() {
        // 「中文的 UTF-8 字节不能被误判成二进制」是钉死的前提：判据若含「非 ASCII
        // 即二进制」，上面 gbk.txt 那条的 GBK 分支就永远测不到 from_utf8 那一步。
        assert!(looks_binary(&[0u8, 1, 2, 255, 0]), "含 NUL 必须判二进制");
        assert!(!looks_binary("中文文本，不含 NUL。".as_bytes()));
        assert!(!looks_binary(b""));
    }

    // ———————— 工具声明与入参形状 ————————

    #[test]
    fn the_tool_meta_is_read_file_read_with_the_pinned_schema() {
        let m = ReadFileTool.meta();
        assert_eq!(m.name, "read_file");
        assert_eq!(m.risk, Risk::Read);
        assert!(
            m.description.contains("行"),
            "分页单位必须写进描述（fetch_url 按字符、本工具按行，模型只看得到描述）：{}",
            m.description
        );
        let s = m.input_schema;
        assert_eq!(s["type"], json!("object"));
        assert_eq!(s["required"], json!(["path"]), "只有 path 是必填");
        let props = s["properties"].as_object().expect("properties 必须是对象");
        assert_eq!(props.len(), 3, "只声明 path/offset/limit：{props:?}");
        assert_eq!(props["path"]["type"], json!("string"));
        assert_eq!(props["offset"]["type"], json!("integer"));
        assert_eq!(props["limit"]["type"], json!("integer"));
    }

    #[tokio::test]
    async fn a_missing_or_non_string_path_is_bad_input() {
        let ctx = test_ctx(vec![]);
        for input in [
            json!({}),
            json!({"path": 42}),
            json!({"path": ""}),
            json!({"path": "  "}),
        ] {
            let e = ReadFileTool.execute(input.clone(), &ctx).await.unwrap_err();
            assert!(
                matches!(e, ToolError::BadInput(_)),
                "{input} 必须 BadInput，实得 {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_non_integer_offset_or_limit_is_bad_input_not_silently_defaulted() {
        // 静默按默认值跑 = 模型以为自己的 offset 生效了，下一页会读到重复内容（同 fetch.rs 口径）。
        let root = temp_root_with("a.txt", b"ok");
        let ctx = test_ctx(vec![root.clone()]);
        for input in [
            json!({"path": p(&root, "a.txt"), "offset": -1}),
            json!({"path": p(&root, "a.txt"), "offset": "3"}),
            json!({"path": p(&root, "a.txt"), "limit": 1.5}),
        ] {
            let e = ReadFileTool.execute(input.clone(), &ctx).await.unwrap_err();
            assert!(
                matches!(&e, ToolError::BadInput(m) if m.contains("整数")),
                "{input} 必须报「必须是非负整数（按行计）」，实得 {e:?}"
            );
        }
        // 显式 null 按「没给」处理（模型常拿 null 填默认值）。
        assert!(
            ReadFileTool
                .execute(json!({"path": p(&root, "a.txt"), "offset": null}), &ctx)
                .await
                .is_ok()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_cancelled_ctx_stops_before_any_read() {
        let root = temp_root_with("a.txt", b"ok");
        let ctx = test_ctx(vec![root.clone()]);
        ctx.cancel.cancel();
        let e = ReadFileTool
            .execute(json!({"path": p(&root, "a.txt")}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(e, ToolError::Cancelled), "{e:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
