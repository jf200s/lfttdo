// T33/R329：每台 MCP server 的信任档位**落盘**。
//
// 为什么单独一个文件、不塞进 `settings.json`：`SettingsPanel` 保存时按「整份 Settings 去掉
// 两个只读键」回写（`SettingsPanel.tsx:20-25` 的注释），它不渲染的键会被抹回 Rust 默认值。
// 把信任表塞进去，等于「用户点一次保存、所有 server 重新回到 Ask」。单独文件只有这一个写入者。
//
// 形状故意最笨：`{"server": "auto"|"ask"|"block"}` 的扁平对象。解析失败一律当成空表
// （宁可重新授权，也不要让一份坏文件把应用卡在启动阶段）。

use std::collections::BTreeMap;
use std::path::Path;

use crate::permission::gate::Verdict;

pub const TRUST_FILE: &str = "mcp_trust.json";

/// 读信任表。文件不存在 ⇒ 空表；坏文件 ⇒ 空表（不报错、不 panic）。
pub fn load(dir: &Path) -> BTreeMap<String, Verdict> {
    let path = dir.join(TRUST_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let Ok(values) = serde_json::from_str::<BTreeMap<String, String>>(&raw) else {
        return BTreeMap::new();
    };
    values
        .into_iter()
        .filter_map(|(server, word)| Verdict::parse(&word).map(|v| (server, v)))
        .collect()
}

/// 写信任表。`BTreeMap` ⇒ 同一份内容两次落盘逐字节相同（便于比对与排查）。
pub fn save(dir: &Path, trust: &BTreeMap<String, Verdict>) -> Result<(), String> {
    let path = dir.join(TRUST_FILE);
    let words: BTreeMap<&str, &str> = trust
        .iter()
        .map(|(server, verdict)| (server.as_str(), verdict.as_str()))
        .collect();
    let body = serde_json::to_string_pretty(&words).map_err(|e| e.to_string())?;
    std::fs::write(&path, body + "\n").map_err(|e| format!("写 {TRUST_FILE} 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_round_trips_and_a_corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load(dir.path()).is_empty(),
            "没有文件 ⇒ 空表（每台 server 回到 Ask）"
        );

        let mut trust = BTreeMap::new();
        trust.insert("search".to_string(), Verdict::Auto);
        trust.insert("evil".to_string(), Verdict::Block);
        save(dir.path(), &trust).unwrap();
        assert_eq!(load(dir.path()), trust, "同一份内容读写一致");

        // 坏文件不许让启动失败：当成空表（重新授权，不是崩溃）。
        std::fs::write(dir.path().join(TRUST_FILE), "{ not json").unwrap();
        assert!(load(dir.path()).is_empty(), "坏文件 ⇒ 空表");

        // 未知档位逐条丢弃，别的照留。
        std::fs::write(
            dir.path().join(TRUST_FILE),
            r#"{"search":"auto","weird":"maybe"}"#,
        )
        .unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.get("search"), Some(&Verdict::Auto));
        assert!(!loaded.contains_key("weird"), "未知档位要丢掉：{loaded:?}");
    }
}
