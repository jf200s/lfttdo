// Task 42：长会话压测 —— spec/计划 §9「长对话不失忆、不 400」的**确定性那一半**。
//
// 这是本仓库第一个集成测试目标（`src-tauri/tests/`）。它刻意**无网络、无 Tauri、无假模型**：
// 编排与摘要调用（`ipc.rs` 的 `compact_before_turn` + `agent/summarize.rs` 的 `collect_summary`）
// 已由 T41 用 fake 在 `src/` 的单元测试里覆盖，而 `compact_before_turn` 是 `pub(crate)`，
// 集成测试根本够不到。本片测的是另一件事：**真实存储（`Db`）+ 真实构建器（`ContextBuilder`）
// 在 100 轮之下是否仍然给出确定、可断言的形状**。
//
// 一片只写测试与文档：`src-tauri/src/` 一行没改。压出来的数字若指向语义问题（见 §4 的
// `stuck_rounds`），那是新的产品决策，写进 `docs/findings/plan-c-long-session.md`，不在这里顺手改。

use qwen_do_lib::agent::context::{
    ContextBuilder, ContextConfig, KEEP_IMAGE_RECENT, SYSTEM_PROMPT, estimate_tokens,
};
use qwen_do_lib::agent::summarize::{SUMMARY_PREFIX, summary_note};
use qwen_do_lib::agent::types::{ContentBlock, ImageMime, Message, Risk, Role, ToolOutput};
use qwen_do_lib::store::db::Db;
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

// ---------- 夹具常量（计价常数见 context.rs:9-18，测试不复用它们的私有版本）----------

/// 镜像 `context.rs:233` 的私有常量 `DEGRADED_IMAGE_NOTE`。
/// 它是 `const`（非 `pub`），集成测试拿不到，而本片**不许**为了拿它去改 `src/`
/// —— 所以这里抄一份字面量，并靠下面那条断言盯住「请求里真的出现过它」。
const PLACEHOLDER: &str = "[图片已移除以节省上下文]";

/// 每轮 user 的汉字数（任务书 §3.A 指定的夹具尺寸）—— 注意它是** token 数**而不是字数：
/// 行标识 `R013u ` 是 6 个非 CJK 字符 = `div_ceil(6/4)` = **2 token**，所以正文只放
/// `CJK_USER - ID_TOKENS` 个汉字，让「一条 user = 4 开销 + 60 token」这条 §2.8 的算式
/// 不被标识符悄悄改掉。实测这条不是洁癖：标识符多花 4 token/轮，滑窗开始丢字就比
/// 「压缩最早能触发」的第 14 轮**提前一轮**，A 会在第 13 轮红在覆盖判据上 ——
/// 那测的是夹具的 token 预算，不是产品。详见 findings §A 的敏感度那一段。
const CJK_USER: usize = 60;
/// 每轮 assistant 的 token 数（同上：200 token，正文 198 个汉字 + 2 token 的标识符）。
const CJK_ASSISTANT: usize = 200;
/// 行标识符自身占的 token 数（见 `CJK_USER` 的注释）。
const ID_TOKENS: usize = 2;

/// 图片夹具的假 base64。计价按 mime 定值（`PNG_TOKENS` = 900），**与长度无关**
/// （context.rs:13-15 的注释明文规定），所以这里刻意只放一小截，不塞几 MB 装作逼真。
const FAKE_B64: &str = "AAECAw==";

/// 行身份证：`build()` 的顺序断言要用它比对「库里的第几行」与「请求里的第几行」。
/// 文本行取首个空格前的标签；工具行没有文本块（`Message::tool` 刻意单块构造），
/// 所以从块里取 id，并给 `ToolCall`/`ToolResult` 加不同前缀，避免两条行撞成同一个身份。
fn row_id(m: &Message) -> String {
    for b in &m.blocks {
        match b {
            ContentBlock::Text { text } => {
                return text.split(' ').next().unwrap_or_default().to_string();
            }
            ContentBlock::ToolCall { id, .. } => return format!("call:{id}"),
            ContentBlock::ToolResult { call_id, .. } => return format!("res:{call_id}"),
            ContentBlock::Image { .. } => {}
        }
    }
    String::new()
}

fn has_tool_result(m: &Message) -> bool {
    m.blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

fn user_row(round: usize, with_image: bool) -> Message {
    let mut blocks = vec![ContentBlock::Text {
        text: format!("R{round:03}u {}", "字".repeat(CJK_USER - ID_TOKENS)),
    }];
    if with_image {
        blocks.push(ContentBlock::Image {
            data: FAKE_B64.into(),
            mime: ImageMime::Png,
        });
    }
    Message {
        role: Role::User,
        blocks,
    }
}

fn assistant_row(round: usize) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: format!("R{round:03}a {}", "答".repeat(CJK_ASSISTANT - ID_TOKENS)),
        }],
    }
}

/// 工具回合的前半：`assistant` 带 `ToolCall`（`types.rs` 没有 assistant 构造器，
/// 现成写法见 `db.rs:531-535` 的测试助手）。
fn assistant_tool_call(round: usize) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolCall {
            id: format!("c{round}"),
            name: "fetch_url".into(),
            input: json!({"url": format!("https://example.com/{round}")}),
        }],
    }
}

/// 工具回合的后半：**生产那两条的形状**（`Message::tool` 的 role 是 `User`，
/// 且单块构造）。这是 T40「一轮」定义的靶心 —— 按「任何 user 都开一轮」数会把
/// 一个工具回合数成两轮，于是 `keep_turns: 10` 实际只保住 5 轮。
fn tool_result_row(round: usize, call_id: &str) -> Message {
    Message::tool(
        call_id,
        ToolOutput::text(format!("R{round:03}t 抓到的正文")),
        false,
    )
}

fn count_images(msgs: &[Message]) -> usize {
    msgs.iter()
        .map(|m| {
            m.blocks
                .iter()
                .filter(|b| matches!(b, ContentBlock::Image { .. }))
                .count()
        })
        .sum()
}

fn count_placeholders(msgs: &[Message]) -> usize {
    msgs.iter()
        .filter(|m| m.text_of().contains(PLACEHOLDER))
        .count()
}

// ---------- 驱动器：A 与 B 共用同一条测量口径（任务书 §4）----------

#[derive(Default)]
struct Drive {
    /// 压缩次数。
    compactions: usize,
    /// 每次压缩删掉的**行数**（`plan.dropped`），按发生顺序。
    compact_rows: Vec<usize>,
    /// 每次压缩的 `drop_to` 落点（对话序列下标），按发生顺序。
    compact_drop_tos: Vec<usize>,
    /// `est_input_tokens` 曲线的首值 / 末值 / 峰值。
    est_first: u32,
    est_last: u32,
    est_peak: u32,
    /// 库内行数（含备注）的峰值 / 终值。
    rows_peak: usize,
    rows_last: usize,
    /// 备注行的条数与 token 占用（终值与峰值）。
    notes_last: usize,
    notes_peak: usize,
    notes_tokens_last: usize,
    notes_tokens_peak: usize,
    /// §4 的病理态：越了线、`compaction_plan` 却说「不压」的轮数。
    stuck_rounds: usize,
    /// 第一次出现病理态的轮号；0 == 从未出现。
    first_stuck_round: usize,
    /// 病理轮里滑窗相对上一轮多丢（或少丢）的条数：`(轮号, built.dropped 的增量)`。
    stuck_drops: Vec<(usize, i64)>,
    /// `built.dropped` 的峰值与末值（滑窗静默丢字的规模）。
    dropped_peak: usize,
    dropped_last: usize,
    /// 库内带图行数的峰值（夹具守卫：B 不许白跑 100 轮）。
    images_in_db_peak: usize,
    /// 请求里真图张数与占位行数的峰值。
    images_in_request_peak: usize,
    placeholders_peak: usize,
    /// C：断言扫过的 `ToolResult` 行数（总次数与单轮峰值）。0 行的测试是假测试。
    tool_rows_scanned: usize,
    tool_rows_peak: usize,
    /// 全量读库的次数（每轮两次：压缩决策一次、组装请求一次）。
    reads: usize,
    elapsed_ms: u128,
}

/// 跑一整段会话：每轮 追加夹具 → 压缩决策与落库 → 组装请求 → 断言。
///
/// 三个开关对应任务书 §3 的相位划分：A/B 要预算与覆盖判据（并排除 `build()` 的
/// `keep <= 1` 逃生口），C 只要刀口判据。
fn drive(
    label: &str,
    cfg: &ContextConfig,
    rounds: usize,
    image_rounds: &[usize],
    tool_every: usize,
    assert_budget: bool,
    assert_coverage: bool,
) -> (TempDir, Db, PathBuf, String, Drive) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.db");
    let db = Db::open(&path).unwrap();
    let sid = db.create_session(label).unwrap();

    let usable = cfg.budget_tokens.saturating_sub(cfg.reserve_output) as usize;
    let line = (usable as f64 * f64::from(cfg.compact_threshold)) as usize;
    let mut d = Drive::default();
    let mut prev_dropped = 0usize;
    let clock = Instant::now();

    for r in 1..=rounds {
        db.append_message(&sid, &user_row(r, image_rounds.contains(&r)))
            .unwrap();
        if tool_every > 0 && r % tool_every == 0 {
            db.append_message(&sid, &assistant_tool_call(r)).unwrap();
            db.append_message(&sid, &tool_result_row(r, &format!("c{r}")))
                .unwrap();
        }
        db.append_message(&sid, &assistant_row(r)).unwrap();

        // ---- ① 压缩决策 + 落库（生产的 `compact_before_turn` 就站在本轮发送之前这个位置）----
        let history = db.messages(&sid).unwrap();
        d.reads += 1;
        let builder = ContextBuilder::new(cfg.clone());

        // 与 `compaction_plan` 内部**同一条算式**（相加性见 context.rs:415 的 `estimate_one`）：
        // 库里存的历史不含 SYSTEM_PROMPT（那是 `build()` 现拼的），所以镜像必须自己加上它。
        // 这不是「大概差不多」：下面 `plan.is_some() ⇒ est > line` 那条断言就是这套数字
        // 与实现算式逐轮对撞的校验，抄错口径会当场红。
        let mut raw = estimate_tokens(&[Message::system_note(SYSTEM_PROMPT)]);
        raw += estimate_tokens(&history);
        let est = raw; // 本测试用新 builder ⇒ calibration 恒为 1.0

        let notes: Vec<Message> = history
            .iter()
            .filter(|m| m.role == Role::System)
            .cloned()
            .collect();
        let notes_tokens = estimate_tokens(&notes);
        d.notes_peak = d.notes_peak.max(notes.len());
        d.notes_tokens_peak = d.notes_tokens_peak.max(notes_tokens);

        let plan = builder.compaction_plan(&history);
        if plan.is_some() {
            assert!(
                est > line,
                "[{label}] 第 {r} 轮：镜像口径算出 est={est} line={line} 却没越线，\
                 而实现决定要压 —— 两边读的不是同一份预算，测量作废"
            );
        }
        // §4 要量的病理态：越了线，但 `compaction_plan` 说「不压」。
        let stuck = est > line && plan.is_none();
        if stuck {
            d.stuck_rounds += 1;
            if d.first_stuck_round == 0 {
                d.first_stuck_round = r;
            }
        }
        if let Some(plan) = plan {
            // `note_text` **必须已经带前缀**：`compact_session` 只做 `Message::system_note(note_text)`，
            // 不拼 `SUMMARY_PREFIX`（前缀只加一次这件事归 `summarize.rs:34` 的 `summary_note`，
            // 生产路径是 `ipc.rs:323`）。这里手写 `format!("{SUMMARY_PREFIX}…")` 就是把那条
            // 规则做成第二份真相，任务书 §2.4 明文禁止。
            let note = summary_note(&format!("第 {r} 轮前的要点"));
            db.compact_session(&sid, &plan, &note).unwrap();
            d.compactions += 1;
            d.compact_rows.push(plan.dropped);
            d.compact_drop_tos.push(plan.drop_to);
        }

        // ---- ② 组装本轮请求 ----
        let history = db.messages(&sid).unwrap();
        d.reads += 1;
        d.rows_last = history.len();
        d.rows_peak = d.rows_peak.max(history.len());
        d.images_in_db_peak = d.images_in_db_peak.max(count_images(&history));
        let built = builder.build(&history);

        let non_system: Vec<&Message> = built
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .collect();
        if assert_budget {
            // 注意 `usable`（= budget − reserve），不是 `cfg.budget_tokens`：
            // `build()` 里的 budget 就是这两个数之差（context.rs:126-129）。
            assert!(
                built.est_input_tokens as usize <= usable,
                "[{label}] 第 {r} 轮：请求粗估 {} > 可用预算 {usable} ⇒ 端点 400，\
                 正是 spec :364 点名的「聊到第 N 轮突然 400，极难排查」",
                built.est_input_tokens
            );
            // 任务书 §3「计划的一处错误」：`build()` 在 `keep <= 1` 时**明确照样返回**
            // （context.rs:158 的出口，spec :365 保证「缩到只剩 1 条仍超预算也不报错」）。
            // 所以 `est <= usable` 不是 `build()` 的承诺 —— 夹具若造出「单条就超预算」的
            // 怪物消息，它会合法地越线。这条 ≥ 2 把那个逃生口排除在断言范围之外：
            // 夹具里出现怪物消息就是夹具坏了，要让它响。
            assert!(
                non_system.len() >= 2,
                "[{label}] 第 {r} 轮：请求里只剩 {} 条非 system 行 —— 夹具造出了「单条就超预算」\
                 的怪物消息，`build()` 的逃生口被踩到了（est={}，可用预算 {usable}）",
                non_system.len(),
                built.est_input_tokens
            );
        }
        if assert_coverage {
            // 这条测的是 §9 的「静默失忆」：原文被滑窗丢了，却没有一条备注覆盖被丢的区间
            // ⇒ 用户看不出来，模型也不会说出来。
            let covered = built
                .messages
                .iter()
                .any(|m| m.role == Role::System && m.text_of().starts_with(SUMMARY_PREFIX));
            assert!(
                built.dropped == 0 || covered,
                "[{label}] 第 {r} 轮：滑窗丢了 {} 条原文，请求里却没有一条 `{SUMMARY_PREFIX}` 备注\
                 覆盖被丢区间 ⇒ 静默失忆",
                built.dropped
            );
        }
        // 顺序不变量（三条测试都要，B 的那条「降级不动刀口」的一半就是它）：
        // 请求里的非 system 行必须是库里非 system 行的**连续后缀**，顺序逐行一致。
        // 降级发生在 `build()` 内部、作用在 clone 上（R148），所以它只会替换块内容，
        // 不会移动或删掉任何一行。
        let db_ids: Vec<String> = history
            .iter()
            .filter(|m| m.role != Role::System)
            .map(row_id)
            .collect();
        let built_ids: Vec<String> = non_system.iter().map(|m| row_id(m)).collect();
        assert_eq!(
            built_ids,
            db_ids[db_ids.len() - built_ids.len()..],
            "[{label}] 第 {r} 轮：请求里的行序与库里对应行的顺序不一致"
        );

        // 图片的两条线：请求里真图不超过额度；库里带图的行（只要还没被压缩删掉）
        // 必须**仍然带着真图** —— 这是「界面/库里看到的」与「模型看到的」分叉不可逆那件事。
        let imgs = count_images(&built.messages);
        d.images_in_request_peak = d.images_in_request_peak.max(imgs);
        let ph = count_placeholders(&built.messages);
        d.placeholders_peak = d.placeholders_peak.max(ph);
        assert!(
            imgs <= KEEP_IMAGE_RECENT,
            "[{label}] 第 {r} 轮：请求里带着 {imgs} 张真图，超过额度 KEEP_IMAGE_RECENT={KEEP_IMAGE_RECENT}"
        );
        for ir in image_rounds {
            let tag = format!("R{ir:03}u");
            if let Some(row) = history.iter().find(|m| row_id(m) == tag) {
                assert!(
                    matches!(
                        row.blocks.last(),
                        Some(ContentBlock::Image {
                            mime: ImageMime::Png,
                            ..
                        })
                    ),
                    "[{label}] 第 {r} 轮：第 {ir} 轮那条图在**库里**已经不再是 Image 块 —— \
                     `build()` 的降级不许写回存储（R148）"
                );
            }
        }

        // C 的扫描：每条带 ToolResult 的行，前面必须存在一条带**同 call_id** 的 ToolCall 行。
        let mut calls: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        for m in &built.messages {
            for b in &m.blocks {
                match b {
                    ContentBlock::ToolCall { id, .. } => calls.push(id.clone()),
                    ContentBlock::ToolResult { call_id, .. } => {
                        scanned += 1;
                        assert!(
                            calls.iter().any(|seen| seen == call_id),
                            "[{label}] 第 {r} 轮：`ToolResult`（call_id={call_id}）前面没有配套的\
                             `ToolCall` ⇒ 这条请求上线就被端点判 400"
                        );
                    }
                    _ => {}
                }
            }
        }
        // 孤儿开头就是 400：`messages[0]` 恒为 head 里的 SYSTEM_PROMPT，这条把事实钉住。
        assert!(
            !has_tool_result(&built.messages[0]),
            "[{label}] 第 {r} 轮：请求第一行就是 ToolResult ⇒ 必被端点判 400"
        );
        d.tool_rows_scanned += scanned;
        d.tool_rows_peak = d.tool_rows_peak.max(scanned);

        if stuck {
            let delta = built.dropped as i64 - prev_dropped as i64;
            d.stuck_drops.push((r, delta));
        }
        prev_dropped = built.dropped;
        d.dropped_peak = d.dropped_peak.max(built.dropped);
        d.dropped_last = built.dropped;
        d.est_last = built.est_input_tokens;
        if r == 1 {
            d.est_first = built.est_input_tokens;
        }
        d.est_peak = d.est_peak.max(built.est_input_tokens);
    }

    let history = db.messages(&sid).unwrap();
    d.notes_last = history.iter().filter(|m| m.role == Role::System).count();
    d.notes_tokens_last = estimate_tokens(
        &history
            .iter()
            .filter(|m| m.role == Role::System)
            .cloned()
            .collect::<Vec<_>>(),
    );
    d.rows_last = history.len();
    d.elapsed_ms = clock.elapsed().as_millis();
    println!(
        "[{label}] usable={usable} line={line} 压缩次数={} 每次删掉的行数={:?} drop_to 落点={:?}",
        d.compactions, d.compact_rows, d.compact_drop_tos
    );
    println!(
        "[{label}] est 首/末/峰 = {}/{}/{}；库内行数 峰/末 = {}/{}；读库次数={}",
        d.est_first, d.est_last, d.est_peak, d.rows_peak, d.rows_last, d.reads
    );
    println!(
        "[{label}] 备注：条数 末/峰 = {}/{}，token 末/峰 = {}/{}（占 usable 的 {}% / {}%）",
        d.notes_last,
        d.notes_peak,
        d.notes_tokens_last,
        d.notes_tokens_peak,
        d.notes_tokens_last * 100 / usable,
        d.notes_tokens_peak * 100 / usable
    );
    println!(
        "[{label}] stuck_rounds={} 首次出现轮号={}（0=从未）；病理轮里 dropped 增量={:?}",
        d.stuck_rounds, d.first_stuck_round, d.stuck_drops
    );
    println!(
        "[{label}] dropped 峰/末 = {}/{}；库内带图行峰值={}；请求真图峰={} 占位行峰={}；\
         扫过的 ToolResult 行 总/峰 = {}/{}；耗时 {} ms",
        d.dropped_peak,
        d.dropped_last,
        d.images_in_db_peak,
        d.images_in_request_peak,
        d.placeholders_peak,
        d.tool_rows_scanned,
        d.tool_rows_peak,
        d.elapsed_ms
    );
    (dir, db, path, sid, d)
}

fn file_len(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// findings 第 2 项：库文件与 WAL 边车的字节数（今天无分页、无 `VACUUM`）。
fn report_bytes(label: &str, path: &PathBuf) -> (u64, u64) {
    let main = file_len(path);
    let wal = file_len(&path.with_extension("db-wal"));
    println!("[{label}] 字节：主库 {main}，-wal {wal}（无分页、无 VACUUM）");
    (main, wal)
}

/// findings 第 3 项：重放一次读出的条数（R69 的实测 —— 它必须**不含**备注行）。
fn report_replay(label: &str, db: &Db, sid: &str) -> (usize, usize) {
    let history = db.messages(sid).unwrap();
    let events = qwen_do_lib::store::db::events_from_messages(&history, &|_name| Risk::Read);
    let notes = history.iter().filter(|m| m.role == Role::System).count();
    // 备注行不许出现在重放里：用户看到的应当是原话，不是给模型读的备忘。
    let leaked = events
        .iter()
        .filter(|e| {
            serde_json::to_string(e)
                .unwrap_or_default()
                .contains(SUMMARY_PREFIX)
        })
        .count();
    assert_eq!(
        leaked, 0,
        "[{label}] 重放里混进了 {leaked} 条 `{SUMMARY_PREFIX}` 备注 ⇒ R69 崩了"
    );
    println!(
        "[{label}] 重放读出 {} 条事件，库内 {} 行（其中 {notes} 条备注，一条都没进重放）",
        events.len(),
        history.len()
    );
    (events.len(), history.len())
}

// ---------- A. 100 轮纯文本：预算 + 覆盖摘要 ----------

#[test]
fn one_hundred_text_rounds_stay_inside_the_budget_with_a_covering_summary() {
    let cfg = ContextConfig {
        budget_tokens: 4096,
        reserve_output: 512,
        window_messages: 40,
        compact_threshold: 0.85,
        keep_turns: 10,
    };
    let (_dir, db, path, sid, d) = drive(
        "A/100 轮文本",
        &cfg,
        100,  // rounds
        &[],  // 无图
        0,    // 无工具回合
        true, // 断言预算
        true, // 断言覆盖
    );
    assert!(
        d.compactions >= 1,
        "100 轮一次都没压 ⇒ 阈值或批量算错了（stuck_rounds={}）",
        d.stuck_rounds
    );
    report_bytes("A/100 轮文本", &path);
    report_replay("A/100 轮文本", &db, &sid);
}

// ---------- B. 图片与压缩共存：降级不许动刀口 ----------

#[test]
fn images_and_compaction_coexist_and_the_degradation_does_not_move_the_cut() {
    // `ContextConfig::default()`（32_000 / 4_096 / 40 / 0.85 / 10）而不是 A 的小预算：
    // 三张 PNG = 3 × `PNG_TOKENS` = 2700 token，而 A 的可用预算只有 3584 ——
    // 在小预算下越线的会是**图**而不是**轮数**，两个变量互相掩盖（任务书 §2.8/§3）。
    let cfg = ContextConfig::default();
    let (_dir, db, path, sid, d) = drive(
        "B/100 轮带图",
        &cfg,
        100,
        &[1, 34, 67], // 第 1/34/67 轮各带一张 PNG
        0,
        true,  // 断言预算（含非 system ≥ 2 那道）
        false, // 覆盖判据归 A：本相位里压缩只落 1 次（compactions=1），而滑窗满 40 行后每轮都在丢（dropped 峰值 116）⇒ 那条判据在这里没有意义
    );
    assert!(
        d.compactions >= 1,
        "100 轮一次都没压 ⇒ 带图的历史把压缩挡住了（stuck_rounds={}）",
        d.stuck_rounds
    );
    // 夹具守卫（变异判据 c 的靶子）：第 1 轮那张图必须真的进过库。
    // 没有这条，B 就只是「白跑 100 轮顺便验证了一下没崩」，什么都没测。
    assert_eq!(
        d.images_in_db_peak, 3,
        "库里带图行的峰值应当是 3 张（第 1/34/67 轮），实际 {} ⇒ 夹具没真的放图进去",
        d.images_in_db_peak
    );
    // R148 的另一半在这里量：请求里真图的**峰值是 1 张而不是 3 张**。
    // 三张图相隔 33 轮，而 `window_messages: 40` 只留最后 40 行（≈20 轮）⇒ 滑窗里任何
    // 时刻最多容得下一张图，`degrade_old_images` 的「只留最近 3 张」这条在 100 轮循环里
    // **从来没有机会生效**（占位行数峰值 = 0 是这件事的实测）。所以降级必须在下面的
    // 四图子测试里实测；这一轮循环守的是另外两件事：带图的历史没把压缩挡住
    // （compactions=1），也没让请求顶出预算（est 峰值远小于 usable）。
    assert_eq!(
        d.images_in_request_peak, 1,
        "三张相隔 33 轮的图落进 40 行的滑窗，真图峰值应当是 1 张（实际 {} 张，额度 {KEEP_IMAGE_RECENT}）",
        d.images_in_request_peak
    );
    assert_eq!(
        d.placeholders_peak, 0,
        "上面那条的镜像：循环里一张图都不该被降级，出现占位就说明测的东西变了"
    );
    report_bytes("B/100 轮带图", &path);
    report_replay("B/100 轮带图", &db, &sid);

    // 上一条断言同时说明了 B 的局限：**三张图的夹具里「更早的图变成占位」这一支是空转的**
    // （3 == `KEEP_IMAGE_RECENT`，`image_positions.len().saturating_sub(3) = 0`）。
    // 所以「降级只动模型看到的那一份、不动库里那一份」这件事必须在**四张图**上实测。
    // 用同一个 `Db`（真实存储，不是内存 Vec），因为要测的正是「写回存储」这个不可逆分叉。
    let s4 = db.create_session("B/四张图").unwrap();
    for r in 1..=4 {
        db.append_message(&s4, &user_row(r, true)).unwrap();
        db.append_message(&s4, &assistant_row(r)).unwrap();
    }
    let before = db.messages(&s4).unwrap();
    assert_eq!(count_images(&before), 4, "夹具守卫：库里必须先真的有四张图");
    let built4 = ContextBuilder::new(ContextConfig::default()).build(&before);
    assert_eq!(
        count_images(&built4.messages),
        KEEP_IMAGE_RECENT,
        "四张历史图进了请求必须只剩额度内的 {} 张真图",
        KEEP_IMAGE_RECENT
    );
    assert_eq!(
        count_placeholders(&built4.messages),
        1,
        "被降级那张必须留下可见痕迹（`DEGRADED_IMAGE_NOTE` 的字面量镜像），不许静默消失"
    );
    assert_eq!(
        built4.dropped, 0,
        "这条测的是降级不是截窗：大预算下不许有任何一行被滑窗裁掉，否则『刀口没动』是截窗假象"
    );
    let after = db.messages(&s4).unwrap();
    assert_eq!(
        after, before,
        "降级作用在 build() 的 clone 上：库里每一行原样未动"
    );
    assert_eq!(
        count_images(&after),
        4,
        "库里的四张真图一张都不许少 —— 「用户界面上看到的」与「模型看到的」分叉必须可逆"
    );
}

// ---------- C. 两段刀口同时生效时不许出现孤儿 ToolResult ----------

#[test]
fn no_orphan_tool_result_at_either_end_of_the_request() {
    // A 的小预算：滑窗真会裁，压缩也真会裁，两段刀口在同一条请求上同时生效。
    let cfg = ContextConfig {
        budget_tokens: 4096,
        reserve_output: 512,
        window_messages: 40,
        compact_threshold: 0.85,
        keep_turns: 10,
    };
    let (_dir, db, path, sid, d) = drive(
        "C/60 轮带工具",
        &cfg,
        60,
        &[],
        5,     // 每 5 轮一个工具回合（assistant 带 ToolCall + 紧随其后的 tool 结果行）
        false, // C 不引入 `est <= usable` 与「非 system ≥ 2」：它测刀口，不测预算（任务书 §3 末）
        false,
    );
    // 三条夹具守卫：少任何一条，上面那个扫描就是空转（0 行的测试是假测试）。
    assert!(
        d.compactions >= 1,
        "压缩刀口一次都没落（compactions=0）⇒ C 只测了滑窗那半"
    );
    assert!(
        d.dropped_peak > 0,
        "滑窗一次都没裁（dropped 峰值 0）⇒ C 只测了压缩那半（compactions={}）",
        d.compactions
    );
    assert!(
        d.tool_rows_scanned > 0,
        "一条 ToolResult 都没扫到 ⇒ 上面那个循环是空转，C 什么都没测"
    );
    report_bytes("C/60 轮带工具", &path);
    report_replay("C/60 轮带工具", &db, &sid);
}
