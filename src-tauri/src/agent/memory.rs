// 长期记忆的模型侧管线(spec 2026-10-25 §4):抽取 → 合并(增/改/失效) → 画像归并。
// 与 summarize.rs 同一条约束:本文件不得 `use tauri`,模型调用只依赖
// `ModelProvider` + `CancellationToken`,假模型即可端到端测。
//
// 分工(与 store::memory 的边界):这里只产出「决定」(候选、四选一、新画像正文),
// 一个字节都不落库 —— 落库(含同事务)归 store::memory,编排归 ipc.rs(T4)。
// 失败语义:每一段的 Err 都只意味着「这一段没有结论」,调用方降级(记日志、
// 跳过该段),绝不允许拿半截结果当成功 —— 与「空摘要即 Err」同一条纪律。

use crate::agent::summarize::drain_text_stream;
use crate::agent::summarize::TextStreamOutcome;
use crate::agent::types::{ContentBlock, Message};
use crate::model::provider::{ChatRequest, ModelProvider};
use crate::store::memory::{MemoryRow, NewMemory};
use tokio_util::sync::CancellationToken;

/// 抽取提示词(spec §4.2 + 待办升级)。kind 口径:`fact`(事实/进行中的事)、
/// `preference`(可检索的稳定偏好)、`profile`(长线的身份与特质,只进画像不进检索库)、
/// `prospective`(未来某个时刻要做的事/承诺/截止日期,必须带 `due_at`)。
pub const EXTRACT_INSTRUCTION: &str = "\
从下面的对话里抽取「值得跨会话记住的用户事实」。只抽对话里明确说过的:\
身份与背景(职业、称呼、常打交道的人)、稳定偏好(格式、语言、工具习惯)、\
正在进行的事(项目名、截止时间、承诺)、未来要做的事(用户提到的待办、承诺、截止日期)。\
不抽:一次性闲聊、模型自己的推测、离开这段对话就没意义的内容。每条输出一个 JSON 对象,\
整体是数组,字段:kind(fact|preference|profile|prospective;长线的身份与特质用 profile,\
用户明确提到未来某个时刻要做的事用 prospective)、subject、predicate、\
object(能写成三元组的就填,写不出就填 null)、body(一句完整的中文句子)、\
confidence(0 到 1 的小数)、due_at(只有 prospective 需要:ISO 8601 本地时间,\
如 \"2026-11-01T09:00:00\";只知道日期就写 \"2026-11-01\";无法确定具体时间就填 null)。\
如果只是「我明天要去」这类没有可执行事情的行程,不要抽成 prospective。\
没有可抽的就输出 []。只输出 JSON,不要任何别的文字。";

/// 合并提示词(spec §4.3 + 强化):同键新旧对照四选一。
/// 四种动作的判据必须写死在这里 —— 模型分不清 ADD 与 REINFORCE 时,
/// 库里会出现「同一件事记了五遍」,而这正是长期记忆最难清理的形态。
pub const CONSOLIDATE_INSTRUCTION: &str = "\
下面是同一条记忆键(subject+predicate)的新旧对照,旧记忆可能已过时。只允许四选一,\
输出单个 JSON 对象:\
{\"action\":\"ADD\"}(新信息,与旧记忆并存,两者都值得记) 或 \
{\"action\":\"UPDATE\",\"id\":<旧记忆id>,\"body\":\"<合并成一句的新正文>\"}\
(旧记忆部分过时,需要改写正文) 或 \
{\"action\":\"INVALIDATE\",\"id\":<旧记忆id>}(新信息直接否定旧记忆) 或 \
{\"action\":\"REINFORCE\",\"id\":<旧记忆id>}\
(新候选与某条旧记忆实质是同一件事,只是措辞不同:不必新增、不必改写,只确认一次)。\
只输出 JSON。";

/// 画像归并提示词(spec §4.3):仍是单段、有上限、永不膨胀。
pub const PROFILE_INSTRUCTION: &str = "\
下面是用户的现有画像和刚抽取的新特质。把新特质合并进去,输出一份仍然是一段话的\
新画像:保留仍然成立的内容,吸收新特质,删掉被新信息取代的内容,不要评价、不要分点。\
只输出画像正文,不要任何前后缀。";

/// 低置信候选的丢弃线(spec §4.3)。
pub const MIN_CONFIDENCE: f64 = 0.7;

/// 画像字数上限的默认值(spec §3/§7;T5 的设置项以此为默认,钳制范围 100..=2000)。
pub const PROFILE_CHAR_CAP: usize = 500;

/// 抽取产物:一条候选,`NewMemory` 减去 session_id(来源归编排层补,单一真相)。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCandidate {
    pub kind: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub body: String,
    pub confidence: f64,
    /// 待办到期时刻(ms)。只有 `kind == "prospective"` 才可能有值 ——
    /// 解析不出时间的 prospective 在 [`keep_valid`] 里已被降级成 fact,
    /// 所以这里出现 `Some` 就一定能被 `due_prospective` 捞到。
    pub due_at: Option<i64>,
}

/// 合并四选一的答案。
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryDecision {
    /// 新键或无冲突:直接入库(带候选正文)。
    Add,
    /// 同键旧记忆改写正文。
    UpdateBody { id: i64, body: String },
    /// 同键旧记忆被否定。
    Invalidate { id: i64 },
    /// 同键旧记忆与新候选实质是同一件事(措辞不同):只把强度加一。
    /// 带 id:模型必须指名强化哪一条,ipc 层还要拿它核对「这条行真的存在」
    /// (指向不存在行的变更会让整个压缩事务失败)。
    Reinforce { id: i64 },
}

impl MemoryCandidate {
    /// 候选 → 写入形态的唯一边界:编排层只经这一处补 session_id,
    /// 「来源会话」字段不散落在各调用点手拼。
    pub fn into_new_memory(self, session_id: Option<String>) -> NewMemory {
        NewMemory {
            session_id,
            kind: self.kind,
            subject: self.subject,
            predicate: self.predicate,
            object: self.object,
            body: self.body,
            confidence: self.confidence,
            due_at: self.due_at,
        }
    }
}

/// 组装抽取请求:指令 + 待压段落**原样**,空工具、不带温度(与摘要请求同一条
/// 理由:temperature 是聊天风格旋钮,抽取要的是「不编造」)。
pub fn extraction_request(source: &[Message]) -> ChatRequest {
    let mut msgs = vec![Message::system_note(EXTRACT_INSTRUCTION)];
    msgs.extend(source.iter().cloned());
    ChatRequest {
        messages: msgs,
        tools: vec![],
        temperature: None,
    }
}

/// 剥掉模型常见的 ```json 围栏。只在确有围栏时动刀;剥完 trim,
/// 「围栏内外解析结果一致」由 `parse_candidates_accepts_raw_and_fenced…` 钉着。
fn strip_json_fence(s: &str) -> &str {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // 去掉 ```json / ``` 那一行
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
    let rest = rest.trim();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

/// 单条的中间形态:字段不齐是**丢条**不是炸批,所以先解析成 Value 再逐条试。
#[derive(serde::Deserialize)]
struct CandJson {
    kind: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    predicate: Option<String>,
    #[serde(default)]
    object: Option<String>,
    body: String,
    #[serde(default = "one")]
    confidence: f64,
    /// 模型给的 ISO 8601 字面量。**在这一层保持字符串**:解析失败要能区分
    /// 「模型没给」(null)与「给了但看不懂」,两者都降级,但只有后者说明提示词
    /// 没被遵守(日志/测试的取证点)。
    #[serde(default)]
    due_at: Option<String>,
}

fn one() -> f64 {
    1.0
}

/// 民用历法的闰年判据(格里高利历)。2000 是闰年(能被 400 整除),2100 不是 ——
/// 只写 `y % 4 == 0` 会把 2100-02-29 当成合法日期。
fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// 某年某月的天数。月份已由调用方校验过(1..=12)。
fn days_in_month(y: i64, m: u32) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// 公历日期 → 距 1970-01-01 的天数(Howard Hinnant 的 `days_from_civil`)。
/// 自己算而不引 chrono:只为这一个换算加一个依赖,会把整条构建链(含 CI 的
/// 预编译 NASM 路径)拖下水,而这段算法是纯整数运算、可被钉死日期的测试完全覆盖。
/// 实现要点:把 3 月当成一年的起点,400 年为一个 era —— 闰日就落在年末,
/// 「能被 4/100/400 整除」的三条规则不必单独分支。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // 3 月 → 0 ... 2 月 → 11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// 只收纯数字串:负号、空格、全角数字都不认(ISO 8601 的字面值里不该出现它们,
/// 放进来只会让「2026-1-1」这类模糊写法蒙混过关)。
fn all_digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// 把抽取侧给的 ISO 8601 字面量换成毫秒时间戳(纯函数,不引 chrono)。
///
/// 只认两种形状:`YYYY-MM-DD`(缺时间按 00:00:00)与 `YYYY-MM-DDTHH:MM:SS`。
/// 分隔符同时容忍 `T` 与空格(模型常写 `2026-11-01 09:00:00`,那是 RFC 3339
/// 允许的替代写法)。其他一切(空串、"明天"、月份 13、2 月 30 日、越界时分秒)
/// 一律 None —— 宁可把一条待办降级成普通事实,也不许按一个猜出来的时刻设闹钟。
///
/// **已知偏差(裁定记录)**:无时区的字面值**按 UTC 解释,不做本地时区换算**。
/// 用户在东八区时 `2026-11-01T09:00:00` 会被当成 UTC 09:00(本地 17:00),
/// 到期提醒因此早 8 小时。取这个取舍是为了不引新依赖;而提醒窗口按 24 小时算,
/// 8 小时的偏移仍落在「当天该提醒」的范围内。要修这一条,需要把本地偏移
/// 显式传进来(或用 chrono/time crate),属另一片切片的范围。
pub fn parse_iso8601_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    // 先挡非 ASCII:下面的切片按**字节**下标取年/月/日,非 ASCII 输入会在
    // 非字符边界上切,panic 起来比返回 None 难查得多。
    if !s.is_ascii() {
        return None;
    }
    let (date, time) = match s.len() {
        10 => (s, "00:00:00"),
        19 => match &s[10..11] {
            // 位置 10 必须是日期与时间的分隔符;缺了它(如 `2026-11-0109:00:00`)
            // 虽然长度够,但不是一个合法字面值。
            "T" | " " => (&s[..10], &s[11..]),
            _ => return None,
        },
        _ => return None,
    };

    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = all_digits(&date[0..4])?;
    let m = all_digits(&date[5..7])?;
    let d = all_digits(&date[8..10])?;
    if !(1..=12).contains(&m) {
        return None;
    }
    if d < 1 || d > days_in_month(y, m as u32) {
        return None;
    }

    let tb = time.as_bytes();
    if tb.len() != 8 || tb[2] != b':' || tb[5] != b':' {
        return None;
    }
    let hh = all_digits(&time[0..2])?;
    let mm = all_digits(&time[3..5])?;
    let ss = all_digits(&time[6..8])?;
    if hh > 23 || mm > 59 || ss > 59 {
        return None;
    }

    let days = days_from_civil(y, m as u32, d as u32);
    Some((days * 86_400 + hh * 3_600 + mm * 60 + ss) * 1_000)
}

/// kind 的合法集合(口径写在 EXTRACT_INSTRUCTION;不认识的丢条 —— 静默当 fact
/// 会把模型的胡话洗成正常数据,丢掉最多损失这一条)。
///
/// 两条边界规则都在这里收口:
/// - `prospective` 的时间解析不出来(包括模型给了 null)→ **降级成 fact**。
///   带着 prospective 落库而 due_at 为空,会变成一条「永不提醒的待办」:
///   用户以为记下了,系统永远不叫他 —— 最坏的半条状态。
/// - 非 prospective 即使模型硬塞了 due_at 也**丢掉**。与 store 层同一口径
///   (`kind != prospective` 不写 due_at),否则会造出一条「事实型闹钟」。
fn keep_valid(c: CandJson) -> Option<MemoryCandidate> {
    if !matches!(
        c.kind.as_str(),
        "fact" | "preference" | "profile" | "prospective"
    ) {
        return None;
    }
    if c.body.trim().is_empty() || c.confidence < MIN_CONFIDENCE {
        return None;
    }
    let (kind, due_at) = if c.kind == "prospective" {
        match c.due_at.as_deref().and_then(parse_iso8601_ms) {
            Some(ms) => (c.kind, Some(ms)),
            None => ("fact".to_string(), None),
        }
    } else {
        (c.kind, None)
    };
    Some(MemoryCandidate {
        kind,
        subject: c.subject,
        predicate: c.predicate,
        object: c.object,
        body: c.body,
        confidence: c.confidence,
        due_at,
    })
}

/// 解析抽取输出(纯函数)。防御规则:
/// - 容忍 ```json 围栏,围栏内外解析结果必须一致;
/// - 整体不是 JSON 数组 → Err(调用方跳过整段抽取,不产半批);
/// - 单条不合格(字段不齐、空 body、confidence < [`MIN_CONFIDENCE`]、kind 不认识)
///   → **丢弃该条**,不炸整批 —— 模型偶发一条格式错不该废掉整段抽取;
/// - `prospective` 的 `due_at` 解析失败(含 null)→ 该条降级为 `fact`,内容不丢
///   (见 [`keep_valid`]);非 prospective 的 `due_at` 一律忽略;
/// - `[]` → Ok(空表):「没什么值得记」是合法答案(spec §4.2)。
pub fn parse_candidates(raw: &str) -> Result<Vec<MemoryCandidate>, String> {
    let vals: Vec<serde_json::Value> =
        serde_json::from_str(strip_json_fence(raw)).map_err(|e| format!("抽取输出不是 JSON 数组:{e}"))?;
    Ok(vals
        .into_iter()
        .filter_map(|v| match serde_json::from_value::<CandJson>(v) {
            Ok(c) => keep_valid(c),
            Err(_) => None, // 单条字段不齐:丢条,同上
        })
        .collect())
}

/// 抽取一段对话(通常是即将被压缩删掉的 `plan.source`)。
pub async fn extract_memories(
    model: &dyn ModelProvider,
    source: &[Message],
    cancel: &CancellationToken,
) -> Result<Vec<MemoryCandidate>, String> {
    // 空段不烧调用:压缩钩子只在真有 plan 时进来,这里挡的是「空 source 也调一次」的浪费。
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let req = extraction_request(source);
    match drain_text_stream(model, req, cancel).await {
        TextStreamOutcome::Done(text) => parse_candidates(&text),
        TextStreamOutcome::Cancelled => Err("记忆抽取已取消".to_string()),
        TextStreamOutcome::Failed(m) => Err(format!("记忆抽取调用失败:{m}")),
    }
}

/// 合并请求:指令 + 新旧对照(候选 + 库内同键有效记忆,按 id 列出)。
pub fn consolidation_request(candidate: &MemoryCandidate, existing: &[MemoryRow]) -> ChatRequest {
    let comparison = serde_json::json!({
        "新候选": {
            "kind": candidate.kind,
            "subject": candidate.subject,
            "predicate": candidate.predicate,
            "object": candidate.object,
            "body": candidate.body,
            "confidence": candidate.confidence,
        },
        "库内同键旧记忆": existing
            .iter()
            .map(|r| serde_json::json!({"id": r.id, "body": r.body, "valid_from": r.valid_from}))
            .collect::<Vec<_>>(),
    });
    ChatRequest {
        messages: vec![
            Message::system_note(CONSOLIDATE_INSTRUCTION),
            Message::user(vec![ContentBlock::Text {
                text: comparison.to_string(),
            }]),
        ],
        tools: vec![],
        temperature: None,
    }
}

#[derive(serde::Deserialize)]
struct DecisionJson {
    action: String,
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    body: Option<String>,
}

/// 解析合并输出(纯函数)。UPDATE 必须带 id+非空 body,INVALIDATE 与 REINFORCE
/// 必须带 id;缺字段或 action 不认识 → Err(该候选跳过,库里不动)。
pub fn parse_decision(raw: &str) -> Result<MemoryDecision, String> {
    let d: DecisionJson = serde_json::from_str(strip_json_fence(raw))
        .map_err(|e| format!("合并输出不是 JSON 对象:{e}"))?;
    match d.action.as_str() {
        "ADD" => Ok(MemoryDecision::Add),
        "UPDATE" => match (d.id, d.body) {
            (Some(id), Some(body)) if !body.trim().is_empty() => {
                Ok(MemoryDecision::UpdateBody { id, body })
            }
            _ => Err("UPDATE 缺 id 或 body(空 body = 零信息落库)".to_string()),
        },
        "INVALIDATE" => match d.id {
            Some(id) => Ok(MemoryDecision::Invalidate { id }),
            None => Err("INVALIDATE 缺 id".to_string()),
        },
        // 缺 id 的 REINFORCE 是一次无处安放的「加一」:既不知道强化哪条行,
        // ipc 也无从核对它是否存在 —— 当 Err 跳过,不猜。
        "REINFORCE" => match d.id {
            Some(id) => Ok(MemoryDecision::Reinforce { id }),
            None => Err("REINFORCE 缺 id".to_string()),
        },
        other => Err(format!(
            "未知 action:{other},只允许 ADD/UPDATE/INVALIDATE/REINFORCE"
        )),
    }
}

/// 一条候选的合并。**existing 为空时不调模型**:无冲突即 ADD,
/// 这是大多数路径(新事实),省一次模型调用是 spec §10 风险表的第一条缓解。
pub async fn consolidate_memory(
    model: &dyn ModelProvider,
    candidate: &MemoryCandidate,
    existing: &[MemoryRow],
    cancel: &CancellationToken,
) -> Result<MemoryDecision, String> {
    if existing.is_empty() {
        return Ok(MemoryDecision::Add);
    }
    let req = consolidation_request(candidate, existing);
    match drain_text_stream(model, req, cancel).await {
        TextStreamOutcome::Done(text) => parse_decision(&text),
        TextStreamOutcome::Cancelled => Err("记忆合并已取消".to_string()),
        TextStreamOutcome::Failed(m) => Err(format!("记忆合并调用失败:{m}")),
    }
}

/// 画像归并请求:指令 + 现有画像 + 新特质逐条列出。
pub fn profile_request(current: &str, additions: &[MemoryCandidate]) -> ChatRequest {
    let shown = if current.trim().is_empty() {
        "(空)"
    } else {
        current
    };
    let list = additions
        .iter()
        .map(|c| format!("- {}", c.body))
        .collect::<Vec<_>>()
        .join("\n");
    ChatRequest {
        messages: vec![
            Message::system_note(PROFILE_INSTRUCTION),
            Message::user(vec![ContentBlock::Text {
                text: format!("现有画像:\n{shown}\n\n新特质:\n{list}"),
            }]),
        ],
        tools: vec![],
        temperature: None,
    }
}

/// 画像归并。输出按调用方给的 `char_cap` 截断（T5：编排层从
/// `Settings.memory_profile_char_cap` 现取，[`PROFILE_CHAR_CAP`] 只是该设置项的
/// 默认锚点 —— 本函数里没有第二份真相）。按**字符**不按字节（镜像标题截断的纪律）；
/// 截断用 `head_chars` 而不是 `truncate_display`：省略号是「给用户看的截断标记」
/// （预览/标题的口径），画像正文是给模型读的，补个「…」只会变成内容的一部分。
/// 空/纯空白输出 → Err:画像合并的合法结果永远非空,「合并成空」等于静默失忆。
pub async fn merge_profile(
    model: &dyn ModelProvider,
    current: &str,
    additions: &[MemoryCandidate],
    char_cap: usize,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let req = profile_request(current, additions);
    match drain_text_stream(model, req, cancel).await {
        TextStreamOutcome::Done(text) if !text.trim().is_empty() => {
            Ok(crate::text::head_chars(&text, char_cap).0)
        }
        TextStreamOutcome::Done(_) => Err("画像合并返回了空文本".to_string()),
        TextStreamOutcome::Cancelled => Err("画像合并已取消".to_string()),
        TextStreamOutcome::Failed(m) => Err(format!("画像合并调用失败:{m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, Role};
    use crate::agent::summarize::TextStreamOutcome;
    use crate::store::memory::NewMemory;
    use async_stream::stream;
    use async_trait::async_trait;
    use futures::Stream;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 与 summarize.rs 的 Recorder 同构:每次 stream() 记下请求快照,
    /// 「模型被叫了几次」只能从模型这一侧数。
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<ChatRequest>>>);

    impl Recorder {
        fn push(&self, r: &ChatRequest) {
            self.0.lock().unwrap().push(r.clone());
        }
        fn calls(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    /// 吐固定文本的模型。
    struct TextModel {
        replies: Vec<String>,
        recorder: Recorder,
    }

    impl TextModel {
        fn one(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                replies: vec![reply.to_string()],
                recorder: Recorder::default(),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for TextModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<crate::model::provider::ModelDelta, crate::model::provider::ModelError>> + Send + 'a>>
        {
            self.recorder.push(&req);
            let deltas = self
                .replies
                .clone()
                .into_iter()
                .map(|t| Ok(crate::model::provider::ModelDelta::Text(t)))
                .collect::<Vec<_>>();
            Box::pin(futures::stream::iter(deltas))
        }
    }

    /// 报 429 的模型。
    struct BrokenModel {
        recorder: Recorder,
    }

    #[async_trait]
    impl ModelProvider for BrokenModel {
        fn stream<'a>(
            &'a self,
            req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<crate::model::provider::ModelDelta, crate::model::provider::ModelError>> + Send + 'a>>
        {
            self.recorder.push(&req);
            Box::pin(futures::stream::iter(vec![Err(
                crate::model::provider::ModelError::Api("429 rate limited".into()),
            )]))
        }
    }

    /// 吐一截后挂住、等被取消的模型。
    struct StallingModel {
        ready: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ModelProvider for StallingModel {
        fn stream<'a>(
            &'a self,
            _req: ChatRequest,
        ) -> std::pin::Pin<Box<dyn Stream<Item = Result<crate::model::provider::ModelDelta, crate::model::provider::ModelError>> + Send + 'a>>
        {
            let ready = self.ready.clone();
            Box::pin(stream! {
                yield Ok(crate::model::provider::ModelDelta::Text("[{\"kind\":".into()));
                ready.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            })
        }
    }

    fn never_cancelled() -> CancellationToken {
        CancellationToken::new()
    }

    fn src_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: "我最近搬到上海工作了".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "记下了".into(),
                }],
            },
        ]
    }

    fn candidate_json(kind: &str, body: &str, confidence: f64) -> String {
        format!(
            r#"{{"kind":"{kind}","subject":"用户","predicate":"住在","object":"上海","body":"{body}","confidence":{confidence}}}"#
        )
    }

    /// 待办候选:`due` 是**已带引号的 JSON 值**(`"2026-11-01T09:00:00"` 或 `null`),
    /// 让「有日期 / 日期是 null / 日期是胡话」三种输入用同一个构造器摆出来。
    fn prospective_candidate_json(body: &str, due: &str) -> String {
        format!(
            r#"{{"kind":"prospective","subject":"用户","predicate":"提交","object":"周报","body":"{body}","confidence":0.95,"due_at":{due}}}"#
        )
    }

    /// 构造一条库内同键旧记忆(给 consolidate 用)。
    fn existing_row(id: i64, body: &str) -> MemoryRow {
        MemoryRow {
            id,
            session_id: None,
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("北京".into()),
            body: body.into(),
            confidence: 0.9,
            valid_from: 1,
            invalid_at: None,
            updated_at: 1,
            reinforcements: 0,
            last_accessed: None,
            due_at: None,
            notified_at: None,
            user_edited: false,
        }
    }

    // ---- 请求组装 ----

    #[test]
    fn the_extraction_request_carries_instruction_and_source_only() {
        let req = extraction_request(&src_messages());
        assert_eq!(req.messages.len(), 1 + src_messages().len());
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[0].text_of(), EXTRACT_INSTRUCTION);
        assert_eq!(&req.messages[1..], src_messages().as_slice());
        assert!(req.tools.is_empty(), "抽取不许带工具声明");
        assert_eq!(req.temperature, None, "抽取不继承聊天的温度旋钮");
    }

    #[test]
    fn the_consolidation_request_lists_the_candidate_and_the_existing_rows() {
        let cand = MemoryCandidate {
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("上海".into()),
            body: "用户搬到上海".into(),
            confidence: 0.9,
            due_at: None,
        };
        let existing = vec![existing_row(7, "用户住在北京")];
        let req = consolidation_request(&cand, &existing);
        assert_eq!(req.messages[0].text_of(), CONSOLIDATE_INSTRUCTION);
        let body = req.messages[1].text_of();
        assert!(body.contains("用户搬到上海"), "候选要在对照里:{body}");
        assert!(body.contains("用户住在北京"), "旧记忆要在对照里:{body}");
        assert!(body.contains("\"id\": 7") || body.contains("\"id\":7"), "旧记忆要带 id:{body}");
        assert!(req.tools.is_empty());
        assert_eq!(req.temperature, None);
    }

    #[test]
    fn the_profile_request_carries_current_and_additions() {
        let additions = vec![MemoryCandidate {
            kind: "profile".into(),
            subject: None,
            predicate: None,
            object: None,
            body: "用户在学日语".into(),
            confidence: 0.9,
            due_at: None,
        }];
        let req = profile_request("用户是 Rust 开发者", &additions);
        assert_eq!(req.messages[0].text_of(), PROFILE_INSTRUCTION);
        assert!(req.messages[1].text_of().contains("用户是 Rust 开发者"));
        assert!(req.messages[1].text_of().contains("用户在学日语"));
        assert!(req.tools.is_empty());
    }

    // ---- 纯解析 ----

    #[test]
    fn parse_candidates_accepts_raw_and_fenced_arrays_identically() {
        let raw = format!(
            "[{},{}]",
            candidate_json("fact", "用户搬到上海", 0.95),
            candidate_json("profile", "用户在学日语", 0.8)
        );
        let fenced = format!("```json\n{raw}\n```");
        let a = parse_candidates(&raw).unwrap();
        let b = parse_candidates(&fenced).unwrap();
        assert_eq!(a, b, "围栏不得改变解析结果");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].kind, "fact");
        assert_eq!(a[0].body, "用户搬到上海");
        assert!((a[0].confidence - 0.95).abs() < 1e-9);
        assert_eq!(a[1].kind, "profile");
    }

    #[test]
    fn parse_candidates_drops_bad_items_without_killing_the_batch() {
        // 单条不合格:空 body、低置信、kind 不认识 —— 丢条不炸批。
        let raw = format!(
            "[{},{},{},{}]",
            candidate_json("fact", "用户搬到上海", 0.95),
            candidate_json("fact", "   ", 0.95),
            candidate_json("fact", "用户偶尔喝了杯咖啡", 0.3),
            candidate_json("gossip", "用户讨厌同事", 0.9),
        );
        let got = parse_candidates(&raw).unwrap();
        assert_eq!(got.len(), 1, "三条被丢:空 body、低置信、未知 kind");
        assert_eq!(got[0].body, "用户搬到上海");
        // 空数组是合法答案
        assert!(parse_candidates("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_candidates_rejects_garbage_envelopes() {
        assert!(parse_candidates("今天聊聊天气").is_err());
        assert!(parse_candidates("{\"kind\":\"fact\"}").is_err(), "单个对象不是数组");
        assert!(parse_candidates("[{").is_err());
    }

    #[test]
    fn parse_decision_validates_each_shape() {
        assert_eq!(
            parse_decision(r#"{"action":"ADD"}"#).unwrap(),
            MemoryDecision::Add
        );
        assert_eq!(
            parse_decision(r#"{"action":"UPDATE","id":7,"body":"用户搬到上海"}"#).unwrap(),
            MemoryDecision::UpdateBody {
                id: 7,
                body: "用户搬到上海".into()
            }
        );
        assert_eq!(
            parse_decision(r#"{"action":"INVALIDATE","id":3}"#).unwrap(),
            MemoryDecision::Invalidate { id: 3 }
        );
        // 缺字段/未知 action → Err(该候选跳过,库里不动)
        assert!(parse_decision(r#"{"action":"UPDATE","id":7}"#).is_err());
        assert!(parse_decision(r#"{"action":"INVALIDATE"}"#).is_err());
        assert!(parse_decision(r#"{"action":"DELETE","id":1}"#).is_err());
        assert!(parse_decision("就改一下吧").is_err());
    }

    /// 第四种动作:同一件事又被说了一遍(措辞不同),只能靠强化表达。
    /// REINFORCE 必须带 id —— 缺 id 就是一次无处安放的「加一」,宁可跳过该候选。
    #[test]
    fn parse_decision_accepts_reinforce_and_requires_its_id() {
        assert_eq!(
            parse_decision(r#"{"action":"REINFORCE","id":7}"#).unwrap(),
            MemoryDecision::Reinforce { id: 7 }
        );
        assert!(
            parse_decision(r#"{"action":"REINFORCE"}"#).is_err(),
            "缺 id 的 REINFORCE 不许被当成功"
        );
    }

    // ---- ISO 8601 → ms(不引 chrono,自己算民用历法) ----

    /// 钉死具体日期:换算错误的单位(秒/毫秒)或历法错误(闰年、1970 前)都会立刻红。
    #[test]
    fn parse_iso8601_ms_maps_the_fixed_dates_we_pin() {
        assert_eq!(parse_iso8601_ms("1970-01-01"), Some(0), "缺时间按 00:00:00");
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00"), Some(0));
        assert_eq!(
            parse_iso8601_ms("2000-01-01"),
            Some(946_684_800_000),
            "Y2K 是闰年规则的分界(能被 400 整除)"
        );
        assert_eq!(
            parse_iso8601_ms("2000-01-01T03:00:00"),
            Some(946_695_600_000),
            "T 后面的时分秒必须算进去(比当日 00:00 多 3 小时 = 10_800_000 ms)"
        );
        assert_eq!(
            parse_iso8601_ms("2026-11-01T09:00:00"),
            Some(1_793_523_600_000),
            "近期日期:day 数算错一天就是 86_400_000 ms 的偏差"
        );
        assert!(parse_iso8601_ms("2024-02-29").is_some(), "闰年 2 月 29 日合法");
        assert_eq!(parse_iso8601_ms("2025-02-29"), None, "平年没有 2 月 29 日");
    }

    /// 模型给的从来不是严格 ISO:空串、中文、越界月份/日期/时分秒都必须落回 None,
    /// 由 keep_valid 把该条降级成普通事实 —— 不许按猜出来的时刻设闹钟。
    #[test]
    fn parse_iso8601_ms_rejects_malformed_or_impossible_timestamps() {
        for bad in [
            "",
            "明天",
            "2026-13-01",
            "2026-00-10",
            "2026-02-30",
            "2026-1-1",
            "2026-11-01T9:00:00",
            "2026-11-01T24:00:00",
            "2026-11-01T09:60:00",
            "2026-11-01T09:00:60",
            "2026/11/01",
            "not-a-date",
        ] {
            assert_eq!(parse_iso8601_ms(bad), None, "{bad:?} 不该被当成一个时刻");
        }
    }

    // ---- 抽取:prospective ----

    /// 带 due_at 的待办候选:ISO 字面量在解析边界上一次性换成 ms,
    /// 下游(store)只认 ms,不再解析字符串。
    #[test]
    fn parse_candidates_turns_a_prospective_due_at_into_milliseconds() {
        let raw = format!(
            "[{}]",
            prospective_candidate_json("用户下月一号要交周报", r#""2026-11-01T09:00:00""#)
        );
        let got = parse_candidates(&raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "prospective");
        assert_eq!(got[0].due_at, Some(1_793_523_600_000));
    }

    /// 一条「永不提醒的待办」是最坏的半条状态:用户以为记下了,系统永远不叫他。
    /// 所以时间解析不出来时把 kind 降级成 fact 保住内容,而不是带着 prospective 落库。
    #[test]
    fn a_prospective_candidate_without_a_parseable_due_at_degrades_to_a_plain_fact() {
        for due in [r#"null"#, r#""明天""#, r#""2026-13-01""#] {
            let raw = format!("[{}]", prospective_candidate_json("用户明天要交周报", due));
            let got = parse_candidates(&raw).unwrap();
            assert_eq!(got.len(), 1, "时间解析失败不许丢掉这条内容:{due}");
            assert_eq!(got[0].kind, "fact", "永不提醒的待办必须降级:{due}");
            assert_eq!(got[0].due_at, None, "降级之后不许残留到期时刻:{due}");
        }
    }

    /// 非待办的 due_at 一律不收:store 层同一个口径(`kind != prospective` 不存 due_at),
    /// 这里提前收下只会制造一条「事实型闹钟」。
    #[test]
    fn a_non_prospective_candidate_never_keeps_a_due_at() {
        let raw = r#"[{"kind":"fact","subject":"用户","predicate":"住在","object":"上海","body":"用户搬到上海","confidence":0.95,"due_at":"2026-11-01T09:00:00"}]"#;
        let got = parse_candidates(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "fact");
        assert_eq!(got[0].due_at, None, "只有 prospective 才有到期时刻");
    }

    // ---- 模型侧 ----

    #[tokio::test]
    async fn extract_memories_happy_path_parses_the_model_output() {
        let model = TextModel::one(&format!(
            "[{}]",
            candidate_json("fact", "用户搬到上海", 0.95)
        ));
        let got = extract_memories(&*model, &src_messages(), &never_cancelled())
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "用户搬到上海");
        assert_eq!(model.recorder.calls(), 1, "一次抽取一次调用");
    }

    #[tokio::test]
    async fn a_failing_extraction_call_is_an_error_not_half_a_batch() {
        let model = Arc::new(BrokenModel {
            recorder: Recorder::default(),
        });
        let err = extract_memories(&*model, &src_messages(), &never_cancelled())
            .await
            .err()
            .expect("429 必须是 Err");
        assert!(err.contains("429"), "报错要能判断该重试还是该改设置:{err}");
    }

    #[tokio::test]
    async fn cancelling_extraction_returns_an_error() {
        let ready = Arc::new(AtomicBool::new(false));
        let model = Arc::new(StallingModel { ready: ready.clone() });
        let cancel = CancellationToken::new();
        let src = src_messages();
        let (got, ()) = tokio::join!(
            extract_memories(&*model, &src, &cancel),
            async {
                while !ready.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                cancel.cancel();
            }
        );
        assert!(got.is_err(), "取消之后不许返回半批:{got:?}");
    }

    #[tokio::test]
    async fn consolidation_without_conflict_takes_the_fast_path() {
        // 无同键旧记忆 → ADD,**零次模型调用**(大多数路径,spec §10 的缓解)。
        let model = TextModel::one("不该被叫到");
        let cand = MemoryCandidate {
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("上海".into()),
            body: "用户搬到上海".into(),
            confidence: 0.9,
            due_at: None,
        };
        let got = consolidate_memory(&*model, &cand, &[], &never_cancelled())
            .await
            .unwrap();
        assert_eq!(got, MemoryDecision::Add);
        assert_eq!(model.recorder.calls(), 0, "无冲突时不许调模型");
    }

    #[tokio::test]
    async fn consolidation_with_conflict_asks_the_model_and_parses_its_choice() {        let model = TextModel::one(r#"{"action":"INVALIDATE","id":7}"#);
        let cand = MemoryCandidate {
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("上海".into()),
            body: "用户搬到上海".into(),
            confidence: 0.9,
            due_at: None,
        };
        let existing = vec![existing_row(7, "用户住在北京")];
        let got = consolidate_memory(&*model, &cand, &existing, &never_cancelled())
            .await
            .unwrap();
        assert_eq!(got, MemoryDecision::Invalidate { id: 7 });
        assert_eq!(model.recorder.calls(), 1);

        // 模型答非所问 → Err,库里不动
        let bad = TextModel::one("删掉旧的就行");
        assert!(
            consolidate_memory(&*bad, &cand, &existing, &never_cancelled())
                .await
                .is_err()
        );
    }

    /// 第四种答案:新旧说的是同一件事,只是措辞不同。管线只做「加一」,
    /// 正文一个字都不改(UPDATE 才是改正文的那条路)。
    #[tokio::test]
    async fn consolidate_memory_returns_reinforce_when_the_model_says_it_is_the_same_fact() {
        let model = TextModel::one(r#"{"action":"REINFORCE","id":7}"#);
        let cand = MemoryCandidate {
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("北京".into()),
            body: "用户在北京住".into(),
            confidence: 0.9,
            due_at: None,
        };
        let existing = vec![existing_row(7, "用户住在北京")];
        let got = consolidate_memory(&*model, &cand, &existing, &never_cancelled())
            .await
            .unwrap();
        assert_eq!(got, MemoryDecision::Reinforce { id: 7 });
        assert_eq!(model.recorder.calls(), 1);
    }

    #[tokio::test]
    async fn merge_profile_caps_the_output_and_rejects_blank_ones() {
        let long = "画像:".to_string() + &"很长".repeat(400);
        let model = TextModel::one(&long);
        let additions = vec![MemoryCandidate {
            kind: "profile".into(),
            subject: None,
            predicate: None,
            object: None,
            body: "用户在学日语".into(),
            confidence: 0.9,
            due_at: None,
        }];
        let merged = merge_profile(
            &*model,
            "现有画像",
            &additions,
            PROFILE_CHAR_CAP,
            &never_cancelled(),
        )
        .await
        .unwrap();
        assert!(
            merged.chars().count() <= PROFILE_CHAR_CAP,
            "画像合并的输出必须受字数上限约束,实际 {} 字",
            merged.chars().count()
        );

        let blank = TextModel::one("   ");
        assert!(
            merge_profile(
                &*blank,
                "现有画像",
                &additions,
                PROFILE_CHAR_CAP,
                &never_cancelled()
            )
            .await
            .is_err(),
            "合并成空白 = 静默清空画像,必须 Err"
        );
    }

    /// T5:cap 由调用方(编排层从 `Settings.memory_profile_char_cap` 现取)给,
    /// 不再吃常量 —— 给多小就截多短。截断用 `head_chars` 不用 `truncate_display`:
    /// 「…」是给用户看的截断标记,给模型读就成了画像内容的一部分(T2 裁定)。
    #[tokio::test]
    async fn merge_profile_honors_the_char_cap_passed_by_the_caller() {
        let model = TextModel::one(&"很长".repeat(200)); // 400 字,远超任何小 cap
        let additions = vec![MemoryCandidate {
            kind: "profile".into(),
            subject: None,
            predicate: None,
            object: None,
            body: "用户在学日语".into(),
            confidence: 0.9,
            due_at: None,
        }];
        let merged = merge_profile(&*model, "现有画像", &additions, 10, &never_cancelled())
            .await
            .unwrap();
        assert_eq!(
            merged.chars().count(),
            10,
            "调用方的 cap 必须生效,实得 {} 字",
            merged.chars().count()
        );
        assert!(
            !merged.contains('…'),
            "截断不许带省略号——模型会把它读成画像的一部分:{merged}"
        );
    }

    /// 从候选到写入形态的边界转换:编排层只经这一处补 session_id。
    #[test]
    fn a_candidate_becomes_a_new_memory_with_the_session_stamped_once() {
        let cand = MemoryCandidate {
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("上海".into()),
            body: "用户搬到上海".into(),
            confidence: 0.9,
            due_at: None,
        };
        let m: NewMemory = cand.clone().into_new_memory(Some("s9".into()));
        assert_eq!(m.session_id.as_deref(), Some("s9"));
        assert_eq!(m.body, "用户搬到上海");
        assert_eq!(m.kind, "fact");
        assert!((m.confidence - 0.9).abs() < 1e-9);
        assert_eq!(m.subject, cand.subject);
    }

    /// prospective 的到期时刻必须一路带到写入形态;漏掉这一处,
    /// 抽取解析得再准也会在边界上丢成 `due_at=None`。
    #[test]
    fn a_prospective_candidate_carries_its_due_at_into_the_write_shape() {
        let cand = MemoryCandidate {
            kind: "prospective".into(),
            subject: Some("用户".into()),
            predicate: Some("提交".into()),
            object: Some("周报".into()),
            body: "用户下月一号要交周报".into(),
            confidence: 0.95,
            due_at: Some(1_793_523_600_000),
        };
        let m = cand.into_new_memory(Some("s9".into()));
        assert_eq!(m.kind, "prospective");
        assert_eq!(m.due_at, Some(1_793_523_600_000));
    }

    // 让 `TextStreamOutcome` 在测试里可见(drain 重构的既有覆盖在 summarize.rs 自己的模块里)。
    #[allow(dead_code)]
    fn _outcome_is_public(_o: TextStreamOutcome) {}
}
