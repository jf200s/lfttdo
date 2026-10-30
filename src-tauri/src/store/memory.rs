// 长期记忆的存储层(spec 2026-10-25 §3):memories 与 memory_profile 两张表,
// 以及「压缩 + 记忆写入同事务」的落库入口。
//
// 本文件不得 `use tauri`(Plan A 约束,同 db.rs);所有方法同步 SQLite,
// async 调用方负责 `spawn_blocking`(与 db.rs 的托管形态同一条纪律)。
//
// 表 DDL 不在这里:建表语句的**唯一真相**在 `db.rs::SCHEMA_SQL`,本文件只提供
// 读写行为 —— 与 `insert_message_row`「SQL 只有一处」是同一条规矩。
//
// 裁定(偏离 spec §3.2 的 FTS5 + sqlite-vec,记入 ledger):关键词检索用 LIKE 式 2-gram
// 打分,不建 FTS 表。FTS5 默认 unicode61 分词把整句中文当成一个 token(搜「北京」永远
// 匹配不上「用户住在北京」),trigram 又要求查询 ≥3 字符而中文常用词恰是 2 字;
// 个人记忆量级(≤几千行 × ≤200 字)上本侧扫描是微秒级,不值得为它造 n-gram 索引表。
// T6 起语义半边也是同一裁定:向量存 memories.embedding BLOB(f32 小端 LE 裸存)+ Rust
// 侧全表余弦,**不引 sqlite-vec** —— 量级不变的前提下全表余弦同样是微秒级,而
// sqlite-vec 是新依赖 + 新虚表的规模化方案,留给二期(spec §3.2 的原案)。

use crate::agent::context::CompactionPlan;
use crate::agent::types::{Message, Role};
use crate::store::db::{Db, conversation_ids, insert_message_row};
use rusqlite::{Connection, params};

/// 一条长期记忆的读形态(列与 `SCHEMA_SQL` 里的 memories 表逐列对应)。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRow {
    pub id: i64,
    /// 来源会话;画像合并出的记忆没有单一来源,可为 None。**刻意不建外键**:
    /// 会话删除(CASCADE)不许连带删记忆 —— 记忆的全部意义就是比会话活得久。
    pub session_id: Option<String>,
    /// `fact` | `preference` | `entity_rel`。口径写在抽取提示词里,这里只存字符串。
    pub kind: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    /// 人读的完整句子。注入 head 用这个,不是拼接三元组。
    pub body: String,
    pub confidence: f64,
    /// ms,创建(或最近确认)时间。
    pub valid_from: i64,
    /// None=有效;非空=已失效(Graphiti 式「失效不删除」,检索唯一口径是
    /// `invalid_at IS NULL`)。
    pub invalid_at: Option<i64>,
    pub updated_at: i64,
    /// 同一事实被**再次确认**的次数(合并时选 REINFORCE 累加)。排序里的「强度」来源:
    /// 用户强调过五遍的事,和只说过一遍的,在库里必须不是一个分量。
    pub reinforcements: i64,
    /// 上次被召回的时刻(访问即再巩固):既是衰减的基准之一,也是「最近还用过」的证据。
    pub last_accessed: Option<i64>,
    /// 待办的到期时刻(ms)。`kind=prospective` 才有值;None = 非待办或没解析出时间。
    pub due_at: Option<i64>,
    /// 已提醒时刻。None = 尚未提醒 —— 每轮扫描只捞这个为 NULL 的,防重复刷屏。
    pub notified_at: Option<i64>,
    /// 用户亲手改过正文(P0,spec §12)。模型的下一次合并**不许**改写这样的行:
    /// 候选降级为 ADD 并存,让用户自己决定删哪条。用户权威优先于系统自洽 ——
    /// 这是业主拍板的方向,不是实现者的偏好。
    pub user_edited: bool,
}

/// 写入侧的输入:不含 id 与时间戳(时间戳归 db 层,单一真相)。
#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    pub session_id: Option<String>,
    pub kind: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub body: String,
    pub confidence: f64,
    /// 待办到期时刻(ms,仅 `kind=prospective` 有意义)。抽取侧解析 ISO 8601 失败时
    /// 传 None —— 那会把这条当作普通事实存下,而不是丢掉:用户说的事本身仍然值得记。
    pub due_at: Option<i64>,
}

/// 一次压缩事务里要落的记忆变更(由合并管线产出,T2)。
/// 空表是合法值:抽取失败时压缩照常、一条记忆都不写(spec §4.1)。
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryWrite {
    Add(NewMemory),
    UpdateBody { id: i64, body: String },
    Invalidate { id: i64 },
    /// 同一事实再次被确认(措辞不同、内容一致):不覆盖正文,只把强度加一。
    /// 这是「重复接触使记忆变强」的工程对应 —— 三选一的 ADD/UPDATE/INVALIDATE
    /// 只能表达「多一条/改写/否定」,表达不了「同一件事又说了一遍」。
    Reinforce { id: i64 },
    SetProfile(String),
}

/// SELECT 的列清单。`query_memories` 系的拼接只发生在本文件,`tail` 全是字面量。
/// 列序与 [`memory_from_row`] 的 get 下标一一对应;新增列**追加在末尾**,
/// 既有下标不动(那是这个文件最容易出错的一处耦合)。
const MEMORY_COLS: &str = "id,session_id,kind,subject,predicate,object,body,confidence,\
valid_from,invalid_at,updated_at,reinforcements,last_accessed,due_at,notified_at,user_edited";

/// `MEMORY_COLS` 的列数。带 `embedding` 的查询把那一列**追加**在末尾,
/// 下标就是它 —— 手写 `r.get(15)` 会在下次加列时静默指向别的字段。
const MEMORY_COL_COUNT: usize = 16;

/// 行 → `MemoryRow` 的唯一解码点(镜像 db.rs 的 `role_from_code` 纪律:
/// 损坏解不开就报错,不返回「看起来完整」的行)。
fn memory_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: r.get(0)?,
        session_id: r.get(1)?,
        kind: r.get(2)?,
        subject: r.get(3)?,
        predicate: r.get(4)?,
        object: r.get(5)?,
        body: r.get(6)?,
        confidence: r.get(7)?,
        valid_from: r.get(8)?,
        invalid_at: r.get(9)?,
        updated_at: r.get(10)?,
        reinforcements: r.get(11)?,
        last_accessed: r.get(12)?,
        due_at: r.get(13)?,
        notified_at: r.get(14)?,
        // SQLite 的 BOOLEAN 习惯是 0/1 整数;`get::<bool>` 在 rusqlite 里按
        // 「非 0 即真」处理,显式写出来免得读代码的人以为是字符串。
        user_edited: r.get::<_, i64>(15)? != 0,
    })
}

/// 召回的打分参数(类人记忆升级,spec 2026-10-25 §11)。集中成结构体而不是四个散参数:
/// 排序公式的每一项都要能被测试单独钉住,而 `Default` 给了「生产用默认、测试改一项」
/// 的写法 —— 散参数做不到这件事。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecallCfg {
    /// 衰减半衰期(天)。事实比偏好忘得快:偏好是人对自己的稳定描述,事实会变
    /// (搬家、换工作),所以两者的时间常数本就该不同。
    pub fact_half_life_days: f64,
    pub preference_half_life_days: f64,
    /// 强度权重:第 n 次确认带来的乘数 = `1 + strength_weight * ln(1 + n)`。
    /// 对数是为了「前几次确认涨得快、之后边际递减」,权重压住它不喧宾夺主
    /// (相关性仍是第一位;强度只在相关性接近时才该改变次序)。
    pub strength_weight: f64,
    /// 衰减下限。**永不归零**:遗忘曲线在人是渐近的,不是到点删除;一条一年没碰的
    /// 记忆仍然存在、仍然可能被搜到,只是排得靠后。归零会让「共同回忆」类查询全军覆没。
    pub decay_floor: f64,
    /// 当前时刻(ms)。显式传入而不是内部读钟:测试要能构造「三个月前的记忆」。
    pub now_ms: i64,
}

impl Default for RecallCfg {
    fn default() -> Self {
        Self {
            fact_half_life_days: 60.0,
            preference_half_life_days: 180.0,
            strength_weight: 0.15,
            decay_floor: 0.35,
            now_ms: Db::now_ms(),
        }
    }
}

impl RecallCfg {
    /// 某个 kind 的半衰期(天)。未列出的 kind 走事实的档:保守方向 ——
    /// 判不准的东西更快下沉,而不是永远赖在榜首。
    /// `prospective` 返回 `None` = **不衰减**:待办的价值与「多久以前记的」无关,
    /// 它只看 `due_at`(到期提醒走另一条独立通道,见 `due_prospective`)。
    fn half_life_days(&self, kind: &str) -> Option<f64> {
        match kind {
            "preference" => Some(self.preference_half_life_days),
            "prospective" => None,
            _ => Some(self.fact_half_life_days),
        }
    }

    /// 时间衰减因子,值域 `[decay_floor, 1.0]`。
    /// 基准时刻取「最近一次被任何方式触碰」的时刻:创建、改写、被召回,三者取最大。
    /// 人被问到某件事时记忆会被重新巩固,所以「上次召回」也是新鲜度的一部分。
    fn decay(&self, m: &MemoryRow) -> f64 {
        let Some(half_life) = self.half_life_days(&m.kind) else {
            return 1.0;
        };
        let last = m
            .valid_from
            .max(m.updated_at)
            .max(m.last_accessed.unwrap_or(i64::MIN));
        let age_days = (self.now_ms - last).max(0) as f64 / 86_400_000.0;
        // 2^{-age/half_life}:age = 0 → 1,age = 半衰期 → 0.5,渐近到 0。
        let raw = 0.5f64.powf(age_days / half_life.max(1e-9));
        self.decay_floor + (1.0 - self.decay_floor) * raw
    }

    /// 强度因子:确认次数越多越强。`reinforcements = 0` 时恒为 1.0
    /// (从未被重复确认过的记忆不带任何加成,也不受罚)。
    fn strength(&self, m: &MemoryRow) -> f64 {
        1.0 + self.strength_weight * ((1 + m.reinforcements.max(0)) as f64).ln()
    }
}

impl Db {
    pub fn add_memory(&self, m: &NewMemory) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        insert_memory_row(&c, m)
    }

    pub fn valid_memories(&self) -> Result<Vec<MemoryRow>, rusqlite::Error> {
        let c = self.lock();
        query_memories(&c, "WHERE invalid_at IS NULL ORDER BY id")
    }

    /// 设置页的审计视图:含已失效的(用户要能看见「程序记过什么、又否定过什么」)。
    pub fn all_memories(&self) -> Result<Vec<MemoryRow>, rusqlite::Error> {
        let c = self.lock();
        query_memories(&c, "ORDER BY id")
    }

    /// 合并管线的查重入口:同 subject+predicate 的**有效**记忆。
    /// 三元组缺 subject/predicate 的行(NULL)天然不命中 —— SQL 的 `= ?` 对 NULL 永假,
    /// 这正是想要的:没有键的记忆无法与任何候选比对,只能共存。
    pub fn find_active_by_key(
        &self,
        subject: &str,
        predicate: &str,
    ) -> Result<Vec<MemoryRow>, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {MEMORY_COLS} FROM memories \
             WHERE invalid_at IS NULL AND subject=?1 AND predicate=?2 ORDER BY id"
        ))?;
        let rows = st
            .query_map(params![subject, predicate], memory_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn update_memory_body(&self, id: i64, body: &str) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        update_memory_body(&c, id, body)
    }

    /// 失效不删除(spec §3):`invalid_at` 打时间戳,行保留。
    pub fn invalidate_memory(&self, id: i64) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        invalidate_memory_row(&c, id)
    }

    /// 物理删除。只归设置页的「删除这一条」与 `clear_memories`;
    /// 语义否定走 `invalidate_memory` —— 两个出口,两种不可逆程度。
    pub fn delete_memory(&self, id: i64) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        let n = c.execute("DELETE FROM memories WHERE id=?1", params![id])?;
        affected_one(n, "删除", id)
    }

    /// 清空全部长期记忆(物理删,含失效行)并把画像归空串。设置页「清空」按钮的唯一后端。
    /// 画像行不删只清空:单行不变量(id=1)由 schema 的 CHECK 钉着,删了再插也一样,
    /// 留着这一行省一次「到底有没有写过画像」的分支。
    pub fn clear_memories(&self) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        c.execute("DELETE FROM memories", [])?;
        c.execute(
            "UPDATE memory_profile SET body='', updated_at=?1 WHERE id=1",
            params![Db::now_ms()],
        )?;
        Ok(())
    }

    /// 写入一行记忆的向量(T6)。发生在压缩事务**之后**的补写(嵌入是一次端点调用,
    /// 进不了那个事务;失败由调用方 warn 降级,行保留、关键词仍可检索,裁定 4)。
    /// 编码唯一入口是 [`to_vec`] —— 手拼字节的第二份编码真相早晚和它分叉。
    pub fn set_memory_embedding(&self, id: i64, vec: &[f32]) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        let n = c.execute(
            "UPDATE memories SET embedding=?2 WHERE id=?1",
            params![id, to_vec(vec)],
        )?;
        affected_one(n, "写向量", id)
    }

    /// 混合召回 + 类人排序(spec 2026-10-25 §11)。
    ///
    /// **相关性**:融合分 = `0.6 * 余弦 + 0.4 * gram分`(T6)。
    /// - `query_vec=None`(off/嵌入失败/空白 query):纯 gram;
    /// - 行无向量(NULL BLOB)**或维度与 query_vec 不一致**:向量分记 0,只剩 `0.4*gram`;
    /// - 余弦夹到 0 以上:召回里的语义分是「相关度」不是「方向」。
    ///
    /// **类人修正**(相关性之后再乘两项,顺序无关、都只缩放):
    /// - 强度 `1 + w*ln(1+reinforcements)`:同一件事被确认得越多越靠前;
    /// - 衰减 `floor + (1-floor)*2^{-age/半衰期}`:久未触碰的下沉,但有下限、永不归零。
    ///
    /// **访问即再巩固**:返回的每一行同时被写一次 `last_accessed = now`
    /// —— 人被问起某件事时记忆会被重新巩固,这条通路就是它的工程对应。
    /// 所以本方法**不是只读的**;设置页的列表走 `all_memories`,不经过这里。
    ///
    /// 空白 query 或 k=0 返回空表(不碰库):没有「全量注入」这个退化路径。
    pub fn search_memories(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        k: usize,
        cfg: &RecallCfg,
    ) -> Result<Vec<MemoryRow>, rusqlite::Error> {
        let c = self.lock();
        let grams = bigrams(query);
        if grams.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // embedding 列只在带向量的检索里读:纯 gram 路径连解码都不付(off 模式
        // 一次 IO 都不多花)。BLOB 解码失败(None)与 NULL 同路:当「无向量」降级。
        let rows: Vec<(MemoryRow, Option<Vec<f32>>)> = if query_vec.is_some() {
            let mut st = c.prepare(&format!(
                "SELECT {MEMORY_COLS},embedding FROM memories WHERE invalid_at IS NULL"
            ))?;
            let rows = st.query_map([], |r| {
                let m = memory_from_row(r)?;
                let blob: Option<Vec<u8>> = r.get(MEMORY_COL_COUNT)?;
                Ok((m, blob.and_then(|b| from_vec(&b))))
            })?;
            rows.collect::<Result<_, _>>()?
        } else {
            query_memories(&c, "WHERE invalid_at IS NULL")?
                .into_iter()
                .map(|m| (m, None))
                .collect()
        };
        let mut scored: Vec<(f64, MemoryRow)> = rows
            .into_iter()
            .map(|(r, row_vec)| {
                let body_grams = bigrams(&r.body);
                let hits = grams.iter().filter(|g| body_grams.contains(g)).count();
                let gram = hits as f64 / grams.len() as f64;
                let relevance = match query_vec {
                    None => gram,
                    Some(qv) => {
                        let cos = row_vec
                            .as_deref()
                            .and_then(|rv| cosine(qv, rv))
                            .unwrap_or(0.0)
                            // 负余弦夹到 0:召回里的语义分是「相关度」(0..1),不是
                            // 「方向」(-1..1)。不夹的话 0.6*(-1)+0.4*gram 会把一条
                            // 关键词命中的记忆推到 0 以下,再被 retain 整条丢掉 ——
                            // 用户看到的是「含关键字的记忆搜不到」,理由不可见。
                            // (a_negative_cosine_never_erases_a_keyword_hit 钉着。)
                            .max(0.0);
                        0.6 * cos + 0.4 * gram
                    }
                };
                (relevance * cfg.strength(&r) * cfg.decay(&r), r)
            })
            .collect();
        // 相关性为 0 的行**不进结果**(没有命中就不注入);强度与衰减只缩放正分,
        // 不会把 0 抬成非 0 —— 两件事因此可以共存。
        scored.retain(|(s, _)| *s > 0.0);
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.updated_at.cmp(&a.1.updated_at))
                .then(b.1.id.cmp(&a.1.id))
        });
        scored.truncate(k);
        let out: Vec<MemoryRow> = scored.into_iter().map(|(_, r)| r).collect();
        // 访问即再巩固:只标这次真被交出去的那几行。
        touch_rows(&c, &out, cfg.now_ms)?;
        Ok(out)
    }

    /// 强化:同一事实再次被确认。只加计数、刷新 `updated_at`,**不动正文**
    /// (正文的改写是 UPDATE 的事;混在一起会让「用户又说了一遍」变成一次内容风险)。
    pub fn reinforce_memory(&self, id: i64) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        reinforce_row(&c, id)
    }

    /// 到期(含已过期)且**尚未提醒**的待办。窗口由调用方给:`now + window`。
    /// `notified_at IS NULL` 是防重复提醒的唯一闸门 —— 提醒过的不再返回,
    /// 直到用户删掉它或被新的确认覆盖。
    pub fn due_prospective(&self, until_ms: i64) -> Result<Vec<MemoryRow>, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare(&format!(
            "SELECT {MEMORY_COLS} FROM memories \
             WHERE invalid_at IS NULL AND due_at IS NOT NULL AND due_at <= ?1 \
               AND notified_at IS NULL ORDER BY due_at, id"
        ))?;
        let rows = st
            .query_map(params![until_ms], memory_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// 标记已提醒。与 `due_prospective` 成对:一次事务里读出来、注入、再标记,
    /// 下一轮就不会重复。标记失败不回滚注入 —— 用户多看到一次同样的提醒,
    /// 好过提醒丢了。
    pub fn mark_notified(&self, ids: &[i64], at_ms: i64) -> Result<(), rusqlite::Error> {
        if ids.is_empty() {
            return Ok(());
        }
        let c = self.lock();
        for id in ids {
            c.execute(
                "UPDATE memories SET notified_at=?2 WHERE id=?1 AND notified_at IS NULL",
                params![id, at_ms],
            )?;
        }
        Ok(())
    }

    /// 记忆库概览(元记忆的原料):条数与按 kind 的分布。
    /// 有了它,模型才知道「我有记忆可查」——没有它,`memory_search` 工具形同虚设,
    /// 模型不会凭空想到去调一个它不知道有内容的库。
    pub fn memory_summary(&self) -> Result<Vec<(String, i64)>, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare(
            "SELECT kind, COUNT(*) FROM memories WHERE invalid_at IS NULL \
             GROUP BY kind ORDER BY kind",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    // ---- 抽取游标(P0,spec §12):「已经抽取到哪一条消息」 ----

    /// 该会话已抽取到的 `messages.id`(0 = 从未抽取)。
    /// 游标的唯一用途是让「压缩时抽取」与「空闲/切换时抽取」**不重复处理同一段对话**:
    /// 重复抽取既烧调用,又会在库里堆出一串措辞不同的近似重复记忆。
    pub fn memory_cursor(&self, session_id: &str) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        c.query_row(
            "SELECT memory_extracted_upto FROM sessions WHERE id=?1",
            params![session_id],
            |r| r.get(0),
        )
    }

    /// 游标之后、**可抽取**的对话行:`(id, Message)`,按 id 升序。
    /// 排除两类行,判据与压缩的「一轮」定义同源:
    /// - `Role::System`:摘要备注不是对话内容(抽它等于让模型总结自己的总结);
    /// - 工具结果行:工具输出是数据不是陈述,把网页正文/文件内容喂给抽取会烧掉大量
    ///   token 并抽出与用户无关的东西。
    /// 只取文本:带图的行在抽取调用里没有意义(vision 模型也不该为记忆付费)。
    pub fn messages_for_extraction(
        &self,
        session_id: &str,
        after_id: i64,
    ) -> Result<Vec<(i64, Message)>, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare(
            "SELECT id, role, blocks_json FROM messages \
             WHERE session_id=?1 AND id>?2 AND role<>?3 ORDER BY id",
        )?;
        let rows = st
            .query_map(
                params![session_id, after_id, crate::store::db::role_code(&Role::System)],
                |r| {
                    let id: i64 = r.get(0)?;
                    let role = crate::store::db::role_from_code(&r.get::<_, String>(1)?, 1)?;
                    let blocks = crate::store::db::decode_blocks(&r.get::<_, String>(2)?, 2)?;
                    Ok((id, Message { role, blocks }))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        // 工具结果行在解码之后才认得出(角色同为 User,只能看块种类)。
        Ok(rows
            .into_iter()
            .filter(|(_, m)| {
                !m.blocks
                    .iter()
                    .any(|b| matches!(b, crate::agent::types::ContentBlock::ToolResult { .. }))
            })
            .filter(|(_, m)| {
                m.blocks
                    .iter()
                    .any(|b| matches!(b, crate::agent::types::ContentBlock::Text { .. }))
            })
            .collect())
    }

    /// 会话的最后活动时刻(`sessions.updated_at`,每次追加消息都会刷新)。
    pub fn session_last_active(&self, session_id: &str) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        c.query_row(
            "SELECT updated_at FROM sessions WHERE id=?1",
            params![session_id],
            |r| r.get(0),
        )
    }

    /// 该会话最大消息 id(空会话 0)。与 [`Self::memory_cursor`] 比较决定「有没有新东西」。
    pub fn max_message_id(&self, session_id: &str) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        c.query_row(
            "SELECT COALESCE(MAX(id),0) FROM messages WHERE session_id=?1",
            params![session_id],
            |r| r.get(0),
        )
    }

    /// 空闲且有待抽取内容的会话:**只返回 id**,抽取本身归调用方(它要模型)。
    /// 三条判据:最后活动早于 `idle_before`、游标落后于最大消息 id、会话存在。
    /// 排序按最后活动升序 —— 最久没动的先抽,一轮 tick 里只处理前几个也不会饿死谁。
    pub fn sessions_needing_extraction(
        &self,
        idle_before: i64,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare(
            "SELECT s.id FROM sessions s \
             WHERE s.updated_at <= ?1 \
               AND s.memory_extracted_upto < \
                   (SELECT COALESCE(MAX(m.id),0) FROM messages m WHERE m.session_id=s.id) \
             ORDER BY s.updated_at ASC",
        )?;
        let rows = st
            .query_map(params![idle_before], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// 空闲/切换抽取的落库入口:**记忆写入 + 游标推进在同一个事务里**。
    /// 两件事必须原子:只写记忆不推游标 ⇒ 下次再抽一遍(重复记忆);只推游标不写记忆
    /// ⇒ 这段对话永久丢失(比不抽更糟)。返回本次新增记忆行的 id(与 Add 顺序对位,
    /// 供 ipc 层补写向量)。
    pub fn apply_memories_and_advance_cursor(
        &self,
        session_id: &str,
        writes: &[MemoryWrite],
        upto_message_id: i64,
    ) -> Result<Vec<i64>, rusqlite::Error> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        let added = apply_memory_writes(&tx, writes)?;
        // 游标只许**前进**:并发路径(空闲 worker 与一个正在压缩的 turn)谁先提交都不该
        // 把游标推回去,否则那一段会被重抽。
        tx.execute(
            "UPDATE sessions SET memory_extracted_upto=?2 \
             WHERE id=?1 AND memory_extracted_upto < ?2",
            params![session_id, upto_message_id],
        )?;
        tx.commit()?;
        Ok(added)
    }

    /// 画像恒单行:返回正文,从未写过时是空串(空串 = 不注入,消费方据此跳过)。
    pub fn profile(&self) -> Result<String, rusqlite::Error> {
        let c = self.lock();
        c.query_row("SELECT body FROM memory_profile WHERE id=1", [], |r| r.get(0))
            .or_else(|e| match e {
                // 「从没写过」不是错误:画像的空串语义由消费方定义(不注入)。
                rusqlite::Error::QueryReturnedNoRows => Ok(String::new()),
                other => Err(other),
            })
    }

    pub fn set_profile(&self, body: &str) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        set_profile_row(&c, body)
    }

    /// 用户亲手改正文(P0,spec §12)。与模型侧 `update_memory_body` 的区别只有三点,
    /// 每一点都是「用户权威」的直接推论:
    /// 1. 置 `user_edited = 1` —— 从此模型不能改写这行(合并管线会降级成 ADD 并存);
    /// 2. `confidence` 抬到 1.0 —— 用户亲手确认过的事,不该再被算法的置信度打折;
    /// 3. 清除 `due_at`/`notified_at` 之外的时间戳不动:`valid_from` 保留原始记忆的
    ///    年龄(用户改的是内容,不是「这件事什么时候发生的」)。
    /// 待办被编辑时 `due_at` **保留** —— 用户改进措辞不该把提醒弄丢。
    pub fn update_memory_body_by_user(&self, id: i64, body: &str) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        reject_empty_body(body)?;
        let n = c.execute(
            "UPDATE memories SET body=?2, user_edited=1, confidence=1.0, updated_at=?3 \
             WHERE id=?1 AND invalid_at IS NULL",
            params![id, body, Db::now_ms()],
        )?;
        affected_one(n, "用户编辑", id)
    }

    /// 用户手动新增(P1,spec §12):与抽取出来的记忆走同一个写入口,只是来源标记不同
    /// (`session_id = None`)且直接 `user_edited = 1` —— 用户写的东西从出生起就是权威的,
    /// 否则模型下一次合并就能把它改成自己的说法。
    ///
    /// `due_at` 由调用方给(前端把 `datetime-local` 的字面值按 UTC 换成毫秒,与抽取侧
    /// `parse_iso8601_ms` 同一口径)。这里**不重复判 kind**:「非待办不存 due_at」的
    /// 唯一判据在 [`insert_memory_row`](两处判据会漂移),本方法只负责原样转交。
    /// `None` = 待办但没有到期时刻(合法状态:用户先记下来,后续再补)。
    pub fn add_user_memory(
        &self,
        kind: &str,
        body: &str,
        due_at: Option<i64>,
    ) -> Result<i64, rusqlite::Error> {
        reject_empty_body(body)?;
        let m = NewMemory {
            session_id: None,
            kind: kind.to_string(),
            subject: None,
            predicate: None,
            object: None,
            body: body.to_string(),
            confidence: 1.0,
            due_at,
        };
        let c = self.lock();
        let id = insert_memory_row(&c, &m)?;
        c.execute(
            "UPDATE memories SET user_edited=1 WHERE id=?1",
            params![id],
        )?;
        Ok(id)
    }

    /// 压缩与记忆写入的**同一个事务**(spec §8 不变量 2):删行、摘要备注、记忆变更
    /// 要么都成、要么都不做。抽取/合并的模型调用发生在这之前(ipc 层),
    /// 这里只收现成的决定。`writes` 为空时行为与 `compact_session` 逐字节一致
    /// —— db.rs 的那个方法就是经这里实现的,「删行+落备注」的 SQL 只有一份。
    ///
    /// 返回值 = **本次事务新增的记忆行 id 列表**(只含 `Add`,顺序即写入顺序;
    /// Update/Invalidate 复用旧行、SetProfile 是单行画像,都不产新行 —— 空写入
    /// 返回空表)。嵌入补写按它与 `writes` 里 `Add` body 的顺序对位,这是 ipc 层
    /// 拿它唯一的用途;嵌入本身进不了事务(它是一次端点调用),所以是提交后的补写。
    ///
    /// **同时推进抽取游标**(P0):压缩抽的就是 `plan.source`,那些行会被删掉;
    /// 不推进游标,空闲抽取会试图重抽已删除区间的后继(判据是 `id > cursor`)——
    /// 更糟的是同一段内容被抽两遍。`cursor_upto` 由调用方给出(它知道 source 的
    /// 最大 id),`None` 表示调用方不关心(既有 db.rs 委托路径)。
    pub fn compact_session_with_memories(
        &self,
        session_id: &str,
        plan: &CompactionPlan,
        note_text: &str,
        writes: &[MemoryWrite],
        cursor_upto: Option<i64>,
    ) -> Result<Vec<i64>, rusqlite::Error> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        let ids = conversation_ids(&tx, session_id)?;
        // 越界沿用 compact_session 的裁定:**报错而不是 slice panic** —— panic 会把
        // Mutex 打成中毒态,症状是「此后每条命令都打不开」。
        if plan.drop_from > plan.drop_to || plan.drop_to > ids.len() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_RANGE),
                Some(format!(
                    "压缩区间 [{}, {}) 越界：会话 {session_id} 现在只有 {} 条对话消息（超出计划要删的行数）",
                    plan.drop_from,
                    plan.drop_to,
                    ids.len()
                )),
            ));
        }
        for id in &ids[plan.drop_from..plan.drop_to] {
            tx.execute("DELETE FROM messages WHERE id=?1", params![id])?;
        }
        // 同一条 append 路径:备注是一行普通消息(与 db.rs 的 compact_session 共用
        // insert_message_row,「一行消息长什么样」只有一处 SQL)。
        insert_message_row(&tx, session_id, &Message::system_note(note_text))?;
        let added = apply_memory_writes(&tx, writes)?;
        // 压缩抽的就是被删那段 `[drop_from..drop_to)`:游标推到那段的最大 id,
        // 与 `messages_for_extraction` 的 `id > cursor` 判据配合,正好跳过已处理内容。
        // 调用方给的 `cursor_upto` 若更高(空闲 worker 已推过新消息)则取大者。
        let deleted_max = ids[plan.drop_from..plan.drop_to].iter().copied().max();
        let upto = match (cursor_upto, deleted_max) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if let Some(upto) = upto {
            // 只许前进(同 apply_memories_and_advance_cursor):空闲 worker 可能刚推过。
            tx.execute(
                "UPDATE sessions SET memory_extracted_upto=?2                  WHERE id=?1 AND memory_extracted_upto < ?2",
                params![session_id, upto],
            )?;
        }
        tx.commit()?;
        Ok(added)
    }
}

// ---- 自由函数:全部收 `&Connection`,事务内外共用(同 insert_message_row 的形状) ----

fn query_memories(c: &Connection, tail: &str) -> Result<Vec<MemoryRow>, rusqlite::Error> {
    let mut st = c.prepare(&format!("SELECT {MEMORY_COLS} FROM memories {tail}"))?;
    let rows = st
        .query_map([], memory_from_row)?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

fn insert_memory_row(c: &Connection, m: &NewMemory) -> Result<i64, rusqlite::Error> {
    reject_empty_body(&m.body)?;
    let t = Db::now_ms();
    c.execute(
        "INSERT INTO memories(session_id,kind,subject,predicate,object,body,confidence,\
         valid_from,invalid_at,embedding,reinforcements,last_accessed,due_at,notified_at,\
         user_edited,created_at,updated_at) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,NULL,NULL,0,NULL,?9,NULL,0,?8,?8)",
        params![
            m.session_id,
            m.kind,
            m.subject,
            m.predicate,
            m.object,
            m.body,
            m.confidence,
            t,
            // 只有待办带到期时刻。其他 kind 就算带着值也**不存** —— 一条「事实」上
            // 挂个 due_at 只会让 `due_prospective` 把非待办的东西当提醒弹出来。
            (m.kind == "prospective").then_some(m.due_at).flatten()
        ],
    )?;
    Ok(c.last_insert_rowid())
}

/// 强化:计数 +1、刷新 updated_at。**不动 body、不动 confidence**
/// —— confidence 是「这条抽得准不准」,被再说一遍并不改变那件事;
/// 强度归 `reinforcements`,两个量各管一件事。
fn reinforce_row(c: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    let n = c.execute(
        "UPDATE memories SET reinforcements=reinforcements+1, updated_at=?2 \
         WHERE id=?1 AND invalid_at IS NULL",
        params![id, Db::now_ms()],
    )?;
    affected_one(n, "强化", id)
}

/// 访问即再巩固:把这次交出去的行标上 `last_accessed`。
/// 只写**有变化的那部分**(`last_accessed IS NULL OR < ?`):同一毫秒内重复调用
/// 是空转,不必每次召回都触发一次写事务的 redo。
fn touch_rows(c: &Connection, rows: &[MemoryRow], at_ms: i64) -> Result<(), rusqlite::Error> {
    for m in rows {
        c.execute(
            "UPDATE memories SET last_accessed=?2 \
             WHERE id=?1 AND (last_accessed IS NULL OR last_accessed < ?2)",
            params![m.id, at_ms],
        )?;
    }
    Ok(())
}

fn update_memory_body(c: &Connection, id: i64, body: &str) -> Result<(), rusqlite::Error> {
    reject_empty_body(body)?;
    let n = c.execute(
        "UPDATE memories SET body=?2, updated_at=?3 WHERE id=?1",
        params![id, body, Db::now_ms()],
    )?;
    affected_one(n, "更新", id)
}

fn invalidate_memory_row(c: &Connection, id: i64) -> Result<(), rusqlite::Error> {
    // `AND invalid_at IS NULL`:重复失效落在 affected=0 上,由 affected_one 报错 ——
    // 幂等错误好过静默成功(成功会谎称「这次失效是它做的」)。
    let n = c.execute(
        "UPDATE memories SET invalid_at=?2, updated_at=?2 WHERE id=?1 AND invalid_at IS NULL",
        params![id, Db::now_ms()],
    )?;
    affected_one(n, "失效", id)
}

fn set_profile_row(c: &Connection, body: &str) -> Result<(), rusqlite::Error> {
    reject_empty_body(body)?;
    // UPSERT:单行不变量由 schema 的 `CHECK (id = 1)` 与这条语句共同钉住。
    c.execute(
        "INSERT INTO memory_profile(id,body,updated_at) VALUES(1,?1,?2) \
         ON CONFLICT(id) DO UPDATE SET body=excluded.body, updated_at=excluded.updated_at",
        params![body, Db::now_ms()],
    )?;
    Ok(())
}

fn apply_memory_writes(c: &Connection, writes: &[MemoryWrite]) -> Result<Vec<i64>, rusqlite::Error> {
    let mut added: Vec<i64> = Vec::new();
    for w in writes {
        match w {
            MemoryWrite::Add(m) => {
                // 只登记 Add 的 id:返回值的语义就是「本事务新增的记忆行」,
                // 与调用方(includes ipc 写路径)的 Add body 按序对位。
                added.push(insert_memory_row(c, m)?);
            }
            MemoryWrite::UpdateBody { id, body } => {
                update_memory_body(c, *id, body)?;
            }
            MemoryWrite::Invalidate { id } => {
                invalidate_memory_row(c, *id)?;
            }
            MemoryWrite::Reinforce { id } => {
                reinforce_row(c, *id)?;
            }
            MemoryWrite::SetProfile(body) => {
                set_profile_row(c, body)?;
            }
        }
    }
    Ok(added)
}

// ---- 嵌入 BLOB 编码(T6 裁定 1):唯一编码点 ----

/// f32 数组 → BLOB:小端 LE 裸存,无头部、无版本号、无长度前缀。
/// 为什么不留版本字节:这对函数是编码的**唯一**真相(写入与读取都在本文件),
/// 版本号防的「将来出现第二种编码」届时该走的是 schema 迁移(user_version),
/// 不是 magic byte —— 提前留位等于给永远不会来的客人摆碗筷。
/// 小端是 x86/ARM 的原生序,Rust 侧 `from_le_bytes` 与 SQLite 的 BLOB 直通,零转换成本。
pub fn to_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// BLOB → f32 数组,[`to_vec`] 的逆。长度不是 4 的倍数 = 损坏字节(手改库、半次写),
/// 返回 None 而不是截断或补零:检索侧把 None 当「无向量」降级(裁定 4),
/// 决不把损坏的字节当语义 —— 那是「看起来完整的行」这一类错误(R71 同族)。
pub fn from_vec(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() % 4 != 0 {
        return None;
    }
    Some(
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// 余弦相似度,f64 中间量(f32 累加在大维度下漂移)。四种输入给 None(调用侧记 0):
/// ① 维度不一致(不同模型/维度的向量不可比);② 任一分量非有限(端点响应理论上
/// 不会带 inf/NaN,但 JSON 的 `1e40` 解出来就是 inf,inf*0 = NaN 会污染整条排序);
/// ③ 任一为零向量(分母为 0)—— 零向量与任何方向都不相似,0 是正确答案不是错误。
/// 返回值域 [-1, 1]:负余弦是「语义相反」,调用侧的 retain 会把它当无关丢弃,
/// 这是想要的 —— 负相关不召回。
fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b.iter()) {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// spec §8 不变量 7:零信息永不落库(「空摘要即 Err」同一条纪律的存储版)。
/// 用 SQLITE_CONSTRAINT 而不是自定义错误类型:调用方(db.rs 一系)的错误通道
/// 就是 `rusqlite::Error`,保持一条通道,消息里写清理由。
fn reject_empty_body(body: &str) -> Result<(), rusqlite::Error> {
    if body.trim().is_empty() {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("记忆正文不能为空或纯空白:零信息永不落库".into()),
        ))
    } else {
        Ok(())
    }
}

/// 按id 的单行写必须真的动了一行;affected=0 意味着 id 不存在或状态已变,
/// 静默成功是第二份真相的开始。
fn affected_one(n: usize, what: &str, id: i64) -> Result<(), rusqlite::Error> {
    if n == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some(format!("{what}失败:记忆 id={id} 不存在或状态已变")),
        ))
    }
}

/// 字符 2-gram(小写化)。窗口取 `chars()` 而不是字节:中文按字切,
/// 字节窗口会把 UTF-8 序列拦腰切断,gram 既不属于 query 也不属于 body。
/// 单字符输入退成 1-gram;与 body 侧同一函数,**没有第二套切法**。
fn bigrams(s: &str) -> Vec<String> {
    let lower: Vec<char> = s.chars().flat_map(|c| c.to_lowercase()).collect();
    if lower.is_empty() {
        return Vec::new();
    }
    if lower.len() == 1 {
        return vec![lower.iter().collect()];
    }
    lower.windows(2).map(|w| w.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ContentBlock, Role};

    /// 与 db.rs 的 tmpdb 同构:临时目录里的全新库。
    fn tmpdb() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mem.db")).unwrap();
        (dir, db)
    }

    /// 三元组事实:`object` 与 `body` 分开传,round-trip 测试靠它们不同证明
    /// 各列独立存储(三元组列不是 body 的复述)。
    fn fact(object: &str, body: &str) -> NewMemory {
        NewMemory {
            session_id: Some("s1".into()),
            kind: "fact".into(),
            subject: Some("用户".into()),
            predicate: Some("住在".into()),
            object: Some(object.into()),
            body: body.into(),
            confidence: 0.9,
            due_at: None,
        }
    }

    /// 无三元组的偏好型记忆。
    fn preference(body: &str) -> NewMemory {
        NewMemory {
            session_id: None,
            kind: "preference".into(),
            subject: None,
            predicate: None,
            object: None,
            body: body.into(),
            confidence: 0.9,
            due_at: None,
        }
    }

    fn u(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    // ==== T8：类人记忆（强化 / 衰减 / 访问强化 / 待办 / 迁移） ====

    /// 以「现在」为锚的 cfg；`days_ago` 用来构造过去的行。
    const DAY: i64 = 86_400_000;

    fn cfg_now() -> RecallCfg {
        RecallCfg::default()
    }

    fn prospective(body: &str, due_in_days: i64) -> NewMemory {
        NewMemory {
            session_id: None,
            kind: "prospective".into(),
            subject: None,
            predicate: None,
            object: None,
            body: body.into(),
            confidence: 0.95,
            due_at: Some(Db::now_ms() + due_in_days * DAY),
        }
    }

    /// 旧库（v2，没有四列）被新二进制打开：列补上、老行拿默认值、数据不丢。
    /// 这条是整个升级的迁移门槛 —— 用户的库可能已经有几百条记忆。
    #[test]
    fn a_v2_database_gains_the_v3_columns_without_losing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let db = Db::open(&path).unwrap();
            db.add_memory(&fact("北京", "用户住在北京")).unwrap();
            // 把库降级成 v2 形状：删掉四列（SQLite 3.35+ 支持 DROP COLUMN）。
            let c = db.lock();
            for col in ["reinforcements", "last_accessed", "due_at", "notified_at"] {
                c.execute(&format!("ALTER TABLE memories DROP COLUMN {col}"), [])
                    .unwrap();
            }
            c.pragma_update(None, "user_version", 2).unwrap();
        }
        // 重开：迁移函数补列
        let db = Db::open(&path).unwrap();
        let rows = db.valid_memories().unwrap();
        assert_eq!(rows.len(), 1, "迁移不许丢行");
        assert_eq!(rows[0].body, "用户住在北京");
        assert_eq!(rows[0].reinforcements, 0, "老行拿默认强度");
        assert!(rows[0].last_accessed.is_none());
        assert!(rows[0].due_at.is_none());
        assert!(rows[0].notified_at.is_none());
    }

    #[test]
    fn reinforcing_a_memory_raises_its_strength_without_touching_the_body() {
        let (_dir, db) = tmpdb();
        let id = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        let before = db.valid_memories().unwrap()[0].clone();

        db.reinforce_memory(id).unwrap();
        db.reinforce_memory(id).unwrap();

        let after = db.valid_memories().unwrap()[0].clone();
        assert_eq!(after.reinforcements, 2);
        assert_eq!(after.body, before.body, "强化不许改正文(改写是 UPDATE 的事)");
        assert_eq!(after.confidence, before.confidence, "置信度与强度是两个量");
        assert!(after.updated_at >= before.updated_at);
        assert!(db.reinforce_memory(id + 999).is_err(), "强化不存在的行要报错");
    }

    /// 强化与衰减的**因子本身**(直接测,不经排序)。经排序测有两个陷阱,
    /// 变异实验实测过:`reinforce` 会刷新 `updated_at`、而旧行的 `updated_at` 本来就旧,
    /// 两种副作用都能让「去掉因子」的变异体照样通过 —— 那些排序级测试必须把变量隔离,
    /// 而把公式钉死最直接的地方就是这两个函数。
    #[test]
    fn the_strength_factor_grows_with_confirmations_and_the_decay_factor_halves_per_half_life() {
        let cfg = RecallCfg::default();
        let row = |days: i64, kind: &str, reinforcements: i64| MemoryRow {
            id: 1,
            session_id: None,
            kind: kind.into(),
            subject: None,
            predicate: None,
            object: None,
            body: "x".into(),
            confidence: 1.0,
            valid_from: cfg.now_ms - days * DAY,
            invalid_at: None,
            updated_at: cfg.now_ms - days * DAY,
            reinforcements,
            last_accessed: None,
            due_at: None,
            notified_at: None,
            user_edited: false,
        };

        // 强度:0 次确认 = 1.0(不奖不罚);n 次确认 = 1 + w*ln(1+n),单调递增。
        assert_eq!(cfg.strength(&row(0, "fact", 0)), 1.0);
        let s1 = cfg.strength(&row(0, "fact", 1));
        let s10 = cfg.strength(&row(0, "fact", 10));
        assert!(s1 > 1.0 && s10 > s1, "确认越多越强:{s1} / {s10}");
        assert!(
            (s10 - 1.0) < 3.0 * (s1 - 1.0) || s1 < 1.2,
            "对数形状:增益必须递减,不是线性膨胀"
        );

        // 衰减:一天不衰减;一个半衰期正好落到 0.5 与下限的中点;两年多后逼近下限。
        let d0 = cfg.decay(&row(0, "fact", 0));
        let d_half = cfg.decay(&row(60, "fact", 0));
        let d_old = cfg.decay(&row(900, "fact", 0));
        assert!((d0 - 1.0).abs() < 1e-9, "刚记下的不衰减");
        let mid = (1.0 + cfg.decay_floor) / 2.0;
        assert!((d_half - mid).abs() < 1e-9, "一个半衰期落到中点:{d_half}");
        assert!(d_old >= cfg.decay_floor - 1e-9, "永不低于下限:{d_old}");
        assert!(d_old > 0.0, "下限必须是正数(0 等于删除)");

        // 偏好忘得更慢:同一个年龄下,偏好的衰减因子更大。
        assert!(cfg.decay(&row(60, "preference", 0)) > d_half);
        // 待办不衰减。
        assert!((cfg.decay(&row(900, "prospective", 0)) - 1.0).abs() < 1e-9);
    }

    /// 强化真的会改变排序 —— 但必须把 `updated_at` 这个副作用**隔离**掉:
    /// 强化会刷新它,而平局排序本来就按它倒序,那会让「去掉强度因子」的变异体照样通过。
    /// 所以这里强化后把两行的时间戳拉平,只留 `reinforcements` 一处差异。
    #[test]
    fn a_reinforced_memory_outranks_an_equally_relevant_one() {
        let (_dir, db) = tmpdb();
        let older = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.add_memory(&fact("上海", "用户住在上海")).unwrap();
        for _ in 0..3 {
            db.reinforce_memory(older).unwrap();
        }
        {
            let c = db.lock();
            let now = Db::now_ms();
            c.execute("UPDATE memories SET valid_from=?1, updated_at=?1", params![now])
                .unwrap();
        }
        let hits = db.search_memories("住在", None, 10, &cfg_now()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].id, older,
            "时间戳拉平后唯一剩下的差异是强度,被确认三次的必须在前"
        );
    }

    /// 衰减真的会把旧的压下去 —— 而且要在**相关性更高**的旧行身上验证,
    /// 否则「旧的下沉」只是平局时按 updated_at 排序的副作用,与被测公式无关。
    /// 夹具:旧行含查询的全部 gram(相关性 1.0),新行只含一部分(0.625)。
    /// 不衰减 → 旧行必然胜出;衰减生效 → 新行胜出。
    #[test]
    fn decay_can_flip_the_order_against_a_higher_relevance_old_row() {
        let (_dir, db) = tmpdb();
        let fresh = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        let old = db
            .add_memory(&fact("北京", "用户住在北京朝阳区"))
            .unwrap();
        {
            let c = db.lock();
            let ancient = Db::now_ms() - 900 * DAY;
            c.execute(
                "UPDATE memories SET valid_from=?2, updated_at=?2 WHERE id=?1",
                params![old, ancient],
            )
            .unwrap();
        }
        let hits = db
            .search_memories("用户住在北京朝阳区", None, 10, &cfg_now())
            .unwrap();
        assert_eq!(hits.len(), 2, "旧记忆必须仍在结果里(渐近遗忘,不是删除)");
        assert!(
            hits[0].id == fresh,
            "旧行的相关性更高(含全部 gram),衰减必须把它压到新的后面;实际前两名 {:?}",
            hits.iter().map(|m| (m.id, m.body.clone())).collect::<Vec<_>>()
        );
        assert_eq!(hits[1].id, old);
    }

    /// 再久也不归零:`decay_floor` 是正数,所以 retain(>0) 永远扫不掉一条相关的记忆。
    #[test]
    fn an_ancient_memory_is_never_dropped_for_being_old() {
        let (_dir, db) = tmpdb();
        let id = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        {
            let c = db.lock();
            let ancient = Db::now_ms() - 100_000 * DAY;
            c.execute(
                "UPDATE memories SET valid_from=?2, updated_at=?2 WHERE id=?1",
                params![id, ancient],
            )
            .unwrap();
        }
        let hits = db.search_memories("用户住在北京", None, 10, &cfg_now()).unwrap();
        assert_eq!(hits.len(), 1, "相关性 > 0 的行不许因为年龄被丢弃");
    }

    /// 待办不衰减：三个月前记的待办与今天记的同等新鲜（价值与 due_at 绑定，与年龄无关）。
    #[test]
    fn a_prospective_memory_does_not_decay() {
        let (_dir, db) = tmpdb();
        let todo = db.add_memory(&prospective("下周把材料交给老王", -80)).unwrap();
        let normal = db.add_memory(&fact("北京", "下周把材料交给老王")).unwrap();
        {
            let c = db.lock();
            let old = Db::now_ms() - 900 * DAY;
            for id in [todo, normal] {
                c.execute(
                    "UPDATE memories SET valid_from=?2, updated_at=?2 WHERE id=?1",
                    params![id, old],
                )
                .unwrap();
            }
        }
        // 相关性相同（同一个 body），待办的衰减因子是 1.0，事实的掉到下限附近。
        let hits = db.search_memories("下周把材料交给老王", None, 10, &cfg_now()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, todo, "待办不因年龄下沉");
        assert_eq!(hits[1].id, normal);
    }

    /// 访问即再巩固：召回会写 last_accessed，而这会让它在下一轮排得更高。
    #[test]
    fn recalling_a_memory_stamps_last_accessed_and_that_refreshes_it() {
        let (_dir, db) = tmpdb();
        let a = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        let b = db.add_memory(&fact("北京", "用户住在北京二号")).unwrap();
        {
            let c = db.lock();
            let old = Db::now_ms() - 400 * DAY;
            for id in [a, b] {
                c.execute(
                    "UPDATE memories SET valid_from=?2, updated_at=?2 WHERE id=?1",
                    params![id, old],
                )
                .unwrap();
            }
        }
        // 先召回一次：两条都会被 touch。
        db.search_memories("用户住在北京", None, 10, &cfg_now()).unwrap();
        let touched = db.all_memories().unwrap();
        assert!(
            touched.iter().all(|m| m.last_accessed.is_some()),
            "召回过的行必须留下访问时间"
        );
        // 只召回其中一条（查询词只命中它），另一条的访问时间保持更早的相对位置。
        let before_two = db.all_memories().unwrap();
        let only_b = before_two.iter().find(|m| m.id == b).unwrap().last_accessed;
        db.search_memories("二号", None, 10, &cfg_now()).unwrap();
        let after = db.all_memories().unwrap();
        let after_b = after.iter().find(|m| m.id == b).unwrap().last_accessed;
        assert!(after_b >= only_b, "被再次召回的时间只会更新");
    }

    /// 待办查询：只返回「已到期/将到期 + 未提醒 + 有效」的。
    #[test]
    fn due_prospectives_respect_the_window_and_the_notified_flag() {
        let (_dir, db) = tmpdb();
        let now = Db::now_ms();
        let overdue = db.add_memory(&prospective("交材料", -1)).unwrap();
        let soon = db.add_memory(&prospective("打电话给医院", 0)).unwrap();
        let far = db.add_memory(&prospective("明年续签", 200)).unwrap();
        let plain = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        let _ = plain; // 非待办永远不进这条查询

        // 窗口 1 天：overdue 与 soon 在内，far 在外。
        let due = db.due_prospective(now + DAY).unwrap();
        let ids: Vec<i64> = due.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![overdue, soon], "按 due_at 升序，超期的排最前");

        // 标记已提醒后不再返回 —— 这是防重复刷屏的唯一闸门。
        db.mark_notified(&[overdue], now).unwrap();
        let due = db.due_prospective(now + DAY).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, soon);

        // 失效的待办不提醒。
        db.invalidate_memory(soon).unwrap();
        assert!(db.due_prospective(now + DAY).unwrap().is_empty());
        assert!(
            far > 0,
            "far 只是不该进窗口，不是被删了（夹具自我说明）"
        );
    }

    /// 概览：按 kind 计有效记忆的条数；失效与画像不计入。
    #[test]
    fn the_summary_counts_valid_rows_by_kind() {
        let (_dir, db) = tmpdb();
        db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.add_memory(&fact("上海", "用户住在上海")).unwrap();
        let gone = db.add_memory(&fact("广州", "用户住在广州")).unwrap();
        db.add_memory(&preference("用户喜欢简洁回答")).unwrap();
        db.invalidate_memory(gone).unwrap();
        db.set_profile("画像不进计数").unwrap();

        let summary = db.memory_summary().unwrap();
        assert_eq!(
            summary,
            vec![("fact".to_string(), 2), ("preference".to_string(), 1)],
            "失效行与画像都不进概览"
        );
    }

    #[test]
    fn a_prospective_due_date_is_ignored_for_non_prospective_kinds() {
        // 一条 fact 挂着 due_at 会让提醒弹出一条非待办的东西 —— 插入时就掐掉。
        let (_dir, db) = tmpdb();
        let mut m = fact("北京", "用户住在北京");
        m.due_at = Some(Db::now_ms() - DAY);
        db.add_memory(&m).unwrap();
        assert!(
            db.due_prospective(Db::now_ms() + DAY).unwrap().is_empty(),
            "非待办的 due_at 必须被忽略"
        );
        assert!(
            db.valid_memories().unwrap()[0].due_at.is_none(),
            "落库时就不该存它"
        );
    }

    #[test]
    fn a_memory_round_trips_with_all_columns() {
        let (_dir, db) = tmpdb();
        let id = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        let rows = db.valid_memories().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, id);
        assert_eq!(r.session_id.as_deref(), Some("s1"));
        assert_eq!(r.kind, "fact");
        assert_eq!(r.subject.as_deref(), Some("用户"));
        assert_eq!(r.predicate.as_deref(), Some("住在"));
        assert_eq!(r.object.as_deref(), Some("北京"));
        assert_eq!(r.body, "用户住在北京");
        assert!((r.confidence - 0.9).abs() < 1e-9);
        assert!(r.valid_from > 0, "时间戳是 ms,Unix 纪元后恒正");
        assert!(r.invalid_at.is_none(), "新建即有效");
    }

    #[test]
    fn empty_or_whitespace_body_is_rejected_everywhere_and_writes_nothing() {
        // spec §8 不变量 7:零信息永不落库 —— 这是「空摘要即 Err」同一条纪律的存储版。
        let (_dir, db) = tmpdb();
        for bad in ["", "   ", "\n\t "] {
            assert!(db.add_memory(&fact("无", bad)).is_err(), "add({bad:?}) 必须拒绝");
        }
        assert!(db.set_profile("  ").is_err());
        let id = db.add_memory(&fact("上海", "用户住在上海")).unwrap();
        assert!(db.update_memory_body(id, "").is_err());
        assert!(db.update_memory_body(id, " ").is_err());
        // 全部拒绝之后,库里仍然干干净净
        assert_eq!(db.valid_memories().unwrap().len(), 1, "被拒的不许留半行");
        assert_eq!(db.profile().unwrap(), "", "画像未被置过");
    }

    #[test]
    fn invalidation_hides_from_valid_and_search_but_not_audit() {
        // Graphiti 式失效:检索视角消失,审计视角保留。
        let (_dir, db) = tmpdb();
        let beijing = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.add_memory(&fact("猫", "用户养了一只猫")).unwrap();

        db.invalidate_memory(beijing).unwrap();

        let valid = db.valid_memories().unwrap();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].body, "用户养了一只猫");
        assert_eq!(
            db.search_memories("北京", None, 10, &RecallCfg::default()).unwrap().len(),
            0,
            "失效记忆不许被召回"
        );
        let audit = db.all_memories().unwrap();
        assert_eq!(audit.len(), 2, "审计视图必须保留被否定的历史");
        assert!(
            audit
                .iter()
                .any(|r| r.id == beijing && r.invalid_at.is_some())
        );
        // 重复失效是幂等错误(行已不在有效集),不是静默成功
        assert!(db.invalidate_memory(beijing).is_err());
    }

    #[test]
    fn the_profile_is_a_single_upserted_row() {
        let (_dir, db) = tmpdb();
        assert_eq!(db.profile().unwrap(), "", "从未写过 = 空串,不是 Err");
        db.set_profile("用户是 Rust 开发者,喜欢简洁回答").unwrap();
        db.set_profile("用户是 Rust 开发者,在学日语").unwrap();
        assert_eq!(db.profile().unwrap(), "用户是 Rust 开发者,在学日语");
        // 单行不变量:清空画像后再写,仍然是单行可读
        db.clear_memories().unwrap();
        assert_eq!(db.profile().unwrap(), "", "清空把画像归空串");
        db.set_profile("新的画像").unwrap();
        assert_eq!(db.profile().unwrap(), "新的画像");
    }

    #[test]
    fn search_ranks_by_gram_overlap_then_recency() {
        let (_dir, db) = tmpdb();
        db.add_memory(&fact("北京", "用户住在北京,在国企上班")).unwrap(); // id 小,先插
        db.add_memory(&fact("上海", "用户住在上海")).unwrap(); // id 大,后插

        // 「住在上海」与上海句全命中(score=1.0),与北京句只命中「住在」(2/4)
        let hits = db.search_memories("住在上海", None, 10, &RecallCfg::default()).unwrap();
        assert_eq!(hits.len(), 2, "部分重叠也要出现,只是排后");
        assert_eq!(hits[0].body, "用户住在上海");
        assert_eq!(hits[1].body, "用户住在北京,在国企上班");

        // 同分(两边都含「住在」)→ 按 updated_at/id 倒序 → 后插的上海句在前
        let tie = db.search_memories("住在", None, 10, &RecallCfg::default()).unwrap();
        assert_eq!(tie[0].body, "用户住在上海");

        // k 截断
        assert_eq!(db.search_memories("住在", None, 1, &RecallCfg::default()).unwrap().len(), 1);
    }

    #[test]
    fn search_ignores_blank_queries_zero_k_and_irrelevant_text() {
        let (_dir, db) = tmpdb();
        db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        for blank in ["", "  ", "\n"] {
            assert!(db.search_memories(blank, None, 10, &RecallCfg::default()).unwrap().is_empty());
        }
        assert!(db.search_memories("用户", None, 0, &RecallCfg::default()).unwrap().is_empty());
        assert!(
            db.search_memories("量子力学", None, 10, &RecallCfg::default()).unwrap().is_empty(),
            "零重叠不入结果 —— 没有命中就不注入"
        );
        // 带向量也一样:空白 query 连向量路径都不召回 —— 空白文本没有可嵌的语义,
        // query_vec 不是「绕过 gram 守卫」的后门。
        assert!(db.search_memories("", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap().is_empty());
        assert!(db.search_memories("量子力学", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap().is_empty());
    }

    // ==== T6:嵌入 BLOB 与混合召回(裁定 1:f32 小端 LE 裸存,Rust 侧余弦) ====

    #[test]
    fn blob_encoding_is_little_endian_f32_and_round_trips() {
        // 编码唯一真相就是这对纯函数:1.0 的 LE 字节序是 [0,0,0x80,0x3F],
        // 钉住它 = 谁「顺手」改成大端或加了头部,这里当场红。
        assert_eq!(to_vec(&[1.0f32]), vec![0x00, 0x00, 0x80, 0x3F]);
        assert_eq!(
            from_vec(&to_vec(&[-2.5, 0.0])).unwrap(),
            vec![-2.5f32, 0.0],
            "to_vec→from_vec 必须还原原值"
        );
        // 空向量是合法值(模型输出维度 0 只在损坏时出现,但空批守卫不造例外)。
        let empty: [f32; 0] = [];
        assert_eq!(to_vec(&empty), Vec::<u8>::new());
        assert_eq!(from_vec(&[]), Some(Vec::<f32>::new()));
        // 长度不是 4 的倍数 = 损坏字节(手改库/半次写),解码必须拒绝 ——
        // 检索侧把 None 当「无向量」降级,决不把损坏的字节当语义。
        for bad_len in [1usize, 2, 3, 5, 7] {
            let blob = vec![0u8; bad_len];
            assert!(from_vec(&blob).is_none(), "长度 {bad_len} 必须拒解码");
        }
    }

    #[test]
    fn set_memory_embedding_requires_an_existing_row_and_search_uses_the_vector() {
        let (_dir, db) = tmpdb();
        let id = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        // BLOB 往返:写进去的是编码前的向量,检索拿回来必须还是同一个语义。
        db.set_memory_embedding(id, &[1.0, 0.0]).unwrap();
        // 直接读 BLOB 验证写的就是编码函数的输出(编码正确性由上一条钉,这里钉通路)。
        {
            let c = db.lock();
            let blob: Vec<u8> = c
                .query_row("SELECT embedding FROM memories WHERE id=?1", params![id], |r| r.get(0))
                .unwrap();
            assert_eq!(from_vec(&blob).unwrap(), vec![1.0f32, 0.0]);
        }
        // 查询同向 → 余弦 1: gram 只有一半命中(「住在」),向量路径把它顶到满分档。
        let hits = db.search_memories("住在,还有别的事", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].body, "用户住在北京");
        // 对不存在的行写向量是错误(affected=0):静默成功会谎称「向量已写」,
        // 行可能刚被用户清空 —— 那是第二份真相的开始。
        assert!(db.set_memory_embedding(id + 999, &[1.0, 0.0]).is_err());
    }

    #[test]
    fn a_vector_aligned_row_beats_an_equal_gram_row_with_a_null_vector() {
        // 融合分 = 0.6*余弦 + 0.4*gram(spec §3.2 的降级形态):两条 gram 分相同的行,
        // 有向量且与查询同向的那条必须胜出 —— 否则 0.6 的权重就是装饰。
        // 同时它就是「NULL 向量行与有向量行共存排序」的那条:上海行没有向量,
        // 靠 0.4*gram 留在结果里,排序落在有向量的北京行之后。
        let (_dir, db) = tmpdb();
        let beijing = db.add_memory(&fact("北京", "用户住在北京")).unwrap(); // 先插,updated_at 同毫秒内 id 更小
        db.add_memory(&fact("上海", "用户住在上海")).unwrap();
        db.set_memory_embedding(beijing, &[1.0, 0.0]).unwrap();

        // 查询「住在」:两行 gram 都是 1.0,平局时(无向量路径)新插的上海在前。
        assert_eq!(db.search_memories("住在", None, 10, &RecallCfg::default()).unwrap()[0].body, "用户住在上海");
        // 带同向查询向量:北京 0.6*1 + 0.4*1 = 1.0,上海(NULL)0.4 → 北京胜出。
        let hits = db.search_memories("住在", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap();
        assert_eq!(hits.len(), 2, "NULL 向量行靠 gram 留场,不许被丢");
        assert_eq!(hits[0].body, "用户住在北京", "同 gram 分下向量行必须胜出");
        assert_eq!(hits[1].body, "用户住在上海");
    }

    #[test]
    fn a_negative_cosine_never_erases_a_keyword_hit() {
        // 语义分在召回里的值域是 0..1(「相关度」),不是 -1..1(「方向」):负余弦
        // 意味着「语义相反」,在召回里的正确含义是**不相关 = 0**,不是「比不相关还差」。
        // 用未夹断的余弦时,0.6*(-1) + 0.4*0.5 = -0.4 < 0 ⇒ retain(>0) 把一条
        // 关键词明明命中的记忆整条丢掉 —— 用户搜「北京」,那条含「北京」的记忆没出现,
        // 而它消失的理由是「向量反向」,这个理由对用户不可见也不可理解。
        let (_dir, db) = tmpdb();
        let beijing = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        // 查询「住在北京」的向量与这条记忆的向量完全反向(cos = -1)。
        db.set_memory_embedding(beijing, &[-1.0, 0.0]).unwrap();

        let hits = db.search_memories("住在北京", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "关键词满分命中的记忆被负余弦挤出了结果:0.6*(-1)+0.4*1 落到了 0 以下"
        );
        assert_eq!(hits[0].body, "用户住在北京");
    }

    #[test]
    fn a_dimension_mismatched_or_zero_vector_degrades_to_gram_scoring() {
        // 维度与查询向量不一致的行:向量分记 0(只剩 0.4*gram),与 NULL 向量行同排 ——
        // 换模型/换维度后旧向量**不许**参与余弦(不同空间的余弦是乱码,不是弱信号)。
        // 夹具特意让「错误使用 3 维向量」会改变排序:若实现把 3 维硬算,北京行的
        // 向量分非零、会顶到上海前面 —— 断言上海在前就是这条守卫的读数。
        let (_dir, db) = tmpdb();
        let beijing = db.add_memory(&fact("北京", "用户住在北京")).unwrap(); // 先插
        db.add_memory(&fact("上海", "用户住在上海")).unwrap();
        db.set_memory_embedding(beijing, &[1.0, 2.0, 3.0]).unwrap(); // 3 维,查询 2 维

        // gram 平局 + 北京向量作废 → 纯 gram 平局 → updated_at/id 倒序 → 上海在前
        let hits = db.search_memories("住在", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap();
        assert_eq!(hits.len(), 2, "降级是向量分记 0,不是把行丢掉");
        assert_eq!(hits[0].body, "用户住在上海", "维度不匹配的向量必须完全作废");
        assert_eq!(hits[1].body, "用户住在北京");

        // 零向量安全:余弦的分母为 0 → 记 0,不是 NaN —— NaN 会让 retain 的
        // `> 0.0` 判假把行**静默丢掉**,「零向量行消失」比「排序不对」更难查。
        db.set_memory_embedding(beijing, &[0.0, 0.0]).unwrap();
        let hits = db.search_memories("住在", Some(&[1.0, 0.0]), 10, &RecallCfg::default()).unwrap();
        assert_eq!(hits.len(), 2, "零向量行不许因 NaN 被丢");
        assert_eq!(hits[0].body, "用户住在上海");
        assert_eq!(hits[1].body, "用户住在北京");
    }

    #[test]
    fn compaction_returns_exactly_the_ids_of_rows_added_in_that_transaction() {
        // compact_session_with_memories 的返回值 = **本次事务新增记忆行的 id 列表**:
        // 嵌入补写按它与 Add body 的顺序对位 —— UpdateBody/Invalidate/SetProfile
        // 不产新行,混进来就是把向量写到旧行头上。
        let (_dir, db) = tmpdb();
        let old = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.set_profile("旧画像").unwrap();

        let sid = db.create_session("新对话").unwrap();
        for text in ["第一轮", "第二轮"] {
            db.append_message(&sid, &u(text)).unwrap();
        }
        let plan = CompactionPlan {
            drop_from: 0,
            drop_to: 2,
            dropped: 2,
            source: vec![u("第一轮"), u("第二轮")],
        };
        let added = db
            .compact_session_with_memories(
                &sid,
                &plan,
                "摘要",
                &[
                    MemoryWrite::Add(fact("上海", "用户搬到上海")),
                    MemoryWrite::UpdateBody { id: old, body: "用户住回北京".into() },
                    MemoryWrite::Invalidate { id: old },
                    MemoryWrite::SetProfile("新画像".into()),
                ],
                None,
            )
            .unwrap();
        assert_eq!(
            added.len(),
            1,
            "四条写入只有 Add 产新行:Update/Invalidate 复用旧行,SetProfile 是单行画像"
        );
        let rows = db.valid_memories().unwrap();
        assert_eq!(rows.len(), 1, "旧行被失效,新增的只有上海那条");
        assert_eq!(added[0], rows[0].id, "返回的 id 必须就是新行的 id");
        assert_eq!(rows[0].body, "用户搬到上海");

        // 零写入返回空表 —— db.rs 的 compact_session(空写入委托)靠这个形态忽略返回值。
        let plan2 = CompactionPlan {
            drop_from: 0,
            drop_to: 0,
            dropped: 0,
            source: vec![],
        };
        let added = db.compact_session_with_memories(&sid, &plan2, "另一条摘要", &[], None).unwrap();
        assert!(added.is_empty());
    }

    #[test]
    fn find_active_by_key_matches_only_same_key_and_only_valid() {
        let (_dir, db) = tmpdb();
        let beijing = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.add_memory(&fact("上海", "用户住在上海")).unwrap();
        db.add_memory(&NewMemory {
            subject: Some("用户".into()),
            predicate: Some("喜欢".into()),
            object: Some("咖啡".into()),
            body: "用户喜欢咖啡".into(),
            ..preference("用户喜欢咖啡")
        })
        .unwrap();
        db.add_memory(&preference("用户讨厌冗长输出")).unwrap();

        let lives = db.find_active_by_key("用户", "住在").unwrap();
        assert_eq!(lives.len(), 2, "同键的两条都要给合并管线看");
        let likes = db.find_active_by_key("用户", "喜欢").unwrap();
        assert_eq!(likes.len(), 1);
        assert!(
            db.find_active_by_key("用户", "讨厌").unwrap().is_empty(),
            "无键记忆(predicate=NULL)对任何键都不命中"
        );

        db.invalidate_memory(beijing).unwrap();
        assert_eq!(db.find_active_by_key("用户", "住在").unwrap().len(), 1);
    }

    #[test]
    fn update_memory_body_changes_what_search_finds() {
        let (_dir, db) = tmpdb();
        let id = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.update_memory_body(id, "用户搬到上海了").unwrap();
        assert!(db.search_memories("北京", None, 10, &RecallCfg::default()).unwrap().is_empty());
        let hits = db.search_memories("上海", None, 10, &RecallCfg::default()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].body, "用户搬到上海了");
        assert!(
            db.update_memory_body(id + 999, "不存在的行").is_err(),
            "改不存在的行必须报错,不是静默成功 —— 那是第二份真相的开始"
        );
    }

    #[test]
    fn delete_memory_is_physical_and_clear_wipes_everything() {
        let (_dir, db) = tmpdb();
        let a = db.add_memory(&fact("北京", "用户住在北京")).unwrap();
        db.add_memory(&preference("用户喜欢咖啡")).unwrap();
        db.invalidate_memory(a).unwrap();
        db.set_profile("画像").unwrap();

        db.delete_memory(a).unwrap();
        assert_eq!(
            db.all_memories().unwrap().len(),
            1,
            "物理删连审计视图也不剩"
        );
        assert!(db.delete_memory(a).is_err(), "再删同一行是错误");

        db.clear_memories().unwrap();
        assert!(db.all_memories().unwrap().is_empty());
        assert_eq!(db.profile().unwrap(), "");
    }

    /// db.rs 同款压缩夹具:3 条对话消息,计划压掉前 2 条。
    /// `CompactionPlan` 手工构造而不是走 `compaction_plan` —— 那个判定要粗估越线,
    /// 这里测的是**事务**,不是「该不该压」。
    fn plan_for_first_two() -> CompactionPlan {
        CompactionPlan {
            drop_from: 0,
            drop_to: 2,
            dropped: 2,
            source: vec![u("第一轮"), u("第二轮")],
        }
    }

    #[test]
    fn compaction_and_memory_writes_commit_together() {
        let (_dir, db) = tmpdb();
        let sid = db.create_session("新对话").unwrap();
        for text in ["第一轮", "第二轮", "第三轮"] {
            db.append_message(&sid, &u(text)).unwrap();
        }
        let plan = plan_for_first_two();

        db.compact_session_with_memories(
            &sid,
            &plan,
            "早期对话摘要:聊了开头两轮",
            &[
                MemoryWrite::Add(fact("北京", "用户住在北京")),
                MemoryWrite::SetProfile("用户住在北京".into()),
            ],
            None,
        )
        .map(|added| {
            // 返回值 = 新增记忆行的 id:嵌入补写按它对位,这里顺手钉住「真的只有一条」。
            assert_eq!(added.len(), 1, "唯一 Add 的 id,实得 {added:?}");
        })
        .unwrap();

        let rows = db.messages(&sid).unwrap();
        // 3 条对话压掉 2 条 + 1 条备注(物理尾部)
        assert_eq!(rows.len(), 2, "保留 1 条原文 + 1 条备注");
        assert!(rows.iter().any(|m| m.text_of().contains("第三轮")));
        assert!(rows.iter().any(|m| m.text_of().contains("早期对话摘要")));
        assert_eq!(db.valid_memories().unwrap().len(), 1);
        assert_eq!(db.profile().unwrap(), "用户住在北京");
    }

    #[test]
    fn a_failed_memory_write_rolls_the_compaction_back_too() {
        // spec §8 不变量 2 的另一半:同一事务 = 一损俱损。镜像 db.rs 的
        // `a_failed_note_insert_rolls_the_deletes_back_too`,失败源换成记忆写入侧
        // (空 body 校验),证明「记忆侧炸了」同样不许留下半条压缩。
        let (_dir, db) = tmpdb();
        let sid = db.create_session("新对话").unwrap();
        for text in ["第一轮", "第二轮", "第三轮"] {
            db.append_message(&sid, &u(text)).unwrap();
        }
        let plan = plan_for_first_two();

        let err = db.compact_session_with_memories(
            &sid,
            &plan,
            "早期对话摘要:聊了开头两轮",
            &[MemoryWrite::Add(fact("无", "  "))],
            None,
        );
        assert!(err.is_err(), "空 body 的写入必须让整个事务失败");

        let rows = db.messages(&sid).unwrap();
        assert_eq!(rows.len(), 3, "删行必须被回滚");
        assert!(
            !rows.iter().any(|m| m.text_of().contains("早期对话摘要")),
            "备注必须被回滚"
        );
        assert!(db.valid_memories().unwrap().is_empty());
    }

    // ---- P0:抽取游标(spec §12)----

    /// 游标三件套:初值 0、可推进、只许前进。
    #[test]
    fn the_extraction_cursor_starts_at_zero_and_only_moves_forward() {
        let (_dir, db) = tmpdb();
        let sid = db.create_session("新对话").unwrap();
        assert_eq!(db.memory_cursor(&sid).unwrap(), 0, "从未抽取过");

        for text in ["一", "二", "三"] {
            db.append_message(&sid, &u(text)).unwrap();
        }
        let max_id = db.max_message_id(&sid).unwrap();
        assert_eq!(max_id, 3);

        db.apply_memories_and_advance_cursor(&sid, &[], max_id).unwrap();
        assert_eq!(db.memory_cursor(&sid).unwrap(), 3);

        // 倒退尝试必须被忽略:并发路径(空闲 worker 与压缩)谁先提交都不该回退,
        // 否则那段会被重抽一遍、堆出近似重复的记忆。
        db.apply_memories_and_advance_cursor(&sid, &[], 1).unwrap();
        assert_eq!(db.memory_cursor(&sid).unwrap(), 3, "游标只许前进");

        // 游标推进与记忆写入同一个事务:写失败 ⇒ 游标不推。
        let bad = db.apply_memories_and_advance_cursor(
            &sid,
            &[MemoryWrite::Add(fact("无", "  "))],
            max_id + 1,
        );
        assert!(bad.is_err());
        assert_eq!(db.memory_cursor(&sid).unwrap(), 3, "事务回滚时游标不许动");
    }

    /// 游标之后的抽取源:排除 system 备注、工具结果行、无文本行。
    #[test]
    fn extraction_source_skips_notes_tool_rows_and_textless_rows() {
        let (_dir, db) = tmpdb();
        let sid = db.create_session("新对话").unwrap();
        db.append_message(&sid, &u("第一句用户的话")).unwrap();
        db.append_message(&sid, &Message::system_note("早期对话摘要:压缩过")).unwrap();
        db.append_message(
            &sid,
            &Message::tool("c1", crate::agent::types::ToolOutput::text("工具输出"), false),
        )
        .unwrap();
        db.append_message(
            &sid,
            &Message {
                role: Role::Assistant,
                blocks: vec![crate::agent::types::ContentBlock::Text {
                    text: "助手的回答".into(),
                }],
            },
        )
        .unwrap();

        let rows = db.messages_for_extraction(&sid, 0).unwrap();
        let texts: Vec<String> = rows.iter().map(|(_, m)| m.text_of()).collect();
        assert_eq!(texts, vec!["第一句用户的话", "助手的回答"], "只留对话文本");

        // 游标之后的都跳过。
        let after = db.messages_for_extraction(&sid, rows[0].0).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].1.text_of(), "助手的回答");
    }

    /// 「需要抽取」的三个判据:空闲够久 + 游标落后 + 有新消息。
    #[test]
    fn sessions_needing_extraction_respects_idleness_and_the_cursor() {
        let (_dir, db) = tmpdb();
        let now = Db::now_ms();
        let idle = db.create_session("空闲的").unwrap();
        let busy = db.create_session("刚活动的").unwrap();
        for sid in [&idle, &busy] {
            db.append_message(sid, &u("说了点东西")).unwrap();
        }
        // busy 刚刚活动;idle 的活动时间推到一小时前。
        {
            let c = db.lock();
            let old = now - 60 * 60 * 1000;
            c.execute(
                "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                params![idle, old],
            )
            .unwrap();
        }

        let due = db.sessions_needing_extraction(now - 5 * 60 * 1000).unwrap();
        assert_eq!(due, vec![idle.clone()], "只挑空闲够久的");

        // 抽过了就不再有它。
        let max = db.max_message_id(&idle).unwrap();
        db.apply_memories_and_advance_cursor(&idle, &[], max).unwrap();
        assert!(db.sessions_needing_extraction(now - 5 * 60 * 1000).unwrap().is_empty());

        // 空闲会话来了新消息 ⇒ 又需要抽。
        db.append_message(&idle, &u("又说了一句")).unwrap();
        {
            let c = db.lock();
            let old = now - 60 * 60 * 1000;
            c.execute(
                "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                params![idle, old],
            )
            .unwrap();
        }
        assert_eq!(
            db.sessions_needing_extraction(now - 5 * 60 * 1000).unwrap(),
            vec![idle]
        );
        let _ = busy;
    }

    /// 压缩路径必须推进游标:否则空闲抽取会把压缩删掉那段的后继再抽一遍。
    #[test]
    fn compaction_advances_the_cursor_to_the_last_deleted_row() {
        let (_dir, db) = tmpdb();
        let sid = db.create_session("新对话").unwrap();
        for text in ["第一轮", "第二轮", "第三轮"] {
            db.append_message(&sid, &u(text)).unwrap();
        }
        assert_eq!(db.memory_cursor(&sid).unwrap(), 0);

        let plan = plan_for_first_two(); // 压掉 id 1..=2
        db.compact_session_with_memories(&sid, &plan, "摘要", &[], None)
            .unwrap();
        assert_eq!(
            db.memory_cursor(&sid).unwrap(),
            2,
            "游标应停在被删区间的最大 id,而不是会话最大 id"
        );

        // 剩下的「第三轮」仍然可抽。
        let left = db.messages_for_extraction(&sid, db.memory_cursor(&sid).unwrap()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1.text_of(), "第三轮");
    }

    // ---- P0:用户编辑与 user_edited 保护(spec §12)----

    #[test]
    fn a_user_edit_marks_the_row_and_raises_confidence_but_keeps_the_due_date() {
        let (_dir, db) = tmpdb();
        let id = db.add_memory(&prospective("下周三交材料", 3)).unwrap();
        {
            let c = db.lock();
            c.execute("UPDATE memories SET confidence=0.7 WHERE id=?1", params![id])
                .unwrap();
        }
        db.update_memory_body_by_user(id, "下周三下午三点前把材料交给老王")
            .unwrap();

        let row = &db.valid_memories().unwrap()[0];
        assert_eq!(row.body, "下周三下午三点前把材料交给老王");
        assert!(row.user_edited, "用户改过的行必须打标");
        assert!(
            (row.confidence - 1.0).abs() < 1e-9,
            "用户亲手确认过的事不该被算法置信度打折"
        );
        assert!(row.due_at.is_some(), "改措辞不许把待办提醒弄丢");
        assert!(db.update_memory_body_by_user(id, "  ").is_err(), "空正文仍要拒");
        assert!(
            db.update_memory_body_by_user(id + 999, "x").is_err(),
            "改不存在的行要报错"
        );
    }

    #[test]
    fn a_manually_added_memory_is_user_owned_from_birth() {
        let (_dir, db) = tmpdb();
        let id = db
            .add_user_memory("preference", "回答尽量短,不要总结", None)
            .unwrap();
        let row = &db.valid_memories().unwrap()[0];
        assert_eq!(row.id, id);
        assert_eq!(row.kind, "preference");
        assert!(row.session_id.is_none(), "手动新增没有来源会话");
        assert!(row.user_edited, "用户写的东西从出生起就是权威的");
        assert!((row.confidence - 1.0).abs() < 1e-9);
        assert!(db.add_user_memory("fact", "   ", None).is_err(), "空正文要拒");
    }

    /// 手动新增一条**带到期时间**的待办:必须能被 `due_prospective` 捞到。
    ///
    /// 这条是 spec §12.5 记录的缺口本身 —— 此前 `add_user_memory` 只认 kind 与 body,
    /// `due_at` 恒为 None,于是面板里亲手加的待办永远等不到提醒。判据不能只断言
    /// 「行里存了 due_at」:真正要钉的是**提醒通道**看得见它(两处 SQL 若各写一份
    /// 判据就会分叉,这里用同一个入口读回来才闭环)。
    #[test]
    fn a_manually_added_prospective_due_at_is_visible_to_the_reminder_channel() {
        let (_dir, db) = tmpdb();
        // 取一个已经到期的时刻(`now - 1s`):提醒窗口是 `due_at <= until`,
        // 用未来时刻会把「窗口判据」也一起测进来,而这条用例只关心「存没存进来」。
        let due = Db::now_ms() - 1_000;
        let id = db
            .add_user_memory("prospective", "下周三交季度报告", Some(due))
            .unwrap();

        let rows = db.due_prospective(Db::now_ms()).unwrap();
        assert_eq!(rows.len(), 1, "手动新增的待办必须进提醒通道");
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].due_at, Some(due), "到期时刻原样落库,不被改写");
        assert!(rows[0].user_edited, "手动新增仍是用户权威行");
    }

    /// 非待办即便带着 due_at 也**不存** —— 判据只有一处(`insert_memory_row`),
    /// `add_user_memory` 不许在它前面另写一遍(两份判据会漂移)。
    /// 反证的意义:若这里存下了,`due_prospective` 会弹出一条「事实型闹钟」。
    #[test]
    fn a_manually_added_non_prospective_drops_its_due_at() {
        let (_dir, db) = tmpdb();
        let due = Db::now_ms() - 1_000;
        let id = db.add_user_memory("fact", "用户搬到上海", Some(due)).unwrap();

        let row = &db.valid_memories().unwrap()[0];
        assert_eq!(row.id, id);
        assert!(row.due_at.is_none(), "非待办的 due_at 必须在插入时丢弃");
        assert!(
            db.due_prospective(Db::now_ms()).unwrap().is_empty(),
            "非待办不许进提醒通道"
        );
    }

    #[test]
    fn an_old_schema_database_gains_memory_tables_on_open() {
        // v1 库(没有 memories 表)被新二进制打开:CREATE IF NOT EXISTS 自动补表,
        // 不搬数据、不拒绝打开 —— 加表是纯增量,与 db.rs 的版本戳政策一致。
        let (dir, db) = tmpdb();
        {
            let c = db.lock();
            c.execute_batch("DROP TABLE memories; DROP TABLE memory_profile;")
                .unwrap();
        }
        drop(db);
        let db2 = Db::open(&dir.path().join("mem.db")).unwrap();
        assert_eq!(db2.add_memory(&fact("北京", "用户住在北京")).unwrap(), 1);
    }
}
