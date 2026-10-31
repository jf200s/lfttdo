use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime: ImageMime,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        call_id: String,
        is_error: bool,
        output: ToolOutput,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImageMime {
    Png,
    Jpeg,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutput {
    Text {
        text: String,
    },
    Image {
        data: String,
        mime: ImageMime,
    },
    /// 多块结果（R317）：MCP 会回 text+image 的混合数组，**保持每块的形状**。
    /// 摊平成文本会把图丢掉（spec 5.2 第 2 点），所以新增这一支而不是降级。
    /// 生产里由 `mcp::bridge::map_result` 产生；`parts` 不嵌套（只有 0/1 块时直接返回那一块）。
    Multi {
        parts: Vec<ToolOutput>,
    },
}

pub const TOOL_IMAGE_FOLLOWUP_NOTE: &str = "上面的工具返回了图片，请结合图片回答用户刚才的问题。";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolImage {
    pub data: String,
    pub mime: ImageMime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}
pub(crate) const MAX_EVENT_IMAGES: usize = 4;
pub(crate) const MAX_EVENT_IMAGE_BASE64: usize = 8 * 1024 * 1024;

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn event_view(&self) -> (String, Vec<ToolImage>) {
        let mut images = Vec::new();
        let mut total = 0usize;
        let mut omitted = 0usize;
        self.collect_event_images(&mut images, &mut total, &mut omitted);
        let mut preview = self.preview();
        if omitted > 0 {
            preview.push_str(&format!("\n[另有 {omitted} 张图片未在卡片中显示]"));
        }
        (preview, images)
    }

    fn collect_event_images(
        &self,
        images: &mut Vec<ToolImage>,
        total: &mut usize,
        omitted: &mut usize,
    ) {
        match self {
            ToolOutput::Image { data, mime } => {
                if images
                    .iter()
                    .any(|kept| kept.data == *data && kept.mime == *mime)
                {
                    return;
                }
                if images.len() >= MAX_EVENT_IMAGES
                    || total.saturating_add(data.len()) > MAX_EVENT_IMAGE_BASE64
                {
                    *omitted += 1;
                    return;
                }
                *total += data.len();
                images.push(ToolImage {
                    data: data.clone(),
                    mime: mime.clone(),
                    path: None,
                });
            }
            ToolOutput::Multi { parts } => {
                for part in parts {
                    part.collect_event_images(images, total, omitted);
                }
            }
            ToolOutput::Text { .. } => {}
        }
    }

    /// 工具输出面向**事件流**的预览。全仓库只此一份实现（R77）：
    /// 直播路径 `agent/session.rs` 在工具执行后调它，重放路径 `store/db.rs` 把库里的
    /// `ToolResult` 块换成事件时调同一个函数。spec:421 花那么大力气就是为了不出现两套
    /// 渲染路径，而预览文本是最容易各写一遍、重开就变味的地方 —— Task 11 的草稿那份
    /// 没有省略号、图片写成 Debug 的 `Png`，正是活例子。
    ///
    /// 只交出前 [`PREVIEW_CHARS`] 个字符：完整输出留在 history（那才是下一轮模型的输入），
    /// 否则一次大页面抓取会同时压垮 IPC 与前端渲染。切法住在 `crate::text`，与
    /// 会话标题、两处日志片段共用同一份按字符边界的实现（评审 #29 标准轴 2）。
    pub fn preview(&self) -> String {
        match self {
            ToolOutput::Text { text } => crate::text::truncate_display(text, PREVIEW_CHARS),
            ToolOutput::Image { mime, .. } => format!("[图片 {}]", mime_word(mime)),
            // R317：多块结果先逐块取预览、再整体截断一次 —— 只此一份实现（R77）。
            ToolOutput::Multi { parts } => {
                let joined = parts
                    .iter()
                    .map(ToolOutput::preview)
                    .collect::<Vec<_>>()
                    .join("\n");
                crate::text::truncate_display(&joined, PREVIEW_CHARS)
            }
        }
    }
}

fn mime_word(mime: &ImageMime) -> &'static str {
    match mime {
        ImageMime::Png => "png",
        ImageMime::Jpeg => "jpeg",
    }
}

/// 事件流里工具输出的预览上限（字符）。见 [`ToolOutput::preview`]。
const PREVIEW_CHARS: usize = 200;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

impl Message {
    pub fn user(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            blocks,
        }
    }
    pub fn system_note(text: &str) -> Message {
        Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }
    /// 构造一条工具结果消息。role 用 `User` 而非 `System`：OpenAI 兼容协议要求 `tool`
    /// 角色消息，而 role 到协议字段的映射归 Task 6 负责，这里刻意不新增 `Role::Tool`
    /// 变体，避免 Task 6 漏映射时编译器给不出提示。
    /// 失败语义只由 `is_error` 承载 —— `ToolOutput` 里没有（也不该有）error 构造器。
    pub fn tool(call_id: &str, output: ToolOutput, is_error: bool) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                call_id: call_id.into(),
                is_error,
                output,
            }],
        }
    }
    /// 只交出 (id, name, input) 三元组：类型层不知道 risk。
    /// risk 由 Task 9 的 registry 查表后组装 `ToolCallView`，此处硬编码任何默认值
    /// 都会在 Plan B 引入写工具时静默放行。
    pub fn tool_calls(&self) -> Vec<(String, String, Value)> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect()
    }
    /// 只拼接 `Text` 块，其余块返回空串——这是**约定**而不是能力：生产调用方有两处，
    /// 一处靠守卫、一处不靠。`model::openai_compat::to_api` 在 system/assistant 两臂先用
    /// `require_slots` 限定块类型（system 只允许 Text；assistant 允许 Text/ToolCall，
    /// ToolCall 另有 `tool_calls` 字段承接，所以这里丢掉的不是任何唯一信息），
    /// 因此不会出现「一条 system 因为只有一个 Image 而序列化成 `"content": ""`」的静默降级
    /// （folded item 1）。`agent::context::build` 的 notes 去重是第二个调用方，它**没有**
    /// 守卫，只拿这个字符串和 `SYSTEM_PROMPT` 比相等，所以空串在这里是无害的比较值。
    /// 绕过 `to_api` 直接调本方法拼请求时，请连同 `require_slots` 一起调。
    /// （R40：这段约定原先只写在 `openai_compat.rs` 私有 `text_of` 头上，而
    /// `context.rs` 里有一份逐字相同的副本没这句话；实现现在只有这一处，
    /// 它描述的是一个 `Message`，所以住在这里。）
    pub fn text_of(&self) -> String {
        self.blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn is_tool_image_followup(&self) -> bool {
        self.role == Role::User
            && matches!(
                self.blocks.first(),
                Some(ContentBlock::Text { text }) if text == TOOL_IMAGE_FOLLOWUP_NOTE
            )
            && self
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
    }
}

/// 从 ContentBlock::ToolCall 派生的视图。risk 由 registry 补全，不来自消息本身。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCallView {
    pub id: String,
    pub name: String,
    pub risk: Risk,
    pub input: Value,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Read,
    Write,
    Exec,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 钉住 tool 结果的线上格式：tag 名、字段顺序，以及 role 恒为 user 的决定。
    #[test]
    fn tool_message_wire_shape() {
        let m = Message::tool("call_1", ToolOutput::text("文件不存在"), true);
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"role":"user","blocks":[{"type":"tool_result","call_id":"call_1","is_error":true,"output":{"kind":"text","text":"文件不存在"}}]}"#
        );
    }

    fn img(data: &str) -> ToolOutput {
        ToolOutput::Image {
            data: data.into(),
            mime: ImageMime::Png,
        }
    }

    #[test]
    fn event_view_carries_images_but_keeps_base64_out_of_the_text_preview() {
        let out = ToolOutput::Multi {
            parts: vec![
                ToolOutput::text("截图如下"),
                img("QUJD"),
                ToolOutput::Image {
                    data: "REVG".into(),
                    mime: ImageMime::Jpeg,
                },
            ],
        };
        let (preview, images) = out.event_view();
        assert_eq!(images.len(), 2, "两张图都要进事件");
        assert_eq!(images[0].data, "QUJD");
        assert_eq!(images[1].mime, ImageMime::Jpeg);
        assert!(preview.contains("截图如下"), "{preview}");
        assert!(
            !preview.contains("QUJD"),
            "base64 不许进文本预览：{preview}"
        );

        let (preview, images) = ToolOutput::text("没有图").event_view();
        assert!(images.is_empty(), "纯文本结果不许凭空长出图片");
        assert_eq!(preview, "没有图");
    }

    #[test]
    fn event_view_dedupes_identical_images_without_calling_them_omitted() {
        let out = ToolOutput::Multi {
            parts: vec![img("QUJD"), img("QUJD"), ToolOutput::text("x")],
        };
        let (preview, images) = out.event_view();
        assert_eq!(images.len(), 1, "同一张图只渲染一次");
        assert!(
            !preview.contains("未在卡片中显示"),
            "去重不是遗漏，不该挂提示：{preview}"
        );
    }

    #[test]
    fn event_view_caps_the_number_of_images_and_says_how_many_were_omitted() {
        let out = ToolOutput::Multi {
            parts: (0..5).map(|i| img(&format!("IMG{i}"))).collect(),
        };
        let (preview, images) = out.event_view();
        assert_eq!(images.len(), MAX_EVENT_IMAGES);
        assert!(
            preview.contains("另有 1 张图片未在卡片中显示"),
            "被上限挡掉的图必须报数量：{preview}"
        );
    }

    #[test]
    fn event_view_caps_total_base64_bytes() {
        let half = MAX_EVENT_IMAGE_BASE64 / 2 + 1;
        let out = ToolOutput::Multi {
            parts: vec![img(&"A".repeat(half)), img(&"B".repeat(half))],
        };
        let (preview, images) = out.event_view();
        assert_eq!(images.len(), 1, "第二张会把总量顶过上限");
        assert!(preview.contains("另有 1 张图片未在卡片中显示"), "{preview}");
    }

    #[test]
    fn only_the_synthetic_tool_image_followup_is_recognized() {
        let image = ContentBlock::Image {
            data: "QUJD".into(),
            mime: ImageMime::Png,
        };
        let synthetic = Message::user(vec![
            ContentBlock::Text {
                text: TOOL_IMAGE_FOLLOWUP_NOTE.into(),
            },
            image.clone(),
        ]);
        assert!(synthetic.is_tool_image_followup());
        let real = Message::user(vec![
            ContentBlock::Text {
                text: "这张图里是什么".into(),
            },
            image.clone(),
        ]);
        assert!(
            !real.is_tool_image_followup(),
            "真实用户带图的那句不许被当成合成消息跳过"
        );
        assert!(
            !Message::tool(
                "c1",
                ToolOutput::Image {
                    data: "QUJD".into(),
                    mime: ImageMime::Png
                },
                false
            )
            .is_tool_image_followup()
        );
    }

    /// 钉住 `rename_all = "lowercase"`：mime 上线必须是小写 `"png"`，
    /// 摘掉该属性不会让任何其他测试变红。
    #[test]
    fn image_output_wire_shape_is_lowercase_mime() {
        let m = Message::tool(
            "call_1",
            ToolOutput::Image {
                data: "iVBOR".into(),
                mime: ImageMime::Png,
            },
            false,
        );
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"role":"user","blocks":[{"type":"tool_result","call_id":"call_1","is_error":false,"output":{"kind":"image","data":"iVBOR","mime":"png"}}]}"#
        );
        assert_eq!(serde_json::to_string(&ImageMime::Png).unwrap(), r#""png""#);
    }

    /// 钉住 Ruling A：`tool_calls()` 交出裸三元组，类型层不接触 risk。
    #[test]
    fn tool_calls_yields_bare_triples_and_never_risk() {
        let m = Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "先看一眼".into(),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "fs.read".into(),
                    input: json!({"path": "a.txt"}),
                },
            ],
        };
        assert_eq!(
            m.tool_calls(),
            vec![(
                "c1".to_string(),
                "fs.read".to_string(),
                json!({"path": "a.txt"})
            )]
        );
    }

    /// R40：`text_of` 是**唯一**的取文本实现（`context.rs` 与 `openai_compat.rs` 各自
    /// 的副本已删除）。这里钉住它约定好的两点：只收 `Text`（其余块贡献空串，
    /// `require_slots` 靠这一点），以及多块之间**无分隔**拼接（协议 content 是单串）。
    #[test]
    fn text_of_joins_only_the_text_blocks_without_a_separator() {
        let m = Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "你是".into(),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "fs.read".into(),
                    input: json!({}),
                },
                ContentBlock::Image {
                    data: "iVBOR".into(),
                    mime: ImageMime::Png,
                },
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    is_error: false,
                    output: ToolOutput::text("不该出现"),
                },
                ContentBlock::Text {
                    text: "助手".into(),
                },
            ],
        };
        assert_eq!(m.text_of(), "你是助手");
        // 非文本消息返回空串（调用方的守卫负责拒绝，而不是在这里发明兜底）。
        assert_eq!(
            Message::tool("c1", ToolOutput::text("只有结果"), false).text_of(),
            ""
        );
        assert_eq!(Message::system_note("").text_of(), "");
    }
}
