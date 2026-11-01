// 长期记忆的主动检索工具(T11,元记忆的出口)。
//
// 为什么需要它:自动召回(`memory_notes_for_turn`)是「送上门」——只按当前消息相关度
// 挑几条。用户问「我上次说的那个项目叫什么」时,相关度匹配往往落空,而**模型自己知道
// 该去查**(元记忆)。没有这把工具,agent 不知道自己有记忆库可翻,记忆就只是一堆
// 被动注入的文本。
//
// 风险等级是 `Risk::Read`:它只读本机 SQLite、不触网、不碰文件系统 —— 与 `fs.read`
// 同档,走权限闸时零弹窗(spec 5.1 的承诺因此仍然成立)。
use super::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::agent::types::{Risk, ToolOutput};
use crate::store::db::Db;
use crate::store::memory::{MemoryRow, RecallCfg};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

/// 单次检索的默认条数与硬上限(与设置项 `memory_recall_k` 的钳位同口径,
/// 但这是**模型显式请求**的条数,不读用户的自动召回设置 —— 两者是不同的事)。
pub const DEFAULT_K: usize = 5;
pub const MAX_K: usize = 20;

pub struct MemorySearchTool {
    db: Arc<Db>,
}

impl MemorySearchTool {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

/// 把一次检索结果渲染成模型读的文本。空结果也要**明确说「没有」**:
/// 静默返回空串会让模型以为工具坏了,而「查过、没有」是一个有用的答案。
/// 每条带上 kind 与确认次数 —— 模型据此判断这条记忆有多可信。
pub fn render_hits(query: &str, hits: &[MemoryRow]) -> String {
    if hits.is_empty() {
        return format!("没有找到与「{query}」相关的长期记忆。");
    }
    let mut out = format!("找到 {} 条相关记忆(按相关度排序):", hits.len());
    for m in hits {
        // 确认次数只在 >0 时显示:0 次是绝大多数,写成「确认 0 次」是纯噪声,
        // 还会让模型误以为那是个负面信号。
        let strength = if m.reinforcements > 0 {
            format!(",确认 {} 次", m.reinforcements)
        } else {
            String::new()
        };
        out.push_str(&format!("\n- [{}] {}{}", m.kind, m.body, strength));
    }
    out
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "memory_search".into(),
            description: "检索用户的长期记忆库(跨会话保存的事实、偏好与待办)。\
                         当用户提到过去说过的事、你需要确认他的背景或偏好时主动调用。\
                         返回按相关度排序的记忆条目。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "要检索的关键词或自然语言问题"
                    },
                    "k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_K,
                        "description": "最多返回几条,默认 5"
                    }
                },
                "required": ["query"]
            }),
            risk: Risk::Read,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // 取消先查:一轮被停下之后不该再跑一次库查询(与 echo 的守卫同形)。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::BadInput("缺 query,或它是空的/纯空白".into()))?;
        // k 缺省走 DEFAULT_K;越界**钳位而不是报错** —— 模型给 999 是个无害的意图,
        // 为它失败一次工具调用没有收益。
        let k = input
            .get("k")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_K)
            .clamp(1, MAX_K);

        // 同步 SQLite 直接调:个人量级(≤几千行全表扫描)是微秒级,tokio 的建议阈值
        // 是 >10ms 才值得 spawn_blocking —— 为它换一次线程切换反而更贵。
        let hits = self
            .db
            .search_memories(query, None, k, &RecallCfg::default())
            .map_err(|e| ToolError::Failed(format!("查询长期记忆失败: {e}")))?;
        Ok(ToolOutput::text(render_hits(query, &hits)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::NewMemory;

    fn tmpdb() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("m.db")).unwrap());
        (dir, db)
    }

    fn fact(body: &str) -> NewMemory {
        NewMemory {
            session_id: None,
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some("北京".into()),
            body: body.into(),
            confidence: 0.9,
            due_at: None,
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            write_root: None,
            http: reqwest::Client::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            roots: Vec::new(),
            skills_dir: None,
            skills_disabled: Vec::new(),
        }
    }

    /// 工具输出必须是文本;别的形状说明工具写错了,直接炸比静默取空好。
    fn text_of(o: &ToolOutput) -> String {
        match o {
            ToolOutput::Text { text } => text.clone(),
            other => panic!("期望文本输出,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_tool_is_a_read_risk_tool_named_memory_search() {
        // 风险等级是权限闸的唯一依据:写成 Write/Exec 会让一条纯读工具开始弹窗,
        // 把 spec 5.1「内置工具零弹窗」的承诺打破。
        let (_d, db) = tmpdb();
        let meta = MemorySearchTool::new(db).meta();
        assert_eq!(meta.name, "memory_search");
        assert_eq!(meta.risk, Risk::Read);
        assert_eq!(meta.input_schema["required"][0], "query");
        assert_eq!(meta.input_schema["properties"]["k"]["maximum"], MAX_K);
    }

    #[tokio::test]
    async fn a_query_returns_matching_memories_with_kind_and_confirmations() {
        let (_d, db) = tmpdb();
        let id = db.add_memory(&fact("用户住在北京")).unwrap();
        db.reinforce_memory(id).unwrap();
        db.add_memory(&fact("用户养了一只猫")).unwrap();

        let out = MemorySearchTool::new(db.clone())
            .execute(json!({"query": "北京"}), &ctx())
            .await
            .expect("只读工具不该失败");
        let text = text_of(&out);
        assert!(text.contains("用户住在北京"), "命中的正文要在结果里:{text}");
        assert!(
            !text.contains("用户养了一只猫"),
            "零重叠的不许混进来:{text}"
        );
        assert!(text.contains("fact"), "kind 要标出来:{text}");
        assert!(
            text.contains("确认 1 次"),
            "确认次数要标出来(模型据此判断可信度):{text}"
        );
    }

    #[tokio::test]
    async fn an_empty_result_says_so_instead_of_returning_nothing() {
        // 静默空串会让模型以为工具坏了;「查过、没有」是有用的答案。
        let (_d, db) = tmpdb();
        db.add_memory(&fact("用户住在北京")).unwrap();
        let out = MemorySearchTool::new(db)
            .execute(json!({"query": "量子纠缠"}), &ctx())
            .await
            .unwrap();
        let text = text_of(&out);
        assert!(text.contains("没有找到"), "空结果必须明说:{text}");
        assert!(
            text.contains("量子纠缠"),
            "要带上查询词,让模型知道查的是什么:{text}"
        );
    }

    #[tokio::test]
    async fn k_is_clamped_and_defaulted() {
        let (_d, db) = tmpdb();
        for i in 0..8 {
            db.add_memory(&fact(&format!("用户住在北京第{i}号")))
                .unwrap();
        }
        let out = MemorySearchTool::new(db.clone())
            .execute(json!({"query": "住在北京", "k": 1}), &ctx())
            .await
            .unwrap();
        let one = text_of(&out);
        assert_eq!(one.matches("- ").count(), 1, "k=1 只给一条:{one}");
        // k 超上限按上限处理(不报错:模型给大了是个无害的意图)。
        let out = MemorySearchTool::new(db)
            .execute(json!({"query": "住在北京", "k": 999}), &ctx())
            .await
            .unwrap();
        let many = text_of(&out);
        assert!(many.matches("- ").count() <= MAX_K);
    }

    #[tokio::test]
    async fn a_blank_query_is_a_bad_input_not_a_full_dump() {
        // 空白 query 若退化成「列出全部」,等于让模型一句话把整个记忆库拉进上下文 ——
        // 那是隐私与预算的双重事故。
        let (_d, db) = tmpdb();
        db.add_memory(&fact("用户住在北京")).unwrap();
        let err = MemorySearchTool::new(db)
            .execute(json!({"query": "   "}), &ctx())
            .await
            .expect_err("空白 query 必须是 BadInput");
        assert!(matches!(err, ToolError::BadInput(_)), "实际:{err:?}");
    }

    #[tokio::test]
    async fn a_missing_query_field_is_a_bad_input() {
        let (_d, db) = tmpdb();
        let err = MemorySearchTool::new(db)
            .execute(json!({"k": 3}), &ctx())
            .await
            .expect_err("缺 query 必须是 BadInput");
        assert!(matches!(err, ToolError::BadInput(_)));
    }

    #[tokio::test]
    async fn cancelling_before_execution_returns_cancelled() {
        let (_d, db) = tmpdb();
        db.add_memory(&fact("用户住在北京")).unwrap();
        let c = ctx();
        c.cancel.cancel();
        let err = MemorySearchTool::new(db)
            .execute(json!({"query": "北京"}), &c)
            .await
            .expect_err("取消之后不许再查");
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[test]
    fn render_hits_marks_confirmations_only_when_there_are_any() {
        // 渲染是纯函数,直接测形状:无确认次数时不显示「确认 0 次」的噪声。
        let row = |reinforcements: i64| MemoryRow {
            id: 1,
            session_id: None,
            kind: "fact".into(),
            subject: None,
            predicate: None,
            object: None,
            body: "用户住在北京".into(),
            confidence: 0.9,
            valid_from: 0,
            invalid_at: None,
            updated_at: 0,
            reinforcements,
            last_accessed: None,
            due_at: None,
            notified_at: None,
            user_edited: false,
        };
        let plain = render_hits("北京", &[row(0)]);
        assert!(plain.contains("用户住在北京"));
        assert!(!plain.contains("确认"), "零确认不显示噪声:{plain}");

        let strong = render_hits("北京", &[row(3)]);
        assert!(strong.contains("确认 3 次"), "有确认就要标出来:{strong}");
    }
}
