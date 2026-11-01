// 上下文组装与滑窗（spec 5.3）。决定「哪些消息进请求」与「这批消息粗估多少 token」。
// 本文件不得 `use tauri`；预算计算、丢弃兜底都不依赖网络，可全量单测。

use super::types::{ContentBlock, ImageMime, Message, Role};

/// 每条消息的固定结构开销（role 名 + JSON 框 + 分隔符）。
/// 提为常量是为了让估算比例的测试写成精确等式：区间断言在这个开销被调整时
/// 会漂成假绿，而这里每一项都是确定的整数。
pub const MSG_TOKEN_OVERHEAD: usize = 4;

pub const SYSTEM_PROMPT: &str = "你是 LFTTDO，一个运行在桌面端的轻量助手。回答简洁、直接，中文优先。可用工具时优先使用工具获取事实，不要凭记忆编造；不确定就说不确定。";

/// 固定追加在系统提示**末尾**的「工作方式」段。
///
/// 为什么必须独立于 `SYSTEM_PROMPT` / 人设：`ContextConfig::system_prompt` 非空时是
/// **整段替换**内置提示词（人设模型靠它生效），于是「先调工具再回答」这条规矩会跟着
/// 人设一起被抹掉 —— 实测症状就是本地 8B 明明有 `fetch_url` / `use_skill` 却直接凭记忆作答。
/// 规矩属于外壳，不属于人设，所以它由会话追加、任何人设都替换不掉。
pub const TOOL_POLICY: &str = "## 工作方式\n\
 - 需要事实、最新信息、文件或网页内容时，**先调用工具**拿到结果再回答；没有依据就说不确定，不要凭记忆编造。\n\
 - 技能：与技能列表描述相符时，先调用 use_skill 读出正文，再按正文步骤执行；正文已经给出来的不必重复读取。\n\
 - 例：用户说“把这段润色一下”，技能列表里有「翻译润色」⇒ 先 use_skill(name=\"翻译润色\")，拿到正文后再照它回答。\n\
 - 文件（仅当工具列表里有 read_file / write_file / edit_file 时）：改动已有文件前先用 read_file 看清原文；只改一段时优先 edit_file（old_string 必须唯一），新建或整文件重写才用 write_file。只改用户明确要求的文件，写完回报路径。\n\
 - MCP 工具与内置工具同等对待：能用就先用它核实，再根据返回结果作答。\n\
 - 一次调用不够就继续调用，直到有依据可说。";

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
    /// 压缩触发线：**占预算的比例**（spec 5.3 默认 0.85）。留给 15% 余量是为了
    /// 让压缩发生在两次对话之间、而不是把请求顶到 400 之后再补救。
    /// 读它的是 [`ContextBuilder::compaction_plan`]（T40 的纯决策，T41 落成方法）；
    /// 发起摘要与落库的是 T41。
    /// `build()` 今天不读它 —— 滑窗用的仍是全额预算。
    pub compact_threshold: f32,
    /// 用户自定义的系统提示词（设置里那一栏）。空串 ⇒ 用 [`SYSTEM_PROMPT`]。
    /// 有值时**整段替换**默认提示词：人设模型（如 `xiaosongqi`）靠它生效。
    pub system_prompt: String,
    /// 压缩时保留原文的最近轮数 K（spec 5.3 默认 10）。K=0 意味着连刚说完的那轮
    /// 都被摘要掉 ⇒ 当场失忆，所以 `Settings::normalize` 夹 `1..=50`。
    /// 「一轮」的定义只写在 [`ContextBuilder::compaction_plan`] 的文档注释里。
    pub keep_turns: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            reserve_output: 4096,
            budget_tokens: 32_000,
            window_messages: 40,
            compact_threshold: 0.85,
            keep_turns: 10,
            system_prompt: String::new(),
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
    /// 长期记忆的常驻备注(画像 + 召回,spec §5)。整段替换而非逐条追加:
    /// 每轮发送都是「按当前消息现算一份」,上一轮的召回不该渗进这一轮
    /// —— 渗进来等于召回结果跨轮存活,而检索口径每轮都该刷新。
    /// 计价与注入都走 `build()` 的 head(自动进 `est_input_tokens`);
    /// `compaction_plan` **看不见**它们(压缩判定跑在备注装配之前,时序见 ipc.rs
    /// 的 run_turn)—— 有意的已知漂移:备注有独立上限,量级远小于阈值留的 15% 余量。
    memory_notes: Vec<String>,
    /// 本轮工具声明的粗估 token（name + description + input_schema）。工具声明不进
    /// `messages`，但**进请求体**：不计入就会系统性低估，MCP 工具多时压缩触发太晚。
    tool_tokens: usize,
}

impl ContextBuilder {
    pub fn new(cfg: ContextConfig) -> Self {
        Self {
            cfg,
            calibration: 1.0,
            memory_notes: Vec::new(),
            tool_tokens: 0,
        }
    }

    /// 装入本轮的记忆备注(画像一条 + 召回一条,由 run_turn 的装配段拼好)。
    /// 空表 = 无记忆,行为与从未调用过逐字节一致(有测试钉着)。
    pub fn set_memory_notes(&mut self, notes: Vec<String>) {
        self.memory_notes = notes;
    }

    /// 装入本轮工具声明的粗估 token（由装配点按同一份注册表算出）。
    /// 默认 0 = 没有工具声明，行为与旧版本逐字节一致。
    pub fn set_tool_tokens(&mut self, tokens: usize) {
        self.tool_tokens = tokens;
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
        self.build_with_extra(history, "")
    }

    /// 与 [`Self::build`] 相同，但在系统提示末尾追加 `extra_system`（技能索引、
    /// 已加载正文、外壳工作方式）。追加段**参与**预算裁剪与 token 粗估：
    /// 先 build 再往外贴，会让一条已经按预算裁好的请求凭空超线。
    pub fn build_with_extra(&self, history: &[Message], extra_system: &str) -> Built {
        // 请求头部 = 本机 SYSTEM_PROMPT + 记忆备注(画像/召回,spec §5) + 历史里已有的
        // system 备注（Plan C 的摘要消息走这一路），但去掉与 SYSTEM_PROMPT 重复的那条，
        // 避免同一提示词进两次。记忆备注在摘要备注**之前**:「当前相关性」先于「过去发生了什么」。
        let mut prompt = if self.cfg.system_prompt.trim().is_empty() {
            SYSTEM_PROMPT.to_string()
        } else {
            self.cfg.system_prompt.clone()
        };
        if !extra_system.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(extra_system);
        }
        let prompt = prompt.as_str();
        let mut head: Vec<Message> = vec![Message::system_note(prompt)];
        head.extend(self.memory_notes.iter().map(|n| Message::system_note(n)));
        head.extend(
            history
                .iter()
                .filter(|m| m.role == Role::System)
                .filter(|m| m.text_of() != prompt)
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
            // 工具声明与消息一起乘校准系数：它同样是真的会发出去的输入 token。
            let est = ((head_est + estimate_tokens(&conversational[start..]) + self.tool_tokens)
                as f64
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

/// 工具声明的粗估：每个声明按 name + description + 序列化后的 schema 计字，
/// 再加一份消息级结构开销。工具不进 `messages`，但一定进请求体。
pub fn tool_defs_tokens(defs: &[crate::model::provider::ToolDef]) -> usize {
    defs.iter()
        .map(|d| {
            MSG_TOKEN_OVERHEAD
                + text_tokens(&d.name)
                + text_tokens(&d.description)
                + text_tokens(&d.input_schema.to_string())
        })
        .sum()
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
                    // R317：多块结果按块累加（图片按张收钱，文本按字）。递归处理是为了将来
                    // 谁真嵌了一层也不静默算成 0 —— `map_result` 今天不产生嵌套。
                    super::types::ToolOutput::Multi { parts } => {
                        parts.iter().map(tokens_of_tool_output).sum()
                    }
                },
            };
        }
    }
    n
}

fn tokens_of_tool_output(output: &super::types::ToolOutput) -> usize {
    match output {
        super::types::ToolOutput::Text { text } => text_tokens(text),
        super::types::ToolOutput::Image { .. } => TOOL_IMAGE_TOKENS,
        super::types::ToolOutput::Multi { parts } => parts.iter().map(tokens_of_tool_output).sum(),
    }
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

/// 一条**完整备注**(画像/召回)进 head 的预算口径:与 `estimate_tokens` 同一套字符
/// 计价,外加每条消息的结构开销。供 ipc 侧在装配时执行 `MEMORY_NOTE_TOKEN_CAP`;
/// 不直接暴露 `text_tokens` —— 消费方要的是「这条进 head 花多少」,不是裸文本估价。
pub(crate) fn note_tokens(s: &str) -> usize {
    MSG_TOKEN_OVERHEAD + text_tokens(s)
}

/// 该消息是否携带 `ToolResult` 块（Task 5 的 `Message::tool(..)` 就是这一形态）。
/// 需要它的是**两处切口**，都在本文件：`build()` 的滑窗 advance，以及
/// [`ContextBuilder::compaction_plan`] 的摘要刀口（外加「一轮」的定义要用它排除工具结果行）。
/// 丢弃位置归本片所有，Task 6 的协议映射不反过来依赖这里 —— 也**不许**为压缩再造
/// 第二个孤儿工具结果谓词，两处必须共用这一个（否则一份改了另一份没改，400 会回来）。
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
pub fn degrade_old_images(msgs: &mut [Message], keep_recent: usize) {
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

// ---- Task 40：`ContextBuilder::compaction_plan`（纯决策，只回答「哪一段该被摘要掉」）----
//
// **与 Task 20 的图片降级划清边界（Step 0，要求写进代码而不是只写进报告）**：
// 「历史图片降级为 `[图片已移除以节省上下文]`」这件事**归 T20 所有**，落在
// [`ContextBuilder::build`] 返回之前的 [`degrade_old_images`] + [`KEEP_IMAGE_RECENT`]。
// 本任务**不实现它、不重命名它、也不调用它**：`compaction_plan` 只决定压哪一段，
// 一次模型调用都不发起、一个字节都不写盘（那是 T41）。
// 两者的唯一交集是**读同一份 `history`**，而且方向是安全的那一侧：因为这里刻意不
// 走降级，带旧图的历史会按「图片仍在」计价，粗估只会**偏高** ⇒ 压缩发生在更早而不是更晚，
// 不会出现「以为没超线、实际超了」。T41 落库时摘要的是「即将被删的那些行」，
// 那些图片行的 `blocks` 原样躺在库里（`db.rs` 不做降级）。
// 这条边界不依赖 T20 是否落地（派发到本文件时它已经在了，见 `mod image_degradation`）。
//
// **T41 §2a 的形状裁定（原 Step 3.5 的第二方案）**：这里曾经是
// `pub fn plan_compaction(history, cfg, calibration)` + 一只 `pub fn calibration()` getter。
// getter 在仓库里没有真实来源 —— `run_turn` 每次发送都重建构建器，所以拿到的恒是 1.0，
// 传下去等于把常量换个名字搬一遍。于是决策被原样搬进 `impl ContextBuilder`，
// 读**自己的** `self.cfg` 与 `self.calibration`，getter 随之删除（R110 的那处授权例外
// 到此兑现，不留双份）。五步、常量、判据、注释逐字未动。
// 好处：哪一天校准真值开始跨轮活下来（那是另一片，要动 state 与库），这个方法一行都不用改。

/// 「最老的一半，**至少 4 轮**」里的 4（spec :359 压缩批量 N）。
/// 由 [`ContextBuilder::compaction_plan`] 私有使用，不进 `ContextConfig`：它是「压得划算吗」这条
/// 判断的一部分，不是用户能设的参数（用户能设的是阈值与 K，都在 `Settings` 里）。
const MIN_COMPACT_TURNS: usize = 4;

/// 「这次压哪一段」的答案。
///
/// **下标口径**：`drop_from`/`drop_to` 是**对话序列**（= `history` 去掉 `Role::System`
/// 那几条之后的序列）里的位置，**不是 `history` 的下标**；`dropped` 与 `source.len()`
/// 都是这段区间里的**消息条数**。理由是 T41 的 `Db::compact_session` 要按「第几条对话
/// 消息」定位待删的行，而 system 摘要备注不参与那个计数（R69 让它们整行跳过事件投影）。
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// 恒为 0：Plan C 只压最老一段。「中段摘要」不在 spec 的参数表里，那是二期。
    pub drop_from: usize,
    /// 区间右端（不含）。它一定落在一次发送的开头，见 [`ContextBuilder::compaction_plan`] 文档第 ④ 步。
    pub drop_to: usize,
    /// 被压掉的**消息条数** = `drop_to - drop_from` = `source.len()`。
    pub dropped: usize,
    /// 要交给模型摘要的那些消息（`conv[drop_from..drop_to]` 的克隆）。
    pub source: Vec<Message>,
}

impl ContextBuilder {
    /// 决定「要不要压、压哪一段」。**纯决策**：不调模型、不做 IO、不写库（那全是 T41 的活），
    /// 也不就地改 `history`。
    ///
    /// **「一轮」的定义（Step 3.3 要求选定并写明）**：一次发送 = 一条 `Role::User` 且
    /// **不携带 `ToolResult`** 的消息开启一轮，往后一直吃到下一条这种消息之前，含它自己的
    /// assistant 行与工具行；`conv` 的下标 0 无条件开启一轮。
    /// 为什么必须把工具结果排除在「开轮」之外：`Message::tool(..)` 的 role 是 `Role::User`
    /// （types.rs 刻意不新增 `Role::Tool`，见该构造器的注释），所以「任何一条 user 都算一轮」
    /// 会把一个工具回合数成两轮 ⇒ 轮数翻倍 ⇒ 一半翻倍 ⇒ **`keep_turns: 10` 实际只保住 5 轮
    /// 真实对话**，而且切口正好最容易落在 `assistant(ToolCall)` 与它的结果之间。
    /// 症状就是 `compaction_takes_the_oldest_half…_keeps_k_verbatim` 那组断言红。
    /// 轮数与 `SessionConfig.max_turns` **无关**：那个是编排循环的回合上限，这个是压缩的计量单位。
    ///
    /// 五步：① 划分备注/对话（与 [`ContextBuilder::build`] 同一套）；② 越线才压；
    /// ③ 批量算术；④ 刀口安全；⑤ `drop_from` 恒 0。
    ///
    /// 校准系数读的是**自己的** `self.calibration`（那条路径已经把系数夹在 `0.2..=3.0`
    /// 并挡掉非有限值）。这里再挡一次非有限值：`(粗估 * NaN) as usize` 饱和成 0
    /// ⇒ 阈值永久静默失效，那正是 `set_calibration` 文档注释明文禁止的后果。
    pub fn compaction_plan(&self, history: &[Message]) -> Option<CompactionPlan> {
        let cfg = &self.cfg;
        // ① 与 `build()` 同一套划分：`Role::System` 是备注（T41 写的摘要备注就走这一路）。
        let notes: Vec<&Message> = history.iter().filter(|m| m.role == Role::System).collect();
        let conv: Vec<&Message> = history.iter().filter(|m| m.role != Role::System).collect();

        // ② 触发判定。预算 = 配置上限 - 输出预留；`saturating_sub` 照 `build()` 的写法
        // （`budget < reserve` 时预算是 0，不是 usize 下溢）。
        let budget = cfg.budget_tokens.saturating_sub(cfg.reserve_output) as usize;
        // 粗估按**逐消息相加**算（`estimate_tokens` 没有块间耦合项，见其文档注释），
        // 而不是把 head+notes+conv 拼成一坨整体估：那要 clone 整段历史，而带图历史
        // 单张 base64 上限 5 MB，几百 MB 级纯 memcpy 花在一个字都不发出去的判断上。
        let mut raw = estimate_tokens(&[Message::system_note(SYSTEM_PROMPT)]);
        raw += notes.iter().map(|m| estimate_one(m)).sum::<usize>();
        raw += conv.iter().map(|m| estimate_one(m)).sum::<usize>();
        // 工具声明同样占输入预算；压缩判定必须看见它，否则大 schema 下「估算没超、
        // 实际已经超」会一直拖到端点 400。
        raw += self.tool_tokens;
        let calibration = if self.calibration.is_finite() {
            self.calibration
        } else {
            1.0
        };
        let est = (raw as f64 * calibration) as usize;
        // 阈值是**比例**，所以触发线 = 预算 × `compact_threshold`。判据写成 `est <= line`
        // 才不压，即线**上**不算越线（spec :357「超过即压缩」）。
        // 浮点：`f32` 的 0.85 存成 0.8500000238418579（**向上**舍入），`as usize` 又向下截断，
        // 两者抵消的结果是 `line == floor(0.85 × 预算)` 精确成立：`0.85 × budget` 的小数部分
        // 只可能是 `k/20`（最大 0.95），而舍入误差在预算 ≤ 32000 时不到 0.001 个 token，
        // 永远跨不过那个 0.05 的格。所以线不会因为浮点漂移到「晚压」那一侧。
        let line = (budget as f64 * f64::from(cfg.compact_threshold)) as usize;
        if est <= line {
            return None;
        }

        // ③ 批量算术，全程以「轮」为单位，最后一步才换成消息下标。
        let starts = turn_start_indices(&conv);
        let turns = starts.len();
        let batch = (turns / 2).max(MIN_COMPACT_TURNS);
        // 最近 K 轮必须留原文，而且「留原文」永远优先于「凑够 4 轮」；再夹一次
        // 「至少留最近一轮」：`keep_turns: 0` 只可能来自手写的 `ContextConfig`
        // （`Settings::normalize` 夹的是 `1..=50`），而「把整段历史压成一句摘要、原文
        // 一条不剩」比 K=0 更糟。两个 `saturating_sub` 都是防 usize 下溢（K > turns 是
        // 常态：刚压完一次之后 turns 就小于 K）。
        let batch = batch
            .min(turns.saturating_sub(cfg.keep_turns))
            .min(turns.saturating_sub(1));
        // 夹完够不到下限 ⇒ 这一压既没到「至少 4 轮」也不会真省下多少，压它只是白烧一次
        // 摘要调用（而且下一轮照样超预算 —— 超的是本轮那条长输入）。让 `build()` 的滑窗兜底。
        if batch < MIN_COMPACT_TURNS {
            return None;
        }

        // ④ 刀口：第 `batch` 轮的起始下标 == 前 `batch` 轮的消息条数。
        // `starts[batch]` 必定存在：上面 `.min(turns - 1)` 保证了 `batch ≤ turns - 1`。
        let drop_from = 0usize;
        let mut drop_to = starts[batch];
        // 复用 `build()` 的同一个谓词（不新造第二个孤儿判定）：切口不许落在「带 ToolCall 的
        // assistant」与「它的 tool 结果」之间，否则 T41 发出的请求被端点判 400（spec :364）。
        // 按轮切本来就切不到工具结果上（工具结果不开新轮），所以这一圈是**双保险**，
        // 守的是「以后有人放宽轮的定义」。终止性同 `build()` 的 advance：单调前进、
        // 被 `drop_to < conv.len()` 夹住，最多 len 步。
        while drop_to < conv.len() && has_tool_result(conv[drop_to]) {
            drop_to += 1;
        }
        // 同样按构造不可达（切口是轮边界，`batch ≥ 1` 且 `batch ≤ turns - 1`）。留着是因为
        // `conv[..drop_to]` 一旦被推到整段历史，就变成「把用户全部内容压成一句摘要」——
        // 那种事必须整个不做，而不是做一半。
        if drop_to == 0 || drop_to >= conv.len() {
            return None;
        }

        // ⑤ 只有被压的那一段需要 owned 副本（它就是 T41 的摘要输入），尾部原地不动。
        let source: Vec<Message> = conv[..drop_to].iter().map(|m| (*m).clone()).collect();
        Some(CompactionPlan {
            drop_from,
            drop_to,
            dropped: drop_to - drop_from,
            source,
        })
    }
}

/// 单条消息的粗估。`estimate_tokens` 是逐消息相加的（见其文档注释），所以这里借
/// `slice::from_ref` 复用**同一个计价口径**，不复制任何规则（副本迟早会和 `PNG_TOKENS`
/// 那类常量分叉）。
fn estimate_one(m: &Message) -> usize {
    estimate_tokens(std::slice::from_ref(m))
}

/// 一轮的起始下标（相对 `conv`，即去掉 `Role::System` 之后的序列）。
/// 定义只在 [`ContextBuilder::compaction_plan`] 的文档注释里写一遍，这里是那份定义的实现；
/// `mod compaction_planning` 里的测试镜像按同一条定义**独立**再算一次，两份对撞
/// 才测得出「K=10 保住 10 轮还是 5 轮」。
fn turn_start_indices(conv: &[&Message]) -> Vec<usize> {
    conv.iter()
        .enumerate()
        // 下标 0 无条件开一轮：历史可能以一条孤立 `ToolResult` 开头（`build()` 的
        // advance 就是为它写的），那条也得归属到某一轮里，否则 `turns` 会少 1、
        // 而 `starts` 与消息下标对不上。
        .filter(|(i, m)| *i == 0 || (m.role == Role::User && !has_tool_result(m)))
        .map(|(i, _)| i)
        .collect()
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

    // ---- 压缩预算的三个参数（Plan C T39）----

    #[test]
    fn compaction_defaults_match_the_spec_table() {
        // 三条都钉成字面量而不是从 `Default` 推：这一条的全部作用就是给 `Default`
        // 一个独立锚点，`compact_threshold`/`keep_turns` 还没有消费者（T41 才用），
        // 漏进 `Default` 是静默的 —— 所有 `..Default::default()` 夹具会拿到 0。
        let cfg = ContextConfig::default();
        assert_eq!(
            (cfg.reserve_output, cfg.budget_tokens, cfg.window_messages),
            (4096, 32_000, 40)
        );
        assert_eq!(
            cfg.compact_threshold, 0.85,
            "spec 5.3：预算的 85% 触发，留余量别卡线"
        );
        assert_eq!(cfg.keep_turns, 10, "spec 5.3：压缩时最近 10 轮留原文");
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
                ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    /// 工具声明要同时进「估算」和「裁剪判据」：只进估算不进判据的话，
    /// 估算超线但一条消息都不裁，请求照样超预算。
    #[test]
    fn tool_declarations_count_against_the_budget_and_the_estimate() {
        let cfg = || ContextConfig {
            reserve_output: 100,
            budget_tokens: 1_000,
            window_messages: 40,
            ..Default::default()
        };
        let hist: Vec<Message> = (0..8)
            .map(|i| u(&("测".repeat(280) + &format!(" {i}"))))
            .collect();
        let plain = ContextBuilder::new(cfg()).build(&hist);
        let mut with_tools = ContextBuilder::new(cfg());
        with_tools.set_tool_tokens(400);
        let tooled = with_tools.build(&hist);
        assert!(
            tooled.messages.len() < plain.messages.len(),
            "400 token 的工具声明必须让裁剪提前：plain={} tooled={}",
            plain.messages.len(),
            tooled.messages.len()
        );
        assert_eq!(
            tooled.est_input_tokens,
            estimate_tokens(&tooled.messages) as u32 + 400,
            "est 必须包含工具声明"
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
            ..Default::default()
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
            ..Default::default()
        };
        let built = ContextBuilder::new(cfg).build(&[u(&"测".repeat(10))]);
        assert_eq!(built.messages[0].role, Role::System);
        assert!(built.messages.len() <= 2, "最多 system + 1 条");

        // keep == 0 且 est > budget：真正会 usize 下溢的组合
        let zero_window = ContextConfig {
            reserve_output: 0,
            budget_tokens: 0,
            window_messages: 0,
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

// ---- 长期记忆备注的注入(spec 2026-10-25 §5)----

#[cfg(test)]
mod memory_injection {
    use super::*;

    fn u(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn notes_of(built: &Built) -> Vec<String> {
        built
            .messages
            .iter()
            .take_while(|m| m.role == Role::System)
            .map(|m| m.text_of())
            .collect()
    }

    #[test]
    fn memory_notes_ride_between_the_prompt_and_the_history_notes() {
        // 顺序是硬性的:系统提示词 → 记忆(画像+召回,本轮现算) → 历史摘要备注 → tail。
        // 记忆是「当前相关性」,摘要备注是「过去发生了什么」,越靠近提示词的越新鲜。
        let mut b = ContextBuilder::new(ContextConfig::default());
        b.set_memory_notes(vec![
            "[用户画像]用户是 Rust 开发者".into(),
            "[长期记忆]用户住在上海".into(),
        ]);
        let hist = vec![
            Message::system_note("早期对话摘要:聊了天气"),
            u("第一"),
            u("第二"),
        ];
        let built = b.build(&hist);
        assert_eq!(
            notes_of(&built),
            vec![
                SYSTEM_PROMPT.to_string(),
                "[用户画像]用户是 Rust 开发者".to_string(),
                "[长期记忆]用户住在上海".to_string(),
                "早期对话摘要:聊了天气".to_string(),
            ]
        );
    }

    #[test]
    fn memory_notes_are_priced_into_the_estimate_of_the_actual_request() {
        // spec §8 不变量 1:记忆与画像计入 est —— est 是状态条显示的数,
        // 也必须描述实际发出的请求(镜像 a_cut_request_reports_the_estimate… 的判据,
        // 且同样测在**真的裁了**的路径上)。
        let cfg = ContextConfig {
            reserve_output: 100,
            budget_tokens: 1_000,
            window_messages: 40,
            ..Default::default()
        };
        let hist: Vec<Message> = (0..12)
            .map(|i| u(&("句子".repeat(150) + &format!(" {i}"))))
            .collect();

        let mut with = ContextBuilder::new(cfg.clone());
        with.set_memory_notes(vec!["画像很长".repeat(40)]);
        let built = with.build(&hist);
        assert!(built.dropped > 1, "夹具前提:必须走到裁剪,否则没测到循环");
        assert_eq!(
            built.est_input_tokens,
            estimate_tokens(&built.messages) as u32,
            "est 与实际发出的消息对不上(含记忆备注)"
        );

        // 同一份历史,带记忆的请求粗估必须严格大于不带的 —— head 多了那一条。
        let bare = ContextBuilder::new(cfg).build(&hist);
        let with_est = estimate_tokens(&with.build(&hist).messages);
        let bare_est = estimate_tokens(&bare.messages);
        assert!(
            with_est > bare_est,
            "记忆备注必须真的进预算:{with_est} vs {bare_est}"
        );
    }

    #[test]
    fn memory_notes_survive_the_window_and_never_count_as_dropped() {
        // spec :365 同一条规矩的推广:备注是 head 的一部分,滑窗只裁对话行。
        let mut b = ContextBuilder::new(ContextConfig::default());
        b.set_memory_notes(vec!["[用户画像]常驻".into()]);
        let hist: Vec<Message> = (0..200).map(|i| u(&format!("轮 {i}"))).collect();
        let built = b.build(&hist);
        assert_eq!(built.messages[1].text_of(), "[用户画像]常驻");
        assert_eq!(
            built.dropped,
            200 - ContextConfig::default().window_messages,
            "dropped 只数对话行,备注与摘要备注都不算"
        );
    }

    #[test]
    fn empty_memory_notes_behave_exactly_like_never_setting_them() {
        let hist = vec![u("第一"), u("第二")];
        let fresh = ContextBuilder::new(ContextConfig::default()).build(&hist);
        let mut cleared = ContextBuilder::new(ContextConfig::default());
        cleared.set_memory_notes(Vec::new());
        let cleared = cleared.build(&hist);
        assert_eq!(fresh.messages, cleared.messages);
        assert_eq!(fresh.est_input_tokens, cleared.est_input_tokens);
        assert_eq!(fresh.dropped, cleared.dropped);
    }

    #[test]
    fn memory_notes_follow_a_custom_system_prompt_the_same_way() {
        // 人设模型(自定义 system_prompt 整段替换)下记忆照样注入 —— 顺序不变。
        let mut b = ContextBuilder::new(ContextConfig {
            system_prompt: "你是小宋祺。".into(),
            ..Default::default()
        });
        b.set_memory_notes(vec!["[长期记忆]用户在学日语".into()]);
        let built = b.build(&[]);
        assert_eq!(built.messages[0].text_of(), "你是小宋祺。");
        assert_eq!(built.messages[1].text_of(), "[长期记忆]用户在学日语");
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

// ---- Task 40：`compaction_plan` 的内联测试 ----

/// 全部断言都是「哪一段该被摘要掉」的算术：阈值、批量、下限、保留 K、刀口安全。
/// 这里**不**测模型调用、不测落库、不测事件（都属 T41），所以夹具一律是内存里的
/// `Vec<Message>`，没有 `Db`、没有 `ScriptedModel`、没有 sink。
#[cfg(test)]
mod compaction_planning {
    use super::*;
    use crate::agent::types::ToolOutput;
    use serde_json::json;

    /// 单条消息的固定成本 = 4 结构开销 + 800 个 ASCII 字符 ÷ 4 = **204 token**。
    /// 刻意用 ASCII 而不是中文：中文/标点的口径已经在 `mod tests` 里钉死了，
    /// 本模块要的是「每条同价、总数可算」，不再重测一遍权重。
    const MSG_COST: usize = MSG_TOKEN_OVERHEAD + 800 / 4;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// **`n` 的单位是「条」不是「轮」**：生成 n 条 user/assistant 交替消息（从 user 起），
    /// 即 `n / 2` 轮。这个单位是 Step 2 那两条断言逼出来的 ——
    /// `CompactionPlan.dropped` 与 `drop_from`/`drop_to` 数的都是**对话序列里的消息条数/下标**
    /// （见 `compaction_plan` 文档注释），而 `assert_eq!(p.dropped, 20)` 与
    /// `assert_eq!(p.source.len(), p.dropped)` 只有在「40 条 = 20 轮、压掉 10 轮 = 20 条」
    /// 这组数上才同时成立；把 n 读成轮数（80 条）时前者会变成 40。
    fn user_assistant_pairs(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                msg(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    &"x".repeat(800),
                )
            })
            .collect()
    }

    /// Task 10 循环真实产出的一个完整工具回合：`user` → `assistant(ToolCall)` →
    /// `tool 结果` → `assistant 收尾`，共 4 条、**只算 1 轮**。
    /// 关键事实：`Message::tool(..)` 的 role 是 `Role::User`（types.rs:101-114 刻意不新增
    /// `Role::Tool`），所以「一条 user 就开一轮」这种定义会把工具结果误当成新一轮的开头 ——
    /// 那正是 Step 3.3 点名的「K=10 实际只保 5 轮」的来源。
    fn tool_round(id: &str) -> Vec<Message> {
        vec![
            msg(Role::User, "帮我读文件"),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolCall {
                    id: id.into(),
                    name: "fs.read".into(),
                    input: json!({"path": "a.txt"}),
                }],
            },
            Message::tool(id, ToolOutput::text("文件内容"), false),
            msg(Role::Assistant, "读完了"),
        ]
    }

    /// 6 轮 / 16 条：中段（轮 2）一个工具回合、**尾部（轮 5）也一个工具回合**。
    /// 尾部那个不是装饰：`keep_turns = 1` 时留在原文里的正好是最后一轮，
    /// 于是「call 与 result 落在刀口同一侧」这条断言才真的有得查（否则尾部一条工具结果都没有，
    /// 循环体一次都不执行，测试永远是绿的）。
    fn pairs_with_a_tool_round_in_the_middle() -> Vec<Message> {
        let mut v = user_assistant_pairs(4);
        v.extend(tool_round("c1"));
        v.extend(user_assistant_pairs(4));
        v.extend(tool_round("c2"));
        v
    }

    /// 与实现同口径的**校准前**粗估：`SYSTEM_PROMPT` 一条 + 全部非 system 历史。
    /// 拆成两段相加而不是拼成一坨整体估，靠的是 `estimate_tokens` 的逐消息相加性质
    /// （见该函数与 `build()` 里同一条注释）。
    fn est_of(hist: &[Message]) -> usize {
        estimate_tokens(&[Message::system_note(SYSTEM_PROMPT)]) + estimate_tokens(hist)
    }

    /// 预算小到任何一条真实历史都必然越线，于是「越线」这个前提被固定住，
    /// 测试里唯一剩下的变量就是 `MIN_COMPACT_TURNS`。
    /// `keep_turns: 0` 同样是刻意的：K 不许成为「不压」的理由（生产里 `Settings::normalize`
    /// 夹的是 `1..=50`，0 只可能来自手写的 cfg），这样 `a_two_turn_session…` 的绿/红
    /// 测到的才是下限本身，而不是 K 的副作用。名字里的 long 指「历史相对预算太长」。
    fn long_budget_cfg() -> ContextConfig {
        ContextConfig {
            budget_tokens: 400,
            reserve_output: 0,
            keep_turns: 0,
            ..Default::default()
        }
    }

    /// 把预算钉成「这份夹具的粗估原样」，于是 est 正好等于 **1.0 × 预算** ——
    /// 触发带 `(0.85, 1.0]` 的最右端，也是「刚好会触发压缩」的那一档。
    /// 从夹具实际算出来而不写常量（Step 2 夹具说明给的理由）：写死的数会随
    /// `ContextConfig::default()` 或 `SYSTEM_PROMPT` 长度漂移，悄悄变成「压根没过阈值」
    /// 或「远超阈值」，那时 `the_cut_never…` 就在测别的东西了。
    /// `keep_turns: 1`：6 轮压 4 轮、剩 2 轮，尾部正好含 `c2` 那个工具回合。
    fn cfg_at_threshold() -> ContextConfig {
        let raw = est_of(&pairs_with_a_tool_round_in_the_middle()) as u32;
        ContextConfig {
            budget_tokens: raw,
            reserve_output: 0,
            keep_turns: 1,
            ..Default::default()
        }
    }

    /// 「一轮」的**测试侧镜像**：定义与 `compaction_plan` 文档注释里那条逐字对应
    /// （一条不带 `ToolResult` 的 user 消息开启一轮；下标 0 无条件开一轮）。
    /// 刻意不直接调用实现的 `turn_start_indices`：K=10 到底保住 10 轮还是 5 轮，
    /// 只有两份独立计算对撞才测得出来（Step 3.3 点名的就是这个症状）。
    fn turn_starts(msgs: &[&Message]) -> Vec<usize> {
        msgs.iter()
            .enumerate()
            .filter(|(i, m)| *i == 0 || (m.role == Role::User && !has_tool_result(m)))
            .map(|(i, _)| i)
            .collect()
    }

    /// 刀口两侧的工具回合完整性检查：返回被拆开的 `call_id`。
    /// 只查尾部会漏掉「assistant 进了摘要、结果留在尾部」这种**正好触发 400** 的切法，
    /// 所以两侧都查：每个 call_id 的 `ToolCall` 与 `ToolResult` 必须在同一侧。
    fn split_tool_rounds(conv: &[&Message], cut: usize) -> Vec<String> {
        let mut calls: Vec<(String, bool)> = Vec::new();
        let mut results: Vec<(String, bool)> = Vec::new();
        for (i, m) in conv.iter().enumerate() {
            let in_source = i < cut;
            for b in &m.blocks {
                match b {
                    ContentBlock::ToolCall { id, .. } => calls.push((id.clone(), in_source)),
                    ContentBlock::ToolResult { call_id, .. } => {
                        results.push((call_id.clone(), in_source));
                    }
                    _ => {}
                }
            }
        }
        let mut bad: Vec<String> = calls
            .iter()
            .filter(|(id, side)| !results.iter().any(|(r, s)| r == id && s == side))
            .map(|(id, _)| id.clone())
            .chain(
                results
                    .iter()
                    .filter(|(id, side)| !calls.iter().any(|(c, s)| c == id && s == side))
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        bad.sort();
        bad.dedup();
        bad
    }

    #[test]
    fn the_fixture_cost_is_the_number_the_arithmetic_assumes() {
        // 夹具守卫：本模块所有期望值（20 条、8 条、10 条…）都是从「一条消息 204 token、
        // 一轮两条 = 408」推出来的。这条一旦红，说明 `MSG_TOKEN_OVERHEAD` 或估算口径搬家了，
        // 而**不是** `compaction_plan` 错了 —— 先修这里，否则剩下六条会一起红得莫名其妙。
        assert_eq!(estimate_tokens(&user_assistant_pairs(1)), MSG_COST);
        assert_eq!(estimate_tokens(&user_assistant_pairs(2)), 2 * MSG_COST);
        assert_eq!(MSG_COST, 204, "单条成本是字面量 204，不是「大约」");
        let hist = user_assistant_pairs(40);
        assert_eq!(
            estimate_tokens(&hist),
            40 * MSG_COST,
            "40 条 = 8160 token：相加性在这里必须真的成立"
        );
        assert_eq!(turn_starts(&hist.iter().collect::<Vec<_>>()).len(), 20);
        let tooly = pairs_with_a_tool_round_in_the_middle();
        assert_eq!(
            turn_starts(&tooly.iter().collect::<Vec<_>>()).len(),
            6,
            "16 条但只有 6 轮：两个工具回合各自 4 条算**一轮**"
        );
    }

    #[test]
    fn no_compaction_while_inside_the_threshold() {
        let builder = ContextBuilder::new(ContextConfig {
            budget_tokens: 4096,
            reserve_output: 512,
            ..Default::default()
        });
        let hist = user_assistant_pairs(3); // 短历史
        assert!(
            est_of(&hist) <= 3046,
            "夹具守卫：粗估 {est} 必须真的在阈内，否则这条测的是别的东西",
            est = est_of(&hist)
        );
        assert!(
            builder.compaction_plan(&hist).is_none(),
            "预算 3584 × 0.85 = 3046，远没到 ⇒ 一次都不该压，压了就是在无谓烧一次模型调用"
        );
    }

    #[test]
    fn the_threshold_leaves_headroom_so_we_never_land_exactly_on_the_wire() {
        // Step 1 原文只写了注释，这里换成真断言。要测的是 spec :357
        // 「超过即压缩，留出余量避免刚好卡线」这条**理由本身**，也就是触发带必须
        // 正好是 `(0.85, 1.0] × 预算`：线上不压、线内要压、顶到硬预算也要压。
        // 缺「带内要压」这一点，「留余量」就只是在巧合；Step 4 判据 (b)
        // （阈值改 1.0 且 `>` 改 `>=`）红的也正是这个点。
        let cfg = ContextConfig {
            budget_tokens: 4000,
            reserve_output: 0,
            keep_turns: 1,
            ..Default::default()
        };
        let builder = ContextBuilder::new(cfg.clone());
        let budget = 4000usize;
        // 与实现同一个表达式。`f32` 的 0.85 是 0.8500000238418579（向上舍入），
        // `as usize` 再向下截断，两个误差抵消后线**精确**等于 floor(0.85 × 预算) = 3400
        // （推导与预算上限的论证见 `compaction_plan` 里那条注释）。
        // 钉成字面量而不是「从 cfg 推」：这条就是那个 3400 的独立锚点，
        // 漂成 3399 或 3401 都意味着阈值口径变了，而下面的四个点会一起搬位置。
        let line = (budget as f64 * f64::from(cfg.compact_threshold)) as usize;
        assert_eq!(
            line, 3400,
            "0.85 阈值（含 f32 舍入）要有独立锚点，不能只靠 Default"
        );
        let mid = (line + budget) / 2;
        assert!(line < mid && mid < budget, "带内那个点必须真的在带内");
        for (est, should_fire) in [
            (line - 1, false),
            (line, false),
            (mid, true),
            (budget, true),
        ] {
            let hist = conv_costing(est);
            let fired = builder.compaction_plan(&hist).is_some();
            assert_eq!(
                fired,
                should_fire,
                "est = {est}（预算 {budget}、线 {line}）应当 {}，实际 {fired}",
                if should_fire { "压" } else { "不压" }
            );
        }
    }

    /// 造一份「校准 1.0 时粗估正好是 `target`」的非 system 历史：4 轮打底，
    /// 再用一条文本长度可调的 user 消息补齐差额（一条消息 = 4 开销 + `ceil(len/4)`，
    /// 差额 ≥ 4 时能**精确**补上，全程不碰浮点）。
    /// 为什么拿长度当旋钮而不是拿 `calibration` 当旋钮：校准系数会被
    /// `set_calibration` 夹进 `0.2..=3.0`，用凑出来的系数会测到一个生产里送不进来的值。
    fn conv_costing(target: usize) -> Vec<Message> {
        let mut hist = user_assistant_pairs(8);
        let used = est_of(&hist);
        assert!(
            target >= used + 4,
            "夹具底子 {used} 已经贵过目标 {target}，filler 补不出差额"
        );
        let gap = target - used;
        hist.push(msg(Role::User, &"y".repeat(4 * (gap - MSG_TOKEN_OVERHEAD))));
        assert_eq!(
            est_of(&hist),
            target,
            "夹具必须精确落到 target，否则测的是别的位置"
        );
        hist
    }

    #[test]
    fn compaction_takes_the_oldest_half_but_at_least_four_turns_and_keeps_k_verbatim() {
        let cfg = ContextConfig {
            budget_tokens: 1024,
            reserve_output: 0,
            keep_turns: 10,
            ..Default::default()
        };
        let hist = user_assistant_pairs(40);
        let p = ContextBuilder::new(cfg.clone())
            .compaction_plan(&hist)
            .expect("远超阈值");
        assert_eq!(
            p.dropped, 20,
            "最老的一半：40 轮压掉 20 轮，不是「压到刚好卡线」——否则每轮都要重压一次"
        );
        assert!(p.drop_to <= 40 - 10, "最近 K=10 轮必须留在原文里");
        assert_eq!(
            p.source.len(),
            p.dropped,
            "source 就是要交给模型摘要的那些消息，不多不少"
        );
        // ↓ 这三条把「一轮」的定义钉住（Step 3.3：选错的症状就是 K=10 只保 5 轮）。
        // 40 条 = 20 轮，一半 = 10 轮 = **20 条**，所以尾部还是 10 轮 / 20 条。
        // 把轮数定义成「消息条数」的实现会切在同一个下标上（数值巧合），但尾部
        // 只剩 5 轮 ⇒ 下面第一条红；把轮数定义成「任何 user 都算一轮」的实现
        // 也一样红在这里。
        let conv: Vec<&Message> = hist.iter().collect();
        assert_eq!(
            turn_starts(&conv[p.drop_to..]).len(),
            cfg.keep_turns,
            "K=10 保住的是 10 **轮**，不是 10 条消息"
        );
        assert_eq!(conv[p.drop_to..].len(), 2 * cfg.keep_turns);
        assert_eq!(conv[p.drop_to].role, Role::User, "切口必须是一次发送的开头");
        assert_eq!(p.drop_from, 0, "Plan C 只压最老一段，不做中段摘要");
    }

    #[test]
    fn a_two_turn_session_is_not_worth_compacting_even_over_budget() {
        // MIN_COMPACT_TURNS=4 的正面：只有 2 轮却超预算时，压它只会把「唯一的历史」变成一句摘要，
        // 而下一轮照样超预算（超的是本轮那条长输入）。此时该让 build() 的滑窗去兜底。
        let hist = user_assistant_pairs(2);
        assert!(
            ContextBuilder::new(long_budget_cfg())
                .compaction_plan(&hist)
                .is_none()
        );
        // 单位提醒：`user_assistant_pairs(n)` 的 n 是**条**（见其文档注释），上面那条
        // 其实只有 1 轮；测试名字要的「2 轮」是 4 条这一条。再往前 3 轮同样压不动：
        // 一半只有 1 轮，抬到 MIN 就要求吃掉 4 轮 > 全部 3 轮。
        assert!(
            ContextBuilder::new(long_budget_cfg())
                .compaction_plan(&user_assistant_pairs(4))
                .is_none()
        );
        let three = user_assistant_pairs(6);
        assert!(
            ContextBuilder::new(long_budget_cfg())
                .compaction_plan(&three)
                .is_none()
        );
        // 夹具守卫：三条都必须**真的**越线，否则 `is_none` 测的是「没到阈值」而不是下限。
        let cfg = long_budget_cfg();
        let line = (cfg.budget_tokens as f64 * f64::from(cfg.compact_threshold)) as usize;
        assert!(
            est_of(&three) > line,
            "夹具没越线（{} > {line}）⇒ 上面三条全是白测",
            est_of(&three)
        );
    }

    #[test]
    fn the_batch_floor_is_four_turns_not_the_half_when_half_is_smaller() {
        // 6 轮：一半只有 3 轮，但 spec :359 写的是「最老的一半，**至少 4 轮**」。
        // 这条是 Step 4 判据 (a)（删掉 `.max(MIN_COMPACT_TURNS)`）唯一有牙的地方：
        // 40 轮/20 轮那两条夹具里 `max()` 根本没生效，删了照样绿。
        assert_eq!(MIN_COMPACT_TURNS, 4, "「至少 4 轮」这条下限本身要有锚点");
        let cfg = ContextConfig {
            budget_tokens: 1024,
            reserve_output: 0,
            keep_turns: 1,
            ..Default::default()
        };
        let hist = user_assistant_pairs(12); // 6 轮
        let p = ContextBuilder::new(cfg.clone())
            .compaction_plan(&hist)
            .expect("6 轮 ≥ MIN，越线就该压");
        assert_eq!(
            p.drop_to, 8,
            "批量被下限抬到 4 轮 = 8 条；纯按「一半」只压 3 轮 = 6 条"
        );
        assert_eq!(p.dropped, 8);
        assert_eq!(p.source.len(), 8);
        let conv: Vec<&Message> = hist.iter().collect();
        assert_eq!(
            turn_starts(&conv[p.drop_to..]).len(),
            2,
            "6 轮压 4 轮，尾部正好剩 2 轮"
        );
    }

    #[test]
    fn the_cut_never_lands_between_an_assistant_tool_call_and_its_result() {
        // `build()` 的 advance 在滑窗侧守的就是这条；摘要侧同样不能把
        // assistant(ToolCall) 收进 source 而把它的 ToolResult 留在 tail —— 那正是 spec :364 的 400。
        let hist = pairs_with_a_tool_round_in_the_middle();
        let p = ContextBuilder::new(cfg_at_threshold())
            .compaction_plan(&hist)
            .unwrap();
        let conv: Vec<&Message> = hist.iter().collect();
        assert!(
            !has_tool_result(conv[p.drop_to]),
            "切口后第一条不许是孤儿工具结果（has_tool_result 已在 context.rs，别新造一个）"
        );
        // 刀口必须落在**轮边界**（第 5 轮开头 = 下标 10），而不是任何「按条数取半」的位置。
        assert_eq!(p.drop_to, 10, "6 轮里压 4 轮：0..10 条");
        assert_eq!(p.dropped, 10);
        // 真正的验收项：两侧的工具回合都完好（`c1` 整段进摘要、`c2` 整段留原文）。
        assert_eq!(
            split_tool_rounds(&conv, p.drop_to),
            Vec::<String>::new(),
            "call 与它的 result 被刀口拆开 ⇒ T41 发出去就是 400"
        );
        assert_eq!(
            turn_starts(&conv[p.drop_to..]).len(),
            2,
            "keep_turns=1 之外只剩 2 轮原文，其中尾部那个就是 `c2` 工具回合"
        );
        assert!(
            conv[..p.drop_to].iter().any(|m| has_tool_result(m))
                && conv[p.drop_to..].iter().any(|m| has_tool_result(m)),
            "夹具守卫：两侧各得有工具结果，否则上面那条拆开检查是空转"
        );
    }

    #[test]
    fn a_non_finite_calibration_does_not_switch_the_threshold_off() {
        let hist = pairs_with_a_tool_round_in_the_middle();
        assert!(
            ContextBuilder::new(cfg_at_threshold())
                .compaction_plan(&hist)
                .is_some(),
            "夹具守卫：这条必须先真的越线"
        );
        // §2a 之后系数住在构建器里，所以这里的「喂坏值」走 `set_calibration` 这一道
        // 真实入口（它自己会挡掉非有限值并保留 1.0）。断言一个字没改：这条承诺的是
        // **行为** —— 坏系数不许把阈值关掉。`compaction_plan` 内部那道再挡一次非有限值的
        // 保险从此只能由 `set_calibration` 的过滤来兑现，两处口径仍然一致。
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut builder = ContextBuilder::new(cfg_at_threshold());
            builder.set_calibration(bad);
            assert!(
                builder.compaction_plan(&hist).is_some(),
                "非有限系数 {bad} 要当 1.0 处理：`(粗估 * NaN) as usize` 饱和成 0 ⇒ 阈值静默失效，\
                 正是 `set_calibration` 文档注释写明禁止的那个后果"
            );
        }
    }

    #[test]
    fn an_extra_system_tail_is_counted_in_the_estimate_and_the_budget() {
        let hist: Vec<Message> = (0..12)
            .map(|i| msg(Role::User, &("轮".repeat(120) + &format!("{i}"))))
            .collect();
        let cfg = ContextConfig {
            reserve_output: 0,
            budget_tokens: 1000,
            ..Default::default()
        };
        let plain = ContextBuilder::new(cfg.clone()).build(&hist);
        let tail = "## 技能\n- 翻译助手：中英互译\n\n## 本轮已加载的技能\n".to_string()
            + &"步".repeat(400);
        let with_tail = ContextBuilder::new(cfg).build_with_extra(&hist, &tail);

        assert!(
            with_tail.messages[0].text_of().ends_with(&tail),
            "追加段必须原样落在系统提示末尾"
        );
        assert!(
            with_tail.est_input_tokens > plain.est_input_tokens,
            "追加段计入粗估，否则 usage 校准与预算都会偏"
        );
        assert!(
            with_tail.messages.len() <= plain.messages.len(),
            "追加段挤占预算时只能少留历史，不能多塞：{} vs {}",
            with_tail.messages.len(),
            plain.messages.len()
        );
        assert!(
            with_tail
                .messages
                .iter()
                .any(|m| m.text_of().contains("轮")),
            "还有预算时不该把历史全部裁光"
        );
    }

    #[test]
    fn a_custom_system_prompt_replaces_the_builtin_one() {
        let custom = ContextBuilder::new(ContextConfig {
            system_prompt: "你是小宋祺。".into(),
            ..Default::default()
        });
        let built = custom.build(&[]);
        assert_eq!(built.messages[0].text_of(), "你是小宋祺。");
        let default = ContextBuilder::new(ContextConfig::default()).build(&[]);
        assert_eq!(default.messages[0].text_of(), SYSTEM_PROMPT);
    }
}
