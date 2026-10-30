# LFTTDO 长期记忆系统设计(吸取各开源项目优点)

> 状态:设计稿,未实现。日期:2026-10-25。
> 前置阅读:`2026-10-19-qwen-do-agent-design.md` §5.3(上下文管理)。
> 本设计不推翻任何现有不变量:单文件 SQLite 唯一真相源、`Role::System` 备注注入路径、
> token 预算计价、无 sidecar 进程、失败降级必须可见。

## 0. 背景与目标

现状:LFTTDO 只有单会话工作记忆(滑窗 + 滚动摘要),会话结束即遗忘,唯一持久的是
用户手填的系统提示词。压缩(`compact_before_turn`)目前是**纯损失**:被压掉的原文
永久删除,只留一条 ≤400 字摘要。

目标:跨会话记住用户的事实与偏好,会话开始时能带回。**全部机制本地化**,不引入
Python 运行时或第二个存储引擎。

## 1. 各项目优点 → 本设计的映射

| 来源项目 | 吸取的优点 | 在本设计中落成 |
|---|---|---|
| mem0 | 两阶段写入:抽取事实 → 与库比对后增/改/删;记忆是**原子事实**不是对话日志 | 写入管线(§4) |
| Memobase | 用户画像是独立记忆类型:**常驻注入**,不靠检索 | profile 单行常驻(§5.1) |
| Letta (MemGPT) | agent 自管记忆:core(常驻)/archival(检索)分层 + 自编辑工具 | 记忆工具走现有 ToolRegistry + 权限闸(§6) |
| Graphiti (Zep) | 时序有效性:**失效不删除**;实体-关系三元组支撑多跳 | validity 列 + 简化三元组(§3),SQLite 递归 CTE 做多跳 |
| cognee | 摄取是显式分阶段的管道,每段可观测可重跑 | 抽取/合并/嵌入三段各有状态与日志(§4.4) |
| sqlite-vec + fastembed-rs | 本地向量检索不离开 SQLite 文件;本地 embedding 可离线 | vec0 虚表 + FTS5 混合召回;embedding 二源可配(§3.2) |

**明确不吸收的**(防过度设计,记录在案):
- Graphiti 的完整双时间轴(bi-temporal)与外置图数据库 → 只取单 validity 区间 + 表内三元组;
- Letta 的平台化(agent 文件系统、sleep-time compute)→ 只取「工具自编辑」这一个思想;
- cognee 的管道框架化(队列/worker)→ 三段函数即可,量级撑不起框架。

## 2. 总体架构

```
写入侧(压缩时 + 会话收尾):
  待压段落 ──抽取(mem0 式提示词)──▶ 候选事实
  候选事实 ──与库内记忆比对──▶ ADD / UPDATE / INVALIDATE(Graphiti 式失效)
  落库事实 ──embedding──▶ vec0 虚表          ┐ 同一个 SQLite 文件
  画像变化 ──合并──▶ profile 单行            ┘

读取侧(每次发送前, run_turn 装配期):
  profile(常驻, Memobase/Letta core) ──▶ 「用户画像:…」 system note ──┐
  当前消息 top-k 混合召回(sqlite-vec + FTS5) ──▶ 「长期记忆:…」 note ──┼─▶ head
  agent 主动 memory.search(可选工具, Letta archival) ──▶ 工具结果 ────┘
```

## 3. 数据模型(同一 SQLite 文件,新增三张表 + 一张虚表)

```sql
CREATE TABLE IF NOT EXISTS memories(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT,              -- 来源会话,可空(画像合并出的记忆无单一来源)
  kind TEXT NOT NULL,           -- 'fact' | 'preference' | 'entity_rel'
  subject TEXT,                 -- 三元组:主体(如 '用户')
  predicate TEXT,               -- 三元组:谓词(如 '住在')
  object TEXT,                  -- 三元组:客体(如 '上海')
  body TEXT NOT NULL,           -- 人读的完整句子(注入用这个,不是拼接三元组)
  valid_from INTEGER NOT NULL,  -- ms,创建/最近确认时间
  invalid_at INTEGER,           -- NULL=有效;非空=已失效(Graphiti:失效不删除)
  confidence REAL DEFAULT 1.0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
-- 生效记忆的向量影子表(sqlite-vec,与 memories 同库):
CREATE VIRTUAL TABLE IF NOT EXISTS memories_vec USING vec0(
  memory_id INTEGER PRIMARY KEY,
  embedding FLOAT[d]            -- d 由首次嵌入时的模型维度定死,写进 user_version 迁移
);
-- 常驻画像:永远只有一行(Memobase/Letta core memory 的极简形态)
CREATE TABLE IF NOT EXISTS memory_profile(
  id INTEGER PRIMARY KEY CHECK (id = 1),
  body TEXT NOT NULL,           -- 受 PROFILE_CHAR_CAP 限制
  updated_at INTEGER NOT NULL
);
```

要点:
- **失效不删除**:「用户住北京→上海」= 旧行 `invalid_at=now`,新行 INSERT。历史可追溯,
  检索只查 `invalid_at IS NULL`。
- `body` 与三元组并存:注入用 `body`(模型读自然语言),多跳用三元组(SQLite 递归
  CTE:`WITH RECURSIVE ... WHERE invalid_at IS NULL`)。抽取提示词允许三元组缺省
  (`kind='fact'` 时不强填)。
- messages 表零改动;重放投影(events_from_messages)零改动——记忆不进会话事件流。

### 3.1 embedding 来源(二源一开关)

| 模式 | 实现 | 取舍 |
|---|---|---|
| `endpoint`(默认) | 现有 OpenAI 兼容端点的 `/v1/embeddings` | 零新依赖;离线不可用;隐私边界与聊天一致(同一家端点本来就看得到原文) |
| `local` | fastembed-rs(ONNX,如 bge-small-zh) | 完全离线;二进制 +模型体积;首次冷启动下载 |

设置项 `memory.embedding_source: endpoint | local | off`。`off` 时写入侧只做画像与
三元组,不建向量(检索退化为 FTS5 关键词)——**记忆系统在无 embedding 模型时仍可用**,
不许整体罢工。

### 3.2 混合召回

top-k = sqlite-vec KNN(语义)与 FTS5(关键词)各取 k,合并去重按
`0.6*语义分 + 0.4*关键词分` 排序,硬上限 `MEMORY_NOTE_TOKEN_CAP`(默认 600 token,
进 `ContextConfig` 的预算口径)。FTS5 是 rusqlite 自带,不新增任何依赖。

## 4. 写入管线(cognee 式三段,每段独立可失败可观测)

### 4.1 触发点:压缩钩子(本设计与现有系统的最大协同)

`compact_before_turn`(ipc.rs)在删原文**之前**已经拿到了 `plan.source`——那段
即将永久消失的对话。抽取就在这里做:

```
compaction_plan 返回 plan
  → extract(plan.source)            // 段①:事实候选
  → consolidate(candidates, 库内有效记忆)  // 段②:ADD/UPDATE/INVALIDATE 决策
  → 一个事务:写 memories(+向量) + compact_session 原有删行/摘要
```

抽取失败**不阻塞压缩**(压缩的既有语义不变),只记日志;压缩照常,代价是该段
没进长期记忆——与现有「摘要失败降级占位」同一哲学:功能降级,不留半条状态。

会话收尾(用户关会话/切会话)做第二次轻量抽取,输入 = 尾部尚未被压缩过的近几轮,
防止「聊完就关、没触发压缩」的事实丢失。

### 4.2 段①抽取(mem0 式提示词,中文)

```
从下面的对话里抽取「值得跨会话记住的用户事实」。只抽取对话明确说过的:
- 身份与背景(职业、称呼、常打交道的人)
- 稳定偏好(格式、语言、工具习惯)
- 正在进行的事(项目名、截止时间、承诺)
不要抽:一次性闲聊、模型自己的推测、临时上下文才有意义的内容。
每条输出:{"kind":..., "subject":..., "predicate":..., "object":..., "body":..., "confidence":...}
没有可抽的就输出空数组。
```

空结果 = 不写,零信息不入库(对应现有「空摘要即 Err」的纪律)。

### 4.3 段②合并(增/改/失效)

对每条候选,检索库内**同 subject+predicate** 的有效记忆,把「新旧对照」交给模型
三选一:ADD(新事实)/ UPDATE(body 更新,updated_at=now)/ INVALIDATE(旧记忆
`invalid_at=now` + 视情况 ADD 新行)。低置信(confidence < 0.7)候选丢弃。
画像候选单独走 profile 合并:与现有 body 一起交给模型输出**仍是单行、仍受
PROFILE_CHAR_CAP(默认 500 字)** 的新画像——画像永不膨胀。

### 4.4 可观测性

三段各自 `tracing::info` 结果计数(候选 N 条 / 采纳 M / 失效 K),UI 不新增事件
(遵循 R69:记忆对用户透明由设置页承担,见 §7)。

## 5. 读取侧

### 5.1 常驻画像

每次 `build()` 的 head 在系统提示词之后拼:

```
[用户画像]
{memory_profile.body}
```

作为一条 `Role::System` 备注——**完全复用**现有 head 装配(context.rs:109 起)、
预算计价、R69 投影跳过。画像空则不拼。

### 5.2 检索注入

`run_turn` 读到用户消息后、`build()` 之前:取用户消息文本(多块取 Text 拼接),
混合召回 top-k,拼:

```
[长期记忆·按相关度]
- {body}({kind},记录于 yyyy-MM-dd)
```

同一条 system note。与画像合计受 `MEMORY_NOTE_TOKEN_CAP` 约束,超了从相关度最低
的砍起。**记忆 note 计入 est_input_tokens**——预算仪表照常工作,不会出现「以为
没超线实际超了」(这是现有系统最怕的方向,见 context.rs 的校准注释)。

### 5.3 自主检索(二期)

不自动全量塞入,给模型一个工具让它**按需深挖**(Letta archival 的思想):
`memory_search(query, k)` → 工具结果返回 body 列表。注册进现有 ToolRegistry,
风险级 = 只读(与 fs.read 同级),走现有权限闸。

## 6. 记忆工具(Letta 自编辑,二期)

| 工具 | 风险级 | 说明 |
|---|---|---|
| `memory_search` | 只读 | §5.3 |
| `memory_remember(body, kind?)` | 写(需闸) | 显式「帮我记住这个」,最常见的用户诉求 |
| `memory_forget(匹配文本)` | 写(需闸) | 置 invalid_at,不物理删 |

`memory_forget` 是隐私出口:用户说「忘掉我说的卡号」,agent 能当场执行。
物理删除仍只在设置页(§7)。

## 7. 设置页(遵循 Settings::normalize 的钳制纪律)

| 设置 | 默认 | 钳制 |
|---|---|---|
| `memory.enabled` | true | — |
| `memory.embedding_source` | endpoint | endpoint/local/off |
| `memory.recall_k` | 5 | 1..=20 |
| `memory.note_token_cap` | 600 | 200..=2000,且 < budget_tokens 的 20% |
| `memory.profile_char_cap` | 500 | 100..=2000 |

设置页提供:**记忆总开关、清空全部记忆(物理删,需确认)、逐条查看/删除**(记忆
对用户必须可审计——Graphiti 的可解释性优点在 UI 侧的对应物)。

## 8. 不变量与测试清单(延续本项目「注释+钉死测试」的文化)

1. 记忆 note 与画像 note 计入 est:断言 `est_input_tokens` 包含它们(镜像
   `a_cut_request_reports_the_estimate...` 的判据)。
2. 抽取在删行**之前**:压缩事务失败回滚时,记忆写入一并回滚(同一事务);抽取
   失败时压缩照常、库无半条记忆(变异实验:把抽取挪到 compact_session 之后,
   `a_failed...` 式测试必须红)。
3. 画像恒单行且 ≤ cap:任何合并路径后断言。
4. `invalid_at IS NULL` 是检索唯一口径:失效记忆永不注入(含混合召回两侧)。
5. embedding 源 off/失败:检索退化为 FTS5,写入不建向量,不报错不阻塞。
6. 记忆写入不影响 messages 行数与事件投影(重放一致性回归)。
7. 空 body / 纯空白 body 的记忆永不落库。
8. `memory_forget` 后同 key 检索不返回该条。

## 9. 分期

- **一期(核心价值)**:memories + profile 表、压缩钩子抽取/合并、常驻画像 + 混合
  召回注入、设置页与清空入口。约等于 mem0 + Memobase 的最小并集。
- **二期(agent 能动性)**:memory_search / memory_remember / memory_forget 工具
  (Letta 优点)。依赖一期稳定运行。
- **三期(结构与时间)**:三元组抽取质量提升 + 递归 CTE 多跳查询 + validity 的
  UI 展示(Graphiti 优点)。只有当用户真的问出「A 是谁介绍的」这类问题时才值得。

## 10. 风险与取舍(如实记录)

- **每次压缩多两次模型调用**(抽取+合并)。缓解:抽取输入就是即将删除的段落,
  本来就要为摘要付一次调用;合并仅在有同键记忆时才发生。设置页可见调用计数。
- **画像与检索 note 挤占对话预算**:600 token cap + 计入 est,用户可调小或关。
- **embedding 端点兼容性**:部分 OpenAI 兼容端点无 /v1/embeddings → 探测失败自动
  降级 off(FTS5 only),设置页提示。
- **记忆错误固化**:错误事实会跨会话存在。缓解:confidence 阈值、设置页逐条删除、
  `memory_forget`。不做自动「记忆衰减」——量级太小,复杂度不值。
- **隐私**:endpoint 模式下记忆正文会发给 embedding 端点。聊天原文本来就发给同
  一端点,边界未扩大,但设置页须明示一句话。

## 11. 类人记忆升级(T8–T13,已实现)

初步实现之后,对着人类记忆的机制重新看了一遍(工作记忆/情景-语义记忆/巩固/遗忘曲线/
编码特异性/元记忆/前瞻记忆),发现原设计缺了五种机制。本节记录补上的部分、为什么补、
以及有意不做的部分。

### 11.1 补上的五个机制

| 人类机制 | 原设计的缺失 | 升级后的实现 |
|---|---|---|
| 强化(重复接触变强) | 同一事实说十遍与说一遍等价 | 合并四选一加 `REINFORCE`:`reinforcements += 1`,正文不动。排序乘 `1 + 0.15·ln(1+n)` |
| 遗忘曲线 | 记忆只增不减,全按相关度排 | 排序乘衰减因子 `floor + (1-floor)·2^(-age/半衰期)`,事实 60 天、偏好 180 天,下限 0.35 **永不归零** |
| 访问即再巩固 | 召回是纯读,「最近用过」无痕迹 | `search_memories` 返回即写 `last_accessed`;它同时是衰减的时间基准 |
| 前瞻记忆 | 接不住「下周三提醒我」 | `kind=prospective` + `due_at`(ISO 8601,自实现民用历法,零新依赖);到期扫描 → `[待办提醒]` 注入 → `notified_at` 防重复 |
| 编码特异性 | 只用当轮消息做 query,追问即失明 | query = 上一轮用户消息(截 200 字)+ 当前消息 |
| 元记忆 | agent 不知道自己有记忆库 | `memory_search` 只读工具(Risk::Read,零弹窗)+ head 里的 `[记忆库]` 概览 |

### 11.2 head 里的顺序(硬性)

```
system prompt → [待办提醒] → [用户画像] → [长期记忆·按相关度] → [记忆库] → 历史摘要备注 → 对话
```

越靠前越紧急/越新鲜。待办**不受** `MEMORY_NOTE_TOKEN_CAP` 约束(最多 5 条、独立限长):
漏掉一个到期承诺比多花几百 token 严重得多。概览排最后、最先被 cap 砍掉。

### 11.3 参数与理由

- **衰减下限 0.35**:遗忘是渐近的,不是删除。归零会让「共同回忆」类查询全军覆没。
- **强度用对数**:前几次确认涨得快、之后边际递减;权重 0.15 压在「相关性仍是第一位」之下。
- **偏好半衰期 = 事实 × 3**:偏好是自我描述、会一直成立;事实会变(搬家、换工作)。
  只开一个旋钮(`memory_decay_half_life_days`),两档比例固定,避免用户面对
  「偏好应该是事实的几倍」这种无法回答的问题。
- **待办不衰减**:它的价值与「多久以前记的」无关,只看 `due_at`。
- **时区按 UTC 解释**:不引 chrono,不猜用户时区;已知偏差 = 用户在东八区时提醒早 8 小时,
  在 24 小时提醒窗口下可接受。**这是明确的已知缺陷**,不是疏漏。
- **合并无冲突时零模型调用**:大多数候选是新事实,不必为 ADD 付一次调用。

### 11.4 有意不做的部分

- **自动记忆衰减到删除**:只降排序,不删行。删除永远是用户的动作。
- **情绪显著性**:人的情绪标记是生物硬件的生存机制;agent 没有对应需求,
  用 `confidence`(模型自评)近似,且明确它标的是「抽得准不准」而不是「用户多在意」。
- **再巩固的不稳定窗口**:提取后记忆进入可改写状态,是生物硬件更新记忆的代价;
  我们的记忆没有那个约束,模拟它只会带来不确定性。
- **间隔重复的主动复习**:没有「到点考你一次」的机制。强化只在事实被再次提及时发生。
- **程序性记忆**:LLM 的技能在权重里,不靠对话记忆。

### 11.5 与人类的根本差异(设计层面的诚实记录)

人的记忆是**重建**的:回忆时按图例补全,所以含糊的查询也常有效。本系统是**检索**的:
余弦 + 关键词,查不到就是查不到。我们的更忠实,但更脆。

人的巩固是**多轮、跨睡眠、按重要性分级**的。本系统是**一次性、容量驱动**的:
预算越线 → 把最老一段抽成一条 → 原文永久删除。没有第二遍,也没有按重要性的分级淘汰。

这两条差异不是缺陷清单上的待办,而是架构选择的代价:要在本地、单文件、零服务的约束下
运行,做得起的就是这些。

## 12. P0/P1:持续失血与用户信任(T14–T18,已实现)

§11 补的是「记忆像不像人」;这一节补的是**让功能真的被用起来**的两组问题。
优先级由业主拍板:P0 是持续失血(不做就一直在错),P1 决定用户信不信。

### 12.1 P0-A:记忆积累不再等压缩

**问题**:记忆只在压缩时写入,而压缩要等预算越过 85%。聊十几轮不触发压缩就是零记忆,
用户会觉得「这功能没在工作」。这是产品级缺陷,不是工程优化。

**解法**:抽取游标 + 两条新触发路径。

- **游标** `sessions.memory_extracted_upto`(最后抽取过的 `messages.id`)。
  没有它,两条路径会重复处理同一段对话:重复抽取不只烧调用,还会把同一件事反复
  ADD 成一堆措辞不同的近似重复记忆。
  **压缩路径也必须推进它**(压缩抽的就是被删那段),否则空闲抽取会把压缩删掉那段的后继再抽一遍。
- **空闲触发**:后台 worker 每 60 秒扫一次;`updated_at <= now - N 分钟`
  且游标落后于最大消息 id、且该会话不在跑 → 抽取。默认 5 分钟,**0 = 关闭**(合法值)。
- **会话切换**:`memory_flush_session`。所有失败返回 `Ok(0)` —— 切换会话不该因为
  记忆抽取失败而失败。
- **不做每 N 轮兜底**:本应用跑的是 API 端点,固定开销是真金白银;而「空闲 5 分钟」
  已经覆盖了「用户聊完停下来」这个真正需要记住的时刻。

**一次抽取的边界**:最多 40 条(与压缩段同量级),游标只推进到**实际送入模型**的最后一条;
抽取失败**不推进游标**(下个 tick 重试);空候选是**成功**(「这段没什么值得记」),
游标照推 —— 否则同一段会被每 tick 重抽。

### 12.2 P0-B:用户权威

**问题**:记忆是模型写的。用户看到一条错的,唯一动作是删掉等它重新抽 ——
面板只能看和删,不能改。

**解法**:`memories.user_edited` + 编辑权。

- 用户改正文 → `user_edited = 1`、`confidence = 1.0`(用户亲手确认的事不该被算法置信度打折)、
  `due_at` **保留**(改措辞不该把提醒弄丢)。
- 模型的下一次合并:**对 `user_edited` 行的 UPDATE / INVALIDATE 一律降级为 ADD**
  (候选并存,用户自己删)。REINFORCE 放行 —— 它不改正文,只加确认计数,
  「用户写的句子被再说一遍」是正确语义。
- 这是业主明确拍板的方向:**用户权威优先于系统自洽**。代价是库里可能出现
  两条互相矛盾的记忆(用户版 + 模型版),需要用户自己删;收益是模型永远不会
  悄悄改掉用户亲手写的东西。

### 12.3 P1-A:让「它在记」可见

新增事件 `AgentEvent::MemorySaved { count }`(第 12 个变体),压缩后用 sink 发出,
前端 toast「已记住 N 条」,点击打开设置面板的记忆区。

- `count == 0` **不发** —— 弹「已记住 0 条」只会让人以为坏了。
- 只发条数,记忆正文一个字都不进帧流(R69 的推广)。
- 空闲/切换路径的回补条数经命令返回值与同一条 toast 显示。

### 12.4 P1-B:手动新增

`memory_add(kind, body)` + 面板入口。手动新增的记忆**从出生起就是 `user_edited = 1`**
—— 用户写的东西不该等模型下一次合并来改写。kind 白名单 fact/preference/prospective,
未知回落 `fact`(用户手写的内容比模型的分类意图更可信)。

### 12.5 缺口收口(§12 首版的三条已全部关闭)

**已关闭:**

- **抽取路径互斥**(原缺口 1):`ExtractLocks` 按会话持有标记集 + RAII guard,
  压缩/空闲/切换三条路径统一认领。撞锁的处置是**放弃而不是排队** —— 排队会让
  用户切会话时等一次模型调用;而游标保证被放弃的那次不会丢内容(另一次正在处理
  同一批消息)。压缩段撞锁时跳过抽取、压缩照常、零额外模型调用。
- **手动待办带到期时间**(原缺口 2):`memory_add(kind, body, due_at)` + 面板在
  `prospective` 时渲染 `datetime-local`。**时区仍按 UTC 解释字面值**(与 T9 抽取侧
  同一口径):前端用 `Date.UTC(...)` 而不是 `new Date(str)`。代价是东八区用户输入
  「09:00」实际在本地 17:00 提醒 —— 与全局已知偏差一致,局部修正反而会让同一个
  时间在两处显示不同。
- **空闲 tick 成本**(原缺口 3):有了量级承诺 —— 100 会话 × 100 消息的库上,
  一次无待抽会话的 tick 必须 < 60ms 且不构造模型客户端(测试钉住,实测在亚毫秒量级)。

**仍未验证(需要真机/真人,不是代码问题):**

- **toast 的 8 秒时长与真实用户节奏**;真窗口的视觉排版、`scrollIntoView` 落点。
- **`datetime-local` 在各平台的显示差异**(WebView2 vs WebKit 的日期顺序)。
- **Tauri v2 的 `dueAt → due_at` 运行时参数映射**只按仓库既有惯例推断,`cargo test`
  覆盖不到真实 IPC 边界;不成立时的症状是 `due_at` 恒为 None(不崩)。
- **真实模型的抽取质量与 ISO 时间产出稳定性**(所有解析路径都由固定字符串覆盖)。
- **空闲 worker 的真循环**(60 秒 sleep + 退出语义)只测了它的 tick 函数与接线守卫。
