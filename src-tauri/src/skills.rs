//! 技能包：`skills/<目录>/SKILL.md`。
//!
//! 每个技能是一个文件夹，里面放一份 `SKILL.md`，开头可选 frontmatter：
//!
//! ```text
//! ---
//! name: 翻译助手
//! description: 中英互译，保留术语表
//! ---
//! 正文：模型调用 use_skill 后读到的完整步骤。
//! ```
//!
//! 这里只读本地文本：不执行脚本、不联网。列表（名字 + 一句话描述）由会话拼进系统
//! 提示，正文只在模型调用 `use_skill` 时才读出来，免得几十份技能把上下文撑满。
//!
//! 两条安全边界：
//! * **模型给的 `name` 永远不会被当成路径**：先发现目录、再按名字/文件夹名匹配，
//!   读取的是发现阶段就验证过落在技能根之内的那个文件。`../` 之类因此没有落点。
//! * 每个 `SKILL.md` 都 canonicalize 后要求仍在（canonicalize 过的）技能根之内，
//!   指向根外的 junction / 符号链接会被跳过。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SKILL_FILE: &str = "SKILL.md";
/// 选择表：只记「被停用的文件夹」。新加进来的技能默认启用，跟主流 agent 的取向一致。
/// 它住在**数据目录**（便携版里就是 U 盘的 `LFTTDO\data`），不写回用户的技能文件夹。
pub const SELECTION_FILE: &str = "skills.json";
/// 交给模型的技能条数上限：再多也只是把系统提示撑长。
pub const MAX_SKILLS: usize = 50;
pub const MAX_NAME_CHARS: usize = 40;
pub const MAX_DESCRIPTION_CHARS: usize = 160;
/// 单份技能正文的上限。技能是给人读的操作说明，不承担传大段资料的角色。
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// 本轮自动加载的技能份数上限：本地小模型一次吞两份长正文就会开始漏看步骤。
pub const MAX_LOADED_PER_TURN: usize = 2;
/// 自动加载时单份正文的上限；超了就只给前缀并让模型需要时自己调 `use_skill` 读全。
pub const MAX_LOADED_BYTES: usize = 16 * 1024;
/// 自动命中用的最短触发词：单字（"看"、"写"）会把什么都匹配上。
const MIN_TRIGGER_CHARS: usize = 2;
/// 读全文时最多读这么多：frontmatter 在文件头，超出部分本来也会被正文上限截掉。
const MAX_READ_BYTES: usize = 1024 * 1024;
/// 扫索引时只读文件头：名字与一句话描述都在开头，没必要每轮把 50 份正文全读一遍。
const DISCOVERY_READ_BYTES: usize = 8 * 1024;
const TRUNCATED_BODY_NOTE: &str = "\n\n[技能正文超过 64 KiB，已截断]";
const TRUNCATED_LOADED_NOTE: &str =
    "\n\n[本轮只加载了前 16 KiB；需要完整步骤请调用 use_skill 读取这个技能]";

#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// 展示名：frontmatter 的 `name`，否则第一个一级标题，否则文件夹名。
    pub name: String,
    /// 一句话用途（进系统提示）。
    pub description: String,
    /// 触发词（frontmatter `keywords` / `关键词`，已转小写）：用户这句话里出现它们
    /// 就把正文直接加载进本轮，不赌模型自己想到要调 `use_skill`。
    pub keywords: Vec<String>,
    /// 文件夹名：模型用中文名或目录名都能取到同一份技能。
    pub folder: String,
    /// 已验证位于技能根之内的 SKILL.md。
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// 选择表。损坏或缺失都按「全启用」处理：宁可让用户再关一次，也不要让一份坏文件
/// 把技能全部静默藏起来。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    #[serde(default)]
    pub disabled: Vec<String>,
}

pub fn load_selection(dir: &Path) -> Selection {
    let Ok(raw) = std::fs::read_to_string(dir.join(SELECTION_FILE)) else {
        return Selection::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_selection(dir: &Path, sel: &Selection) -> Result<(), String> {
    let mut disabled = sel.disabled.clone();
    disabled.sort();
    disabled.dedup();
    let body = serde_json::to_string_pretty(&Selection { disabled })
        .map_err(|e| format!("序列化 {SELECTION_FILE} 失败：{e}"))?;
    std::fs::write(dir.join(SELECTION_FILE), body + "\n")
        .map_err(|e| format!("写 {SELECTION_FILE} 失败：{e}"))
}

pub fn is_enabled(sel: &Selection, folder: &str) -> bool {
    !sel.disabled.iter().any(|f| f == folder)
}

pub fn set_enabled(sel: &mut Selection, folder: &str, enabled: bool) {
    sel.disabled.retain(|f| f != folder);
    if !enabled {
        sel.disabled.push(folder.to_string());
    }
}

/// 扫描技能根：只认**直接子目录**里的 `SKILL.md`，按展示名排序。
/// 根不存在、读不了、或一份技能都没有时返回空表（调用方据此隐藏 `use_skill`）。
pub fn discover(dir: &Path) -> Vec<SkillEntry> {
    let Ok(root) = dir.canonicalize() else {
        return Vec::new();
    };
    let Ok(children) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for child in children.flatten() {
        let child_path = child.path();
        if !child_path.is_dir() {
            continue;
        }
        let Some(path) = skill_file_in(&child_path) else {
            continue;
        };
        // 解析后的路径必须仍在根内：指向根外的 junction / 符号链接会在这里被挡下。
        let Ok(real) = path.canonicalize() else {
            continue;
        };
        if !real.starts_with(&root) {
            continue;
        }
        let Ok(raw) = read_limited(&real, DISCOVERY_READ_BYTES) else {
            continue;
        };
        let folder = child_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let parsed = parse(&raw, &folder);
        entries.push(SkillEntry {
            name: parsed.name,
            description: parsed.description,
            keywords: parsed.keywords,
            folder,
            path: real,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.folder.cmp(&b.folder)));
    // 不在这里截断：先截会让第 51 份之后启用了也读不到（`read` 也走 discover）。
    // 提示长度由 `prompt_section` 的 take(MAX_SKILLS) 控制。
    entries
}

/// 按展示名或文件夹名取一份技能的完整正文。名字匹配不区分 ASCII 大小写。
/// `disabled` 里的文件夹会被拒：停用既要藏出索引，也要挡住模型凭空喊它。
pub fn read(dir: &Path, name: &str, disabled: &[String]) -> Result<Skill, String> {
    let entries = discover(dir);
    let want = name.trim();
    let entry = entries
        .iter()
        .find(|e| e.name == want || e.folder == want)
        .or_else(|| {
            entries
                .iter()
                .find(|e| e.name.eq_ignore_ascii_case(want) || e.folder.eq_ignore_ascii_case(want))
        })
        .ok_or_else(|| {
            if entries.is_empty() {
                "当前没有可用技能（技能目录为空）".to_string()
            } else {
                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                format!("没有找到技能「{want}」。可用技能：{}", names.join("、"))
            }
        })?;
    if disabled.iter().any(|f| f == &entry.folder) {
        return Err(format!(
            "技能「{}」已停用；在侧栏「技能」里启用后再用",
            entry.name
        ));
    }
    let raw = read_limited(&entry.path, MAX_READ_BYTES)
        .map_err(|e| format!("读取技能失败: {}: {e}", entry.path.display()))?;
    Ok(body_of(entry, &raw))
}

/// 从已发现的条目直接取正文（本轮自动加载走这条路，不再重扫一遍目录）。
pub fn body_of_entry(entry: &SkillEntry) -> Result<Skill, String> {
    let raw = read_limited(&entry.path, MAX_READ_BYTES)
        .map_err(|e| format!("读取技能失败: {}: {e}", entry.path.display()))?;
    Ok(body_of(entry, &raw))
}

fn body_of(entry: &SkillEntry, raw: &str) -> Skill {
    let parsed = parse(raw, &entry.folder);
    Skill {
        name: parsed.name,
        description: parsed.description,
        body: parsed.body,
    }
}

/// 用户这句话命中了哪些技能：技能名或任一触发词作为子串出现即算命中（不区分 ASCII 大小写）。
/// 短于 `MIN_TRIGGER_CHARS` 的词一律不算，否则「看」「写」这种字会把所有技能都点着。
pub fn matched<'a>(entries: &'a [SkillEntry], text: &str, limit: usize) -> Vec<&'a SkillEntry> {
    let hay = text.to_lowercase();
    let mut out: Vec<&SkillEntry> = Vec::new();
    for entry in entries {
        let name = entry.name.to_lowercase();
        let hit = hits(&name, &hay) || entry.keywords.iter().any(|k| hits(k, &hay));
        if hit {
            out.push(entry);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn hits(needle: &str, hay: &str) -> bool {
    if needle.chars().count() < MIN_TRIGGER_CHARS {
        return false;
    }
    // 中文按子串就够了；英文短词要词边界，否则 "ai" 会命中 "said"、"go" 命中 "going"。
    if !needle.is_ascii() {
        return hay.contains(needle);
    }
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// 本轮自动加载的技能正文那一段。开头就把「不必再调 use_skill」说死，
/// 否则小模型会再读一遍、白烧一个往返。
pub fn loaded_section(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "### 本轮已加载的技能\n\
         下面这些技能的完整说明已经在上下文里了，直接照它执行，**不要**再调用 use_skill 重复读取。\n",
    );
    for s in skills {
        out.push_str("\n#### ");
        out.push_str(&one_line(&s.name, MAX_NAME_CHARS));
        out.push('\n');
        if s.body.len() > MAX_LOADED_BYTES {
            let keep = floor_char_boundary(&s.body, MAX_LOADED_BYTES);
            out.push_str(&s.body[..keep]);
            out.push_str(TRUNCATED_LOADED_NOTE);
        } else {
            out.push_str(&s.body);
            out.push('\n');
        }
    }
    Some(out.trim_end().to_string())
}

/// 拼进系统提示的那一段；没有技能时返回 `None`（连标题都不出现）。
pub fn prompt_section(entries: &[SkillEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## 技能\n\
         下面是技能目录里的技能包。规则：用户的请求与某条描述相符时，**先**调用 use_skill\
         （参数 name 填技能名）读出正文，再按正文步骤做；与技能无关时不要调用。\n",
    );
    for e in entries.iter().take(MAX_SKILLS) {
        out.push_str("- ");
        out.push_str(&one_line(&e.name, MAX_NAME_CHARS));
        out.push('：');
        out.push_str(&one_line(&e.description, MAX_DESCRIPTION_CHARS));
        out.push('\n');
    }
    Some(out.trim_end().to_string())
}

/// `keywords: 翻译, 润色、译成英文` → 小写触发词表（去空、去重）。
/// 英文短语（"word wrap"）整体保留：拆成两个词会把 "word" 这种常见词变成误触源；
/// 纯中文的空白分隔（"翻译 润色"）仍然拆开，那是中文用户最常见的写法。
fn split_keywords(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in value.split([',', '，', '、', ';', '；', '/', '|']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.len() > 1 && tokens.iter().all(|t| !t.is_ascii()) {
            out.extend(tokens.into_iter().map(|t| t.to_lowercase()));
        } else {
            out.push(part.to_lowercase());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 在子目录里找 `SKILL.md`（Windows 文件名本来就不区分大小写，这里显式放宽，
/// 免得别的文件系统上 `skill.md` 被无声忽略）。
fn skill_file_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().eq_ignore_ascii_case(SKILL_FILE) {
            let path = entry.path();
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn read_limited(path: &Path, cap: usize) -> std::io::Result<String> {
    // 只读到 cap 为止：`fs::read` 会把整份文件搬进内存，一个误放的巨大 SKILL.md
    // 会在每轮发现阶段被完整读一遍。
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct Parsed {
    name: String,
    description: String,
    keywords: Vec<String>,
    body: String,
}

/// 容错解析：frontmatter 缺什么就用正文/文件夹名补什么。
/// 项目不引 YAML 依赖 —— 只需要 name / description / keywords 三个键，
/// 手写这几行比多一个解析库划算。
fn parse(raw: &str, folder: &str) -> Parsed {
    let text = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (header, body) = split_frontmatter(&text);
    let mut name = String::new();
    let mut description = String::new();
    let mut keywords: Vec<String> = Vec::new();
    if let Some(header) = header {
        for line in header.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            match key.trim().to_ascii_lowercase().as_str() {
                "name" | "名称" => name = value,
                "description" | "描述" => description = value,
                "keywords" | "关键词" => keywords = split_keywords(&value),
                _ => {}
            }
        }
    }
    let body = body.trim_start_matches('\n');
    if name.trim().is_empty() {
        name = body
            .lines()
            .find_map(|line| {
                let line = line.trim_start();
                line.strip_prefix("# ").map(|t| t.trim().to_string())
            })
            .unwrap_or_default();
    }
    if name.trim().is_empty() {
        name = folder.to_string();
    }
    if description.trim().is_empty() {
        description = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .unwrap_or("")
            .to_string();
    }
    let mut body = body.to_string();
    if body.len() > MAX_BODY_BYTES {
        let keep = floor_char_boundary(&body, MAX_BODY_BYTES);
        body.truncate(keep);
        body.push_str(TRUNCATED_BODY_NOTE);
    }
    Parsed {
        name: one_line(&name, MAX_NAME_CHARS),
        description: one_line(&description, MAX_DESCRIPTION_CHARS),
        keywords,
        body,
    }
}

/// 返回 frontmatter 内容与正文。没有闭合的 `---` 就当没有 frontmatter，
/// 整份都算正文（宁可多一行标记，也不把用户的操作说明吞掉）。
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim() == "---" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

fn one_line(s: &str, max_chars: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = joined.chars().take(max_chars).collect();
    if joined.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("lfttdo-skills-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_skill(root: &Path, folder: &str, text: &str) -> PathBuf {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(SKILL_FILE);
        std::fs::write(&file, text).unwrap();
        file
    }

    #[test]
    fn frontmatter_supplies_name_and_description_and_is_not_part_of_the_body() {
        let root = tmp_dir("frontmatter");
        write_skill(
            &root,
            "translate",
            "---\r\nname: 翻译助手\r\ndescription: 中英互译，保留术语\r\n---\r\n\r\n# 翻译\r\n步骤一\r\n",
        );
        let entries = discover(&root);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].name, "翻译助手");
        assert_eq!(entries[0].description, "中英互译，保留术语");
        assert_eq!(entries[0].folder, "translate");

        let skill = read(&root, "翻译助手", &[]).unwrap();
        assert!(skill.body.contains("步骤一"), "{}", skill.body);
        assert!(
            !skill.body.contains("description:"),
            "frontmatter 不该交给模型"
        );
        assert!(!skill.body.starts_with('\n'), "正文开头的空行要裁掉");

        // 中文键也认。
        write_skill(
            &root,
            "cn",
            "---\n名称: 周报\n描述: 把要点整理成周报\n---\n正文\n",
        );
        let skill = read(&root, "周报", &[]).unwrap();
        assert_eq!(skill.description, "把要点整理成周报");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn heading_and_folder_name_are_the_fallbacks() {
        let root = tmp_dir("fallback");
        write_skill(&root, "weekly", "# 周报助手\n\n把要点整理成周报。\n");
        write_skill(&root, "naked", "没有标题，也没有 frontmatter。\n");
        let entries = discover(&root);
        assert_eq!(entries.len(), 2, "{entries:?}");
        let weekly = entries.iter().find(|e| e.folder == "weekly").unwrap();
        assert_eq!(weekly.name, "周报助手");
        assert_eq!(weekly.description, "把要点整理成周报。");
        let naked = entries.iter().find(|e| e.folder == "naked").unwrap();
        assert_eq!(naked.name, "naked", "连标题都没有时用文件夹名");
        assert_eq!(naked.description, "没有标题，也没有 frontmatter。");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discovery_sorts_by_name_ignores_files_and_caps_the_list() {
        let root = tmp_dir("sort");
        write_skill(&root, "d1", "---\nname: 丙\ndescription: c\n---\n");
        write_skill(&root, "d2", "---\nname: 甲\ndescription: a\n---\n");
        write_skill(&root, "d3", "---\nname: 乙\ndescription: b\n---\n");
        std::fs::write(root.join("说明.txt"), "不是技能").unwrap();
        let names: Vec<String> = discover(&root).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["丙", "乙", "甲"], "按字符序稳定输出");

        for i in 0..MAX_SKILLS + 3 {
            write_skill(
                &root,
                &format!("many-{i:02}"),
                &format!("# 技能{i:02}\n描述\n"),
            );
        }
        assert_eq!(
            discover(&root).len(),
            MAX_SKILLS + 6,
            "发现不再截断：第 51 份之后启用了也要读得到（提示长度由 prompt_section 控制）"
        );
        assert!(
            read(&root, "技能52", &[]).is_ok(),
            "第 51 份之后的技能必须能被 use_skill 读到"
        );
        let prompt = prompt_section(&discover(&root)).expect("有技能就有索引");
        assert_eq!(
            prompt.lines().filter(|l| l.starts_with("- ")).count(),
            MAX_SKILLS,
            "系统提示只列前 50 个，避免撑长上下文"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ascii_triggers_need_word_boundaries_and_phrases_stay_whole() {
        let root = tmp_dir("ascii");
        write_skill(
            &root,
            "ai",
            "---\nname: AI 助手\ndescription: d\nkeywords: ai, word wrap\n---\n正文\n",
        );
        let entries = discover(&root);
        assert!(
            matched(&entries, "he said going", 2).is_empty(),
            "ai 不许命中 said、go 不许命中 going"
        );
        assert_eq!(
            matched(&entries, "use AI here", 2).len(),
            1,
            "独立的单词 AI 要命中（大小写不敏感）"
        );
        assert_eq!(
            matched(&entries, "please word wrap this", 2).len(),
            1,
            "英文短语整体匹配"
        );
        assert!(
            matched(&entries, "a word only", 2).is_empty(),
            "英文短语不许拆成单词误触"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_matches_name_or_folder_case_insensitively_and_lists_choices_on_a_miss() {
        let root = tmp_dir("match");
        write_skill(
            &root,
            "Translate",
            "---\nname: 翻译助手\ndescription: 翻译\n---\n正文在这里\n",
        );
        for key in ["翻译助手", "Translate", "translate", "TRANSLATE"] {
            let s = read(&root, key, &[]).unwrap_or_else(|e| panic!("{key} 取不到：{e}"));
            assert!(s.body.contains("正文在这里"));
        }
        let e = read(&root, "不存在", &[]).unwrap_err();
        assert!(e.contains("没有找到技能") && e.contains("翻译助手"), "{e}");

        let empty = tmp_dir("empty");
        assert!(
            read(&empty, "x", &[]).unwrap_err().contains("技能目录为空"),
            "空目录要和「名字写错」分开说"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&empty).ok();
    }

    #[test]
    fn a_giant_body_is_cut_on_a_char_boundary_with_a_note() {
        let root = tmp_dir("giant");
        let body = "中".repeat(MAX_BODY_BYTES); // 3 字节一个字符，必然超限
        write_skill(
            &root,
            "big",
            &format!("---\nname: 大技能\ndescription: 大\n---\n{body}"),
        );
        let skill = read(&root, "大技能", &[]).unwrap();
        assert!(skill.body.ends_with(TRUNCATED_BODY_NOTE));
        assert!(
            skill
                .body
                .chars()
                .all(|c| c == '中' || TRUNCATED_BODY_NOTE.contains(c)),
            "截断后仍是合法 UTF-8 文本"
        );
        assert!(skill.body.len() < MAX_BODY_BYTES + 200);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn names_and_descriptions_are_flat_single_line_strings() {
        let root = tmp_dir("flat");
        write_skill(
            &root,
            "flat",
            "---\nname: 第一行\n  第二行\ndescription: 描述   里有\n换行\n---\n正文\n",
        );
        let e = discover(&root).into_iter().next().unwrap();
        assert!(!e.name.contains('\n') && !e.description.contains('\n'));
        assert_eq!(e.description, "描述 里有", "连续空白要压成一个空格");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_prompt_section_lists_every_skill_and_names_the_tool() {
        let root = tmp_dir("prompt");
        write_skill(
            &root,
            "translate",
            "---\nname: 翻译助手\ndescription: 中英互译\n---\n正文\n",
        );
        let entries = discover(&root);
        let section = prompt_section(&entries).expect("有技能就必须有一段");
        assert!(section.contains("use_skill"), "{section}");
        assert!(
            section.contains("- 翻译助手：中英互译"),
            "模型靠这一行决定要不要读正文：{section}"
        );
        assert!(prompt_section(&[]).is_none(), "没有技能时连标题都不出现");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 根外的技能必须看不见：发现阶段就 canonicalize 并比对，模型侧从来没有一条
    /// 「用路径读技能」的通路（`use_skill` 只按名字匹配）。
    #[cfg(windows)]
    #[test]
    fn a_junction_pointing_outside_the_skill_root_is_skipped() {
        let dir = tmp_dir("junction");
        let root = dir.join("skills");
        std::fs::create_dir_all(&root).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            outside.join(SKILL_FILE),
            "---\nname: 外面的技能\ndescription: 不该被看见\n---\n秘密\n",
        )
        .unwrap();
        let out = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &root.join("leak").display().to_string(),
                &outside.display().to_string(),
            ])
            .output()
            .expect("启动 cmd.exe 建 junction 失败");
        assert!(
            out.status.success(),
            "夹具前提：junction 要建得出来: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let entries = discover(&root);
        assert!(
            entries.is_empty(),
            "根外技能不许出现在索引里（也不许能被 use_skill 读到）：{entries:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn selection_round_trips_and_defaults_to_all_enabled() {
        let dir = tmp_dir("selection");
        assert_eq!(
            load_selection(&dir),
            Selection::default(),
            "没有文件 ⇒ 全启用"
        );

        let mut sel = Selection::default();
        set_enabled(&mut sel, "translate", false);
        set_enabled(&mut sel, "translate", false);
        assert!(!is_enabled(&sel, "translate"));
        save_selection(&dir, &sel).unwrap();
        assert_eq!(
            load_selection(&dir).disabled,
            vec!["translate".to_string()],
            "同一个文件夹写两次也只留一条"
        );

        set_enabled(&mut sel, "translate", true);
        assert!(is_enabled(&sel, "translate"));
        save_selection(&dir, &sel).unwrap();
        assert!(load_selection(&dir).disabled.is_empty());

        std::fs::write(dir.join(SELECTION_FILE), "{ not json").unwrap();
        assert_eq!(
            load_selection(&dir),
            Selection::default(),
            "坏文件 ⇒ 全启用（宁可再关一次，也不许把技能静默藏光）"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keywords_are_parsed_and_match_the_users_own_words() {
        let root = tmp_dir("keywords");
        write_skill(
            &root,
            "translate",
            "---\nname: 翻译助手\ndescription: 中英互译\nkeywords: 翻译, 润色、译成英文\n---\n步骤一\n",
        );
        let entries = discover(&root);
        let mut want = vec!["润色", "译成英文", "翻译"];
        want.sort();
        assert_eq!(entries[0].keywords, want, "触发词要解析、去空并排序");
        // 用户自己的词命中。
        assert_eq!(matched(&entries, "帮我把这段润色一下", 2).len(), 1);
        // 技能名命中（面板「用这个技能」填的那句话就靠这条）。
        assert_eq!(matched(&entries, "使用「翻译助手」技能：hi", 2).len(), 1);
        // 无关的话一条都不命中。
        assert!(matched(&entries, "今天天气怎么样", 2).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn matching_needs_a_real_trigger_word_and_respects_the_limit() {
        let root = tmp_dir("match");
        // 名字与触发词都只有一个字：不该把任何话都点着。
        write_skill(
            &root,
            "one",
            "---\nname: 看\ndescription: 单字名\nkeywords: 看\n---\n正文\n",
        );
        write_skill(
            &root,
            "a",
            "---\nname: 甲技能\ndescription: d\nkeywords: 甲\n---\n甲正文\n",
        );
        write_skill(
            &root,
            "b",
            "---\nname: 乙技能\ndescription: d\nkeywords: 乙\n---\n乙正文\n",
        );
        let entries = discover(&root);
        assert!(
            matched(&entries, "帮我看看这个", 2).is_empty(),
            "单字触发词不许把技能点着"
        );
        assert_eq!(
            matched(&entries, "甲技能和乙技能都要用", MAX_LOADED_PER_TURN).len(),
            MAX_LOADED_PER_TURN,
            "命中数要夹在上限内，别把上下文塞爆"
        );

        let loaded: Vec<Skill> = matched(&entries, "甲技能和乙技能都要用", 2)
            .into_iter()
            .map(|e| body_of_entry(e).unwrap())
            .collect();
        let section = loaded_section(&loaded).expect("有加载就要有这一段");
        assert!(
            section.contains("不要"),
            "要说清不必再调 use_skill：{section}"
        );
        assert!(section.contains("#### 甲技能"), "{section}");
        assert!(section.contains("甲正文"), "{section}");
        assert!(loaded_section(&[]).is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_disabled_skill_is_still_listed_but_not_readable() {
        let root = tmp_dir("disabled");
        write_skill(
            &root,
            "translate",
            "---\nname: 翻译助手\ndescription: 中英互译\n---\n正文\n",
        );
        let e = read(&root, "翻译助手", &["translate".to_string()]).unwrap_err();
        assert!(
            e.contains("已停用"),
            "停用的技能必须明说，不能假装不存在：{e}"
        );
        assert_eq!(
            discover(&root).len(),
            1,
            "停用只挡读取，不影响发现：面板仍要能列出它并让用户重新启用"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
