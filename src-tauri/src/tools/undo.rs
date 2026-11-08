//! undo：写类工具的快照与撤销（对齐 OpenCode 的 per-step snapshot + /undo）。
//!
//! 机制：`write_file` / `edit_file` 在**通过全部校验之后、落盘之前**把旧内容
//! （仅当文件已存在）按内容哈希存进 `.lfttdo/undo/`，并在 `journal.json`
//! 追加一条 {target, snapshot}。`undo` 工具弹出最后一条，把快照**原样写回**
//! （字节级，含 CRLF）。**新建不进 journal —— undo 永不删除用户文件**：
//! 撤销的语义是「把内容恢复到之前的状态」，不是「抹掉这个文件曾经被创建」，
//! 删除是比覆盖更难逆转的动作，不做。校验失败的工具调用不会走到快照。
//!
//! Risk::Write：undo 覆写的是用户文件（恢复旧内容也是一次写入），照常弹窗；
//! 这与 OpenCode 把 undo 做成用户命令（/undo）是同一谨慎档位。
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::tools::todo::APP_STATE_DIR;
use crate::tools::todo::usable_with_roots as base_usable;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

pub const NAME: &str = "undo";

/// 复用 todo 的可用判据（roots 非空）：undo 的落点同样是 write_root / roots[0]。
pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    base_usable(roots)
}

fn undo_dir(base: &Path) -> PathBuf {
    base.join(APP_STATE_DIR).join("undo")
}

fn journal_path(base: &Path) -> PathBuf {
    undo_dir(base).join("journal.json")
}

/// 基目录：write_root 优先，缺省 roots[0]；都没有 = 没有可记录的落点。
fn base_dir(ctx: &ToolCtx) -> Result<PathBuf, ToolError> {
    ctx.write_root
        .clone()
        .or_else(|| ctx.roots.first().cloned())
        .ok_or_else(|| {
            ToolError::BadInput("没有可用的工作目录（roots 与 write_root 都为空）".to_string())
        })
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct JournalEntry {
    target: String,
    /// 变更前的内容文件名；None = 变更前不存在（新建）。
    #[serde(default)]
    snapshot: Option<String>,
}

fn read_journal(base: &Path) -> Vec<JournalEntry> {
    match std::fs::read_to_string(journal_path(base)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_journal(base: &Path, entries: &[JournalEntry]) -> Result<(), ToolError> {
    let path = journal_path(base);
    std::fs::create_dir_all(path.parent().expect("journal 路径必有父目录"))
        .map_err(|e| ToolError::Failed(format!("无法创建撤销目录: {e}")))?;
    let text = serde_json::to_string(entries)
        .map_err(|e| ToolError::Failed(format!("journal 序列化失败: {e}")))?;
    crate::tools::fs_write::atomic_write(&path, &text)
}

/// 写类工具的变更前快照：把 target 当前状态记入 journal。
/// **调用时机是契约**：全部校验之后、`atomic_write` 之前 —— 失败的调用不留痕。
pub(crate) fn snapshot_before(ctx: &ToolCtx, target: &Path) -> Result<(), ToolError> {
    let base = base_dir(ctx)?;
    // 文件不存在（这是一次新建）：不记 journal —— undo 永不删除用户文件。
    let Ok(content) = std::fs::read_to_string(target) else {
        return Ok(());
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    let name = format!("{:016x}.txt", hasher.finish());
    let path = undo_dir(&base).join(&name);
    std::fs::create_dir_all(path.parent().expect("undo 路径必有父目录"))
        .map_err(|e| ToolError::Failed(format!("无法创建撤销目录: {e}")))?;
    crate::tools::fs_write::atomic_write(&path, &content)?;
    let mut entries = read_journal(&base);
    entries.push(JournalEntry {
        target: target.display().to_string(),
        snapshot: Some(name),
    });
    write_journal(&base, &entries)
}

pub struct UndoTool;

#[async_trait]
impl Tool for UndoTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.to_string(),
            description: "撤销最近一次 write_file / edit_file 对文件系统的变更：恢复该文件到\
                          变更之前的内容（新建的文件会被删除）。可以连续调用逐级回退；\
                          没有可撤销的变更时报错。只作用于本工作目录内的文件，其他修改\
                          （run_command 的副作用）不在快照范围内。"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            risk: Risk::Write,
        }
    }

    async fn execute(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let base = base_dir(ctx)?;
        let mut entries = read_journal(&base);
        let entry = entries
            .pop()
            .ok_or_else(|| ToolError::Failed("没有可撤销的变更（journal 为空）".to_string()))?;
        let target = PathBuf::from(&entry.target);
        let Some(name) = &entry.snapshot else {
            // journal 里理论上只有「已存在文件的变更」；这个分支只为老 journal 兜底。
            return Err(ToolError::Failed(format!(
                "这条变更没有快照（新建不参与撤销）: {}",
                target.display()
            )));
        };
        let snap = undo_dir(&base).join(name);
        let content = std::fs::read_to_string(&snap).map_err(|e| {
            ToolError::Failed(format!("快照丢失，无法撤销 {}: {e}", target.display()))
        })?;
        crate::tools::fs_write::atomic_write(&target, &content)?;
        std::fs::remove_file(&snap).ok();
        write_journal(&base, &entries)?;
        Ok(ToolOutput::text(format!(
            "已撤销：{} 恢复到变更前的内容",
            target.display()
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

    async fn run(c: &ToolCtx) -> Result<ToolOutput, ToolError> {
        UndoTool.execute(json!({}), c).await
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    /// 完整循环：新建**不可撤销**（undo 永不删除用户文件）；对已有文件的变更
    /// 逐级回退；journal 空了必须报错。
    #[tokio::test]
    async fn snapshots_and_undo_walk_back_through_every_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(
            vec![tmp.path().to_path_buf()],
            Some(tmp.path().to_path_buf()),
        );
        let file = tmp.path().join("note.txt");

        // 新建：不进 journal ⇒ 撤销报错，文件原样保留。
        snapshot_before(&c, &file).unwrap();
        std::fs::write(&file, "hello").unwrap();
        let err = run(&c).await.unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("没有可撤销")),
            "新建不可撤销：{err:?}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");

        // 覆盖 → 编辑 → 两次撤销逐级回退，最后 journal 空了报错。
        snapshot_before(&c, &file).unwrap();
        std::fs::write(&file, "world").unwrap();
        snapshot_before(&c, &file).unwrap();
        std::fs::write(&file, "again").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "again");
        let text = text_of(run(&c).await.unwrap());
        assert!(text.contains("恢复到变更前"), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "world");
        run(&c).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
        let err = run(&c).await.unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("没有可撤销")),
            "journal 空了必须报错：{err:?}"
        );
    }

    /// 跨实例：持久化经文件，新实例的 undo 一样能撤（生产 = 每轮新装配）。
    #[tokio::test]
    async fn the_journal_survives_across_instances() {
        let tmp = tempfile::tempdir().unwrap();
        let c1 = ctx(vec![], Some(tmp.path().to_path_buf()));
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "v0").unwrap();
        snapshot_before(&c1, &file).unwrap();
        std::fs::write(&file, "v1").unwrap();

        let c2 = ctx(vec![tmp.path().to_path_buf()], None);
        run(&c2).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0");
    }

    /// 快照按内容哈希命名：同一内容两次快照不翻倍占盘。
    #[test]
    fn identical_snapshots_share_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let c = ctx(vec![], Some(tmp.path().to_path_buf()));
        let file = tmp.path().join("b.txt");
        std::fs::write(&file, "same").unwrap();
        snapshot_before(&c, &file).unwrap();
        snapshot_before(&c, &file).unwrap();
        let mut files: Vec<PathBuf> = std::fs::read_dir(undo_dir(tmp.path()))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "txt"))
            .collect();
        files.sort();
        assert_eq!(files.len(), 1, "内容哈希相同 ⇒ 同一个快照文件");
    }

    #[test]
    fn name_constant_is_the_registered_name() {
        assert_eq!(UndoTool.meta().name, NAME);
        assert_eq!(NAME, "undo");
        assert!(usable_with_roots(&[PathBuf::from("/tmp")]));
        assert!(!usable_with_roots(&[]));
    }

    #[test]
    fn undo_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(UndoTool));
        assert!(registry.get(NAME).is_some());
        assert_eq!(registry.risk_of(NAME), Risk::Write, "覆写用户文件必须弹窗");
    }
}
