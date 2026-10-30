//! `use_skill`：按名字读取技能包（`skills/<目录>/SKILL.md`）的完整正文。
//!
//! `Risk::Read`，和 `fetch_url`/`read_file` 一样不弹窗。它读的是用户自己放在技能目录里的
//! 说明文本；模型拿到的 `name` **不参与拼路径**（匹配与读取都在 `skills` 模块里，按发现
//! 出来的条目定位），所以没有穿越面。
//!
//! 声明表只在技能目录至少有一份技能时才把它交给模型（`agent/session.rs::request`），
//! 与 `read_file` 的隐藏同一条理由：不邀请一次注定失败的调用。隐藏不是授权 —— 模型凭空
//! 喊它照样会因为没有技能目录而失败。

use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use async_trait::async_trait;
use serde_json::{Value, json};

pub const NAME: &str = "use_skill";

pub struct UseSkillTool;

#[async_trait]
impl Tool for UseSkillTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: "读取一个技能包（skills 目录里的 SKILL.md）的完整说明。系统提示里\
                          列出了可用技能的名字和一句话用途；当用户的请求与某个技能相符时，\
                          先用本工具读出完整步骤，再按步骤执行。参数 name 填技能名（也可以\
                          填技能文件夹名）。返回的是技能正文；未找到时会在错误里列出可用\
                          技能。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "技能名（系统提示里列出的那一个），或技能文件夹名"
                    }
                },
                "required": ["name"]
            }),
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::BadInput("use_skill 需要非空字符串参数 name（技能名）".to_string())
            })?;
        let Some(dir) = &ctx.skills_dir else {
            return Err(ToolError::Failed(
                "当前没有可用的技能目录（skills 不存在或未配置）".to_string(),
            ));
        };
        let skill =
            crate::skills::read(dir, name, &ctx.skills_disabled).map_err(ToolError::BadInput)?;
        Ok(ToolOutput::text(format!(
            "【技能：{}】\n{}",
            skill.name, skill.body
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio_util::sync::CancellationToken;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("lfttdo-use-skill-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ctx(skills_dir: Option<PathBuf>) -> ToolCtx {
        ctx_with_disabled(skills_dir, Vec::new())
    }

    fn ctx_with_disabled(skills_dir: Option<PathBuf>, skills_disabled: Vec<String>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots: Vec::new(),
            skills_dir,
            skills_disabled,
        }
    }

    fn write_skill(root: &std::path::Path, folder: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: 翻译助手\ndescription: 中英互译\n---\n先识别语言，再翻译。\n",
        )
        .unwrap();
    }

    #[test]
    fn the_tool_meta_is_use_skill_read_with_the_pinned_schema() {
        let m = UseSkillTool.meta();
        assert_eq!(m.name, "use_skill");
        assert_eq!(m.risk, Risk::Read, "读本地说明文本，必须是零弹窗的只读档");
        assert!(m.description.contains("技能"), "{}", m.description);
        assert_eq!(m.input_schema["type"], json!("object"));
        assert_eq!(m.input_schema["required"], json!(["name"]));
        assert_eq!(
            m.input_schema["properties"]["name"]["type"],
            json!("string")
        );
    }

    #[tokio::test]
    async fn it_returns_the_skill_body_by_name_or_folder() {
        let root = tmp_dir("body");
        write_skill(&root, "translate");
        for key in ["翻译助手", "translate"] {
            let out = UseSkillTool
                .execute(json!({"name": key}), &ctx(Some(root.clone())))
                .await
                .unwrap();
            let ToolOutput::Text { text } = out else {
                panic!("use_skill 只产出文本");
            };
            assert!(text.contains("翻译助手") && text.contains("先识别语言，再翻译。"));
            assert!(!text.contains("description:"), "frontmatter 不进上下文");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_missing_or_non_string_name_is_bad_input() {
        let root = tmp_dir("bad-input");
        write_skill(&root, "translate");
        for input in [
            json!({}),
            json!({"name": 1}),
            json!({"name": ""}),
            json!({"name": "  "}),
        ] {
            let e = UseSkillTool
                .execute(input.clone(), &ctx(Some(root.clone())))
                .await
                .unwrap_err();
            assert!(
                matches!(e, ToolError::BadInput(_)),
                "{input} 必须 BadInput，实得 {e:?}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn an_unknown_skill_lists_what_is_available() {
        let root = tmp_dir("unknown");
        write_skill(&root, "translate");
        let e = UseSkillTool
            .execute(json!({"name": "不存在"}), &ctx(Some(root.clone())))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("翻译助手"), "错误里要能直接照着改：{e}");
        assert!(!e.contains("description:"), "错误里不该带正文");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_disabled_skill_cannot_be_read_even_if_the_model_asks_for_it() {
        let root = tmp_dir("disabled");
        write_skill(&root, "translate");
        let e = UseSkillTool
            .execute(
                json!({"name": "翻译助手"}),
                &ctx_with_disabled(Some(root.clone()), vec!["translate".to_string()]),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("已停用"),
            "隐藏声明不是授权：停用后直接喊名字也必须拒：{e}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn without_a_skills_directory_it_fails_instead_of_reading_elsewhere() {
        let e = UseSkillTool
            .execute(json!({"name": "翻译助手"}), &ctx(None))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, ToolError::Failed(m) if m.contains("技能目录")),
            "没有目录时必须是可归因的失败，而不是读到别的文件：{e:?}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_ctx_stops_before_any_read() {
        let root = tmp_dir("cancel");
        write_skill(&root, "translate");
        let c = ctx(Some(root.clone()));
        c.cancel.cancel();
        let e = UseSkillTool
            .execute(json!({"name": "翻译助手"}), &c)
            .await
            .unwrap_err();
        assert!(matches!(e, ToolError::Cancelled), "{e:?}");
        std::fs::remove_dir_all(&root).ok();
    }
}
