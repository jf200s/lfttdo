// Task 11：会话持久化与重放映射（spec 6.2）。
//
// 设计决定落在这里：**界面状态不落盘，持久化的 messages 是唯一真相源**。重启后把
// messages 映射成等价事件序列，重放同一个 reducer 得到界面 —— 于是永远只有一套渲染
// 路径，也不会有「库里的历史」和「屏幕上的历史」两份真相。
//
// 本模块与 Tauri 无关（Plan A 约束：只有 `ipc.rs` 允许 `use tauri`）：数据库路径由
// 调用方给，不碰 tauri 的 path API 去找 app data 目录。

use crate::agent::context::CompactionPlan;
use crate::agent::events::{AgentEvent, StopReason};
use crate::agent::types::{ContentBlock, Message, Risk, Role, ToolCallView};
use rusqlite::types::Type;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// schema 版本（R74）。`blocks_json` 存的就是 Task 5 的线上格式（`tag = "type"` 那一套），
/// 所以这个库的格式和协议格式是同一条演进线：Plan B/C 加表加列时要靠它认库
/// （T12 的 settings 是同目录下的**另一个文件**，不占这里的版本号）。
/// **schema 变了就 +1，旧库不自动迁移** —— 这里只标记，不搬数据；谁要迁移，读到的就是这一行的值。
/// v2（长期记忆，spec 2026-10-25）：新增 memories / memory_profile 两张表。纯增量 ——
/// v1 库被新二进制打开时靠下面的 `CREATE TABLE IF NOT EXISTS` 自动补上**空表**，
/// 不搬数据也不拒绝打开；版本戳的既定政策不变（只给 user_version==0 的新库盖章）。
/// v3（类人记忆升级，spec 2026-10-25 §11）：memories 加四列 —— `reinforcements`
/// （同一事实被再次确认的次数）、`last_accessed`（上次被召回：访问即再巩固）、
/// `due_at`（待办的到期时刻）、`notified_at`（已提醒时刻，防每轮重复提醒）。
/// `CREATE TABLE IF NOT EXISTS` 对**已存在**的表一个字都不改，所以老库的四列由
/// `attach_memory_columns` 显式补（ALTER TABLE ADD COLUMN 是纯增量：老行拿默认值，
/// 不搬数据、不拒绝打开）。版本戳政策不变。
/// v4（P0 抽取游标 + 用户编辑，spec 2026-10-25 §12）：
/// - `sessions.memory_extracted_upto`：该会话已抽取到的 `messages.id`。没有它，
///   「空闲时抽取」与「压缩时抽取」会重复处理同一段对话 —— 重复抽取不只烧调用，
///   还会把同一件事反复 ADD 成一堆近似重复的记忆。压缩路径也必须推进它。
/// - `memories.user_edited`：用户亲手改过正文。模型的下一次合并**不许**改写这样的行
///   （候选并存、用户自己删）—— 用户权威优先于系统自洽，这是业主拍板的方向。
const SCHEMA_VERSION: i64 = 5;

/// 会话标题上限（字符）。这是**标题**规则（40），不是工具输出预览（200，见
/// `ToolOutput::preview()`）：两个槽位、各自一处计算，别让它们互相「复用」出一个第三种形状。
const TITLE_CHARS: usize = 40;

/// 建表语句。`ON DELETE CASCADE` 是 `delete_session` 只发一条 DELETE 的前提（R73）。
/// 那句 `PRAGMA foreign_keys = ON` 在本仓库当前**是重复的**：`libsqlite3-sys` 的 bundled
/// 构建已经 `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`（`build.rs:153`），实测把 pragma 那一行整行删掉
/// 后相关测试仍全绿 —— 说明 CASCADE 的真机制是编译期那个宏。显式设它买的是「换系统
/// SQLite、或上游默认变了以后仍然成立」。它也不是空话：实测把它改成 `OFF` 会一次红掉
/// 三条级联测试，而 `foreign_keys()` 那条断言（R73）就是盯这一点的哨兵。
const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS sessions(
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    memory_extracted_upto INTEGER NOT NULL DEFAULT 0,
    workspace_dir TEXT);
CREATE TABLE IF NOT EXISTS messages(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    blocks_json TEXT NOT NULL,
    created_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_msg_session ON messages(session_id, id);
CREATE TABLE IF NOT EXISTS memories(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    kind TEXT NOT NULL,
    subject TEXT,
    predicate TEXT,
    object TEXT,
    body TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    valid_from INTEGER NOT NULL,
    invalid_at INTEGER,
    embedding BLOB,
    reinforcements INTEGER NOT NULL DEFAULT 0,
    last_accessed INTEGER,
    due_at INTEGER,
    notified_at INTEGER,
    user_edited INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS memory_profile(
    id INTEGER PRIMARY KEY CHECK (id = 1),
    body TEXT NOT NULL,
    updated_at INTEGER NOT NULL);
PRAGMA foreign_keys = ON;
";

/// 会话库。托管形态归 T13（R76 的交接）：`Db` 本身**不是** `Clone`，要进 Tauri managed
/// state 请持 `Arc<Db>`（plan:2961 那句 `state.db.clone()` 得改成克隆 Arc）；
/// 这里的调用全是同步 SQLite，async 命令里请放进 `spawn_blocking`，
/// 否则一条写会占住一个 tokio worker 线程。
pub struct Db {
    /// `Connection` 本身只 `Send` 不 `Sync`；`Sync` 完全由这层 `Mutex` 提供
    /// （下面那条编译期断言负责盯住这个替换）。
    conn: Mutex<Connection>,
}

/// 编译期断言（R76）：`Db` 必须 `Send + Sync`，因为 Tauri 的 `manage()` 只收这种类型，
/// 而 T13 要在 `spawn_blocking` 里用它。闭包永不执行，只做类型检查：谁把 `Mutex` 换成
/// `RefCell`、或把 `Connection` 直接暴露给调用方，这里立刻编译失败。
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Db>();
};

impl Db {
    /// 打开（或创建）数据库。路径由调用方给：本模块不碰 Tauri 的 path API，
    /// app data 目录归 `ipc.rs` 决定（Plan A 约束）。
    pub fn open(path: &Path) -> Result<Db, rusqlite::Error> {
        let conn = Connection::open(path)?;
        // WAL 买的是**跨连接**的「读不被写挡住」，不是本进程内的并发优化：Plan A 只有这一个
        // 连接，所有写都排在下面那把 Mutex 后面（R78），开不开 WAL 对本进程没有区别。
        // 留它的理由是 spec 风险表 #9 点名的并发写 —— 同一个库文件被第二个进程打开时
        // （跑 `cargo test` 时 app 恰好也在写同一路径），默认的 DELETE 日志模式下读者直接
        // `SQLITE_BUSY`，WAL 下读者照样能读。**双连接并发本任务未实测**，真撞上再补。
        // （T12 的 settings 是**独立文件**存储、不落这个库，所以这里不存在日常的第二条连接。）
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA_SQL)?;
        // v3 的纯增量迁移：`CREATE TABLE IF NOT EXISTS` 不会给**已存在**的 memories 加列,
        // 所以 v2 库这里补四列。新建库走到这里时列已经在（`table_info` 里查得到），
        // 这一步是空转 —— 两条路径共用同一个函数,「缺什么补什么」只有一处判据。
        attach_memory_columns(&conn)?;
        attach_session_columns(&conn)?;
        // 只给**未标记**的库（0 = 这个钩子之前的库，其 schema 与本版本一致）盖章。
        // 已有版本号的一律不覆盖：无条件写 1 会让每次打开都把 T12 推上去的标记抹回原点，
        // 那正是这个钩子要防的「静默改格式」。反过来（版本号比本实现更高 = 新库被旧程序打开）
        // 这里也刻意不拒绝：本任务没有迁移可谈，拒绝策略归 T12/T21 定。
        if pragma_i64(&conn, "user_version")? == 0 {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    /// 锁中毒后继续服务，而不是 panic 把整个应用永久锁死（R76）。
    /// 这个取舍成立的前提是**这里的写不会在 panic 中留下未回滚的事务**：
    /// 单语句写（`append_message` 等）本来就没有跨锁事务；T41 的 `compact_session` 确实
    /// 跨锁持有一个事务，但它靠 `rusqlite::Transaction` 的 **RAII 回滚**兑现同一个前提 ——
    /// 事务对象被 drop（正常返回 `Err`、`?` 提前退出、或 panic 展开）时自动 ROLLBACK，
    /// 所以中毒时连接上不会挂着半条事务。将来谁在 `compact_session` 里 `std::mem::forget`
    /// 那个事务、或把 `Transaction` 换成 `unchecked_transaction` + 手动提交，这里必须重新讨论。
    /// 另外 `compact_session` 的越界分支刻意**报错而不是 slice panic**：panic 会把锁打成
    /// 中毒态，症状是「此后每条命令都打不开」（见那方法的文档注释）。
    /// 这层 `Mutex` 不可重入：`Db` 的方法都是「先取锁再碰连接」，将来谁在持锁期间
    /// 再调一个 `Db` 方法，就是自己等自己 —— 表现是界面卡死而不是报错，所以别试。
    /// （`compact_session` 全程只取一次锁、内部只调自由函数 `insert_message_row`，不碰 `&self`。）
    /// `pub(crate)`（长期记忆）：`store::memory` 的同款方法簇与压缩同事务入口要用它;
    /// 可见性放宽不是开第二个真相 —— 连接仍只有这一把锁。
    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        crate::lock::guard(&self.conn)
    }

    pub fn journal_mode(&self) -> Result<String, rusqlite::Error> {
        let c = self.lock();
        c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
    }

    /// R73 的断言入口：`foreign_keys` 必须是 1，否则 CASCADE 静默失效、孤儿行谁也看不见。
    pub fn foreign_keys(&self) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        pragma_i64(&c, "foreign_keys")
    }

    /// R74 的断言入口，也是 T12 的迁移读入口。
    pub fn user_version(&self) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        pragma_i64(&c, "user_version")
    }

    /// 毫秒时间戳（R72）。秒级粒度下，同一秒内建的两个会话 `updated_at` 相同，
    /// 「最新的排最前」就变成 SQLite 的自由裁量，brief 那条排序测试是 flaky 的。
    /// `pub(crate)`（长期记忆）：memories 的 valid_from/updated_at 与会话表用**同一个钟**,
    /// 排序与审计的时间口径只许有一处。
    pub(crate) fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            // 系统时钟走到 UNIX_EPOCH 之前才会走这支：0 只把这一行排到最旧，
            // 不改内容，而这里没有第二份时间真相需要保持一致。
            .unwrap_or(0)
    }

    pub fn create_session(&self, title: &str) -> Result<String, rusqlite::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let t = Self::now_ms();
        let c = self.lock();
        c.execute(
            "INSERT INTO sessions(id,title,created_at,updated_at) VALUES(?1,?2,?3,?3)",
            params![id, title, t],
        )?;
        Ok(id)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, rusqlite::Error> {
        let c = self.lock();
        // 末尾的 `rowid DESC` 是 R72 的兜底：`updated_at` 与 `created_at` 都相等时
        // （同一毫秒建两条）没有它 SQLite 不保证顺序。sessions 用 TEXT 主键，
        // 但它是普通 rowid 表，rowid 仍然随插入单调递增。
        let mut st = c.prepare(
            "SELECT id,title,updated_at FROM sessions \
             ORDER BY updated_at DESC,created_at DESC,rowid DESC",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn session_exists(&self, id: &str) -> Result<bool, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare("SELECT 1 FROM sessions WHERE id=?1")?;
        st.exists([id])
    }

    pub fn session_workspace(&self, id: &str) -> Result<Option<String>, rusqlite::Error> {
        let c = self.lock();
        let mut st = c.prepare("SELECT workspace_dir FROM sessions WHERE id=?1")?;
        let mut rows = st.query([id])?;
        match rows.next()? {
            Some(row) => row.get(0),
            None => Ok(None),
        }
    }

    pub fn set_session_workspace(
        &self,
        id: &str,
        dir: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        let changed = c.execute(
            "UPDATE sessions SET workspace_dir=?2 WHERE id=?1",
            params![id, dir],
        )?;
        if changed == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// 改名同时把会话顶到最新：用户刚动过的那条排在列表最前。
    pub fn rename_session(&self, id: &str, title: &str) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        c.execute(
            "UPDATE sessions SET title=?2, updated_at=?3 WHERE id=?1",
            params![id, title, Self::now_ms()],
        )?;
        Ok(())
    }

    /// 只发一条 `DELETE FROM sessions`，消息由 schema 里的 `ON DELETE CASCADE` 连带删掉
    /// （R73）。brief 手动删两遍，于是 CASCADE 从来没被真正测过：那条写法的测试在
    /// 「CASCADE 子句被删掉」时照样绿，因为消失是手动那条做的。
    pub fn delete_session(&self, id: &str) -> Result<(), rusqlite::Error> {
        let c = self.lock();
        c.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
        Ok(())
    }

    /// 追加一行，返回 rowid。刻意不包事务：这里碰两行（messages 与 sessions.updated_at），
    /// 撕裂的代价只是排序键旧了一格；包成事务会让上面的「中毒即恢复」承担回滚语义。
    /// SQL 与 `blocks_json` 的编码都在 [`insert_message_row`]（T41 要求：落一行消息的写法
    /// 只有一处真相，压缩备注必须走同一条路）。
    pub fn append_message(&self, session_id: &str, m: &Message) -> Result<i64, rusqlite::Error> {
        let c = self.lock();
        insert_message_row(&c, session_id, m)
    }

    /// 一轮新增行的**原子**落库入口：要么整轮写进去，要么一行都不写。
    /// 逐行调用 `append_message` 时，中途失败会留下「半轮」历史 —— 界面已经按完整一轮
    /// 渲染，库里却只有前几行，重开后同一句话变成两半。
    pub fn append_messages(
        &self,
        session_id: &str,
        rows: &[Message],
    ) -> Result<(), rusqlite::Error> {
        let mut c = self.lock();
        let tx = c.transaction()?;
        for m in rows {
            insert_message_row(&tx, session_id, m)?;
        }
        tx.commit()
    }

    /// 把 [`CompactionPlan`](crate::agent::context::CompactionPlan) 圈定的最老一段对话行删掉，
    /// 并在**同一个事务**里落一条 `Role::System` 摘要备注。
    ///
    /// **一个事务 = 删除与插入要么都成、要么都不做**（计划 Step 5）。两条失败路径各有各的
    /// 覆盖：取消发生在「模型还在写摘要」时，压根不会走到这里（`ipc.rs` 的
    /// `compact_before_turn` 先收完摘要再调本方法，所以库自然一字未改）；而**事务中途失败**
    /// 由 `a_failed_note_insert_rolls_the_deletes_back_too` 用触发器实测 —— 把这里拆成两次
    /// 非事务调用，那条会留下「原文已删、备注没落」的半条状态。
    ///
    /// **下标口径**：`plan.drop_from`/`drop_to` 数的是**对话消息**（去掉 `role = 'system'`
    /// 之后按 `ORDER BY id` 的序列）里的位置，与 `compaction_plan` 的 `conv` 同一个口径；
    /// 映射见 [`conversation_ids`]，那里写明了为什么不能用 `LIMIT/OFFSET`。
    ///
    /// **备注落在物理尾部**（前置事实 3）：`id INTEGER PRIMARY KEY AUTOINCREMENT` 让新插入的
    /// 一行拿到最大 id，`messages()` 读回来就在最后 —— 「插在最前」物理上做不到，也不需要：
    /// `ContextBuilder::build()` 把 `Role::System` 整行摘出来当 `notes` 拼在 head 之后，
    /// `events_from_messages` 的 `Role::System => {}` 又让它对用户不可见。这条不变量由
    /// 测试里的 `build()` 断言钉住，而不是钉在一条写不出来的 SQL 上。
    ///
    /// 实现经 `store::memory` 的 [`Db::compact_session_with_memories`](crate::store::memory::Db::compact_session_with_memories)
    /// （长期记忆把「压缩+记忆写入」并进了同一个事务,本方法 = 那个入口的空写入形态;
    /// 删行、越界裁定、备注 SQL 的唯一真相都在那边）。
    pub fn compact_session(
        &self,
        session_id: &str,
        plan: &CompactionPlan,
        note_text: &str,
    ) -> Result<(), rusqlite::Error> {
        // 返回值(本事务新增记忆行的 id)只对嵌入补写有意义,本方法零写入 ⇒ 恒空表,
        // 忽略之 —— 「删除 + 备注」这条老入口的签名与语义都保持原样。
        self.compact_session_with_memories(session_id, plan, note_text, &[], None)
            .map(|_| ())
    }

    /// 读回一个会话的全部消息，顺序 == 写入顺序（`ORDER BY id`，不是时间戳：
    /// 同一毫秒追加的两行，时间戳并列时对话顺序就没了依据）。
    /// 任何一行解不开都直接报错，不返回「一条看起来完整」的历史（R71）：
    /// 重放的前提是库里就是真相，静默补一条空消息等于把数据损坏藏起来。
    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>, rusqlite::Error> {
        let c = self.lock();
        let mut st =
            c.prepare("SELECT role,blocks_json FROM messages WHERE session_id=?1 ORDER BY id")?;
        let out = st
            .query_map(params![session_id], |r| {
                let role = role_from_code(&r.get::<_, String>(0)?, 0)?;
                let blocks = decode_blocks(&r.get::<_, String>(1)?, 1)?;
                Ok(Message { role, blocks })
            })?
            .collect::<Result<_, _>>()?;
        Ok(out)
    }

    /// 取最近一句**真人**发言当会话标题。两个坑都在 role 上（R75），要**两道**筛选才挡得住：
    /// ① `Message::tool` 的 role 也是 `User`（`agent/types.rs:105`）—— 不跳过就返回 `Some("")`，
    /// T13 拿它当标题会把会话改名成空串。纯结果行的 `text_of()` 是空串，「非空文本」那道
    /// 顺带就挡住了；但**带字的工具行**（Text + ToolResult 混在一行）只有 `is_tool_row` 挡得住，
    /// 变异实验实测：只留「非空文本」这一道，标题会变成工具附带的说明。
    /// ② 纯图片行（切片 2 的入口）没有 Text 块，`text_of()` 同样是空串，不该顶掉有字的那句。
    /// 交 T13：自动改名只允许覆盖仍是默认「新对话」的标题（plan:2972 现在无条件 rename，
    /// 用户自己起的名字每次发送都会被冲掉）。
    pub fn latest_user_message(&self, session_id: &str) -> Result<Option<String>, rusqlite::Error> {
        Ok(self
            .messages(session_id)?
            .iter()
            .rev()
            .filter(|m| m.role == Role::User && !is_tool_row(m) && !m.is_tool_image_followup())
            .map(|m| m.text_of())
            .find(|t| !t.is_empty())
            .map(|t| crate::text::truncate_display(&t, TITLE_CHARS)))
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    /// 毫秒（R72）。前端拿它排序就够了，不需要格式化，所以不存 ISO 串。
    pub updated_at: i64,
}

/// v3 的列补齐（见 `SCHEMA_VERSION` 的 v3 段）。**只做 ALTER ADD COLUMN**，
/// 不改既有列、不搬数据 —— 所以它对「已经很新的库」是空转，对 v2 库是补四列。
/// 用 `PRAGMA table_info` 而不是「ALTER 一次吞错误」：后者会把真正的失败
/// （磁盘满、库被别处锁住）也当成「列已存在」静默吞掉。
fn attach_memory_columns(c: &Connection) -> Result<(), rusqlite::Error> {
    let mut st = c.prepare("PRAGMA table_info(memories)")?;
    let existing: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    // (列名, DDL 尾巴)。顺序固定，便于日志与测试逐条核对。
    let wanted = [
        (
            "reinforcements",
            "reinforcements INTEGER NOT NULL DEFAULT 0",
        ),
        ("last_accessed", "last_accessed INTEGER"),
        ("due_at", "due_at INTEGER"),
        ("notified_at", "notified_at INTEGER"),
        ("user_edited", "user_edited INTEGER NOT NULL DEFAULT 0"),
    ];
    for (name, ddl) in wanted {
        if !existing.iter().any(|c| c == name) {
            c.execute(&format!("ALTER TABLE memories ADD COLUMN {ddl}"), [])?;
        }
    }
    Ok(())
}

/// v4 的 sessions 列补齐。与 [`attach_memory_columns`] 同一套判据（查 `table_info`、
/// 缺才 ALTER），只是目标表不同 —— 分成两个函数而不是一个带表名的:表名一旦拼进 SQL，
/// 「只有字面量进 SQL」这条纪律就破了个口子。
fn attach_session_columns(c: &Connection) -> Result<(), rusqlite::Error> {
    let mut st = c.prepare("PRAGMA table_info(sessions)")?;
    let existing: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    if !existing.iter().any(|c| c == "memory_extracted_upto") {
        c.execute(
            "ALTER TABLE sessions ADD COLUMN memory_extracted_upto INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !existing.iter().any(|c| c == "workspace_dir") {
        c.execute("ALTER TABLE sessions ADD COLUMN workspace_dir TEXT", [])?;
    }
    Ok(())
}

fn pragma_i64(c: &Connection, name: &str) -> Result<i64, rusqlite::Error> {
    // `name` 只来自本文件里的字面量，没有拼接外部输入的路径。
    c.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
}

/// 落一行消息的**唯一** SQL 与**唯一**的 `blocks_json` 编码点（T41 Step 5：
/// 「插入走已有 `append_message` 的 SQL，不新写一条 INSERT」—— 两条 INSERT 语句就是两份
/// 「一行消息长什么样」的真相，早晚和 `role_code` 分叉）。
///
/// 收 `&Connection` 而不是 `&mut Connection`：`rusqlite::Transaction` 实现了
/// `Deref<Target = Connection>`（rusqlite 0.40.2 `transaction.rs:233`），所以
/// `compact_session` 在事务里直接把 `&tx` 递进来，两处共用同一条语句。
/// `session_id` 不存在时由外键报错（`a_row_for_an_unknown_session_is_rejected_by_the_foreign_key`），
/// 这里不另做一次存在性检查 —— 那是第二份真相。
/// `pub(crate)`（长期记忆）：`store::memory` 的压缩同事务入口要落同一条备注,
/// 「一行消息长什么样」仍然只有这一处 SQL。
pub(crate) fn insert_message_row(
    c: &Connection,
    session_id: &str,
    m: &Message,
) -> Result<i64, rusqlite::Error> {
    // blocks_json 只装 blocks 数组，role 有自己的列（可被 SQL 过滤，且是 R71 的显式 match）。
    let json = serde_json::to_string(&m.blocks)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let t = Db::now_ms();
    c.execute(
        "INSERT INTO messages(session_id,role,blocks_json,created_at) VALUES(?1,?2,?3,?4)",
        params![session_id, role_code(&m.role), json, t],
    )?;
    let rowid = c.last_insert_rowid();
    c.execute(
        "UPDATE sessions SET updated_at=?2 WHERE id=?1",
        params![session_id, t],
    )?;
    Ok(rowid)
}

/// `CompactionPlan` 的**对话下标**到 `messages.id` 的映射，即「第几条对话消息」对应哪一行。
/// 顺序与 `Db::messages()` 逐字相同（同一条 `ORDER BY id`）—— 那是这条映射的唯一根据。
/// **刻意不用 `LIMIT`/`OFFSET`**：SQLite 的 OFFSET 数的是过滤前的行序，在 role 过滤下与
/// `messages()` 的切片不对齐，切错了就是删错用户的原话。
/// `'system'` 那个字面量来自 [`role_code`]（口径只有一处）：把 `role<>'system'` 改成硬编码
/// 字符串，将来 `role_code` 换名就会让备注被当成对话消息删掉
/// （`the_second_compaction_does_not_count_the_note_as_a_conversation_row` 盯这一点）。
/// `pub(crate)`（长期记忆）：压缩同事务入口与这里共用**同一条**下标映射,
/// 第二份「第几条对话消息是哪行」的答案迟早删错用户的原话。
pub(crate) fn conversation_ids(
    c: &Connection,
    session_id: &str,
) -> Result<Vec<i64>, rusqlite::Error> {
    let mut st =
        c.prepare("SELECT id FROM messages WHERE session_id=?1 AND role<>?2 ORDER BY id")?;
    let ids = st
        .query_map(params![session_id, role_code(&Role::System)], |r| {
            r.get::<_, i64>(0)
        })?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// 一行损坏的库内容。`Display` 里带上原始报错，T13 把它 `to_string()` 给前端时
/// 用户至少能知道「哪一列解不开」而不是收到一个空历史。
#[derive(Debug)]
struct DecodeError(&'static str, String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0, self.1)
    }
}

impl std::error::Error for DecodeError {}

fn from_sql_failure(col: usize, reason: &'static str, detail: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        col,
        Type::Text,
        Box::new(DecodeError(reason, detail)),
    )
}

pub(crate) fn decode_blocks(raw: &str, col: usize) -> Result<Vec<ContentBlock>, rusqlite::Error> {
    serde_json::from_str(raw).map_err(|e| {
        from_sql_failure(
            col,
            "messages.blocks_json 解不出 ContentBlock 数组",
            e.to_string(),
        )
    })
}

/// role 列的**显式**映射（R71）。不借 `serde_json::to_string(role).unwrap().trim_matches('"')`：
/// 那是把序列化格式当字符串刮一遍，`Role` 一旦变成带数据的变体就静默产出垃圾。
/// 读回来也不许兜底成 `User`：那是把库里一行损坏伪装成「用户说过一句话」。
/// 新增 `Role` 变体时这两个 `match` 都会编译失败 —— 那正是想要的提醒。
pub(crate) fn role_code(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

pub(crate) fn role_from_code(raw: &str, col: usize) -> Result<Role, rusqlite::Error> {
    match raw {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(from_sql_failure(
            col,
            "messages.role 不是本仓库认识的 role（协议里的 `tool` 角色在类型层由 User 承载，库里也一样）",
            other.to_string(),
        )),
    }
}

/// 工具结果行：role 与真人发言同为 `User`，**只能靠块种类区分**（R69/R75）。
fn is_tool_row(m: &Message) -> bool {
    m.blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// 重放映射（spec 6.2）：把持久化消息换成等价事件序列，前端用**同一个 reducer** 得到界面。
/// 纯函数，不碰数据库、不碰注册表。
///
/// 分支依据是**块种类，不是 role**（R69）：`Message::tool(..)` 的 role 也是 `User`，
/// 按 role 走会把一条工具结果重放成用户气泡。
///
/// `risk_of` 由调用方给（R70）：风险徽章是安全语义，写死 `Risk::Read` 会让 Plan B 的
/// `write_file` 重开后显示「自动允许」，等于界面替用户撒了谎。T13 传
/// `|name| registry.risk_of(name)`。**这里不参与任何决策** —— 前端只渲染徽章，
/// 放行与否仍在 Rust 的权限门里判（Plan A 约束：UI 不参与决策）。
pub fn events_from_messages(msgs: &[Message], risk_of: &dyn Fn(&str) -> Risk) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    // 轮号必须复用**直播路径的规则**，不是「从 0 往后连号」：`session.rs:119` 是
    // `for turn in 1..=max_turns`，一次 `run()`（= 一次发送）里第一条助手行是 `turn: 1`，
    // 每多一个工具轮加 1，下一次发送又从 1 起。所以这里遇到真人发言就重置。
    // 重放若连号，前端拿到的「第 N 轮」与直播错位 —— 正是 spec:421 要消灭的那两套真相。
    let mut turn = 1u32;
    for m in msgs {
        match m.role {
            // System 行整行跳过（R69）：系统提示词与 Plan C 的压缩摘要都是给模型读的，
            // 不是聊天记录。它们落库只为了让下一轮的上下文完整。
            Role::System => {}
            Role::User => {
                if m.is_tool_image_followup() {
                    continue;
                }
                if is_tool_row(m) {
                    // T10 从不用「Text + ToolResult 混在一行」的形状（结果一律 `Message::tool`
                    // 单块构造），所以这一臂只发结果事件、把同行的 Text 丢掉。
                    // 上面那条规则由这道 debug 断言守着：将来谁真的写出混合行，测试与 dev
                    // 构建会在这里响，而不是让用户的话静默从重放里消失。
                    debug_assert!(
                        m.blocks
                            .iter()
                            .all(|b| matches!(b, ContentBlock::ToolResult { .. })),
                        "工具结果行混进了非 ToolResult 块：这一臂会把它的文本丢掉"
                    );
                    for b in &m.blocks {
                        if let ContentBlock::ToolResult {
                            call_id,
                            is_error,
                            output,
                        } = b
                        {
                            let (preview, images) = output.event_view();
                            out.push(AgentEvent::ToolResult {
                                call_id: call_id.clone(),
                                // is_error 原样交回（T10 交接③）：库里失败就是失败，不许降级成成功。
                                is_error: *is_error,
                                // 重放不还原耗时（库里没这一列）。交 T14：0 别显示成「0 ms」。
                                ms: 0,
                                preview,
                                images,
                            });
                        }
                        // 工具行里混 Text 块在 Plan A 不可达（T10 用 `Message::tool` 单块构造）。
                        // 真出现了也不在这里发明事件：R69 的规则是「含 ToolResult 就走结果臂」。
                    }
                } else {
                    // R68：用户的话必须是 `user_message`，不能塞进 `text_delta`。
                    // live 时这句话由前端乐观插入（plan:4335），事件流里没有它；
                    // 重放没有「插入」这一步，用 text_delta 就等于把用户发言并进助手气泡。
                    // blocks 原样带上：重开会话要能看到当时的图片缩略图。
                    // （User 行里的 ToolCall 块在 Plan A 不可达 —— 调用只出自助手行；
                    // 它只会原样留在 blocks 里，显示与否由前端决定。）
                    turn = 1; // 一次发送的轮号从这里重新开始（见函数开头那条规则）
                    out.push(AgentEvent::UserMessage {
                        blocks: m.blocks.clone(),
                    });
                }
            }
            Role::Assistant => {
                // 轮号沿用函数开头那条规则：从 1 起、每次真人发言重置，和 `session.rs:119`
                // 的 `for turn in 1..=max_turns` 同一个形状（不是从 0 连号）。
                // 前端 reducer 用 turn 定位「本轮」，错位一次就会把两段助手文本并到同一轮。
                out.push(AgentEvent::TurnStart { turn });
                turn += 1;
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => {
                            out.push(AgentEvent::TextDelta { text: text.clone() })
                        }
                        ContentBlock::ToolCall { id, name, input } => {
                            out.push(AgentEvent::tool_call(ToolCallView {
                                id: id.clone(),
                                name: name.clone(),
                                risk: risk_of(name),
                                input: input.clone(),
                            }));
                        }
                        // 助手行里的 Image 块（模型不回图）与 ToolResult 块（结果一律单独成行）
                        // 在 Plan A 都不可达。不可达的两支不映射：宁可库里原样留着，
                        // 也不在重放层为 Plan B/C 的多模态输出编造一个事件形状。
                        ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
                    }
                }
            }
        }
    }
    // 重放末尾恒发 done：前端靠它把 streaming 关掉、输入框解禁（spec 6.2 的事件表）。
    // 代价是 `usage` 这类瞬时事件重开即消失 —— 它们本就不该假装是历史。
    out.push(AgentEvent::Done {
        stop_reason: StopReason::EndTurn,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{ImageMime, ToolOutput};
    use serde_json::{Value, json};

    fn tmpdb() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("t.db")).unwrap();
        (d, db)
    }

    fn user_text(text: &str) -> Message {
        Message::user(vec![ContentBlock::Text {
            text: text.to_string(),
        }])
    }

    /// `types.rs` 没有 assistant 构造器（只有 user/system_note/tool），测试里自己拼。
    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            blocks,
        }
    }

    fn text_block(t: &str) -> ContentBlock {
        ContentBlock::Text {
            text: t.to_string(),
        }
    }

    /// 重放映射要一个 risk 查表（R70）。Plan A 的内置工具全是 `Read` ⇒ 全程零弹窗，
    /// 但这里必须显式给，不能让 store 自己编一个默认值。
    fn read_of(_name: &str) -> Risk {
        Risk::Read
    }

    /// 同一个名字，查表给 `Write`（Plan B 的 `write_file` 就是这样）。
    fn write_of(_name: &str) -> Risk {
        Risk::Write
    }

    /// 事件的线上 `t` 标签序列 —— 断言写的是前端实际读到的字节，不是 Rust 变体名。
    fn tags(evts: &[AgentEvent]) -> Vec<String> {
        evts.iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["t"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn values(evts: &[AgentEvent]) -> Vec<Value> {
        evts.iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    fn tool_call_value(evts: &[AgentEvent]) -> Value {
        values(evts)
            .into_iter()
            .find(|v| v["t"] == "tool_call")
            .expect("重放序列里必须有 tool_call")
    }

    // ---------- 往返 ----------

    #[test]
    fn round_trips_text_message_preserving_role() {
        let (_d, db) = tmpdb();
        let s = db.create_session("测试").unwrap();
        db.append_message(&s, &user_text("你好")).unwrap();
        let got = db.messages(&s).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].role, Role::User);
        assert!(
            matches!(&got[0].blocks[0], ContentBlock::Text { text } if text == "你好"),
            "读回的不是那条原话：{:?}",
            got[0].blocks
        );
    }

    #[test]
    fn round_trips_image_and_tool_blocks() {
        let (_d, db) = tmpdb();
        let s = db.create_session("s").unwrap();
        let m = assistant(vec![
            ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Jpeg,
            },
            ContentBlock::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: json!({"t": 1}),
            },
            ContentBlock::ToolResult {
                call_id: "c1".into(),
                is_error: true,
                output: ToolOutput::text("boom"),
            },
        ]);
        db.append_message(&s, &m).unwrap();
        assert_eq!(db.messages(&s).unwrap()[0], m, "三种块必须无损往返");
    }

    /// 重放的前提：进程退出再打开，读回顺序 == 写入顺序（`ORDER BY id`，不是时间戳）。
    #[test]
    fn the_whole_history_survives_reopening_the_file() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("t.db");
        let s = {
            let db = Db::open(&path).unwrap();
            let s = db.create_session("重启前").unwrap();
            for t in ["第一句", "第二句", "第三句"] {
                db.append_message(&s, &user_text(t)).unwrap();
            }
            s
        };
        let db = Db::open(&path).unwrap();
        let texts: Vec<String> = db
            .messages(&s)
            .unwrap()
            .iter()
            .map(|m| m.text_of())
            .collect();
        assert_eq!(texts, ["第一句", "第二句", "第三句"]);
    }

    // ---------- 会话表 ----------

    #[test]
    fn sessions_order_newest_first_and_delete_is_cascading() {
        let (_d, db) = tmpdb();
        let a = db.create_session("A").unwrap();
        let b = db.create_session("B").unwrap();
        db.append_message(&b, &user_text("x")).unwrap();
        let list = db.list_sessions().unwrap();
        assert_eq!(list[0].id, b, "最新的排最前");
        // `delete_session` 只发一条 `DELETE FROM sessions`，消息消失必须是 schema 里那句
        // ON DELETE CASCADE 干的：把那个子句删掉，这条会在 delete_session 直接报错。
        db.delete_session(&b).unwrap();
        assert!(db.messages(&b).unwrap().is_empty(), "删会话必须连带删消息");
        let rest = db.list_sessions().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, a);
    }

    /// 上一条的前提：外键真的在生效，孤儿行插不进来。
    /// （`PRAGMA foreign_keys` 没应用时，这条会静默成功，CASCADE 的断言就空心了。）
    #[test]
    fn a_row_for_an_unknown_session_is_rejected_by_the_foreign_key() {
        let (_d, db) = tmpdb();
        assert!(
            db.append_message("没有这个会话", &user_text("x")).is_err(),
            "外键没生效：未知 session_id 的一行被写进了库"
        );
    }

    #[test]
    fn rename_session_sets_the_title_and_moves_it_to_the_front() {
        let (_d, db) = tmpdb();
        let a = db.create_session("A").unwrap();
        let b = db.create_session("B").unwrap();
        // 把 B 的时间戳推回一小时，模拟「旧会话」。不推的话两次操作落在同一毫秒，
        // 排序就只剩 rowid 兜底 —— 那条性质由下面 R72 的专用测试负责，这里要测的是时间戳。
        let c = db.lock();
        c.execute(
            "UPDATE sessions SET updated_at = updated_at - 3600000 WHERE id=?1",
            params![b],
        )
        .unwrap();
        drop(c);
        db.rename_session(&a, "改名了").unwrap();
        let list = db.list_sessions().unwrap();
        assert_eq!(list[0].id, a, "改名要顶到最新：用户刚动过的会话排在最前");
        assert_eq!(list[0].title, "改名了");
        assert_eq!(list.len(), 2, "改名不是删除");
    }

    #[test]
    fn session_timestamps_are_milliseconds_not_seconds() {
        let (_d, db) = tmpdb();
        db.create_session("A").unwrap();
        let row = db.list_sessions().unwrap().remove(0);
        // 秒级时间戳现在约 1.7e9，毫秒约 1.7e12：跨过数量级才证明单位没写错。
        assert!(
            row.updated_at > 1_000_000_000_000,
            "updated_at 必须是毫秒，读到 {}",
            row.updated_at
        );
    }

    /// R72：`updated_at` 相等时 SQLite 不保证顺序，tiebreak 只能靠 rowid。
    #[test]
    fn two_sessions_stamped_in_the_same_millisecond_still_order_newest_first() {
        let (_d, db) = tmpdb();
        let a = db.create_session("A").unwrap();
        let b = db.create_session("B").unwrap();
        let c = db.lock();
        c.execute(
            "UPDATE sessions SET updated_at=(SELECT updated_at FROM sessions WHERE id=?2), \
             created_at=(SELECT created_at FROM sessions WHERE id=?2) WHERE id=?1",
            params![b, a],
        )
        .unwrap();
        drop(c);
        let list = db.list_sessions().unwrap();
        assert_eq!(
            list[0].id, b,
            "时间戳并列时后插入的那条算最新（rowid DESC 兜底）"
        );
    }

    /// WAL 买的是**崩溃耐久** + 将来第二个连接（T12 的 settings 同库）能读。
    /// 别说成「并发写不锁表」：`Mutex<Connection>` 意味着本任务压根没有并发写（R78）。
    #[test]
    fn opening_the_database_sets_wal_foreign_keys_and_a_schema_version() {
        let (_d, db) = tmpdb();
        assert_eq!(db.journal_mode().unwrap(), "wal");
        assert_eq!(
            db.foreign_keys().unwrap(),
            1,
            "foreign_keys 没生效 = CASCADE 静默失效，孤儿行谁也看不见"
        );
        assert_eq!(
            db.user_version().unwrap(),
            5,
            "R74：迁移钩子要有初值。v5 = v4 的迁移再加 sessions.workspace_dir,见 SCHEMA_VERSION"
        );
    }

    /// R74 的钩子只对**未标记**的库成立：已有版本号的一律不覆盖。
    /// T12 往同一个文件加 settings 表时会把它推到 2，无条件写 1 等于每次打开都抹掉别人的迁移标记。
    #[test]
    fn a_database_already_stamped_with_a_later_version_keeps_it() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("t.db");
        {
            let db = Db::open(&path).unwrap();
            let c = db.lock();
            c.execute_batch("PRAGMA user_version = 2").unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(db.user_version().unwrap(), 2);
    }


    #[test]
    fn session_workspace_round_trips_and_clears() {
        let (_d, db) = tmpdb();
        let sid = db.create_session("新对话").unwrap();
        assert!(db.session_exists(&sid).unwrap());
        assert!(!db.session_exists("missing").unwrap());
        assert_eq!(db.session_workspace(&sid).unwrap(), None);
        db.set_session_workspace(&sid, Some("C:\\work")).unwrap();
        assert_eq!(
            db.session_workspace(&sid).unwrap().as_deref(),
            Some("C:\\work")
        );
        db.set_session_workspace(&sid, None).unwrap();
        assert_eq!(
            db.session_workspace(&sid).unwrap(),
            None,
            "清除后必须回落到默认位置"
        );
        assert!(
            db.set_session_workspace("missing", Some("C:\\work"))
                .is_err(),
            "不存在的会话不能被静默改"
        );
    }

    #[test]
    fn a_v4_database_gains_the_workspace_column() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("v4.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY,title TEXT NOT NULL,\
                 created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);\
                 PRAGMA user_version = 4;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let sid = db.create_session("旧库").unwrap();
        assert_eq!(db.session_workspace(&sid).unwrap(), None);
        db.set_session_workspace(&sid, Some("C:\\work")).unwrap();
        assert_eq!(
            db.session_workspace(&sid).unwrap().as_deref(),
            Some("C:\\work")
        );
    }

    // ---------- 标题 ----------

    #[test]
    fn latest_user_message_becomes_session_title() {
        let (_d, db) = tmpdb();
        let s = db.create_session("新对话").unwrap();
        db.append_message(&s, &user_text("帮我查一下明天天气"))
            .unwrap();
        assert_eq!(
            db.latest_user_message(&s).unwrap().as_deref(),
            Some("帮我查一下明天天气")
        );
    }

    /// R75：工具结果行的 role **也是 User**（`agent/types.rs:105` 的决定）。不跳过的话，
    /// 第一个工具轮之后这里返回 `Some("")`，T13（plan:2972）拿它当标题就把会话改名成空串。
    /// 末尾那条**混了 Text 的工具行**才是这条裁定的真证据：纯结果行靠「文本非空」那道就挡住了，
    /// 带字的工具行只能按块种类判（与 `events_from_messages` 的 R69 同一个规则）。
    #[test]
    fn latest_user_message_skips_tool_result_rows() {
        let (_d, db) = tmpdb();
        let s = db.create_session("新对话").unwrap();
        db.append_message(&s, &user_text("读一下 a.txt")).unwrap();
        db.append_message(
            &s,
            &assistant(vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: json!({}),
            }]),
        )
        .unwrap();
        db.append_message(
            &s,
            &Message::tool("c1", ToolOutput::text("文件内容"), false),
        )
        .unwrap();
        db.append_message(&s, &assistant(vec![text_block("读完了")]))
            .unwrap();
        assert_eq!(
            db.latest_user_message(&s).unwrap().as_deref(),
            Some("读一下 a.txt")
        );
        db.append_message(
            &s,
            &Message::user(vec![
                text_block("这是工具结果附带的说明"),
                ContentBlock::ToolResult {
                    call_id: "c2".into(),
                    is_error: false,
                    output: ToolOutput::text("第二段结果"),
                },
            ]),
        )
        .unwrap();
        assert_eq!(
            db.latest_user_message(&s).unwrap().as_deref(),
            Some("读一下 a.txt"),
            "含 ToolResult 的行不是真人发言，哪怕它自己带着一段话"
        );
    }

    /// 纯图片行（切片 2 的入口）没有 Text 块，`text_of()` 是空串，也不许顶掉有字的那句。
    #[test]
    fn latest_user_message_ignores_rows_without_any_text() {
        let (_d, db) = tmpdb();
        let s = db.create_session("新对话").unwrap();
        db.append_message(&s, &user_text("先说一句")).unwrap();
        db.append_message(
            &s,
            &Message::user(vec![ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Png,
            }]),
        )
        .unwrap();
        assert_eq!(
            db.latest_user_message(&s).unwrap().as_deref(),
            Some("先说一句")
        );
    }

    /// 中文一个字 3 字节：按字节下标切会在非边界 panic，所以截断走 `chars()`。
    #[test]
    fn a_long_title_is_truncated_by_chars_not_bytes() {
        let (_d, db) = tmpdb();
        let s = db.create_session("新对话").unwrap();
        db.append_message(&s, &user_text(&"一".repeat(50))).unwrap();
        let title = db.latest_user_message(&s).unwrap().unwrap();
        assert_eq!(title.chars().count(), 41, "40 个字符 + 一个省略号");
        assert!(title.ends_with('…'), "截断必须留标记：{title}");
    }

    // ---------- 重放映射 ----------

    #[test]
    fn events_from_messages_produce_replayable_projection() {
        let msgs = vec![
            user_text("查一下"),
            assistant(vec![
                text_block("好的"),
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: json!({}),
                },
            ]),
            Message::tool("c1", ToolOutput::text("结果"), false),
            assistant(vec![text_block("拿到结果")]),
        ];
        let kinds = tags(&events_from_messages(&msgs, &read_of));
        // 重放必须能重建：一句用户话 + 两段助手文本 + 一张工具卡片
        assert_eq!(
            kinds.iter().filter(|k| **k == "user_message").count(),
            1,
            "R68：用户的话落成助手气泡了 —— {kinds:?}"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == "text_delta").count(),
            2,
            "text_delta 只留给助手：{kinds:?}"
        );
        assert_eq!(kinds.iter().filter(|k| **k == "tool_call").count(), 1);
        assert_eq!(kinds.iter().filter(|k| **k == "tool_result").count(), 1);
        assert_eq!(*kinds.last().unwrap(), "done");
    }

    /// R68：`AgentEvent` 原先没有「用户消息」这一支，live 路径靠前端乐观插入
    /// （plan:4335），重开就会把用户的话并进助手气泡。spec:421 要求重放流能表达
    /// 「这句话是谁说的」，所以新增 `user_message`，并把块原样带上（重开要能看到缩略图）。
    #[test]
    fn a_user_line_replays_as_user_message_carrying_its_blocks() {
        let msgs = vec![Message::user(vec![
            text_block("看这张图"),
            ContentBlock::Image {
                data: "AAAA".into(),
                mime: ImageMime::Png,
            },
        ])];
        let evts = events_from_messages(&msgs, &read_of);
        assert_eq!(evts.len(), 2, "一条 user_message + 一条 done，不多发");
        let v = &values(&evts)[0];
        assert_eq!(v["t"], "user_message");
        assert_eq!(v["blocks"][0]["text"], "看这张图");
        assert_eq!(v["blocks"][1]["type"], "image");
        assert_eq!(v["blocks"][1]["data"], "AAAA");
        assert_eq!(values(&evts)[1]["t"], "done");
    }

    /// R69：映射表按**块种类**分支，不按 role —— `Message::tool` 的 role 是 User，
    /// 按 role 走会把工具结果当用户发言重放成一条气泡。
    #[test]
    fn a_tool_result_row_never_replays_as_a_user_message() {
        let evts = events_from_messages(
            &[Message::tool("c1", ToolOutput::text("27°C"), false)],
            &read_of,
        );
        assert_eq!(tags(&evts).first().unwrap(), "tool_result");
        assert_eq!(
            tags(&evts).iter().filter(|k| **k == "user_message").count(),
            0,
            "工具结果被重放成了用户气泡：{:?}",
            tags(&evts)
        );
    }

    /// 上面那条「工具行只发结果事件」的规则得有哨兵：行里混进 Text 块时这一臂会把用户的话
    /// 静默丢掉。`debug_assert!` 在 dev/测试构建里响一声（release 把它编译掉了 = 维持静默丢弃，
    /// 这是刻意的取舍 —— 见映射函数上那段注释），这条 `should_panic` 就是那声响的证明。
    #[test]
    #[should_panic(expected = "工具结果行混进了非 ToolResult 块")]
    fn a_mixed_tool_row_trips_the_debug_assert_instead_of_losing_text() {
        let mixed = Message {
            role: Role::User,
            blocks: vec![
                text_block("别把我的话吞了"),
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    is_error: false,
                    output: ToolOutput::text("ok"),
                },
            ],
        };
        events_from_messages(&[mixed], &read_of);
    }

    /// R69：System 行跳过。系统提示词与 Plan C 的压缩摘要都不该变成聊天气泡。
    #[test]
    fn system_rows_never_become_chat_bubbles() {
        let msgs = vec![
            Message::system_note("你是一个助手"),
            user_text("hi"),
            Message::system_note("（已压缩早期对话）"),
        ];
        assert_eq!(
            tags(&events_from_messages(&msgs, &read_of)),
            ["user_message", "done"]
        );
    }

    /// 轮号复用直播路径的规则：`session.rs:119` 的 `for turn in 1..=max_turns` ——
    /// 一次发送里第一条助手行是 `turn: 1`，每多一个工具轮加 1，下一次发送又从 1 起。
    /// 重放若自己往后连号，前端 reducer 的「本轮」索引就会和直播错位。
    #[test]
    fn assistant_turn_numbers_restart_at_one_on_every_user_line() {
        let msgs = vec![
            // 第一次发送：一个工具轮 + 一句收尾文本 = 直播里的 turn 1、turn 2
            user_text("一"),
            assistant(vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: json!({}),
            }]),
            Message::tool("c1", ToolOutput::text("ok"), false),
            assistant(vec![text_block("答一")]),
            // 第二次发送：号码从头再来
            user_text("二"),
            assistant(vec![text_block("答二")]),
        ];
        let turns: Vec<u32> = events_from_messages(&msgs, &read_of)
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TurnStart { turn } => Some(*turn),
                _ => None,
            })
            .collect();
        assert_eq!(
            turns,
            [1, 2, 1],
            "轮号要从 1 起、随助手行递增、遇真人发言重置"
        );
    }

    /// 上一条靠真人发言把初值盖掉了，所以计数器起点没人验。历史不以真人发言开头
    /// （第一行就是助手 —— Plan A 现在不产这种形状，但重放层不该为它特判）时，
    /// 第一条助手行仍然要是 `turn: 1`，不能露出 off-by-one。
    #[test]
    fn the_first_assistant_row_is_turn_one_even_without_a_leading_user_line() {
        let turns: Vec<u32> = events_from_messages(&[assistant(vec![text_block("答")])], &read_of)
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TurnStart { turn } => Some(*turn),
                _ => None,
            })
            .collect();
        assert_eq!(turns, [1], "初值必须是 1，和直播的第一轮同一个号");
    }

    /// R70：风险徽章是安全语义，硬编码 `Risk::Read` 会让 Plan B 的 `write_file`
    /// 重放后显示「自动允许」—— 界面替用户撒了谎。risk 只能跟着调用方的查表走。
    #[test]
    fn replayed_risk_follows_the_lookup_not_a_hardcoded_default() {
        let msgs = vec![assistant(vec![ContentBlock::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: json!({"path": "a.txt"}),
        }])];
        let read = tool_call_value(&events_from_messages(&msgs, &read_of));
        let write = tool_call_value(&events_from_messages(&msgs, &write_of));
        assert_eq!(read["risk"], "read");
        assert_eq!(write["risk"], "write", "risk 没跟着查表走，被写死了");
        assert_eq!(
            write["call"]["risk"], write["risk"],
            "顶层 risk 与 call.risk 分叉：请用 `AgentEvent::tool_call` 构造"
        );
    }

    /// R77：预览文本只许有一份实现（`ToolOutput::preview()`）。直播那条
    /// `session.rs:1582 a_long_tool_output_reaches_the_ui_as_a_bounded_preview` 断言的是
    /// 「200 字符 + 省略号」，重放这边必须逐字相同，否则 spec:421 消灭掉的
    /// 「两套渲染路径」在库里复活。
    ///
    /// 下面那条 zip 证明的是「重放发出的 preview == 同一个 ToolOutput 自己算的 preview」，
    /// 也就是**中间没人再切一遍**；它**不**证明直播路径也调的是这个函数 ——
    /// 那一半靠 `session.rs:250` 只此一处调 `output.preview()`，两处截断形状若分叉，
    /// 红的是 1582 那条，不是这条。
    #[test]
    fn replayed_preview_is_the_same_string_the_live_path_emits() {
        let long = "字".repeat(250);
        let outs = [
            ToolOutput::text(long.clone()),
            ToolOutput::Image {
                data: "AAAA".into(),
                mime: ImageMime::Png,
            },
        ];
        let msgs = vec![
            Message::tool("c1", outs[0].clone(), false),
            Message::tool("c2", outs[1].clone(), false),
        ];
        let previews: Vec<String> = events_from_messages(&msgs, &read_of)
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { preview, .. } => Some(preview.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(previews.len(), 2);
        for (got, o) in previews.iter().zip(&outs) {
            assert_eq!(got, &o.preview(), "重放预览与 ToolOutput::preview() 分叉了");
        }
        assert_eq!(
            previews[0].chars().count(),
            201,
            "200 字符 + 省略号，与直播同形"
        );
        assert!(previews[0].ends_with('…'));
        assert_eq!(
            previews[1], "[图片 png]",
            "mime 要走小写线上名，不是 Debug 的 `Png`"
        );
    }

    /// T10 交接③：库里失败的形状是 `ToolResult{is_error:true}` + 文本，读回不许降级成成功。
    /// `ms: 0` 是「重放不还原耗时」，交 T14 别把它显示成「0 ms」（R69）。
    #[test]
    fn a_failed_tool_result_does_not_replay_as_success() {
        let evts = events_from_messages(
            &[Message::tool(
                "c1",
                ToolOutput::text("工具执行失败: 网络不通"),
                true,
            )],
            &read_of,
        );
        match &evts[0] {
            AgentEvent::ToolResult {
                call_id,
                is_error,
                ms,
                preview,
                images: _,
            } => {
                assert_eq!(call_id, "c1");
                assert!(is_error, "失败被降级成成功");
                assert_eq!(preview, "工具执行失败: 网络不通");
                assert_eq!(*ms, 0);
            }
            other => panic!("工具结果行必须重放成 tool_result，拿到的是 {other:?}"),
        }
    }

    #[test]
    fn a_tool_result_with_images_replays_the_images_into_the_event() {
        let evts = events_from_messages(
            &[Message::tool(
                "c1",
                ToolOutput::Multi {
                    parts: vec![
                        ToolOutput::text("截图如下"),
                        ToolOutput::Image {
                            data: "QUJD".into(),
                            mime: ImageMime::Png,
                        },
                    ],
                },
                false,
            )],
            &read_of,
        );
        match &evts[0] {
            AgentEvent::ToolResult {
                images, preview, ..
            } => {
                assert_eq!(images.len(), 1, "重放必须把图片带回界面");
                assert_eq!(images[0].data, "QUJD");
                assert_eq!(images[0].mime, ImageMime::Png);
                assert!(preview.contains("[图片 png]"), "{preview}");
            }
            other => panic!("工具结果行必须重放成 tool_result，拿到的是 {other:?}"),
        }
    }

    #[test]
    fn replay_skips_the_synthetic_tool_image_followup() {
        let evts = events_from_messages(
            &[
                user_text("看图"),
                Message::tool(
                    "c1",
                    ToolOutput::Image {
                        data: "QUJD".into(),
                        mime: ImageMime::Png,
                    },
                    false,
                ),
                Message::user(vec![
                    ContentBlock::Text {
                        text: crate::agent::types::TOOL_IMAGE_FOLLOWUP_NOTE.into(),
                    },
                    ContentBlock::Image {
                        data: "QUJD".into(),
                        mime: ImageMime::Png,
                    },
                ]),
            ],
            &read_of,
        );
        let user_events: Vec<_> = evts
            .iter()
            .filter(|e| matches!(e, AgentEvent::UserMessage { .. }))
            .collect();
        assert_eq!(
            user_events.len(),
            1,
            "只有真实用户那句可以重放成 user_message"
        );
        assert!(
            evts.iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { images, .. } if images.len() == 1)),
            "工具结果的图片仍要从 tool_result 事件交给卡片"
        );
    }

    #[test]
    fn latest_user_message_skips_the_synthetic_tool_image_followup() {
        let (_d, db) = tmpdb();
        let s = db.create_session("新对话").unwrap();
        db.append_message(&s, &user_text("看图")).unwrap();
        db.append_message(
            &s,
            &Message::user(vec![
                ContentBlock::Text {
                    text: crate::agent::types::TOOL_IMAGE_FOLLOWUP_NOTE.into(),
                },
                ContentBlock::Image {
                    data: "QUJD".into(),
                    mime: ImageMime::Png,
                },
            ]),
        )
        .unwrap();
        assert_eq!(
            db.latest_user_message(&s).unwrap().as_deref(),
            Some("看图"),
            "自动标题不能采用模型专用的图片说明"
        );
    }

    // ---------- 解码失败必须报错（R71） ----------

    #[test]
    fn a_corrupt_blocks_json_row_errors_instead_of_a_quiet_empty_message() {
        let (_d, db) = tmpdb();
        let s = db.create_session("s").unwrap();
        db.append_message(&s, &user_text("原话")).unwrap();
        let changed = {
            let c = db.lock();
            c.execute(
                "UPDATE messages SET blocks_json=?1",
                params!["{\"type\":\"text\""],
            )
            .unwrap()
        };
        assert_eq!(changed, 1, "前置：确实改坏了那一行");
        assert!(
            db.messages(&s).is_err(),
            "坏 JSON 必须报错，不许静默变成一条空消息"
        );
        assert!(
            db.latest_user_message(&s).is_err(),
            "同一条读路径，标题也不能兜底"
        );
    }

    #[test]
    fn an_unknown_role_value_errors_instead_of_becoming_user() {
        let (_d, db) = tmpdb();
        let s = db.create_session("s").unwrap();
        db.append_message(&s, &user_text("原话")).unwrap();
        {
            let c = db.lock();
            c.execute("UPDATE messages SET role=?1", params!["tool"])
                .unwrap();
        }
        assert!(
            db.messages(&s).is_err(),
            "兜底成 User 会把库里的一行损坏伪装成「用户说过话」"
        );
    }

    // ---------- Db 的可托管性与锁语义（R76） ----------

    // ---------- Task 41：`compact_session`（一个事务里删一段 + 落一条备注）----------

    // `CompactionPlan` 由文件顶部的 `use` 带进来（生产签名要用它），这里不重复 import。
    use crate::agent::context::{ContextBuilder, ContextConfig, SYSTEM_PROMPT};

    /// 种 `rounds` 轮真实形状的历史：一条 user + 一条 assistant，文本带轮号。
    /// 行序就是 `messages()` 读回来的顺序（`ORDER BY id`），所以下面的区间断言可以直接对撞。
    fn seed_rounds(db: &Db, s: &str, rounds: usize) -> Vec<Message> {
        let mut all = Vec::new();
        for k in 0..rounds {
            all.push(user_text(&format!("轮 {k}")));
            all.push(assistant(vec![text_block(&format!("答 {k}"))]));
        }
        for m in &all {
            db.append_message(s, m).unwrap();
        }
        all
    }

    /// 本层只吃 `drop_from`/`drop_to` 这两个下标（`source` 是调用方送去摘要的那些消息，
    /// 落库用不上）。手搭而不是调 `compaction_plan`：这里测的是「按下标删行 + 落备注」，
    /// 决策那半归 `context.rs` 的 `mod compaction_planning`。
    fn range(from: usize, to: usize) -> CompactionPlan {
        CompactionPlan {
            drop_from: from,
            drop_to: to,
            dropped: to - from,
            source: vec![],
        }
    }

    /// 备注文本的期望值（前缀由 `summary_note` 负责，这里直接写落库后的样子）。
    const NOTE_A: &str = "早期对话摘要：轮 0 到轮 3 谈的是天气";
    const NOTE_B: &str = "早期对话摘要：又压了一段";

    #[test]
    fn compact_session_removes_the_planned_rows_and_appends_exactly_one_note() {
        let (_d, db) = tmpdb();
        let s = db.create_session("压缩前").unwrap();
        let all = seed_rounds(&db, &s, 20); // 40 行

        db.compact_session(&s, &range(0, 8), NOTE_A).unwrap();
        let after = db.messages(&s).unwrap();

        // 删 8 行、落 1 行 ⇒ 33。写两条备注就是「同一段历史被摘要了两遍」。
        assert_eq!(after.len(), 40 - 8 + 1, "实际留下 {} 行", after.len());
        let notes: Vec<&Message> = after.iter().filter(|m| m.role == Role::System).collect();
        assert_eq!(notes.len(), 1, "每次压缩恰好一条备注");
        assert_eq!(notes[0].text_of(), NOTE_A, "备注正文原样进库");

        // 被删的必须是**区间内那 8 行**，其余原样、原序（`ORDER BY id` 就是写入序）。
        let conv: Vec<&Message> = after.iter().filter(|m| m.role != Role::System).collect();
        assert_eq!(conv, all[8..].iter().collect::<Vec<_>>().as_slice());

        // 前置事实 3：新插入的备注拿到**最大 id**、读回来排在**最后**。
        // 「插在最前」物理上做不到（`id INTEGER PRIMARY KEY AUTOINCREMENT`），
        // 而它没必要 —— 下面那条 `build()` 断言才是这条不变量的真身。
        assert_eq!(
            after.last().unwrap().role,
            Role::System,
            "备注行的物理位置在尾部：这条断言钉的是事实，不是愿望"
        );

        // 但模型看到的顺序仍然对：`build()` 把 System 行整行摘出来当 notes、拼在 head 之后。
        let built = ContextBuilder::new(ContextConfig::default()).build(&after);
        assert_eq!(built.messages[0].text_of(), SYSTEM_PROMPT);
        assert_eq!(built.messages[1].text_of(), NOTE_A);
        assert_eq!(
            built.messages.last().unwrap().text_of(),
            all.last().unwrap().text_of(),
            "尾部那行必须是历史里最后一条，备注不许插进中间把它挤掉"
        );
        assert!(
            built
                .messages
                .iter()
                .filter(|m| m.text_of() == NOTE_A)
                .count()
                == 1,
            "双重前缀 / 备注进了一次以上：`build()` 原样带上 notes，那里不该再拼前缀"
        );
    }

    #[test]
    fn the_second_compaction_does_not_count_the_note_as_a_conversation_row() {
        // 下标口径的唯一根据是「role<>'system' 之后的序列」，与 `messages()` 里
        // 非 system 行的相对顺序一致。把 `SELECT id` 那条查询的 role 过滤摘掉，
        // 第二条备注就会被当成「第 33 条对话消息」参与删除 —— 那是要吃掉用户原话的。
        let (_d, db) = tmpdb();
        let s = db.create_session("两次压缩").unwrap();
        let all = seed_rounds(&db, &s, 20); // 40 行

        db.compact_session(&s, &range(0, 8), NOTE_A).unwrap();
        db.compact_session(&s, &range(2, 10), NOTE_B).unwrap();
        let after = db.messages(&s).unwrap();

        let notes: Vec<String> = after
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.text_of())
            .collect();
        assert_eq!(
            notes,
            [NOTE_A, NOTE_B],
            "两次压缩各留一条备注，且第一条不许被第二次当成对话消息删掉"
        );
        let conv: Vec<&Message> = after.iter().filter(|m| m.role != Role::System).collect();
        // 第一次留下 all[8..40]；第二次吃掉其中下标 2..10 ⇒ 剩 all[8..10] + all[18..40]。
        let mut want: Vec<&Message> = all[8..10].iter().collect();
        want.extend(all[18..40].iter());
        assert_eq!(conv, want, "第二次的区间必须相对**当下**的对话序列数");
        // 40 - 8 + 1 - 8 + 1 = 26：两次各删 8 行、各落 1 条备注。
        assert_eq!(after.len(), 24 + 2, "留下 24 条对话 + 2 条备注");

        // 这条测试真正的牙在这里：越界判据数的必须是**对话行数**，不是**总行数**。
        // 此刻库里 24 条对话 + 2 条备注 = 26 行。把 `conversation_ids` 里 `role<>?2` 那道
        // 过滤摘掉，两条备注就成了「第 25、26 条对话消息」，下面这个区间于是合法，
        // 会把用户仅剩的原话连同两条摘要一起吃掉 —— 而它现在是**越界拒绝**。
        let err = db
            .compact_session(&s, &range(0, want.len() + 1), NOTE_B)
            .expect_err("区间越过对话行数就该拒绝，即使总行数够（备注不是对话消息）");
        assert!(
            err.to_string().contains("越界") || err.to_string().contains("超出"),
            "错误文本要说得出是哪件事：{err}"
        );
        assert_eq!(
            db.messages(&s).unwrap(),
            after,
            "拒绝之后库里一个字都没动，两条备注都还在"
        );
    }

    #[test]
    fn a_plan_beyond_the_stored_rows_is_refused_and_writes_nothing() {
        // `run_turn` 拿的是刚读出来的历史，正常不会越界；越界意味着 plan 与库已经分叉
        // （单轮并发闸门之外的写入、或将来有人把这两步拆开）。
        // 这里要的是**报错**，不是 slice panic —— panic 会把 `Mutex` 打成中毒态，
        // 症状是「此后每条命令都打不开」（`lock()` 的文档注释就是为这一刻写的）。
        let (_d, db) = tmpdb();
        let s = db.create_session("越界").unwrap();
        let all = seed_rounds(&db, &s, 4); // 8 行
        let err = db
            .compact_session(&s, &range(0, 99), NOTE_A)
            .expect_err("越界的区间必须报错，不许静默少删");
        assert!(
            err.to_string().contains("越界") || err.to_string().contains("超出"),
            "错误文本要说得出是哪件事：{err}"
        );
        assert_eq!(db.messages(&s).unwrap(), all, "拒绝之后库里一个字都没动");
    }

    /// 这条就是「一个事务」四个字的牙齿。把删除与插入拆成两次非事务调用（计划 Step 8
    /// 判据 (c) 的变异），这里会留下「原文已删、备注没落」的半条状态 —— 用户的历史
    /// 凭空少 8 行而界面上没有任何摘要，而且下一轮连重压的机会都没有（备注才是线索）。
    /// 夹具用触发器让**事务里的第二条语句**失败：取消测试测不到这一侧，因为取消发生在
    /// 事务开始之前，拆不拆事务它都照样绿。
    #[test]
    fn a_failed_note_insert_rolls_the_deletes_back_too() {
        let (_d, db) = tmpdb();
        let s = db.create_session("半条事务").unwrap();
        let all = seed_rounds(&db, &s, 10); // 20 行
        {
            let c = db.lock();
            c.execute_batch(
                "CREATE TRIGGER no_message_insert BEFORE INSERT ON messages \
                 BEGIN SELECT RAISE(ABORT, '夹具：这一条插入必须失败'); END",
            )
            .unwrap();
        }
        db.compact_session(&s, &range(0, 8), NOTE_A)
            .expect_err("插入被触发器挡下，compact_session 必须报错");
        assert_eq!(
            db.messages(&s).unwrap(),
            all,
            "备注没落地 ⇒ 同一事务里的删除必须一起回滚"
        );
    }

    // ---------- Db 的可托管性与锁语义（R76） ----------

    #[test]
    fn a_poisoned_lock_still_serves_the_next_call() {
        let (_d, db) = tmpdb();
        let s = db.create_session("s").unwrap();
        db.append_message(&s, &user_text("x")).unwrap();
        // 故意让一个线程持锁 panic。panic hook 是进程级全局的，这里临时静音，
        // 否则测试输出里会喷一条我们自己造的假故障。
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let panicked = std::thread::scope(|sc| {
            let h = sc.spawn(|| {
                let _g = db.conn.lock().unwrap();
                panic!("持锁期间 panic，故意把锁弄中毒");
            });
            h.join().is_err()
        });
        std::panic::set_hook(previous);
        assert!(panicked, "前置：线程确实 panic 了，否则锁根本没中毒");
        let poisoned = db.conn.lock();
        assert!(poisoned.is_err(), "前置：锁确实处于中毒状态");
        drop(poisoned);
        assert_eq!(db.messages(&s).unwrap().len(), 1, "中毒之后照样读得出来");
    }
}
