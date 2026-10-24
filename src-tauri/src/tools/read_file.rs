// Plan B 的第二把生产工具：`read_file` —— 只在用户配置的 roots 内做只读文本读取。
//
// 本任务**不注册进生产**（`ipc.rs` 那张空 `ToolRegistry` 一字不动，注册与 UI 卡片
// 是 Task 27 的活）；交付的是工具本体 + 测试。
//
// # containment 归闸门，不归本文件（R279）
//
// 首轮这里做过一次「把闸门结果整条再 canonicalize、交回闸门复验」—— 判据是真的（父目录-only
// canonicalize 确实放不住「末段自己是指向 root 外的 junction」），归属是错的：同一条安全判据
// 长出两个强度版本，下一个消费者（`write_file`、任何只读工具）不记得抄一遍就能穿过末段
// junction 读写到 root 外。修复轮 1 把它搬进 `registry.rs::resolve_within_roots`（规则 5），
// 本文件打开的就是闸门给的那一份 `PathBuf`，**不留第二把尺子**。
//
// # 这道门能防什么、防不住什么（R281，账面诚实）
//
// * **能防 reparse point 逃逸**：符号链接 / junction 会被 `canonicalize()` 展开，落到 root
//   外就报越界。目录 junction 标准用户可建、已实测；文件级符号链接本机建不了（非开发者模式，
//   实测报「没有足够的权限」），同形状由目录 junction 钉住，那一头标 `未实测`。
// * **防不住硬链接**（`mklink /H`，标准用户就能建）：硬链接不是 reparse point，canonicalize
//   看不出任何异常 —— 它在 root 里就是一条正常的名字，两把 canonicalize 闸门都会放行。
//   状态记为 `未实测（硬链接形状）`，且**不**为它写一条会自我实现的测试。它不在本轮的威胁
//   模型里：模型是「工具不越界」，不是「root 里可能有用户自己放的恶意文件」—— 硬链接指向的
//   内容本来就在这位用户自己的机器上、他自己读得到。真要堵需要 `OpenFileById` + 句柄反查
//   （`GetFileInformationByHandle` 比对卷号 + 文件 ID）那一套：为一条防「用户已经拥有的东西」
//   的防线多引一次句柄级 API 和一份 ID 台账，代价大于收益（R281 的取向）。
//
// # 分页单位：按行；体积闸管的也是「这一页」（R278）
//
// 与 `fetch_url` 的按字符分页刻意不同（读日志/源码的日常直觉是「第几行」），两处各自在
// `description` 里写清单位，防止模型混用。
//
// 首轮的 `metadata.len() > MAX_BYTES ⇒ 整文件拒读` 被评审判成「建议不可达」：拒读信息教模型
// 「用 offset/limit 分页」，而照提示改成 `limit=50` 重试拿到的是**逐字相同**的错误 ⇒ 那一页
// 永远读不出来，模型只会原地循环。现在闸管的是**交出去的那一页**：按行累计，到上限就停在当前
// 行边界（绝不切进一行中间），提示行给出实测的已交出行数与下一页的 offset。
//
// 唯一的例外是「一行本身就超过上限」（压缩过的 JS、一行几 MB 的 log）：不许整页空、也不许把
// 整行吞掉 ⇒ 那一行只能按**字符**截到上限。这里不得不用字符：按字节切会落在 UTF-8 序列中间，
// 要么 panic（`&s[..n]` 的经典事故），要么交出半个字符被模型当内容读；而这一行本来就长到一页
// 装不完，「切在最后一个完整字符上」是唯一说得过去的形状。`fetch_url` 的按字符分页
// （`slice_page` → `crate::text::head_chars`）恰好在这个边界处与本工具汇合 —— 汇合只有这一处，
// 页的单位仍然是行。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;

/// 单次交付的字节上限：**这一页交出去的字节总数**（正文 + 末尾的提示行）不得超过它。
/// 它管的不是「文件有多大」—— 文件可以比它大得多，那是分页要解决的事（R278）。
pub const MAX_BYTES: usize = 512 * 1024;

/// 提示行也要进模型的上下文，所以正文先把这段余量留给「末尾那一行提示 + 分隔它的换行符」，
/// `text.len() <= MAX_BYTES` 这条对模型可验证的承诺才不会因为提示行本身被挤破。
/// 实测两条提示行的长度：行边界那条 80 字节（数字位数还会涨，留的是当前值），
/// 单行截断那条 203 字节（见 `one_giant_line_still_delivers_bytes_instead_of_nothing`）
/// ⇒ 512 的余量是当前最长一条的 2.5 倍，够到 offset 涨到六七位数。
const MARKER_RESERVE: usize = 512;

/// 一页正文的字节预算 = 上限 - 提示行余量。累计到它就停在**行边界**上；
/// 单行超过它时按字符截（见文件头那段「唯一的例外」）。
const CONTENT_BUDGET: usize = MAX_BYTES - MARKER_RESERVE;

/// `limit` 缺省值（按**行**）。512 KiB 封顶之下，一次交 200 行既贴合「先看一屏日志」
/// 的直觉，也不会让模型在第一页就吞下整份文件。
pub const DEFAULT_LINE_LIMIT: usize = 200;

/// 二进制判定的头部窗口：NUL 一旦出现在二进制里必然挤在开头几 KiB（编辑器与 `file`
/// 命令都是这条启发式），窗口之外的字节不值得为它多扫一遍。
const BINARY_HEAD_WINDOW: usize = 8 * 1024;

/// NUL 字节启发式。**只认 NUL**：中文的 UTF-8 字节大量落在 ASCII 之外，若按「非 ASCII
/// 即二进制」判，正经的中文文本会被误杀（`looks_binary_flags_nul_and_passes_utf8_chinese`
/// 钉住这条前提，否则 Step 3 的 GBK 分支根本走不到解码那一步）。
///
/// 判据不动，但账面的边界要写清（R281）—— 已知这三条**不在**这条启发式里：
/// * NUL 出现在 8 KiB 之后 ⇒ 这一页会被当文本交出去（窗口外的字节我们不扫）。
/// * UTF-16/UTF-32 文本几乎必然含 NUL ⇒ 被判成「二进制」：结果是对的（那确实不是
///   模型能读的文本），词是错的（它不是二进制）。
/// * 控制字符与 ESC 序列（清屏、改色）不做清理 ⇒ 会原样进上下文。
///
/// 为什么停在这里：MVP 要的是「不要把几十万字节的乱码塞进上下文」，不是「识别所有编码」——
/// 后者是另一条功能的活（编码探测 + 转码，要有自己的判据和测试），拿一条启发式去冒充它
/// 只会让下一条真需求无处安放。
pub fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// scanner 交出的一行元数据：字节留在调用方的缓冲里，避免每行一次分配。
struct Scanned {
    /// 该行原始字节 > `CONTENT_BUDGET` ⇒ 缓冲里只有前缀，剩下的字节被丢弃式读过了。
    over_cap: bool,
    /// 行尾真的有一个 `\n`（`str::lines()` 只在这种情况下才吃掉 `\r`，这里逐字同形）。
    had_newline: bool,
}

/// 按 `\n` 切行、**每行最多缓冲 `CONTENT_BUDGET` 字节**的极简 scanner。
///
/// 为什么不用现成的 `BufRead::lines()`：它把整行吸进内存，而本轮的判据恰恰是「文件可以比
/// 一页大得多」—— 一行 200 MB 的压缩 JS 就会把 200 MB 装进进程，而首轮那把
/// `take(MAX_BYTES + 1)` 的内存闸正是为堵这个而存在的。搬走体积闸不能顺手把内存闸也搬走，
/// 所以改成「行可以无限长、缓冲封顶」：攒够一份前缀就把该行的剩余字节丢弃式读过（只找 `\n`），
/// 于是内存天花板回到「一行前缀 + 一页」。
struct LineScanner<R> {
    src: R,
    /// 只为了把读错误写成「哪条路径读不下去」，与首轮的文案一致。
    path: std::path::PathBuf,
}

impl<R: std::io::BufRead> LineScanner<R> {
    fn new(src: R, path: std::path::PathBuf) -> Self {
        Self { src, path }
    }

    /// 读下一行写进 `out`（先清空），EOF 且没有残余字节时返回 `None`。
    fn next_line(&mut self, out: &mut Vec<u8>) -> Result<Option<Scanned>, ToolError> {
        out.clear();
        let mut over_cap = false;
        let mut saw_any = false;
        // 只借 `path` 这一个字段（不是整个 self），好让 `fill_buf` 的可变借用与它共存。
        let path = &self.path;
        loop {
            let chunk = self.src.fill_buf().map_err(|e| scan_read_failed(path, e))?;
            if chunk.is_empty() {
                // EOF：末行没有换行符时也要照样交出去（`str::lines()` 就是这个形状）。
                return Ok(if out.is_empty() && !saw_any {
                    None
                } else {
                    Some(Scanned {
                        over_cap,
                        had_newline: false,
                    })
                });
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if !over_cap {
                        over_cap = push_bounded(out, &chunk[..i]);
                    }
                    self.src.consume(i + 1);
                    return Ok(Some(Scanned {
                        over_cap,
                        had_newline: true,
                    }));
                }
                None => {
                    if !over_cap {
                        over_cap = push_bounded(out, chunk);
                    }
                    let n = chunk.len();
                    self.src.consume(n);
                    saw_any = true;
                }
            }
        }
    }

    /// 数到 EOF 还剩多少行（**不**缓冲、不解码、每块查一次取消）：分页提示里的「共 N 行」
    /// 要的是实测总数，而我们已经不再把整个文件读进内存了。
    fn count_lines_to_eof(
        &mut self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<usize, ToolError> {
        let mut newlines = 0usize;
        let mut trailing = false;
        let path = &self.path;
        loop {
            if cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let chunk = self.src.fill_buf().map_err(|e| scan_read_failed(path, e))?;
            if chunk.is_empty() {
                break;
            }
            for &b in chunk {
                if b == b'\n' {
                    newlines += 1;
                    trailing = false;
                } else {
                    trailing = true;
                }
            }
            let n = chunk.len();
            self.src.consume(n);
        }
        Ok(newlines + usize::from(trailing))
    }
}

/// 读不继续（IO 错误）要说清是哪条路径，与首轮的文案一致。
fn scan_read_failed(path: &Path, e: std::io::Error) -> ToolError {
    ToolError::Failed(format!("读取失败: {}: {e}", path.display()))
}

/// 追加到行缓冲，封顶 `CONTENT_BUDGET`：返回 `true` 表示这行还有字节被丢掉（单行超额）。
fn push_bounded(out: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let room = CONTENT_BUDGET.saturating_sub(out.len());
    if bytes.len() <= room {
        out.extend_from_slice(bytes);
        false
    } else {
        out.extend_from_slice(&bytes[..room]);
        true
    }
}

/// 把一行的原始字节解码成交出去的文本切片。两条边界都对着 `str::lines()` 的旧行为校准：
/// * `\r` 只在**真有换行结尾**时才吃掉 —— 末行没有 `\n` 时 `lines()` 也留着那个 `\r`。
/// * `over_cap` 时上限可能正好切在一个多字节字符中间：那是**我们自己切的**，退回最后一个
///   完整字符即可（不是 lossy 替换 —— 被退掉的字节本来就不属于本页，而交 U+FFFD 给模型会被
///   当真实内容引用，R265 那条理由在这里同样成立）。非 `over_cap` 的解码失败才是真坏文件。
fn decode_line<'a>(
    bytes: &'a [u8],
    over_cap: bool,
    had_newline: bool,
    path: &Path,
) -> Result<&'a str, ToolError> {
    let mut raw = bytes;
    if had_newline && !over_cap {
        raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    }
    match std::str::from_utf8(raw) {
        Ok(s) => Ok(s),
        Err(e) if over_cap => {
            let keep = e.valid_up_to();
            // `valid_up_to` 定义上就是有效前缀，这里不可能再失败；退而给空串也不改变结论。
            Ok(std::str::from_utf8(&raw[..keep]).unwrap_or_default())
        }
        Err(_) => Err(ToolError::Failed(format!(
            "文件不是合法 UTF-8 文本，不做静默替换（模型会把 U+FFFD 当真实内容读）: {}；先转成 UTF-8（如 iconv -f GBK -t UTF-8）再来读。",
            path.display()
        ))),
    }
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

/// `read_file`：只读 roots 内的 UTF-8 文本文件。`Risk::Read` —— spec 5.1「内置工具全为 Read
/// ⇒ 全程零弹窗」对这把工具成立的前提是 containment 门**关得住 reparse point 逃逸**
/// （`registry.rs::resolve_within_roots` 规则 2 + 规则 5），而**不是**「root 里没有别的东西」：
/// 用户自己建的硬链接在两把 canonicalize 闸门下都是正常的名字，见文件头那段账面诚实（R281）。
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
                          的按字符分页是两种单位，别混用）。文件比一页大不是错误、不会被拒读：\
                          一页最多交 524288 字节（含末尾那行提示），到上限就停在行边界并挂\
                          「[本页已达 524288 字节上限，已交出 N 行，从 offset=M 继续]」，续页同样填\
                          那个 M；只有一行本身就超过上限时才会按字符截断，并在提示里说明那一行\
                          没有读完。含 NUL 的二进制文件、非 UTF-8 文本会返回一条说明原因的错误，\
                          而不是乱码。"
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

        // 闸门给的那一份 PathBuf 就是打开用的路径：末段 reparse 的复验已经搬进
        // `resolve_within_roots`（R279），本文件不留第二把尺子。**顺序仍是判据的一部分** ——
        // 越界必须在任何 open/read 之前定下来，否则「最后一段是链接」的逃逸就靠 open 失败兜底，
        // 而那是个随文件系统心情变化的错误。
        let guarded = ctx.resolve_within_roots(raw)?;

        let meta = std::fs::metadata(&guarded)
            .map_err(|e| ToolError::Failed(format!("stat 失败: {}: {e}", guarded.display())))?;
        if !meta.is_file() {
            return Err(ToolError::Failed(format!(
                "不是普通文件（目录或特殊文件）: {}",
                guarded.display()
            )));
        }
        // 这里**不再**按 `meta.len()` 拒读（R278）：文件总大小与「这一页交多少字节」是两件事，
        // 拿前者拒读会让那条「用 offset/limit 分页」的建议不可达。体积闸搬到下面按页累计。
        let file = std::fs::File::open(&guarded)
            .map_err(|e| ToolError::Failed(format!("打开失败: {}: {e}", guarded.display())))?;
        // 64 KiB 的读块：行边界靠 `fill_buf` 找，块太小会让逐行扫描退化成一次一次系统调用。
        let mut scanner = LineScanner::new(
            std::io::BufReader::with_capacity(64 * 1024, file),
            guarded.clone(),
        );

        let mut page = String::new();
        // 字节闸的提示行；`None` 表示这一页是被行上限或文件末尾收尾的。
        let mut note: Option<String> = None;
        let mut delivered = 0usize;
        let mut line_buf: Vec<u8> = Vec::new();
        let mut head: Vec<u8> = Vec::new();
        // 已经数过的行数（0 起计的行号就是下一个要处理的行）。
        let mut index = 0usize;
        // 只有扫到 EOF（或被行上限收尾，那时顺手数到 EOF）才知道总行数；
        // 被字节闸提前截断的那一页**不装知道**。
        let mut total: Option<usize> = None;

        loop {
            let Some(scanned) = scanner.next_line(&mut line_buf)? else {
                total = Some(index);
                break;
            };
            // 数行、判二进制都要从文件头开始，所以窗口之前的行也读过去了 —— 但**不解码**，
            // 也就不会因为页外的坏字节拒绝这一页（坏字节在哪一页，才由哪一页负责报错）。
            if head.len() < BINARY_HEAD_WINDOW {
                // 窗口按**正文字节**累计（换行符不进 `line_buf`），与首轮那句「文件前 8 KiB」
                // 差的就是那几个换行字节 —— 启发式的意图没变：NUL 若在二进制里，必然挤在这
                // 开头几 KiB 内，差几个字节不会把「像二进制」变成「像文本」。
                let take = (BINARY_HEAD_WINDOW - head.len()).min(line_buf.len());
                head.extend_from_slice(&line_buf[..take]);
                // 二进制判定必须在解码之前：`[0,1,2,255,0]` 那种夹具既含 NUL 又含非法 UTF-8，
                // 先解码就会报成「非 UTF-8」而不是「二进制」，两条承诺当场错位。
                if looks_binary(&head) {
                    return Err(ToolError::Failed(format!(
                        "文件疑似二进制（含 NUL 字节），不作为乱码交给模型: {}",
                        guarded.display()
                    )));
                }
            }
            let line_no = index;
            index += 1;
            if ctx.cancel.is_cancelled() {
                // 大 offset / 大文件时这一趟是纯扫描，得能停得下来（注册表那侧也查一次，R53）。
                return Err(ToolError::Cancelled);
            }
            if line_no < offset {
                continue;
            }
            if line_no >= offset.saturating_add(limit) {
                // 行上限到了：这一行已在手里，所以总数 = 已数到的行 + 剩下的行。
                // `saturating_add` 而不是 `offset + limit`：模型给个巨大的 offset 时不该 panic。
                total = Some(line_no + 1 + scanner.count_lines_to_eof(&ctx.cancel)?);
                break;
            }
            let text = decode_line(&line_buf, scanned.over_cap, scanned.had_newline, &guarded)?;
            if scanned.over_cap {
                if delivered > 0 {
                    // 单行超额、而本页已经交了别的行 ⇒ 停在**行边界**，把这一行整个留给下一页
                    // 的第一行去按字符截（那里才有「本页还没交东西」的前提，见文件头）。
                    note = Some(byte_marker(delivered, line_no));
                    break;
                }
                // 本页第一行就超过上限：按字符截到上限交出前缀 + 说明这一行没读完、它的行号。
                let got = text.chars().count();
                note = Some(format!(
                    "[第 {} 行（offset={line_no}）单行就超过 {MAX_BYTES} 字节上限：已按字符截到上限，\
                     只交出该行前 {got} 字，这一行没有读完（分页按行计，续页拿不到本行剩下的字节）]",
                    line_no + 1
                ));
                page.push_str(text);
                break;
            }
            // 加上这一行（含分隔它的换行符）会不会超预算：超就停在当前行边界，绝不切进行中间。
            let extra = if delivered == 0 {
                text.len()
            } else {
                text.len() + 1
            };
            if page.len() + extra > CONTENT_BUDGET {
                note = Some(byte_marker(delivered, line_no));
                break;
            }
            if delivered > 0 {
                page.push('\n');
            }
            page.push_str(text);
            delivered += 1;
        }

        if let Some(total) = total {
            if total == 0 {
                // 交空串等于让模型自己猜「是空文件还是没读到」，单独一句（同 slice_page 口径）。
                return Ok(ToolOutput::text("[文件为空，没有可读文本]"));
            }
            if offset >= total {
                return Ok(ToolOutput::text(format!(
                    "[offset {offset} 超出范围：文件共 {total} 行]"
                )));
            }
        }
        let text = match note {
            // 字节闸的提示行优先：它比「共 N 行」更紧迫（总行数在这一页压根没数完）。
            Some(note) if page.is_empty() => ToolOutput::text(note),
            Some(note) => ToolOutput::text(format!("{page}\n{note}")),
            None => {
                let end = offset + delivered;
                match total {
                    Some(total) if end < total => ToolOutput::text(format!(
                        "{page}\n[已截断，共 {total} 行，续页请填 offset={end}]"
                    )),
                    // 取到末尾不挂标记，否则模型以为还有下文。
                    _ => ToolOutput::text(page),
                }
            }
        };
        Ok(text)
    }
}

/// 字节闸命中时挂在页尾那一行（R278 的承诺）：`{n}` 是**实测的已交出行数**，`{next}` 是
/// 下一页的 offset —— 直接给出续页值，让模型自己算 = 多一次算错就白烧一个往返。
/// 上限数字用 `{MAX_BYTES}` 插值，524288 这个数仍然必须出现在提示里（首轮的原承诺）。
fn byte_marker(delivered: usize, next_offset: usize) -> String {
    format!("[本页已达 {MAX_BYTES} 字节上限，已交出 {delivered} 行，从 offset={next_offset} 继续]")
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
    // 唯一借出来的是 `registry::drive_of`（`#[cfg(test)] pub(crate)`）：跨盘符夹具的
    // 前提检查两处一模一样，没必要留第二份。
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
            .unwrap_err();
        // R280：这条原来写的是 `contains("root") || contains("越界")`，而夹具目录本身就叫
        // `root` ⇒ 任何带路径的错误都能满足它（near-vacuous）。收成与 registry 同一形状。
        assert!(
            matches!(&e, ToolError::BadInput(m) if m.contains("越界")),
            "符号链接逃逸必须被 containment 门以「越界」拒掉，实得 {e:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_as_the_final_component_is_denied_before_any_read() {
        // 修复轮 1（R279）之后，这道判定长在闸门上（registry 规则 5），本工具不再自己
        // canonicalize 一遍。这条测试因此变成**工具侧的配对**：它证明消费者确实继承了
        // 那道防线，而不是靠自己补的 —— 把闸门里那段摘掉，这条与
        // `the_shared_gate_itself_denies_a_final_component_junction` 会同时变红。
        // 判据仍是「必须报越界」而不是「读它总会失败的文件系统错误」——后者说明闸门
        // 放行了、只是靠 open 出错兜底，换成能读的形态（文件级链接）就真的读走了 root 外的内容。
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
        // R280：夹具全换成**真存在**的东西 —— 第二个 tempdir 里的真文件（跨 root，而不是
        // 跨「不存在的形状」），于是「拒」的原因只能是越界。写死 `C:\Windows\win.ini`
        // 这类依赖本机盘符布局的夹具按裁定禁止。
        let dir = temp_root();
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        write(&root, "a.txt", b"ok");
        let other = dir.join("other");
        std::fs::create_dir(&other).unwrap();
        write(&other, "other.txt", b"not-for-you");
        // `root/sub/../../escape.txt` 词法上从 root 出发、落地在 dir（root 外）——
        // 所以 escape.txt 必须真放在 dir 这一层，那条 `..` 才是「打进来了才被闸门拦下」。
        std::fs::write(dir.join("escape.txt"), b"not-for-you-either").unwrap();
        let ctx = test_ctx(vec![root.clone()]);

        for path in [p(&other, "other.txt"), p(&root, "sub/../../escape.txt")] {
            let e = ReadFileTool
                .execute(json!({"path": path}), &ctx)
                .await
                .unwrap_err();
            assert!(
                matches!(&e, ToolError::BadInput(m) if m.contains("越界")),
                "{path} 必须逐条报越界，实得 {e:?}"
            );
        }

        // 跨盘符那一支照 registry.rs 的取向：从 `current_dir()` 推另一条盘，前提不成立
        // （temp 与仓库同盘）就打印 SKIP，不制造跨机器的假失败。
        let here = std::env::current_dir().unwrap();
        let root_c = root.canonicalize().unwrap();
        let here_c = here.canonicalize().unwrap();
        if crate::tools::registry::drive_of(&root_c) != crate::tools::registry::drive_of(&here_c) {
            let cand = here.join("qwen-do-other-drive.txt");
            let e = ReadFileTool
                .execute(json!({"path": cand.to_string_lossy()}), &ctx)
                .await
                .unwrap_err();
            assert!(
                matches!(&e, ToolError::BadInput(m) if m.contains("越界")),
                "跨盘符绝对路径必须报越界，实得 {e:?}"
            );
        } else {
            println!("SKIP cross_drive: root 与 cwd 同盘符，跨盘符前提不成立");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dotdot_that_would_lexically_escape_is_denied_after_canonicalization() {
        // 这条钉的是「`..` 靠 canonicalize 打赢，而不是靠夹具不存在蒙过」：`root/sub` 真存在、
        // root 外真放一个**可读**文件，路径词法上仍以 root 开头 —— 字符串比较版的闸门会放行它，
        // 于是这条在那种实现下当场变红（变异核验见 task-25 修复轮报告）。
        let dir = temp_root();
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, b"not-yours").unwrap();
        let tricky = root.join("sub").join("..").join("..").join("outside.txt");
        assert!(
            tricky.starts_with(&root),
            "夹具前提：这条路径词法上必须看着像在 root 里（否则测不到穿越）"
        );
        assert!(
            outside.is_file(),
            "夹具前提：root 外那个文件必须真存在且可读"
        );

        let ctx = test_ctx(vec![root.clone()]);
        let e = ReadFileTool
            .execute(json!({"path": tricky.to_string_lossy()}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(&e, ToolError::BadInput(m) if m.contains("越界")),
            "词法上像 root 内、canonicalize 后落到 root 外的路径必须报越界，实得 {e:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
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

        // R278 改判据（理由写在报告里）：这条原来钉的是「超过 524288 字节的文件一律拒读」，
        // 而拒读信息又教模型去分页 —— 分页拿回的是逐字相同的错误，建议不可达。
        // 现在该钉的是「上限这个数字出现在交出去的那一页的提示里」，而且**必须交得出内容**。
        let out = ReadFileTool
            .execute(json!({"path": p(&root, "big.txt")}), &ctx)
            .await
            .expect("超过单页上限的文件必须交出第一页，而不是整文件拒读");
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        assert!(
            text.contains("524288") && text.contains("offset"),
            "提示里要带上限数字与续页指引（原承诺改钉的东西）：{text}"
        );
        assert!(
            text.len() <= MAX_BYTES,
            "交出去的总字节（含提示行）必须仍在闸内，实得 {}",
            text.len()
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

    // ———————— R278：体积闸管「这一页」，不管「整个文件」 ————————

    #[tokio::test]
    async fn a_2mb_log_is_readable_in_pages_instead_of_refused() {
        // 评审的复现（Important-1 + Important-2）：2 MB 日志在 `eee3044` 上拿到
        // 「文件超大……用 offset/limit 按行分页读取」，而照提示翻页拿回的是**逐字相同**的错误
        // ⇒ 那一页永远读不出来，模型只会原地循环。这条钉「建议可达」。
        let root = temp_root();
        let body: String = (0..40_000)
            .map(|i| format!("log-line-{i:06} payload={}\n", "x".repeat(32)))
            .collect();
        assert!(
            body.len() > 2 * 1024 * 1024 && body.len() > MAX_BYTES,
            "夹具前提：这份日志必须比单页上限大（实得 {} 字节）",
            body.len()
        );
        write(&root, "app.log", body.as_bytes());
        let ctx = test_ctx(vec![root.clone()]);

        // 第一页：默认 offset/limit 就必须成功，并带续页提示。
        let out = ReadFileTool
            .execute(json!({"path": p(&root, "app.log")}), &ctx)
            .await
            .expect("2 MB 日志的第一页必须交得出来，而不是拒读");
        let ToolOutput::Text { text: first } = out else {
            panic!("read_file 只产出文本");
        };
        assert!(
            first.contains("[已截断") && first.contains("offset=200"),
            "默认页只交 200 行，必须带给出续页 offset 的提示：{first}"
        );
        assert!(first.contains("log-line-000000"), "第一页要含第 0 行");

        // 照提示翻第二页：读到的是**不同的内容**（不是同一条拒读）。
        let out = ReadFileTool
            .execute(json!({"path": p(&root, "app.log"), "offset": 200}), &ctx)
            .await
            .expect("按提示翻页必须还能读到内容");
        let ToolOutput::Text { text: second } = out else {
            panic!("read_file 只产出文本");
        };
        assert!(
            second.contains("log-line-000200"),
            "第二页要从第 200 行起：{second}"
        );
        assert!(
            !second.contains("log-line-000000"),
            "第二页不得重复第一页的内容：{second}"
        );

        // 把 limit 开大到必然撞上字节闸：闸管的是**交出去的那一页**，停在行边界并给出实测行数。
        let out = ReadFileTool
            .execute(
                json!({"path": p(&root, "app.log"), "limit": 1_000_000}),
                &ctx,
            )
            .await
            .expect("字节闸不得变成拒读");
        let ToolOutput::Text { text: wide } = out else {
            panic!("read_file 只产出文本");
        };
        let at = wide
            .rfind("[本页已达")
            .expect("被字节闸截断时末尾必须挂那一行提示");
        let (page, note) = wide.split_at(at);
        let note = note.trim_start_matches('\n');
        let delivered = page.lines().count();
        assert!(
            note.contains("524288"),
            "上限数字（512*1024 的实测值）必须出现在提示里：{note}"
        );
        assert!(
            note.contains(&format!("已交出 {delivered} 行")),
            "提示里的行数必须是实测的已交出行数：{note} / 实得 {delivered} 行"
        );
        let next: String = note
            .split("offset=")
            .nth(1)
            .expect("提示行必须给出续页的 offset")
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        assert_eq!(
            next,
            delivered.to_string(),
            "续页 offset 必须等于已交出的行数（默认 offset=0），提示与页面要自洽"
        );
        assert!(
            wide.len() <= MAX_BYTES,
            "含提示行在内的整页字节数必须 ≤ 上限，实得 {}",
            wide.len()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn one_giant_line_still_delivers_bytes_instead_of_nothing() {
        // 压缩过的 JS / 一行几 MB 的 log：既不许整页空（那是「读不到」的假话），
        // 也不许把整行吞掉。按字符截到上限，并在提示里说明这一行没读完 + 它的行号。
        let root = temp_root();
        write(&root, "minified.js", &vec![b'x'; 1024 * 1024]);
        let ctx = test_ctx(vec![root.clone()]);
        let out = ReadFileTool
            .execute(json!({"path": p(&root, "minified.js")}), &ctx)
            .await
            .expect("单行超大也要交出这一行的前缀");
        let ToolOutput::Text { text } = out else {
            panic!("read_file 只产出文本");
        };
        let at = text
            .rfind("[第")
            .expect("单行超额的提示行必须挂在页尾（它的行号在提示里）");
        let (page, note) = text.split_at(at);
        let body = page.trim_end_matches('\n');
        assert!(!body.is_empty(), "整页不能是空的");
        assert!(
            body.chars().all(|c| c == 'x') && body.len() < 1024 * 1024,
            "交出的必须是该行被截断的前缀（实得 {} 字节）",
            body.len()
        );
        assert!(
            text.len() <= MAX_BYTES,
            "含提示行在内的字节数必须 ≤ 上限，实得 {}",
            text.len()
        );
        assert!(note.contains("第 1 行"), "提示要带上那一行的行号：{note}");
        assert!(note.contains("524288"), "提示要带上限数字：{note}");
        assert!(
            note.contains("字符"),
            "这一处的截断单位是字符（与 fetch_url 的按字符分页在这里汇合），提示要说清：{note}"
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
