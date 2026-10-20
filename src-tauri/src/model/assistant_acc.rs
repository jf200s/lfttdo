// 把 `ModelDelta` 流还原成一条 assistant `Message`：文本合并、tool_call 的 arguments
// 按 `index` 归组后整体解析。本模块不依赖桌面框架也不依赖网络（Plan A 约束：桌面框架
// 只允许出现在 IPC 边界层），也不持有 sink 或发射器——`apply` 的返回值就是文本抵达 UI
// 的唯一通道（UI 只消费事件）。
use crate::agent::types::{ContentBlock, Message, Role};
use crate::model::provider::ModelDelta;
use serde_json::Value;
use std::collections::BTreeMap;

/// 分组键只有 `index`：`async-openai 0.42.2` 把 `ChatCompletionMessageToolCallChunk.index`
/// 声明为非可选 `u32` 且无 `#[serde(default)]`（`chat_.rs:1301-1311`），所以「分片不带 index」
/// 会让整个 chunk 在 `openai_compat` 那一层就落进 `ModelError::Decode`，任何 index-less
/// 片段都到不了这里，也无需在这里兜底（R29）。
#[derive(Default)]
pub struct AssistantAcc {
    text: String,
    /// `BTreeMap` 按 key 升序迭代，因此 `finish` 输出的工具调用顺序只由 `index` 决定，
    /// 与分片的到达顺序无关（分片顺序受网络抖动影响，不能进消息）。
    calls: BTreeMap<u32, PartialCall>,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

impl AssistantAcc {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 返回需要即时发给 UI 的文本增量；tool_call 与 usage 类 delta 返回 `None`。
    /// 这里不返回 `Result`：累加器不能失败，参数能不能解析要等 `finish` 拿到完整片段后才知道。
    ///
    /// `ModelDelta::Usage` 除了返回 `None`，它的 token 计数也会被**丢弃**：本类型没有像
    /// `text()` 那样的读取口能把它们找回来。调用方（Task 10 的流式循环）要在把 Usage
    /// delta 交进来之前就自己取走，否则统计静默为 0。
    pub fn apply(&mut self, d: ModelDelta) -> Option<String> {
        match d {
            ModelDelta::Text(t) => {
                self.text.push_str(&t);
                Some(t)
            }
            ModelDelta::ToolCallStart { index, id, name } => {
                // `or_default()` 只在真正缺失该 index 时插入空壳；已存在时**只补**本次带值的
                // id/name（`ModelDelta::ToolCallStart` 的文档注释：部分兼容端点只在中间分片给
                // name），`args` 一律保持原样——重置它会让工具静默收到空参数。
                let e = self.calls.entry(index).or_default();
                if let Some(id) = id {
                    e.id = id;
                }
                if let Some(name) = name {
                    e.name = name;
                }
                None
            }
            ModelDelta::ToolCallArgs { index, fragment } => {
                self.calls
                    .entry(index)
                    .or_default()
                    .args
                    .push_str(&fragment);
                None
            }
            ModelDelta::Usage { .. } => None,
        }
    }

    /// 块序固定为：一个合并 Text（仅当非空）→ 按 `index` 升序的 ToolCall。
    /// 文本恒在工具调用之前是刻意选择（R30）：交错顺序不保证被 OpenAI 兼容端点接受，
    /// 而交错本身对 UI 没有损失——流式期间每段文本都已经由 `apply` 实时投出去了。
    ///
    /// 空流会返回一条**零块**消息（累加器无从知道调用方是否想要一个空轮，所以这里不加
    /// 新 API、也不报错）。这样的消息在下游是发不出去的：`require_slots` 对空块列表直接
    /// 放行，`ChatCompletionRequestAssistantMessageArgs` 字段全有 `default` setter 所以
    /// `build()` 也不会失败，最终会上网成既没有 content 也没有 tool_calls 的
    /// `{"role":"assistant"}`。调用方（Task 10）必须自己判 `blocks.is_empty()`。
    pub fn finish(self) -> Message {
        let mut blocks = Vec::with_capacity(self.calls.len() + 1);
        if !self.text.is_empty() {
            blocks.push(ContentBlock::Text { text: self.text });
        }
        for (index, c) in &self.calls {
            let id = if c.id.is_empty() {
                format!("synth_{index}")
            } else {
                c.id.clone()
            };
            let input = parse_args(&c.args);
            blocks.push(ContentBlock::ToolCall {
                id,
                name: c.name.clone(),
                input,
            });
        }
        Message {
            role: Role::Assistant,
            blocks,
        }
    }
}

/// **R32**：解析失败时写进消息的原始片段上限，单位是**字符**不是字节。
/// 与 `openai_compat` 的 `RAW_BODY_KEEP`（`openai_compat.rs:438`）取同一个 200，理由共用：
/// 200 个字符足够看出坏在哪，再多就只是把垃圾搬走。区别在于那边截的是**日志**，
/// 这里截的是**消息内容**——这条 `Value` 会被 `assistant_msg` 重新序列化进下一轮的
/// `arguments`（`openai_compat.rs:315`）、计入 Task 8 的滑窗预算、由 Task 11 落库，
/// 整块内联等于让一坨截断输出永久骑在之后每一个请求上。
const RAW_ARGS_KEEP: usize = 200;

/// 按字符边界截断，不用 `&raw[..n]`：这里的输入是模型输出，中日韩字符一个占 3 字节，
/// 按字节下标切片会在非边界上 panic。丢弃的字符数写进标记，失败保持可归因。
fn args_snippet(raw: &str) -> String {
    let mut it = raw.chars();
    let head: String = it.by_ref().take(RAW_ARGS_KEEP).collect();
    let dropped = it.count();
    if dropped == 0 {
        head
    } else {
        format!("{head}…（后续 {dropped} 个字符已丢弃）")
    }
}

fn parse_args(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str::<Value>(trimmed) {
        // 合法但不是对象（`[1,2]` / `"str"` / `null`）原样透传：报错应由工具自己的入参校验产生，
        // 那种错误模型读得懂，而这里改写会把「模型输出了数组」这个事实抹平。
        Ok(v) => v,
        // 模型输出被截断时不能 panic，也不能静默变空对象——
        // 空参数会让工具报一个模型看不懂归因的错。标记后交给工具层报错。
        // 原因和片段一起走（R32）：片段只留前缀，长度由 `args_snippet` 兜住。
        Err(e) => serde_json::json!({
            "__parse_error": format!(
                "工具参数无法解析: {e}; 原始片段: {}",
                args_snippet(trimmed)
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, Role};
    use crate::model::provider::ModelDelta;

    #[test]
    fn assembles_arguments_split_across_chunks() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c1".into()),
            name: Some("fetch_url".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: r#"{"ur"#.into(),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: r#"l":"http://a"}"#.into(),
        });
        let msg = a.finish();
        assert_eq!(msg.blocks.len(), 1);
        assert_eq!(msg.role, Role::Assistant);
        match &msg.blocks[0] {
            ContentBlock::ToolCall { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "fetch_url");
                assert_eq!(input["url"], "http://a");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn groups_two_parallel_calls_by_index_not_arrival_order() {
        let mut a = AssistantAcc::default();
        // 刻意乱序：先 index=1 再 index=0
        a.apply(ModelDelta::ToolCallStart {
            index: 1,
            id: Some("b".into()),
            name: Some("second".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 1,
            fragment: "{\"n\":2}".into(),
        });
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("a".into()),
            name: Some("first".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"n\":1}".into(),
        });
        let msg = a.finish();
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 2);
        // 输出顺序必须按 index 稳定，不能随网络抖动变化
        assert_eq!(calls[0].1, "first");
        assert_eq!(calls[1].1, "second");
        // id 也要跟着 index 走：只钉 name/args 的话，「按 index 排好了但 id 张冠李戴」
        // 这种变异照样能过——而错 id 会让 tool_result 关联回错误的调用。
        assert_eq!(calls[0].0, "a", "index 0 的 id 没跟着 index 走");
        assert_eq!(calls[1].0, "b", "index 1 的 id 没跟着 index 走");
        assert_eq!(calls[0].2["n"], 1);
        assert_eq!(calls[1].2["n"], 2);
    }

    #[test]
    fn empty_arguments_become_empty_object_not_panic() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c".into()),
            name: Some("read_file".into()),
        });
        let msg = a.finish();
        match &msg.blocks[0] {
            ContentBlock::ToolCall { input, .. } => assert_eq!(input, &serde_json::json!({})),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn truncated_json_degrades_to_error_object_that_keeps_the_raw_fragment() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c".into()),
            name: Some("x".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"url\":\"http".into(),
        });
        let msg = a.finish();
        match &msg.blocks[0] {
            ContentBlock::ToolCall { input, .. } => {
                let raw = input["__parse_error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("缺少 __parse_error：{input}"));
                // 原始片段必须留在消息里：模型下一轮看得见自己刚才输出了什么，
                // 否则它只会收到一个无法归因的错误。这条片段的长度在 `RAW_ARGS_KEEP`
                // 之内（R32 只砍超过上限的部分），所以要求它**逐字**保住。
                assert!(
                    raw.contains(r#"{"url":"http"#),
                    "__parse_error 没保住原始片段：{raw}"
                );
                assert!(
                    !raw.contains("已丢弃"),
                    "短于上限的片段不该出现截断标记：{raw}"
                );
                // 报错原因与片段同行：只剩「原始片段」的话模型不知道是哪儿坏了。
                assert!(
                    raw.starts_with("工具参数无法解析: "),
                    "__parse_error 丢了 serde_json 的解析原因：{raw}"
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    /// R32：截断的**长**片段只准留前缀。这条 `Value` 是消息内容而不是日志行——它会被
    /// `assistant_msg` 重新序列化进下一轮的 `arguments`、计入 Task 8 的滑窗预算、由
    /// Task 11 落库，整块内联等于把一坨垃圾永久骑在之后每一个请求上。
    /// 片段用 800 个互不相同的 CJK 字符：既远超上限，又保证按字节切片会 panic、
    /// 且「尾部没出现」这条断言不会因为有重复子串而白过。
    #[test]
    fn a_long_truncated_fragment_is_capped_instead_of_being_inlined_whole() {
        let junk: String = (0..800u32)
            .map(|i| char::from_u32(0x4E00 + i).unwrap())
            .collect();
        let fragment = format!(r#"{{"url":"{junk}"#);
        let fragment_chars = fragment.chars().count();
        assert_eq!(fragment_chars, 808);
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c".into()),
            name: Some("x".into()),
        });
        a.apply(ModelDelta::ToolCallArgs { index: 0, fragment });
        let calls = a.finish().tool_calls();
        let raw = calls[0].2["__parse_error"]
            .as_str()
            .unwrap_or_else(|| panic!("缺少 __parse_error：{}", calls[0].2));
        // 上限生效：整段垃圾没有被原样搬进消息。
        assert!(
            !raw.contains(&junk),
            "{fragment_chars} 个字符的片段被整块内联了，标记里留着它自己的长度：{}",
            raw.chars().count()
        );
        let tail: String = junk.chars().skip(RAW_ARGS_KEEP).collect();
        assert!(
            !raw.contains(&tail),
            "超出 {RAW_ARGS_KEEP} 个字符的尾部仍然出现在消息里：{raw}"
        );
        // 前缀留着（可归因），丢弃量如实标注（808 - 200 = 608）。
        let head: String = junk.chars().take(100).collect();
        assert!(raw.contains(&head), "保留的前缀被丢了：{raw}");
        assert!(
            raw.contains("后续 608 个字符已丢弃"),
            "截断标记与实际丢弃字符数对不上（片段共 {fragment_chars} 个字符，上限 {RAW_ARGS_KEEP}）：{raw}"
        );
        // 消息侧总长度有界，两道（实测 305 个字符 vs 片段 808 个字符）：
        // 1) 严格小于片段本身——「垃圾没被整块搬进消息」的最直接形状；
        // 2) 不超过「上限 + 截断标记 + 解析原因」。用 saturating_add 是为了让**改动上限**
        //    这种变异红在测试里，而不是红成 `usize::MAX + 250` 的编译期 overflow 错误。
        let serialized = serde_json::to_string(&calls[0].2).unwrap();
        let serialized_chars = serialized.chars().count();
        assert!(
            serialized_chars < fragment_chars,
            "序列化后的 arguments 比原始片段还大（{serialized_chars} vs {fragment_chars}）——垃圾被整块搬进消息了"
        );
        assert!(
            serialized_chars <= RAW_ARGS_KEEP.saturating_add(250),
            "序列化后的 arguments 超出了「上限 {RAW_ARGS_KEEP} + 标记 + 解析原因」：{serialized_chars} 个字符"
        );
    }

    /// R30：文本恒在工具调用之前且**只有一个合并 Text 块**。交错顺序不保证被
    /// OpenAI 兼容端点接受，而 UI 在流式期间已经通过 `apply` 的返回值实时看到了每一段
    /// 文本，所以 finish 不需要（也不应该）重建交错顺序。
    #[test]
    fn text_and_tool_call_interleave_yield_one_merged_text_then_the_call() {
        let mut a = AssistantAcc::default();
        // `apply` 的返回值是文本抵达 UI 的唯一通道（UI 只消费事件），逐段钉住。
        assert_eq!(
            a.apply(ModelDelta::Text("我看".into())).as_deref(),
            Some("我看")
        );
        assert_eq!(
            a.apply(ModelDelta::Text("一下。".into())).as_deref(),
            Some("一下。")
        );
        assert_eq!(a.text(), "我看一下。");
        assert_eq!(
            a.apply(ModelDelta::ToolCallStart {
                index: 0,
                id: Some("c".into()),
                name: Some("f".into())
            })
            .as_deref(),
            None
        );
        assert_eq!(
            a.apply(ModelDelta::ToolCallArgs {
                index: 0,
                fragment: "{}".into()
            })
            .as_deref(),
            None
        );
        assert_eq!(
            a.apply(ModelDelta::Text("完成".into())).as_deref(),
            Some("完成")
        );
        let msg = a.finish();
        assert_eq!(
            msg.blocks.len(),
            2,
            "一个合并 Text + 一个 ToolCall：{msg:?}"
        );
        assert_eq!(
            msg.blocks[0],
            ContentBlock::Text {
                text: "我看一下。完成".into()
            }
        );
        assert!(matches!(msg.blocks[1], ContentBlock::ToolCall { .. }));
    }

    // ---- brief 的五条之外，错误后果是「工具收到空参数」的边界 ----

    /// 兼容端点会把 name 放在中间分片（`ModelDelta::ToolCallStart` 的文档注释）。
    /// 真正的风险是补 name 时把已经攒下的 args 一起清零。
    #[test]
    fn a_late_start_fills_in_name_without_resetting_accumulated_args() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c1".into()),
            name: None,
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: r#"{"a"#.into(),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: r#"":1}"#.into(),
        });
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: None,
            name: Some("late_name".into()),
        });
        let msg = a.finish();
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "late_name", "迟到的 name 没有落位");
        assert_eq!(calls[0].0, "c1", "只有 id 的 Start 不该清空 id");
        assert_eq!(
            calls[0].2,
            serde_json::json!({"a": 1}),
            "补 name 时 args 被重置了——工具会收到空参数"
        );
    }

    /// 钉住实际行为：没有任何 Start 的 index 会输出 `synth_{index}` + **空 name**。
    /// 空 name 是注册表（查表匹配工具名）必须拒绝的输入，不是累加器能补出来的东西；
    /// 这里不 panic、不丢参数片段，把可归因的残缺交给下游。
    #[test]
    fn args_with_no_start_at_all_yield_a_synth_id_and_the_empty_name_the_registry_must_reject() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallArgs {
            index: 3,
            fragment: "{\"n\":7}".into(),
        });
        let msg = a.finish();
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "synth_3");
        assert_eq!(calls[0].1, "", "空 name 是本条要钉住的行为，不是笔误");
        assert_eq!(calls[0].2["n"], 7, "参数片段不能因为缺 Start 就丢掉");
    }

    /// 合法但**不是对象**的参数（`[1,2]` / `"str"` / `null`）原样透传。
    /// 累加器不做 schema 检查：报错应由工具自身的入参校验产生，那样模型才读得懂；
    /// 在这里改成 `__parse_error` 或 `Null` 反而会把「模型输出了数组」这个事实抹平。
    /// `null` 是这里最不显眼的一个形状：它恰好和空参数表现得一模一样（`input["path"]`
    /// 在 `Value::Null` 上返回 `Null`），正是本任务要拦的「工具静默收到空参数」，
    /// 而且它一路没有标记。但「模型输出了 JSON 字面量 null」和「模型什么都没输」的区分
    /// 不是累加器的判断，所以这里同样不加防御分支——报错归工具的 schema 检查，
    /// 与上面两个形状的裁定一致。
    #[test]
    fn valid_but_non_object_arguments_pass_through_untouched_for_the_tool_to_reject() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c1".into()),
            name: Some("f".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "[1,2]".into(),
        });
        a.apply(ModelDelta::ToolCallStart {
            index: 1,
            id: Some("c2".into()),
            name: Some("g".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 1,
            fragment: "\"str\"".into(),
        });
        a.apply(ModelDelta::ToolCallStart {
            index: 2,
            id: Some("c3".into()),
            name: Some("h".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 2,
            fragment: "null".into(),
        });
        let calls = a.finish().tool_calls();
        assert_eq!(calls[0].2, serde_json::json!([1, 2]));
        assert_eq!(calls[1].2, serde_json::json!("str"));
        assert_eq!(
            calls[2].2,
            serde_json::json!(null),
            "`null` 要透传成 Value::Null，既不换成空对象也不报错"
        );
        assert!(
            calls[0].2.as_object().is_none()
                && calls[1].2.as_object().is_none()
                && calls[2].2.as_object().is_none()
                && calls[0].2.get("__parse_error").is_none()
                && calls[1].2.get("__parse_error").is_none()
                && calls[2].2.get("__parse_error").is_none(),
            "非对象参数不该被累加器改写：{calls:?}"
        );
    }

    /// `trim()` 的另一面：端点把「没有参数」发成一个空白字符串时，仍然是 `{}`
    /// 而不是 `__parse_error`。这条钉住 trim 与空判定共用同一份文本，不会一边判空
    /// 一边解析。
    #[test]
    fn whitespace_only_arguments_are_the_empty_object_not_a_parse_error() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c".into()),
            name: Some("get_time".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "   \n".into(),
        });
        match &a.finish().blocks[0] {
            ContentBlock::ToolCall { input, .. } => {
                assert_eq!(input, &serde_json::json!({}));
                assert!(
                    input.get("__parse_error").is_none(),
                    "空白参数不该报错：{input}"
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    /// 钉住「同一个 index 来两个都带 id 的 Start」= **后写覆盖**（last-writer-wins）。
    /// 选这个方向而不是保留第一个：id/name 是端点在流里逐步补全的字段，最后一个非
    /// `None` 值就是它对当前分组的最新陈述；`args` 走的是追加语义，两者互不干扰。
    #[test]
    fn a_second_start_carrying_both_id_and_name_overwrites_the_first() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("first_id".into()),
            name: Some("first_name".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"k\":1}".into(),
        });
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("second_id".into()),
            name: Some("second_name".into()),
        });
        let calls = a.finish().tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "second_id", "id 是 last-writer-wins");
        assert_eq!(calls[0].1, "second_name", "name 是 last-writer-wins");
        assert_eq!(calls[0].2["k"], 1, "覆盖 id/name 不得影响已攒的 args");
    }

    #[test]
    fn usage_returns_none_and_disturbs_nothing_already_accumulated() {
        let mut a = AssistantAcc::default();
        a.apply(ModelDelta::Text("前".into()));
        a.apply(ModelDelta::ToolCallStart {
            index: 0,
            id: Some("c".into()),
            name: Some("f".into()),
        });
        a.apply(ModelDelta::ToolCallArgs {
            index: 0,
            fragment: "{\"n\":1}".into(),
        });
        assert_eq!(
            a.apply(ModelDelta::Usage {
                input_tokens: 10,
                output_tokens: 20
            })
            .as_deref(),
            None
        );
        assert_eq!(a.text(), "前", "Usage 不该动文本");
        let calls = a.finish().tool_calls();
        assert_eq!(calls.len(), 1, "Usage 不该动工具调用");
        assert_eq!(calls[0].2["n"], 1);
    }

    #[test]
    fn a_stream_that_produced_nothing_finishes_as_an_empty_assistant_message() {
        let msg = AssistantAcc::default().finish();
        assert_eq!(msg.role, Role::Assistant);
        assert!(msg.blocks.is_empty(), "空流不该凭空造块：{msg:?}");
    }
}
