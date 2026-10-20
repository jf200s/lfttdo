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
    Text { text: String },
    Image { data: String, mime: ImageMime },
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

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
}
