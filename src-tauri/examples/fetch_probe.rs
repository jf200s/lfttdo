//! 风险 7 的判定门探针（非生产代码）：HTML 抽取器交给模型的正文到底够不够读。
//!
//! 为什么要有这个文件：spec 5.5 选的抽取器「够用」是一个**人眼**判断，单元测试证不了
//! （单测只能证明「抽出了文本」，证明不了「抽出的是正文还是导航条」）。所以这里走一遍
//! 生产路径，把每个页面的抽取结果原样交出来看。
//!
//! T24 用它判了 `htmd`（结论：不达标，见 `docs/findings/plan-b-fetch-quality.md`）；
//! T24b 用**同一个探针、同一批页面**复跑换掉之后的 `readability` + `htmd`
//! （修复轮 1 的 R267：定界那一步留下的正文片段喂给 `htmd`，脚本样式在定界时就已经不在
//! 片段里 —— 与 T24 判门的**整页**输入不是同一件事），前后对照写进同一份 findings 的新一节。
//!
//! 用法：
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live
//! cargo run --manifest-path src-tauri/Cargo.toml --example fetch_probe -- --live <URL> …
//! ```
//! 不带 `--live` 时**一个字节都不发出去**，只打印计划要打的页面清单 —— 这条是
//! 「测试不许联网」那同一套纪律的延伸：本文件不会被 `cargo test` 跑，但它必须能在
//! 离线机器上编译、运行，且不给人「随手一跑就联网」的错觉。
//!
//! 打的是**生产路径**：`RedirectWalker::new(SystemResolver, ReqwestTransport)` +
//! `render_markdown`，也就是 `fetch_url` 自己那一条链。换一套 HTTP 栈或换一个解析器
//! 测出来的质量，都不是本工具交付的质量。
//!
//! 六个固定页面种类（brief Step 4 点名的六种形状）：新闻正文 / 博客 / 带大量导航的
//! 门户 / 纯文本文件 / JSON 接口 / SPA 空壳。URL 按「本机实测可达 + UTF-8」挑
//! （bbc / cnn / wikipedia 在这台机器上连不上 —— 不可达不是抽取问题，不能混进判定门），
//! 每个种类另备一条兜底，第一条失败才用第二条。
//!
//! 每个页面打印：状态、content-type、跳数、字节数、**抓取耗时 / 抽取耗时**、
//! 抽取后总字符数、前 400 字原文、末尾 200 字原文，再加三个**代理指标**
//! （链接行占比、长行占比）。代理指标只是把「噪声占比」和「碎片化」这两句人话
//! 折成可复跑的数字；判定门本身仍然是人眼读那 400 字 + 200 字，
//! 结论与依据都写进 `docs/findings/plan-b-fetch-quality.md`。
//!
//! HTML 那一支另有一列**对照**：同一份 HTML 再用「不经过 readability 定界」的
//! `htmd::convert(整页)` 抽一遍 —— 它量的是「readability 这次定界到底替模型挡掉了
//! 多少字符」。对照列**不是**生产路径：生产那一趟是 `render_markdown_with_label`
//! （`render_markdown` 是它的对外包装），两者都不含上面那句未经 readability 定界的整页转换。
use std::sync::Arc;
use std::time::Instant;

use qwen_do_lib::text::head_chars;
use qwen_do_lib::tools::fetch::{
    DEFAULT_MAX_BYTES, RedirectWalker, ReqwestTransport, SystemResolver, USER_AGENT,
    render_markdown,
};

/// 一个页面种类 + 它的候选 URL（第一条失败就用下一条，全部失败才记 `未实测`）。
struct Page {
    kind: &'static str,
    /// 这个种类在判定门里要回答的问题（写进输出，免得 findings 与探针各说一套）。
    ask: &'static str,
    urls: &'static [&'static str],
}

const PAGES: &[Page] = &[
    Page {
        kind: "1 新闻正文",
        ask: "长段落正文是否完整、段落之间是否被链接条打断",
        urls: &[
            "https://www.qbitai.com/2026/10/504734.html",
            "https://www.ithome.com/0/823/017.htm",
        ],
    },
    Page {
        kind: "2 博客",
        ask: "单作者长文的正文是否连贯、代码块是否保住",
        urls: &[
            "https://overreacted.io/how-are-function-components-different-from-classes/",
            "https://jvns.ca/blog/2024/11/28/some-gdb-macros/",
        ],
    },
    Page {
        kind: "3 带大量导航的门户",
        ask: "导航/页脚噪声占比是否明显（判定门的第一条）",
        urls: &["https://www.ithome.com/", "https://news.cnblogs.com/"],
    },
    Page {
        kind: "4 纯文本文件",
        ask: "text/plain 这条路是否原样透传（不经过抽取器）",
        urls: &["https://www.rfc-editor.org/rfc/rfc1945.txt"],
    },
    Page {
        kind: "5 JSON 接口",
        ask: "application/json 这条路是否原样透传",
        urls: &["https://api.github.com/repos/tokio-rs/tokio"],
    },
    Page {
        kind: "6 SPA 空壳",
        ask: "JS 渲染的页面抽出来是不是接近空（`slice_page` 那句「正文为空」会不会被打到）",
        urls: &["https://36kr.com/", "https://juejin.cn/"],
    },
    // 附加观察，不属于六个种类：老 charset 的中文门户。R265/R268 之后 `render_markdown`
    // 按**声明**的字符集解（`encoding_rs`，解不了就报错、绝不 lossy），这里量一下真网上
    // 到底能不能打到这一支（首轮复测：这一页返回的其实是 UTF-8，一次都没打到）。
    Page {
        kind: "附加：非 UTF-8 门户（gb2312/gbk）",
        ask: "charset 声明参与解码在真网上的形态（乱码率 / 报错，不计入判定门）",
        urls: &["https://www.sina.com.cn/"],
    },
];

/// 一次实测的全部读数。`Fetched` 是生产类型，**不**为探针加字段（耗时、URL 都不属于它），
/// 所以探针自己装一份。
struct Shot {
    url: String,
    final_url: String,
    status: u16,
    content_type: String,
    bytes: usize,
    hops: usize,
    cross_host: bool,
    fetch_ms: u128,
    extract_ms: u128,
    /// `render_markdown` 的产物。
    text: String,
    /// true = 走了白名单里的透传分支（text/plain、application/json），没经过抽取器。
    passed_through: bool,
    /// 对照读数：同一份 HTML **不做正文定界**、整页过一次 `htmd`。
    /// **生产路径不是这一套** —— 本任务的判定门只认 `render_markdown` 的结果，
    /// 加这一列只为了量出「readability 的正文定界替模型挡掉了多少字符」，
    /// 好让 T24 那份 findings 的前后对照有可对证的数。
    control: Option<Control>,
}

/// 对照抽取的读数。
enum Control {
    /// 抽出来了；`text` 是「整页过一次 htmd、不做正文定界」的结果。
    Converted { text: String, convert_ms: u128 },
    /// 对照趟自己失败了。生产路径那一趟（`render_markdown`）**已经成功**，所以这一条
    /// 只让对照列缺一格数，不影响这一页的判定读数。
    Failed(String),
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let live = args.iter().any(|a| a == "--live");
    let extra: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();

    if !live {
        println!("离线模式：不发起任何请求。要实测请加 --live。\n计划打的页面种类：");
        for p in PAGES {
            println!("  {:<30} {}", p.kind, p.urls.join(" | "));
        }
        return;
    }

    println!("UA: {USER_AGENT}");
    println!("体积闸: {DEFAULT_MAX_BYTES} 字节（超过就是 GuardError::TooLargeBody，不是截断）\n");

    // 生产接线，和 `FetchTool::new()` 同一对组件。
    let walker = RedirectWalker::new(Arc::new(SystemResolver), Arc::new(ReqwestTransport));

    // (种类, 要回答的问题, 候选 URL)。`--live <URL>…` 时同一套读数打在自定义地址上
    // （复核某一个页面时用得上），种类名换成「命令行指定的页面」，不与六个种类混记。
    let queue: Vec<(String, String, Vec<String>)> = if extra.is_empty() {
        PAGES
            .iter()
            .map(|p| {
                (
                    p.kind.to_string(),
                    p.ask.to_string(),
                    p.urls.iter().map(|u| u.to_string()).collect(),
                )
            })
            .collect()
    } else {
        extra
            .iter()
            .map(|u| {
                (
                    "命令行指定的页面".to_string(),
                    "自定义".to_string(),
                    vec![u.clone()],
                )
            })
            .collect()
    };

    let mut shots: Vec<Shot> = Vec::new();
    for (kind, ask, urls) in queue {
        // 兜底顺序：一条 URL 失败（拒绝/超时/体积超限/非白名单格式）就换下一条，
        // 全部失败才记「未实测」。判定门要的是**这种页面形状**的抽取质量，
        // 不是某一个站点的可达性；可达性失败要另说，不能冒充抽取失败。
        let mut tried: Vec<String> = Vec::new();
        let mut got: Option<Shot> = None;
        for url in &urls {
            match run_one(&walker, url).await {
                Ok(shot) => {
                    got = Some(shot);
                    break;
                }
                Err(why) => tried.push(format!("{url} -> {why}")),
            }
        }
        match got {
            Some(shot) => {
                println!("### {kind}");
                println!("问：{ask}");
                print_shot(&shot);
                shots.push(shot);
            }
            None => {
                println!("### {kind}\n候选：{}", urls.join(", "));
                println!("未实测 —— 全部候选都失败：");
                for t in &tried {
                    println!("  - {t}");
                }
                println!();
            }
        }
    }

    println!("\n== 汇总（人眼判定的输入，判定门本身在 findings 里下）==");
    for s in &shots {
        let total = s.text.chars().count();
        let control = match &s.control {
            None => "—".to_string(),
            Some(Control::Converted { text, .. }) => format!("{} 字符", text.chars().count()),
            Some(Control::Failed(_)) => "对照失败".to_string(),
        };
        println!(
            "{:<34} {:>8} 字符  对照 {:>10}  抓 {:>6} ms  抽 {:>6} ms  {}",
            s.url,
            total,
            control,
            s.fetch_ms,
            s.extract_ms,
            if s.passed_through {
                "透传"
            } else {
                "readability+htmd"
            }
        );
    }
}

/// 抓 + 抽取一次；返回 `Err(原因)` 表示这一条 URL 没测成，调用方换下一条。
async fn run_one(walker: &RedirectWalker, url: &str) -> Result<Shot, String> {
    let t0 = Instant::now();
    let f = match walker.fetch(url).await {
        Ok(f) => f,
        Err(e) => return Err(format!("抓取失败：{e}")),
    };
    let fetch_ms = t0.elapsed().as_millis();
    let passed_through = !is_html(&f.content_type);

    let t1 = Instant::now();
    // R270：基地址就是生产那一条 `final_url`（实际取回内容那一跳），探针不造占位地址。
    let text = match render_markdown(&f.content_type, &f.body, &f.final_url) {
        Ok(s) => s,
        Err(e) => {
            return Err(format!(
                "抽取失败：{e}（状态 {}，content-type {}）",
                f.status, f.content_type
            ));
        }
    };
    let extract_ms = t1.elapsed().as_millis();

    // 对照趟：同一份 HTML 再抽一遍，这次**不做正文定界**（整页过一次 htmd）。
    // 只在真的走了抽取分支时才做（透传那两条没有 HTML 可抽）。
    let control = if passed_through {
        None
    } else {
        // 对照列**不是**生产路径，也不参与 charset 判据：这里只需要「整页过一次 htmd」
        // 的字符数，所以直接 lossy —— 生产那三支持续不许出现这个函数（R265）。
        let html = String::from_utf8_lossy(&f.body).into_owned();
        let t2 = Instant::now();
        match htmd::convert(&html) {
            Ok(converted) => Some(Control::Converted {
                text: converted,
                convert_ms: t2.elapsed().as_millis(),
            }),
            // 对照列自己坏了：让这一格缺一组数，**不**把这一页判成「未实测」——
            // 生产路径那一趟在上面已经成功，判定门读的是那一趟的结果。
            Err(e) => Some(Control::Failed(format!("{e}"))),
        }
    };

    Ok(Shot {
        url: url.to_string(),
        final_url: f.final_url.clone(),
        status: f.status,
        content_type: f.content_type.clone(),
        bytes: f.body.len(),
        hops: f.hops,
        cross_host: f.cross_host,
        fetch_ms,
        extract_ms,
        text,
        passed_through,
        control,
    })
}

fn print_shot(s: &Shot) {
    println!("URL: {}", s.url);
    println!(
        "最终地址: {}（跟了 {} 跳，换注册域: {}）",
        s.final_url, s.hops, s.cross_host
    );
    println!(
        "状态: {} | content-type: {} | body: {} 字节 | {}",
        s.status,
        s.content_type,
        s.bytes,
        if s.passed_through {
            "白名单透传分支（不过抽取器）"
        } else {
            "readability 定界 + htmd 转换"
        }
    );
    println!(
        "耗时: 抓取 {} ms + 抽取 {} ms（抽取是纯 CPU，网络时间不进判定）",
        s.fetch_ms, s.extract_ms
    );
    metrics(&s.text);
    match &s.control {
        None => {}
        Some(Control::Converted { text, convert_ms }) => {
            let control_total = text.chars().count();
            let prod_total = s.text.chars().count();
            let dropped = control_total.saturating_sub(prod_total);
            println!(
                "对照（整页、不做正文定界）: {control_total} 字符（生产路径 {prod_total} 字符，\
                 正文定界这一类省掉 {} 字符 = {}），抽取 {convert_ms} ms",
                dropped,
                pct(dropped, control_total)
            );
            let (head, _) = head_chars(text, 200);
            println!("--- 对照前 200 字 ---\n{head}");
        }
        Some(Control::Failed(why)) => {
            println!("对照（整页、不做正文定界）: 这一格失败了（{why}）；生产路径的读数不受影响");
        }
    }
    println!();
}

/// 百分比读数；`whole == 0` 时不说 `NaN%`。
fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        "0%".to_string()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

/// 总字符数 + 三个代理指标 + 首尾原文。
fn metrics(text: &str) {
    let total = text.chars().count();
    let lines = text.lines().count();
    // 「链接行」= 含 Markdown 链接语法 `](` 的行：导航条、页脚、推荐列表都是这个形状。
    let link_lines = text.lines().filter(|l| l.contains("](")).count();
    let link_chars: usize = text
        .lines()
        .filter(|l| l.contains("]("))
        .map(|l| l.chars().count())
        .sum();
    // 「散文行」= 超过 40 个字符的行：正文段落的下限；短行是菜单、按钮、面包屑。
    let prose_chars: usize = text
        .lines()
        .filter(|l| l.chars().count() > 40)
        .map(|l| l.chars().count())
        .sum();

    println!("抽取后总字符数: {total}");
    println!(
        "代理指标：行数 {lines} | 链接行 {} 行（占 {}，占字符 {}） | 长行字符占比 {}（碎片化代理指标）",
        link_lines,
        pct(link_lines, lines.max(1)),
        pct(link_chars, total),
        pct(prose_chars, total)
    );

    let (head, _) = head_chars(text, 400);
    println!("--- 前 400 字 ---\n{head}");
    let tail: String = text.chars().skip(total.saturating_sub(200)).collect();
    println!("--- 末尾 200 字 ---\n{tail}");
}

/// 与 `render_markdown` 同一套媒体类型归一（这里只用来标注「走了哪条分支」）。
fn is_html(content_type: &str) -> bool {
    let mt = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(mt.as_str(), "text/html" | "application/xhtml+xml")
}
