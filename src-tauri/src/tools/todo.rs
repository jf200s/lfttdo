//! todo_write：结构化任务清单的读写（对齐 OpenCode 的 todowrite）。
//!
//! 多步编码任务需要「计划-执行-回报」的锚点：模型把步骤写下来，后续轮次读回，
//! 自己与用户都能看到进度。持久化在 **write_root（缺省 roots[0]）下的
//! `.lfttdo/todos.json`** —— 路径不由模型给，写入面被钉死在这一个文件上，
//! 所以 Risk 定为 Read（零弹窗），与 OpenCode 把 todowrite 默认放行是同一取向；
//! 模型拿它写不了任何别的文件。
//!
//! 状态机收在三个词（pending / in_progress / completed）：清单是给人看的状态，
//! 自由文本状态会退化成一堆同义词。非法值在入口拒绝，不静默归一 —— 静默归一
//! 会让模型以为写进去了。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const NAME: &str = "todo_write";

/// 应用态目录名（`.lfttdo`）。后续的输出限幅落盘（G09）、undo 快照（G11）也住这里；
/// 提为常量是为了三处共用同一份字面量，不各写各的。
pub(crate) const APP_STATE_DIR: &str = ".lfttdo";
const TODO_FILE: &str = "todos.json";

pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    !roots.is_empty()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

fn validate_status(status: &str) -> Result<(), ToolError> {
    match status {
        "pending" | "in_progress" | "completed" => Ok(()),
        other => Err(ToolError::BadInput(format!(
            "status 必须是 pending / in_progress / completed 之一，实得 \"{other}\""
        ))),
    }
}

/// 清单文件的位置。`write_root` 优先（生成的文件本来就去那里），没有则退回
/// 第一个 root；两者皆无 = 这轮根本没有可写的落点，明确报错而不是静默丢数据。
fn todo_path(ctx: &ToolCtx) -> Result<PathBuf, ToolError> {
    let base = ctx
        .write_root
        .clone()
        .or_else(|| ctx.roots.first().cloned())
        .ok_or_else(|| {
            ToolError::BadInput("没有可用的工作目录（roots 与 write_root 都为空）".to_string())
        })?;
    Ok(base.join(APP_STATE_DIR).join(TODO_FILE))
}

fn read_items(path: &Path) -> Vec<TodoItem> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_items(path: &Path, items: &[TodoItem]) -> Result<(), ToolError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ToolError::Failed(format!("无法创建 {} : {e}", dir.display())))?;
    }
    let text = serde_json::to_string_pretty(items)
        .map_err(|e| ToolError::Failed(format!("清单序列化失败: {e}")))?;
    crate::tools::fs_write::atomic_write(path, &text)
}

/// 清单的展示文本：状态用**状态词本身**（不是 [x]/[~] 之类的记号）——
/// 读回要能让模型原样认出每个条目的状态，自由记号会逼它再猜一次。
/// 排序按固定词序（in_progress 在前），让人一眼看到正在做的那条。
fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "当前没有任务清单。".to_string();
    }
    let order = |s: &str| match s {
        "in_progress" => 0,
        "pending" => 1,
        _ => 2,
    };
    let mut sorted: Vec<&TodoItem> = items.iter().collect();
    sorted.sort_by_key(|i| order(&i.status));
    let mut out = Vec::new();
    for (n, item) in sorted.iter().enumerate() {
        out.push(format!("{}. [{}] {}", n + 1, item.status, item.content));
    }
    out.join("\n")
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: "写入或读取本任务的清单（多步任务先列计划，边做边更新状态）。\
                         status 只接受 pending（待做）/ in_progress（进行中）/ completed（已完成）。\
                         带 items = 整表替换并返回更新后的清单；不带 items = 只读当前清单。\
                         清单存在工作目录的 .lfttdo/todos.json，会话与重启之间保留。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "整表替换的新清单；省略 = 只读当前清单",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "这一步要做什么（非空）" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"]
                        }
                    }
                }
            }),
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let path = todo_path(ctx)?;
        let Some(items_in) = input.get("items") else {
            // 读回当前清单（省略 items = 只读）。没有清单不是错误，如实说。
            let items = read_items(&path);
            return Ok(ToolOutput::text(render(&items)));
        };
        let Value::Array(arr) = items_in else {
            return Err(ToolError::BadInput(
                "items 必须是数组（每项含 content 与 status）".to_string(),
            ));
        };
        let mut items: Vec<TodoItem> = Vec::with_capacity(arr.len());
        for v in arr {
            let content = v
                .get("content")
                .and_then(|c| c.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError::BadInput("每个 item 需要非空字符串 content".to_string()))?
                .to_string();
            let status = v
                .get("status")
                .and_then(|s| s.as_str())
                .ok_or_else(|| {
                    ToolError::BadInput(
                        "每个 item 需要字符串 status（pending/in_progress/completed）".to_string(),
                    )
                })?
                .to_string();
            validate_status(&status)?;
            items.push(TodoItem { content, status });
        }
        write_items(&path, &items)?;
        Ok(ToolOutput::text(format!(
            "清单已更新（{} 项）：\n{}",
            items.len(),
            render(&items)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx(roots: Vec<PathBuf>, write_root: Option<PathBuf>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
            write_root,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: None,
            events: None,
        }
    }

    async fn run(input: Value, c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        TodoTool.execute(input, c).await
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn write_then_read_back_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(
            vec![tmp.path().to_path_buf()],
            Some(tmp.path().to_path_buf()),
        );
        let out = run(
            json!({"items": [
                {"content": "实现 grep", "status": "completed"},
                {"content": "实现 glob", "status": "in_progress"},
                {"content": "写文档", "status": "pending"}
            ]}),
            &c,
        )
        .await
        .unwrap();
        let text = text_of(out);
        assert!(
            text.contains("实现 glob") && text.contains("[in_progress]"),
            "{text}"
        );

        // 新实例读回（持久化经文件，不靠内存态）。
        let fresh = ctx(vec![], Some(tmp.path().to_path_buf()));
        let text = text_of(run(json!({}), &fresh).await.unwrap());
        assert!(text.contains("写文档"), "{text}");
        assert!(text.contains("in_progress"), "{text}");
        // 落盘位置钉死在 .lfttdo/todos.json。
        assert!(tmp.path().join(APP_STATE_DIR).join(TODO_FILE).is_file());
    }

    #[tokio::test]
    async fn an_invalid_status_is_rejected_at_the_door() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(
            vec![tmp.path().to_path_buf()],
            Some(tmp.path().to_path_buf()),
        );
        let err = run(
            json!({"items": [{"content": "坏数据", "status": "done?"}]}),
            &c,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("pending")),
            "{err:?}"
        );
        // 拒绝即不落盘：坏数据不许污染既有清单。
        assert!(!tmp.path().join(APP_STATE_DIR).join(TODO_FILE).exists());
    }

    #[tokio::test]
    async fn an_empty_content_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(
            vec![tmp.path().to_path_buf()],
            Some(tmp.path().to_path_buf()),
        );
        let err = run(
            json!({"items": [{"content": "   ", "status": "pending"}]}),
            &c,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    #[tokio::test]
    async fn reading_before_any_write_says_so_without_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(
            vec![tmp.path().to_path_buf()],
            Some(tmp.path().to_path_buf()),
        );
        let text = text_of(run(json!({}), &c).await.unwrap());
        assert!(text.contains("没有任务清单"), "{text}");
    }

    #[tokio::test]
    async fn no_usable_base_dir_is_a_clear_error() {
        let c = ctx(vec![], None);
        let err = run(json!({}), &c).await.unwrap_err();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    #[test]
    fn name_constant_is_the_registered_name() {
        assert_eq!(TodoTool.meta().name, NAME);
        assert_eq!(NAME, "todo_write");
        assert!(usable_with_roots(&[PathBuf::from("/tmp")]));
        assert!(!usable_with_roots(&[]));
    }

    #[test]
    fn todo_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(TodoTool));
        assert!(registry.get(NAME).is_some());
        // 写入面被钉死在 .lfttdo/todos.json，路径不由模型给 ⇒ Read 档零弹窗。
        assert_eq!(registry.risk_of(NAME), Risk::Read);
    }
}
