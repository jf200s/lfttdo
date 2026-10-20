// 上下文组装与滑窗（spec 5.3）。决定「哪些消息进请求」与「这批消息粗估多少 token」。
// 本文件不得 `use tauri`；预算计算、丢弃兜底都不依赖网络，可全量单测。

use super::types::{ContentBlock, ImageMime, Message, Role};

/// 每条消息的固定结构开销（role 名 + JSON 框 + 分隔符）。
/// 提为常量是为了让估算比例的测试写成精确等式：区间断言在这个开销被调整时
/// 会漂成假绿，而这里每一项都是确定的整数。
pub const MSG_TOKEN_OVERHEAD: usize = 4;

pub const SYSTEM_PROMPT: &str = "你是 Qwen Do，一个运行在桌面端的轻量助手。回答简洁、直接，中文优先。可用工具时优先使用工具获取事实，不要凭记忆编造；不确定就说不确定。";

/// 单图计价：视觉 token 与 base64 字符数无关，按格式给固定权重。
/// 刻意不按 `data.len()` 估 —— base64 长度 ≈ 4/3 字节数，用它判断预算会完全失真，
/// 模型侧计费也是按视觉 token 而非字符。Task 20 的图片降级规则与此配合。
const PNG_TOKENS: usize = 900;
const JPEG_TOKENS: usize = 800;
const TOOL_IMAGE_TOKENS: usize = 900;

#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// 留给回复本身的 token（spec 5.3 默认 4096）
    pub reserve_output: u32,
    /// 输入侧配置上限（spec 5.3 默认 32000，用户可下调省钱）
    pub budget_tokens: u32,
    /// 滑窗兜底：最多保留多少条非 system 消息（spec 5.3 默认 40 轮）
    pub window_messages: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            reserve_output: 4096,
            budget_tokens: 32_000,
            window_messages: 40,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Built {
    /// 进请求的消息：`[system] + 历史 system 备注 + 保留的最近消息`
    pub messages: Vec<Message>,
    /// 被滑窗整条丢弃的非 system 消息数。Plan A 无消费者（`ContextCompacted` 属切片 5），
    /// 但按 spec :365 如实返回，Task 10 不得为它造事件。
    pub dropped: usize,
    /// 已乘校准系数的粗估输入 token
    pub est_input_tokens: u32,
}

pub struct ContextBuilder {
    cfg: ContextConfig,
    calibration: f64,
}

impl ContextBuilder {
    pub fn new(cfg: ContextConfig) -> Self {
        Self {
            cfg,
            calibration: 1.0,
        }
    }

    /// 用真实 usage 校准粗估系数：**factor 的语义是 `真实 ÷ 粗估`**，
    /// 因为 `est = 粗估 × calibration`（spec :362）。端点实际比粗估省 token → factor < 1；
    /// 反过来接成 `粗估 ÷ 真实` 不会报错，只会让估算一直朝危险的方向偏。
    /// 接线方（切片 5 的 usage 校准）负责传入 `input_tokens ÷ est_input_tokens`。
    /// 夹在 `0.2..=3.0`：一次坏 usage（比如 0 或 NaN 之外的极值）不许把估算清零、
    /// 否则预算形同虚设并把请求顶到 400。
    pub fn set_calibration(&mut self, factor: f64) {
        self.calibration = factor.clamp(0.2, 3.0);
    }

    /// 组装本次请求的消息。预算 = 配置上限 - 输出预留，先受滑窗上限约束，
    /// 再从最近往前缩到预算内；缩到只剩 1 条仍超预算时**照样返回**（spec :365 保证不报错）。
    /// 切口永不会落在 assistant 的 `ToolCall` 与它的 tool 结果之间（见循环里的 advance）。
    pub fn build(&self, history: &[Message]) -> Built {
        let system = Message::system_note(SYSTEM_PROMPT);
        // 保留历史里已有的 system 备注（Plan C 的摘要消息走这一路），
        // 但去掉与本机 SYSTEM_PROMPT 重复的那条，避免同一提示词进两次。
        let notes: Vec<Message> = history
            .iter()
            .filter(|m| m.role == Role::System)
            .filter(|m| m.text_of() != SYSTEM_PROMPT)
            .cloned()
            .collect();

        let conversational: Vec<&Message> =
            history.iter().filter(|m| m.role != Role::System).collect();

        let mut keep = conversational.len().min(self.cfg.window_messages);
        let budget = self
            .cfg
            .budget_tokens
            .saturating_sub(self.cfg.reserve_output) as usize;

        // 从最近往前扩，累到预算或窗口上限为止
        loop {
            let mut start = conversational.len() - keep;
            // 切口不许落在「带 ToolCall 的 assistant 消息」与「它的 tool 结果」之间：
            // Task 6 的 `to_api` 把 ToolResult 映射成 `role:"tool"` + `tool_call_id`
            // （openai_compat.rs 的 `tool_msg`），而 OpenAI 兼容端点要求它前面必须有一条
            // 带同一 `tool_call_id` 的 assistant 消息，否则回 400 —— 正是 spec :364 点名的
            // 「聊到第 N 轮突然 400，极难排查」。Task 10 的循环确实会成对 push
            // assistant(ToolCall) 与 Message::tool(..)，所以这条在 Plan A 里可达。
            // 终止性：advance 只让 `start` **单调前进**并被 `start < conversational.len()`
            // 夹住，最多 len 步；外层每轮 `keep -= 1` 让下一轮的起始 start 更大，
            // 故被跳过的 ToolResult 不会被重新包含，`start` 也不可能越过 len（无越界、
            // 无下溢：`keep` 由 `len - start` 反推，start ≤ len）。
            while start < conversational.len() && has_tool_result(conversational[start]) {
                start += 1;
            }
            // keep 由最终的 start 反推：被 advance 跳过的孤儿工具结果同样要计入 dropped。
            keep = conversational.len() - start;
            let tail: Vec<Message> = conversational[start..]
                .iter()
                .map(|m| (*m).clone())
                .collect();
            let mut cand = vec![system.clone()];
            cand.extend_from_slice(&notes);
            cand.extend(tail);
            // est 与 messages.len() 都必须来自 advance 之后的 cand。
            let est = (estimate_tokens(&cand) as f64 * self.calibration) as usize;
            // `keep <= 1` 而非 `keep == 1`：keep 可以是 0（空历史、window_messages: 0、
            // 或 budget_tokens: 0），此时 `keep -= 1` 会 usize 下溢并在 debug 直接 panic。
            if est <= budget || keep <= 1 {
                return Built {
                    messages: cand,
                    dropped: conversational.len() - keep,
                    est_input_tokens: est as u32,
                };
            }
            keep -= 1;
        }
    }
}

pub fn estimate_tokens(msgs: &[Message]) -> usize {
    let mut n = 0usize;
    for m in msgs {
        n += MSG_TOKEN_OVERHEAD; // role 与结构开销
        for b in &m.blocks {
            n += match b {
                ContentBlock::Text { text } => text_tokens(text),
                ContentBlock::Image { mime, .. } => match mime {
                    ImageMime::Png => PNG_TOKENS,
                    ImageMime::Jpeg => JPEG_TOKENS,
                },
                ContentBlock::ToolCall { name, input, .. } => {
                    name.len().div_ceil(4) + text_tokens(&input.to_string())
                }
                ContentBlock::ToolResult { output, .. } => match output {
                    super::types::ToolOutput::Text { text } => text_tokens(text),
                    super::types::ToolOutput::Image { .. } => TOOL_IMAGE_TOKENS,
                },
            };
        }
    }
    n
}

/// CJK 约 1 字 = 1 token；其余约 4 字符 = 1 token。混排时按字符类型分段累加。
fn text_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in s.chars() {
        if is_cjk(ch) { cjk += 1 } else { other += 1 }
    }
    cjk + other.div_ceil(4)
}

/// 中文标点（`0x3000..=0x303F` 的 `、。「」《》` 与 `0xFF00..=0xFFEF` 的全角
/// `，！？：；（）`）**必须**算进 CJK：漏掉它们会把真实中文散文按 1/4 token/字
/// 低估，而 spec :362 点名的正是「低估会在中文长文上严重低估」这条危险方向。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF
        | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F
        | 0x3000..=0x303F | 0xFF00..=0xFFEF)
}

/// 该消息是否携带 `ToolResult` 块（Task 5 的 `Message::tool(..)` 就是这一形态）。
/// 只有 `build` 的切口需要它：丢弃位置归本片所有，Task 6 的协议映射不反过来依赖这里。
fn has_tool_result(m: &Message) -> bool {
    m.blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

// 覆盖 spec :503 交给本片的「预算计算」与「丢弃兜底不报错」两条；
// 「图片降级」「摘要失败降级」分别归 Task 20 与切片 5。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, ImageMime, Message, Role, ToolOutput};
    use serde_json::json;

    fn u(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Task 10 循环真实产出的成对消息：assistant 带 `ToolCall`，紧跟一条 `Message::tool(..)`
    /// 结果（用的是 `agent::types` 的真实变体，不是 mock）。
    fn tool_pair(id: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: id.into(),
                    name: "fs.read".into(),
                    input: json!({"path": "a.txt"}),
                }],
            },
            Message::tool(id, ToolOutput::text("文件内容"), false),
        ]
    }

    /// 保留段（`messages[1..]` = 备注 + tail）里「找不到对应 ToolCall 的 ToolResult」，
    /// 返回它的 call_id。非 None 就意味着这条请求上线必被端点判 400。
    fn dangling_tool_result(messages: &[Message]) -> Option<String> {
        let mut calls: Vec<String> = Vec::new();
        for m in messages.iter().skip(1) {
            for b in &m.blocks {
                if let ContentBlock::ToolCall { id, .. } = b {
                    calls.push(id.clone());
                }
                if let ContentBlock::ToolResult { call_id, .. } = b
                    && !calls.iter().any(|seen| seen == call_id)
                {
                    return Some(call_id.clone());
                }
            }
        }
        None
    }

    // ---- 估算口径 ----

    #[test]
    fn cjk_counts_about_one_token_per_char() {
        // 中文按 1 字 ≈ 1 token 估，低估会在长中文对话上撑爆预算
        assert_eq!(
            estimate_tokens(&[u(&"测".repeat(50))]),
            50 + MSG_TOKEN_OVERHEAD
        );
    }

    #[test]
    fn english_counts_about_four_chars_per_token() {
        assert_eq!(
            estimate_tokens(&[u(&"a".repeat(100))]),
            25 + MSG_TOKEN_OVERHEAD
        );
    }

    #[test]
    fn cjk_punctuation_counts_as_one_token_per_char() {
        // R37：中文标点必须按 1 字 = 1 token 计。漏掉 0x3000..=0x303F 或全角区
        // 会把真实中文散文按 1/4 token/字 低估，而低估是撞 400 的危险方向。
        assert_eq!(
            estimate_tokens(&[u(&"，".repeat(10))]),
            10 + MSG_TOKEN_OVERHEAD
        );
        assert_eq!(
            estimate_tokens(&[u(&"、。".repeat(5))]),
            10 + MSG_TOKEN_OVERHEAD
        );
    }

    #[test]
    fn a_sentence_with_punctuation_costs_more_than_the_same_words_without_it() {
        // 摘掉 is_cjk 的标点区间 → 差额从 3 掉到 1，本测试立刻变红。
        let punctuated = "今天天气不错，我们出门走走、去看看展览。";
        let bare: String = punctuated
            .chars()
            .filter(|c| !matches!(c, '，' | '、' | '。'))
            .collect();
        assert_eq!(
            estimate_tokens(&[u(punctuated)]),
            estimate_tokens(&[u(&bare)]) + 3
        );
    }

    #[test]
    fn image_blocks_cost_a_fixed_high_amount_not_by_base64_length() {
        // 图片按内容长度算会让预算判断完全失真（base64 长度 ≈ 4/3 字节数）
        let small = Message {
            role: Role::User,
            blocks: vec![ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Png,
            }],
        };
        let big = Message {
            role: Role::User,
            blocks: vec![ContentBlock::Image {
                data: "A".repeat(100_000),
                mime: ImageMime::Png,
            }],
        };
        let s = estimate_tokens(&[small]);
        let g = estimate_tokens(&[big]);
        assert_eq!(s, g);
        assert!(s >= 700, "单图至少按 ~800 token 计");
    }

    // ---- 滑窗与预算 ----

    #[test]
    fn system_message_is_always_first_and_always_kept() {
        let hist: Vec<Message> = (0..200).map(|i| u(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(built.messages[0].role, Role::System);
        assert_eq!(built.messages[0].text_of(), SYSTEM_PROMPT);
    }

    #[test]
    fn window_drops_oldest_non_system_and_reports_count() {
        let hist: Vec<Message> = (0..200).map(|i| u(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(
            built.messages.len(),
            1 + ContextConfig::default().window_messages
        );
        assert!(built.dropped > 0);
        let last = built.messages.last().unwrap();
        assert!(last.text_of().contains("轮 199"));
    }

    #[test]
    fn a_window_cut_never_starts_with_a_dangling_tool_result() {
        // I-1：窗口只容得下最后一条，而最后一条是 tool 结果。
        // 直接切会让请求的第一条业务消息就是 `role:"tool"`（它的 assistant 轮已被切掉），
        // OpenAI 兼容端点回 400 —— spec :364 点名的「聊到第 N 轮突然 400，极难排查」。
        let mut hist = vec![u("先读一下 a.txt")];
        hist.extend(tool_pair("c1"));
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 100_000,
            window_messages: 1,
        };
        let built = ContextBuilder::new(cfg).build(&hist);
        assert_eq!(dangling_tool_result(&built.messages), None);
        // advance 把这条孤儿结果也跳掉了：3 条全丢，只剩 system。
        assert_eq!(built.messages.len(), 1);
        assert_eq!(
            built.dropped, 3,
            "被 advance 跳过的工具结果也要计入 dropped"
        );
    }

    #[test]
    fn a_window_cut_that_skips_a_dangling_result_still_keeps_the_next_pair() {
        // advance 只许丢掉「孤儿结果」，不许把成对的两条一起丢：
        // window=3 时 start 落在第 1 对的结果上，前进一格后第 2 对必须完整保留。
        let mut hist = tool_pair("c1");
        hist.extend(tool_pair("c2"));
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 100_000,
            window_messages: 3,
        };
        let built = ContextBuilder::new(cfg).build(&hist);
        assert_eq!(dangling_tool_result(&built.messages), None);
        assert_eq!(built.messages.len(), 3, "system + 第二对（调用 + 结果）");
        assert_eq!(built.messages[1].role, Role::Assistant);
        assert_eq!(built.messages[1].tool_calls().len(), 1);
        assert_eq!(
            built.dropped, 2,
            "1 条窗口外的 + 1 条被 advance 跳过的孤儿结果"
        );
    }

    #[test]
    fn a_budget_cut_lands_outside_a_tool_pair_too() {
        // 同一条不变式在「预算而不是窗口」逼着切口前进时也必须成立。
        // 预算 40 连 system 一条都装不下（SYSTEM_PROMPT 的粗估就有 ~100），
        // 所以循环一路缩到最后一条 = tool 结果，advance 把它也跳掉。
        let mut hist = vec![u("先读一下 a.txt")];
        hist.extend(tool_pair("c1"));
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 40,
            window_messages: 40,
        };
        let built = ContextBuilder::new(cfg).build(&hist);
        assert_eq!(dangling_tool_result(&built.messages), None);
        assert_eq!(built.messages.len(), 1);
        assert_eq!(built.dropped, 3);
    }

    #[test]
    fn a_history_of_only_orphan_tool_results_terminates_at_the_end() {
        // advance 的终止性边界：整段历史都是 tool 结果（没有任何 assistant 调用配对），
        // start 必须被 `start < conversational.len()` 夹住停在 len，
        // keep 由此归 0 而不是下溢，且每种窗口配置都不报错、不留孤儿。
        let hist: Vec<Message> = (0..5)
            .map(|i| Message::tool(&format!("c{i}"), ToolOutput::text("孤儿结果"), true))
            .collect();
        for window in [0usize, 1, 3, 5, 40] {
            let cfg = ContextConfig {
                reserve_output: 0,
                budget_tokens: 100_000,
                window_messages: window,
            };
            let built = ContextBuilder::new(cfg).build(&hist);
            assert_eq!(
                dangling_tool_result(&built.messages),
                None,
                "window {window}"
            );
            assert_eq!(built.messages.len(), 1, "window {window}");
            assert_eq!(built.dropped, 5, "window {window}");
        }
    }

    #[test]
    fn token_budget_can_drop_more_than_window_allows() {
        // 每条 4000 token 的巨轮：window 允许 40 条，但预算只容得下 3 条
        let cfg = ContextConfig {
            reserve_output: 4096,
            budget_tokens: 20_000,
            window_messages: 40,
        };
        let hist: Vec<Message> = (0..40)
            .map(|i| u(&("测".repeat(4000) + &format!(" {i}"))))
            .collect();
        let built = ContextBuilder::new(cfg).build(&hist);
        let total = estimate_tokens(&built.messages[1..]);
        assert!(total <= 20_000, "超预算 {total}");
        assert!(built.messages.len() < 40);
    }

    #[test]
    fn empty_history_returns_only_the_system_prompt() {
        // R34：spec :365「保证不报错」。空历史与零预算都不许让循环下溢。
        let built = ContextBuilder::new(ContextConfig::default()).build(&[]);
        assert_eq!(built.messages.len(), 1);
        assert_eq!(built.messages[0].role, Role::System);
        assert_eq!(built.dropped, 0);

        let zero = ContextConfig {
            reserve_output: 0,
            budget_tokens: 0,
            window_messages: 40,
        };
        let built = ContextBuilder::new(zero).build(&[]);
        assert_eq!(built.messages.len(), 1);
        assert_eq!(built.dropped, 0);
    }

    #[test]
    fn a_zero_budget_still_returns_instead_of_panicking() {
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 0,
            window_messages: 40,
        };
        let built = ContextBuilder::new(cfg).build(&[u(&"测".repeat(10))]);
        assert_eq!(built.messages[0].role, Role::System);
        assert!(built.messages.len() <= 2, "最多 system + 1 条");

        // keep == 0 且 est > budget：真正会 usize 下溢的组合
        let zero_window = ContextConfig {
            reserve_output: 0,
            budget_tokens: 0,
            window_messages: 0,
        };
        let built = ContextBuilder::new(zero_window).build(&[u("一"), u("二"), u("三")]);
        assert_eq!(built.messages.len(), 1);
        assert_eq!(built.dropped, 3);
    }

    #[test]
    fn system_notes_in_history_survive_the_window() {
        // spec :365 只丢弃「非 system」消息；Plan C 的摘要备注必须常驻，
        // 也不得被算进 dropped。
        let mut hist = vec![Message::system_note("早期对话摘要：前三轮聊了天气")];
        hist.extend((0..200).map(|i| u(&format!("轮 {i}"))));
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(
            built.messages.len(),
            2 + ContextConfig::default().window_messages
        );
        assert_eq!(built.messages[1].text_of(), "早期对话摘要：前三轮聊了天气");
        assert_eq!(
            built.dropped,
            200 - ContextConfig::default().window_messages
        );
    }

    #[test]
    fn the_system_prompt_is_not_duplicated_when_history_already_carries_it() {
        let hist = vec![Message::system_note(SYSTEM_PROMPT), u("第一"), u("第二")];
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(
            built
                .messages
                .iter()
                .filter(|m| m.text_of() == SYSTEM_PROMPT)
                .count(),
            1
        );
        assert_eq!(built.messages.len(), 3);
    }

    // ---- 校准系数 ----

    #[test]
    fn calibration_shrinks_estimate_when_usage_reports_less() {
        // R35：历史 1 条时 messages.len() 恒为 2，断言永真。这里让系数真的改变保留条数。
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 1000,
            window_messages: 40,
        };
        let hist: Vec<Message> = (0..4).map(|_| u(&"测".repeat(300))).collect();

        let mut b = ContextBuilder::new(cfg.clone());
        let before = b.build(&hist);
        b.set_calibration(0.2);
        let after = b.build(&hist);

        assert_eq!(before.messages.len(), 4, "粗估 1.0 时预算只容得下 3 条");
        assert_eq!(after.messages.len(), 5, "校准到 0.2 后四条全留");
        assert!(after.messages.len() > before.messages.len());

        // 同一批消息（相同的 tail），系数小 → 估算严格变小
        let tail = &hist[..3];
        let unc = ContextBuilder::new(cfg.clone());
        let mut cal = ContextBuilder::new(cfg);
        cal.set_calibration(0.2);
        assert!(
            cal.build(tail).est_input_tokens < unc.build(tail).est_input_tokens,
            "同一条 tail 在 0.2 下的估算必须小于 1.0"
        );
    }

    #[test]
    fn a_calibration_below_the_floor_is_clamped_to_the_floor() {
        // R38：0.05 必须按 0.2 处理，否则一次坏 usage 就能把估算清零、无限塞消息。
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 1000,
            window_messages: 40,
        };
        let hist: Vec<Message> = (0..4).map(|_| u(&"测".repeat(300))).collect();
        let raw = estimate_tokens(&hist);

        let mut tiny = ContextBuilder::new(cfg.clone());
        tiny.set_calibration(0.05);
        let mut floor = ContextBuilder::new(cfg);
        floor.set_calibration(0.2);

        assert_eq!(
            tiny.build(&hist).est_input_tokens,
            floor.build(&hist).est_input_tokens
        );
        assert!(
            tiny.build(&hist).est_input_tokens >= (raw as f64 * 0.1) as u32,
            "0.05 必须被抬到下限 0.2"
        );
    }

    #[test]
    fn a_calibration_above_the_ceiling_is_clamped_to_the_ceiling() {
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 1000,
            window_messages: 40,
        };
        let hist: Vec<Message> = (0..4).map(|_| u(&"测".repeat(300))).collect();

        let mut huge = ContextBuilder::new(cfg.clone());
        huge.set_calibration(50.0);
        let mut ceiling = ContextBuilder::new(cfg);
        ceiling.set_calibration(3.0);

        assert_eq!(
            huge.build(&hist).est_input_tokens,
            ceiling.build(&hist).est_input_tokens
        );
        assert_eq!(huge.build(&hist).messages.len(), 2);
    }
}
