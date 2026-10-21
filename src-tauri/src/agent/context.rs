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
    /// 没有进入 `messages` 的非 system 消息数：滑窗切掉的**加上** I-1 前进指针跳过的孤立
    /// `ToolResult`（两者合成一个数，不重复计）。Plan A 无消费者（`ContextCompacted` 属切片 5），
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
    /// 夹在 `0.2..=3.0`：一次坏 usage（比如 0）不许把估算清零，否则预算形同虚设并把请求顶到 400。
    /// **非有限输入（NaN、±inf）直接忽略并保留上一个系数**：`f64::clamp` 对 NaN 返回 NaN，
    /// 而 `(粗估 as f64 * NaN) as usize` 饱和成 0 —— 于是每条估算都变成 0、预算再也不生效，
    /// 正是上面禁止的那个后果，而且是静默的。`est_input_tokens == 0` 时接线的除法还会产出 inf。
    pub fn set_calibration(&mut self, factor: f64) {
        if !factor.is_finite() {
            return;
        }
        self.calibration = factor.clamp(0.2, 3.0);
    }

    /// 组装本次请求的消息。预算 = 配置上限 - 输出预留，先受滑窗上限约束，
    /// 再从最近往前缩到预算内；缩到只剩 1 条仍超预算时**照样返回**（spec :365 保证不报错）。
    /// 切口永不会落在 assistant 的 `ToolCall` 与它的 tool 结果之间（见循环里的 advance）。
    /// 已知取舍：历史里的 system 备注一律排在保留 tail **之前**，与它们在历史中的位置无关
    /// （今天的摘要备注就该在最前）；切片 5 若引入需要按时序穿插的系统备注，要改这里。
    pub fn build(&self, history: &[Message]) -> Built {
        // 请求头部 = 本机 SYSTEM_PROMPT + 历史里已有的 system 备注（Plan C 的摘要消息走
        // 这一路），但去掉与 SYSTEM_PROMPT 重复的那条，避免同一提示词进两次。
        let mut head: Vec<Message> = vec![Message::system_note(SYSTEM_PROMPT)];
        head.extend(
            history
                .iter()
                .filter(|m| m.role == Role::System)
                .filter(|m| m.text_of() != SYSTEM_PROMPT)
                .cloned(),
        );
        // 头部只估一次就够，靠的是 `estimate_tokens` 的**逐消息相加**性质（见该函数：
        // 每条各加 `MSG_TOKEN_OVERHEAD` 与各块权重，块之间没有任何耦合项）。
        let head_est = estimate_tokens(&head);

        // R148：历史图片的降级点**只有这一处**，跑在窗口循环之前、作用在整段非 system
        // 历史上。放在循环之后（拼好的请求上）会让 `keep` 按「图片仍在」的预算裁掉更多文本，
        // 并让 `est_input_tokens` 虚高于实际发出的请求。
        // 这里 clone 成 owned `Vec<Message>` 而不是继续用 `Vec<&Message>`：降级要改块内容，
        // 而 `history` 是调用方（Task 10 的循环）的数据，就地改等于偷偷把「模型看到的图」
        // 和「用户界面/库里存的」分叉掉，那份分叉不可逆。
        let mut conversational: Vec<Message> = history
            .iter()
            .filter(|m| m.role != Role::System)
            .cloned()
            .collect();
        degrade_old_images(&mut conversational, KEEP_IMAGE_RECENT);

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
            while start < conversational.len() && has_tool_result(&conversational[start]) {
                start += 1;
            }
            // keep 由最终的 start 反推：被 advance 跳过的孤儿工具结果同样要计入 dropped。
            keep = conversational.len() - start;
            // 判据算的是 `head_est + 尾部切片`，而不是「把 head 与 tail 拼成一坨再整体估」：
            // 拼接要在每一轮复制整段尾部，而降级后的尾部仍可能带着 `KEEP_IMAGE_RECENT` 张图、
            // 单张 base64 上限 5 MB（`image_prep.rs` 的 `MAX_BASE64_BYTES`）—— 最多 40 轮
            // × 十几 MB 就是几百 MB 级的纯 memcpy，还发生在第一个字节出门之前。
            // 两者数值相等（相加性，见上），所以出口只需 clone 一次。
            let est = ((head_est + estimate_tokens(&conversational[start..])) as f64
                * self.calibration) as usize;
            // `keep <= 1` 而非 `keep == 1`：keep 可以是 0（空历史、window_messages: 0、
            // 或 budget_tokens: 0），此时 `keep -= 1` 会 usize 下溢并在 debug 直接 panic。
            if est <= budget || keep <= 1 {
                // est 与 messages 都出自 advance 之后的同一个 `start`，所以 est 必然等于
                // 实际发出那条请求的粗估（`a_cut_request_reports_the_estimate...` 钉这条）。
                let mut messages = head.clone();
                messages.extend_from_slice(&conversational[start..]);
                return Built {
                    messages,
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

/// 保留的最近图片张数（按**图片出现顺序**数，一条消息可以带多张）。
pub const KEEP_IMAGE_RECENT: usize = 3;

/// 降级后留在原位的可见痕迹。刻意不是空串：图片静默消失会让用户以为模型看到了图。
const DEGRADED_IMAGE_NOTE: &str = "[图片已移除以节省上下文]";

/// 把除最近 `keep_recent` **张**之外的历史图片块替换成 [`DEGRADED_IMAGE_NOTE`] 文本块。
///
/// 按图片出现顺序而不是消息下标计数：一条消息可以带多张图，按消息数保留会让真实张数
/// 浮动（「一轮 3 张图」+ `keep_recent = 3` 会留下 4 张），而 `PNG_TOKENS` 是按张收钱的。
/// 只替换 `ContentBlock::Image`，同条消息的 `Text` 块一律不动 —— 把用户的文本降级掉
/// 比把图片降级掉更糟，而且是不可逆的信息丢失。
///
/// **调用位置只能在 [`ContextBuilder::build`] 的窗口循环之前、作用在整段历史上**
/// （R148）：循环用 `estimate_tokens(&cand)` 决定 `keep`，`Built.est_input_tokens` 又是
/// 界面状态条显示的数。降级若在 est 之后，滑窗会按「图片仍在」的预算多裁掉几轮文本，
/// est 也会虚高到与实际发出的请求不符。同一个 build 里不许有第二个降级点。
///
/// 不碰 `ContentBlock::ToolResult` 里的 `ToolOutput::Image`：Plan A 的 registry 为空，
/// 工具结果进历史今天不可达，为它扩实现会让「图片降级」和「工具结果图片」两套规则混在
/// 一起（R150）。工具结果图片的降级随 Plan B 的工具注册一起做。
pub fn degrade_old_images(msgs: &mut Vec<Message>, keep_recent: usize) {
    let mut image_positions: Vec<(usize, usize)> = Vec::new();
    for (mi, m) in msgs.iter().enumerate() {
        for (bi, b) in m.blocks.iter().enumerate() {
            if matches!(b, ContentBlock::Image { .. }) {
                image_positions.push((mi, bi));
            }
        }
    }
    // `saturating_sub`：图片总数不足 keep_recent 时一个都不许降级（`len - keep` 会下溢）。
    let keep_from = image_positions.len().saturating_sub(keep_recent);
    for &(mi, bi) in &image_positions[..keep_from] {
        msgs[mi].blocks[bi] = ContentBlock::Text {
            text: DEGRADED_IMAGE_NOTE.into(),
        };
    }
}

// 覆盖 spec :503 交给本片的「预算计算」与「丢弃兜底不报错」两条；
// 「图片降级」由 Task 20 落在本文件（见 `degrade_old_images` 与 `mod image_degradation`），
// 「摘要失败降级」仍归切片 5。

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
        // 差额按这句话**自己的**标点个数算，而不是写死 3：改句子不会让它因为
        // 一个看不出来的原因变红（每个中文标点正好是 1 字符 = 1 token，
        // 且去掉标点不改变非 CJK 字符数，所以 `other.div_ceil(4)` 那段不贡献差额）。
        let punctuated = "今天天气不错，我们出门走走、去看看展览。";
        let bare: String = punctuated
            .chars()
            .filter(|c| !matches!(c, '，' | '、' | '。'))
            .collect();
        let marks = punctuated
            .chars()
            .filter(|c| matches!(c, '，' | '、' | '。'))
            .count();
        assert!(marks > 0, "夹具里必须真的有标点，否则这个等式什么都不断言");
        assert_eq!(
            estimate_tokens(&[u(punctuated)]),
            estimate_tokens(&[u(&bare)]) + marks
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

    #[test]
    fn the_three_image_weights_are_pinned_to_numbers() {
        // `>= 700` 那种「够高就行」的断言挡不住把 PNG 从 900 改成 701、
        // 或把工具图片与 JPEG 悄悄改成同一个值：这里把三个权重钉成数字。
        let cost = |b: ContentBlock| {
            estimate_tokens(&[Message {
                role: Role::User,
                blocks: vec![b],
            }]) - MSG_TOKEN_OVERHEAD
        };
        assert_eq!(
            cost(ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Png,
            }),
            PNG_TOKENS
        );
        assert_eq!(
            cost(ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Jpeg,
            }),
            JPEG_TOKENS
        );
        assert_eq!(
            cost(ContentBlock::Image {
                data: "/9j/AAAA".into(),
                mime: ImageMime::Jpeg,
            }),
            cost(ContentBlock::Image {
                data: "A".repeat(100_000),
                mime: ImageMime::Jpeg,
            }),
            "非 PNG 也一样不按长度计价"
        );
        assert_eq!(
            cost(ContentBlock::ToolResult {
                call_id: "c1".into(),
                is_error: false,
                output: ToolOutput::Image {
                    data: "iVBOR".into(),
                    mime: ImageMime::Png,
                },
            }),
            TOOL_IMAGE_TOKENS
        );
        assert_eq!(PNG_TOKENS, 900);
        assert_eq!(JPEG_TOKENS, 800);
        assert_eq!(TOOL_IMAGE_TOKENS, 900);
        assert_ne!(
            JPEG_TOKENS, PNG_TOKENS,
            "两种 mime 必须给不同的价，否则分支是白写的"
        );
    }

    #[test]
    fn a_tool_call_and_a_tool_result_are_not_priced_at_zero() {
        // 结构上「什么都不收」是最容易被静默引入的一条退化
        // （`ContentBlock::ToolCall { .. } => 0`）：带工具的历史会被估成几乎免费，
        // 于是滑窗再也不丢东西，请求长度一路撞到 400。
        let call = Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "fs.read".into(),
                input: json!({"path": "a.txt"}),
            }],
        };
        let result = Message::tool("c1", ToolOutput::text("文件内容在这里"), false);
        assert!(
            estimate_tokens(std::slice::from_ref(&call)) > MSG_TOKEN_OVERHEAD,
            "ToolCall 的 name + input 必须收钱"
        );
        assert!(
            estimate_tokens(std::slice::from_ref(&result)) > MSG_TOKEN_OVERHEAD,
            "ToolResult 的 output 文本必须收钱"
        );
        // 收的钱就是那条算式的值，不是「大于 0」这种弱断言：
        // name `"fs.read"` = 7 字符 → div_ceil(4) = 2；input 序列化成
        // `{"path":"a.txt"}` = 16 个非 CJK 字符 → 16 ÷ 4 = 4；合计 6，再加每条消息的 4。
        assert_eq!(estimate_tokens(&[call]), MSG_TOKEN_OVERHEAD + 2 + 4);
        // output 文本 `"文件内容在这里"` = 7 个 CJK 字 → 7，再加每条消息的 4。
        assert_eq!(estimate_tokens(&[result]), MSG_TOKEN_OVERHEAD + 7);
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
        // 只断言「丢了一些」和尾巴内容钉不住切口位置：这里补上起点。
        // 200 条 + 窗口 40 → 起点必须是「轮 160」。
        assert!(
            built.messages[1].text_of().contains("轮 160"),
            "窗口 40 时切口必须落在轮 160，实际是 {:?}",
            built.messages[1].text_of()
        );
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
        // 这条测试的存在意义就是「绑住它的是预算而不是窗口」，所以断言必须钉在
        // 真实预算 = budget_tokens - reserve_output 上；写 20_000 的话，
        // 忘记减 reserve_output 的退化（预算 20_000 → 留 4 条 = 16_020）照样通过。
        assert!(
            total <= 20_000 - 4_096,
            "超预算 {total}（预算 = 20_000 - 4_096 = 15_904）"
        );
        assert!(built.messages.len() < 40);
    }

    #[test]
    fn a_cut_request_reports_the_estimate_of_the_messages_it_actually_built() {
        // 判据 = 「`est_input_tokens` 就是这次发出去的那条请求的粗估」，而且必须测在
        // **真的裁了**的路径上：不裁的路径里 `messages` 就是整段历史，两者天然相等，
        // 测不出「循环里的算式和出口拼装的清单不一致」这类错。这条不一致不是洁癖：
        // 切片 5 的校准要算 `input_tokens ÷ est_input_tokens`（`set_calibration` 的文档），
        // est 虚高 → factor 长期 < 1 → 预算越放越宽，最终把请求顶到端点的 400。
        //
        // 三种块都在预算里：`SYSTEM_PROMPT`、历史里保留的 system 备注（`notes`，它住在
        // system 与 tail **之间**，最容易漏算）、以及带图的 tail（图是定值 900，与
        // base64 长度无关 —— 见 `image_blocks_cost_a_fixed_high_amount_not_by_base64_length`）。
        let cfg = ContextConfig {
            reserve_output: 100,
            budget_tokens: 1_000,
            window_messages: 40,
        };
        let mut hist = vec![
            Message::system_note(&"历史里的摘要备注，它同样占预算。".repeat(4)),
            Message::user(vec![
                ContentBlock::Text {
                    text: "看一下这张图".into(),
                },
                ContentBlock::Image {
                    data: "AAAA".into(),
                    mime: ImageMime::Png,
                },
            ]),
        ];
        for i in 0..12 {
            hist.push(u(&("句子".repeat(150) + &format!(" {i}"))));
        }
        let built = ContextBuilder::new(cfg).build(&hist);
        assert!(
            built.dropped > 1,
            "本例必须走到裁剪，否则没测到循环（dropped={})",
            built.dropped
        );
        assert_eq!(
            built.est_input_tokens,
            estimate_tokens(&built.messages) as u32,
            "est 与实际发出的消息对不上"
        );
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

        let mut tiny = ContextBuilder::new(cfg.clone());
        tiny.set_calibration(0.05);
        let mut floor = ContextBuilder::new(cfg);
        floor.set_calibration(0.2);

        // 与下限 0.2 的构建结果逐字节相同 == 0.05 已被抬到 0.2，无需再断言一条
        // `>= raw * 0.1`（那只是把同一个结论换个方向再说一遍，Minor 9 裁定删除）。
        assert_eq!(
            tiny.build(&hist).est_input_tokens,
            floor.build(&hist).est_input_tokens
        );
    }

    #[test]
    fn a_non_finite_calibration_keeps_the_previous_factor() {
        // Minor 7：NaN 会穿过 clamp（`f64::clamp` 对 NaN 返回 NaN），
        // 而 `(x as f64 * NaN) as usize` 饱和成 0 —— 每条估算变 0、预算彻底失效，
        // 正是 set_calibration 文档承诺禁止的那个后果。inf 同理（除法可得 inf）。
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 1000,
            window_messages: 40,
        };
        let hist: Vec<Message> = (0..4).map(|_| u(&"测".repeat(300))).collect();
        let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];

        // 默认系数 1.0 之下：坏输入必须让 build() 的表现逐字节不变。
        for factor in bad {
            let mut b = ContextBuilder::new(cfg.clone());
            b.set_calibration(factor);
            let poisoned = b.build(&hist);
            let untouched = ContextBuilder::new(cfg.clone()).build(&hist);
            assert_eq!(
                poisoned.est_input_tokens, untouched.est_input_tokens,
                "{factor} 不许把估算清零"
            );
            assert_eq!(poisoned.messages.len(), untouched.messages.len());
        }

        // 已校准到 0.2 之后再喂 NaN：保留的还是 0.2 的表现。
        let mut b = ContextBuilder::new(cfg);
        b.set_calibration(0.2);
        let kept = b.build(&hist);
        b.set_calibration(f64::NAN);
        let after_nan = b.build(&hist);
        assert_eq!(after_nan.est_input_tokens, kept.est_input_tokens);
        assert_eq!(after_nan.messages.len(), kept.messages.len());
        assert_eq!(kept.messages.len(), 5, "0.2 下四条全留（夹具前提）");
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

// ---- Task 20：历史图片降级 ----

/// 历史图片必须在**组装请求前**被降级成文本占位，只保留最近 `KEEP_IMAGE_RECENT` 张。
/// 两条动机：① 单图固定 900/800 token，长对话不降级会一路撞到 400；
/// ② 多数兼容端点不接受出现在非最近位置的历史图片。
#[cfg(test)]
mod image_degradation {
    use super::*;
    use crate::agent::types::ImageMime;

    /// 一条「文本 + 图片」的用户消息 —— Task 10 循环真实产出的形态（图片挂在用户消息上，
    /// 而不是只有工具结果里），用的是 `agent::types` 的真实变体。
    fn with_image(tag: &str) -> Message {
        Message::user(vec![
            ContentBlock::Text { text: tag.into() },
            ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Png,
            },
        ])
    }

    const PLACEHOLDER: &str = "[图片已移除以节省上下文]";

    #[test]
    fn only_the_most_recent_images_survive_to_the_request() {
        // 钉成字面量：本测试其余断言的期望值都是从 `KEEP_IMAGE_RECENT` 推出来的，
        // 不钉住常量的话「把 3 改成 1」会让实现和断言一起搬家（假绿）。
        assert_eq!(KEEP_IMAGE_RECENT, 3, "保留张数本身要有独立锚点");

        // 35 条带图历史（brief 原文是 10 条）：10 条离预算太远，降级与否都不截窗，
        // 于是 R151 要的 `dropped == 0` 谁都骗不动。35 条按未降级计价 31785 > 预算 27904
        // 会截窗、按降级后计价 3363 ≪ 预算不截窗，正好把「降级必须在循环之前」钉死。
        let hist: Vec<Message> = (0..35).map(|i| with_image(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        let imgs = built
            .messages
            .iter()
            .filter(|m| {
                m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }))
            })
            .count();
        assert_eq!(
            imgs, KEEP_IMAGE_RECENT,
            "历史图片必须降级，否则聊到 20 轮突然 400 且极难排查"
        );
        assert_eq!(
            built.dropped, 0,
            "这条测的是降级，不是截窗；截窗把图片裁掉了也会一样绿"
        );
        // R148：降级跑在窗口循环**之前**，所以 est 必须是「降级之后」的数 ——
        // 它是界面状态条要显示的那个数，也必须与真正发出的请求一致。
        // 把降级挪到 return 前的 `cand` 上，est 就会按「图片仍在」计价而虚高。
        assert_eq!(
            built.est_input_tokens,
            estimate_tokens(&built.messages) as u32,
            "est_input_tokens 必须描述实际发出的（已降级的）消息"
        );
    }

    #[test]
    fn degraded_image_leaves_an_explicit_placeholder_not_an_empty_text() {
        let hist: Vec<Message> = (0..6).map(|i| with_image(&format!("轮 {i}"))).collect();
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        let old = &built.messages[1];
        let t = old.text_of();
        assert!(
            t.contains(PLACEHOLDER),
            "必须留下可见痕迹，否则用户以为模型看到了图"
        );
    }

    #[test]
    fn degradation_never_drops_the_text_of_a_message() {
        // 降级只许替换 Image 块，不许连带把用户自己的文本一起换掉/清空。
        let hist: Vec<Message> = vec![
            with_image("轮 1"),
            with_image("轮 2"),
            with_image("轮 3"),
            with_image("轮 4"),
        ];
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        for m in built.messages.iter().filter(|m| m.role == Role::User) {
            assert!(m.text_of().starts_with("轮 "), "文本必须原样保留");
        }
        // 夹具必须真的发生降级，否则上面那个循环只是在复述「没降级时文本还在」，
        // 本测试就永远测不到它想测的那件事（同 a_sentence_with_punctuation… 的夹具守卫）。
        assert!(
            built
                .messages
                .iter()
                .any(|m| m.text_of().contains(PLACEHOLDER)),
            "4 张图 + 保留 3 张，必须真的有一条被降级"
        );
    }

    #[test]
    fn estimated_tokens_drop_after_degradation() {
        // R152：base64 长度不参与计价（PNG_TOKENS 是固定值），所以夹具用 "AAAA" 就够，
        // 断言与写 200_000 个字符时逐字相同，只是省下内存。
        let big = || {
            Message::user(vec![
                ContentBlock::Text { text: "x".into() },
                ContentBlock::Image {
                    data: "AAAA".into(),
                    mime: ImageMime::Png,
                },
            ])
        };
        let hist: Vec<Message> = (0..8).map(|_| big()).collect();
        let with_all = estimate_tokens(&hist);
        let mut degraded = hist.clone();
        degrade_old_images(&mut degraded, KEEP_IMAGE_RECENT);
        assert!(
            estimate_tokens(&degraded) < with_all,
            "降级必须真的把粗估拉下来，否则滑窗照旧多裁文本"
        );
    }

    /// 一条消息里的**图片块张数**。`count_images` 与它分开写是有意的：这两个数在
    /// 「一条消息带多张图」的夹具上才会分叉，而那条分叉正是下面第一条测试的全部内容。
    fn images_of(m: &Message) -> usize {
        m.blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count()
    }

    fn count_images(msgs: &[Message]) -> usize {
        msgs.iter().map(images_of).sum()
    }

    fn img_png() -> ContentBlock {
        ContentBlock::Image {
            data: "AAAA".into(),
            mime: ImageMime::Png,
        }
    }

    #[test]
    fn the_cap_counts_images_not_messages_when_one_message_carries_several() {
        // 6 张图 / 额度 3：降级必须落在「图片出现顺序」的前 3 张 —— 「轮 1」、「轮 2」
        // 和这条四图消息里的第 1 张，保留它自己的后 3 张。
        // 若实现按**消息下标**保留最近 3 条，这条消息的 4 张会整条保命，请求里变 5 张图，
        // 而 PNG_TOKENS 按张收钱 —— 症状正是本任务要消掉的「聊到 20 轮突然 400」。
        let four = Message::user(vec![
            ContentBlock::Text {
                text: "一次四张".into(),
            },
            img_png(),
            img_png(),
            img_png(),
            img_png(),
        ]);
        let hist = vec![with_image("轮 1"), with_image("轮 2"), four];
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(
            count_images(&built.messages),
            KEEP_IMAGE_RECENT,
            "跨消息也必须总共只留 KEEP_IMAGE_RECENT 张"
        );
        let last = built.messages.last().unwrap();
        assert_eq!(
            images_of(last),
            3,
            "同一条消息内部也要从最旧那张开始降级，不是整条保命"
        );
        assert_eq!(
            last.blocks.len(),
            5,
            "只换块内容、不删块：1 文本 + 1 占位 + 3 图片"
        );
        assert!(
            built.messages[1].text_of().contains(PLACEHOLDER),
            "「轮 1」的图必须被降级并留下痕迹"
        );
        assert!(
            built.messages[2].text_of().contains(PLACEHOLDER),
            "「轮 2」的图同样"
        );
        assert_eq!(built.dropped, 0, "同 R151：这条测的是降级，不是截窗");
    }

    #[test]
    fn fewer_images_than_the_cap_degrades_nothing_and_never_underflows() {
        // `keep_from` 用 `saturating_sub`：写成 `len - keep_recent` 在「2 张 < 保留 3 张」
        // 时 usize 下溢成 18446744073709551615，`&image_positions[..keep_from]` 当场 panic。
        // 那是「用户上传第一张图就崩」的路径，不是边角。
        let hist = vec![with_image("轮 1"), with_image("轮 2")];
        let built = ContextBuilder::new(ContextConfig::default()).build(&hist);
        assert_eq!(count_images(&built.messages), 2, "未超额度时一张都不许动");
        assert!(
            !built
                .messages
                .iter()
                .any(|m| m.text_of().contains(PLACEHOLDER)),
            "没降级就不许出现占位文字"
        );
        assert!(built.messages.iter().all(|m| images_of(m) <= 1));
        // `keep_recent == 0` 的另一头：额度为零时全额降级，但不许吃掉文本。
        let mut all = hist.clone();
        degrade_old_images(&mut all, 0);
        assert_eq!(count_images(&all), 0, "keep_recent=0 必须一张不留");
        assert!(
            all.iter()
                .filter(|m| m.role == Role::User)
                .all(|m| m.text_of().starts_with("轮 ")),
            "全额降级也只替换图片块"
        );
    }
}
