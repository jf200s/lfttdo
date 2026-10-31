import { useCallback, useEffect, useRef, useState, type ChangeEvent } from "react";
import {
  MAX_PERSONA_CHARS,
  downloadPersonaText,
  normalizePersonaText,
} from "../persona";
import { THEME_NAMES, type Mode } from "../theme/theme";
import type { MemoryView, Settings, SettingsView } from "../types";

interface SettingsPanelProps {
  initial: SettingsView;
  onSave: (s: Settings) => Promise<void>;
  onSaveKey: (k: string) => Promise<void>;
  onClearKey: () => Promise<void>;
  onClose: () => void;
  /**
   * 长期记忆子区（设计文档 §7：记忆对用户必须可审计）的三个回调，App 装配处接
   * `commands.ts` 的三个命令。**三个都不 catch**：失败原文由面板进自己的 error 通道
   * （同 `onSave`/`onClearKey` 的口径），列表状态只活在面板挂载期间，App 不持有第二份。
   */
  onLoadMemories: () => Promise<MemoryView[]>;
  onDeleteMemory: (id: number) => Promise<void>;
  onClearMemories: () => Promise<void>;
  /**
   * T12：常驻画像的读取入口（命令 `memory_profile` 已存在）。与上面三个同一条口径：
   * 不 catch、失败进面板的 error 通道；空串 = 后端从未写过画像，面板不渲染那一行。
   */
  onLoadProfile: () => Promise<string>;
  /**
   * P1：用户权威的两个写入口（spec §11.4 的取向：删除永远不是用户的唯一出口）。
   * - `onUpdateMemory` 走 `memory_update_body`：后端打 `user_edited = 1 + confidence = 1.0`，
   *   模型此后的合并只会降级成「并存」，不再改写用户亲手写下的句子。
   * - `onAddMemory` 走 `memory_add`，返回新行 id；`kind` 的白名单与回落都在 Rust。
   *   第三参 `dueAt` 是**毫秒时间戳**（`null` = 待办但不提醒）：只有选择「待办」时
   *   面板才渲染提醒时间输入框，且字面值按 UTC 换算（见 `utcMsFromDatetimeLocal`）。
   *
   * 两个都**不 catch**：失败原文进面板自己的 error 通道（同 `onSave`/`onDeleteMemory`）。
   * 成功后的列表真值一律来自**重拉**，不在前端 splice / push —— 那是第二份真相。
   */
  onUpdateMemory: (id: number, body: string) => Promise<void>;
  onAddMemory: (kind: string, body: string, dueAt: number | null) => Promise<number>;
  onPickFolder: () => Promise<string | null>;
  /**
   * P1：轻提示点击「已记住 N 条」时由 App 置真：面板打开后把长期记忆那一组滚进视野。
   * 为什么不在 App 里 `scrollIntoView`：那一刻面板还没挂载（`setSettingsView` 是异步的），
   * 能滚的节点根本不存在；把「滚哪一组」的知识留在面板里也才不需要父组件知道 DOM 结构。
   */
  scrollToMemories?: boolean;
}

/** 从 `SettingsView` 里剥掉那两只**只读**键，剩下的就是 `Settings` 全部。
 *  `api_key_masked` 与 `corrupt_backup` 不许写回 `settings_set`（把掩码当设置存盘是另一件事，
 *  `commands.ts:62` 的注释就写着）。
 *  这里**故意不逐字段列名单**：`settings.rs:260` 的结构体级 `#[serde(default)]` 让「少带一键」
 *  不报错、只把它抹回 Rust 默认值 ⇒ 面板每保存一次，就把手改进 `settings.json` 的
 *  `temperature`/`compact_threshold`/`keep_turns` 清零一遍。T39 之后 Rust 还会继续加键
 *  （T43 的 `accent`），列一次名单就是埋一次这样的雷。 */
const settingsOf = ({ api_key_masked, corrupt_backup, ...rest }: SettingsView): Settings => rest;

/** 数值项的名称表：`NaN` 必须在提交前被拦下并**报出是哪一项**。
 *  serde 收到 NaN 会直接拒收，症状是「点了保存什么也没发生」，而用户看不出是哪个框。 */
const MODEL_NUMERIC_FIELDS: { key: keyof Settings; label: string }[] = [
  { key: "reserve_output", label: "输出预留" },
  { key: "budget_tokens", label: "上下文预算" },
  { key: "window_messages", label: "窗口条数" },
  { key: "font_size", label: "字号" },
];

/** 长期记忆组的三个数值项（设计文档 §7）。标签是设置页口径：
 *  「召回条数」= 每轮注入的最大条数；「记忆备注上限」= 画像 + 检索两条 note 合计的 token
 *  硬顶；「画像字数上限」= 常驻画像单行的字符上限。钳制区间只有 Rust `normalize()` 那一处
 *  （R123：两处边界会漂移），所以这里**连范围提示都不写** —— 写了就是第二份会漂的边界。 */
const MEMORY_NUMERIC_FIELDS: { key: keyof Settings; label: string }[] = [
  { key: "memory_recall_k", label: "召回条数" },
  { key: "memory_note_token_cap", label: "记忆备注上限" },
  { key: "memory_profile_char_cap", label: "画像字数上限" },
  // T12 的两个节奏旋钮。标签里的单位（天/小时）是这行文字唯一承载的语义：
  // 区间仍只在 Rust `normalize()` 一处（R123），这里不写「1–3650」那种会漂的提示。
  { key: "memory_decay_half_life_days", label: "记忆衰减半衰期（天）" },
  { key: "memory_prospective_window_hours", label: "待办提醒窗口（小时）" },
];

/** NaN 拦截按**全表**走：分组建表只服务渲染分组，提交前的逐项点名永远看这一份全表。 */
const NUMERIC_FIELDS: { key: keyof Settings; label: string }[] = [
  ...MODEL_NUMERIC_FIELDS,
  ...MEMORY_NUMERIC_FIELDS,
];

/** 列表里 body 只展示前 60 个字符：这一屏是「审计」不是「阅读」，完整正文反正会以注入
 *  形态进上下文，设置页不许变成第二个阅读器。 */
const brief = (body: string): string =>
  body.length > 60 ? body.slice(0, 60) + "…" : body;

/**
 * T12：毫秒 → **本地** `YYYY-MM-DD`，手工拼而不是 `toLocaleDateString`。
 *
 * 两个理由，都不是风格问题：
 * 1. `toLocaleDateString` 的输出随 ICU/语言环境/`timeZone` 选项漂移（`2026/11/1`、
 *    `2026-11-01`、`11/1/2026` 都合法）。列表里一个日期格式随机器变，用户没法拿它做
 *    任何比较；测试也只能写出「某个日历日出现在某处」这种宽松断言。
 * 2. 取的是 `getFullYear/getMonth/getDate` 这组**本地时区**读数，与 `new Date(2026, 10, 1)`
 *    构造出的那一瞬间严格互逆 —— 测试用本地构造固定时间戳，期望值就不随 CI 时区变。
 *
 * Rust 只给毫秒数（`MemoryView.due_at`），「显示成哪一天」是纯展示决定，留在前端这一处。
 */
const localDate = (ms: number): string => {
  const d = new Date(ms);
  const month = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${month}-${day}`;
};

/**
 * 把 `<input type="datetime-local">` 的字面值换成毫秒时间戳，且**按 UTC 解释**。
 *
 * **为什么不能用 `new Date(str)`**：`datetime-local` 的 `value` 是**不带时区**的
 * 本地字面值，而 `new Date("2026-11-01T09:00")` 会按**浏览器本地时区**解析
 * （东八区得到 UTC 01:00）。后端 `parse_iso8601_ms` 的既有裁定（T9/T10）却是
 * 「无时区字面值按 UTC 解释」。两边各算一次，同一个「09:00」会在列表里显示成
 * 09:00、实际提醒却落在另一个时刻 —— 用户永远看不出哪一边对。
 * 所以这里显式用 `Date.UTC(y, m-1, d, h, min)`：字面值的年月日时分**原样当 UTC**，
 * 与 Rust 侧逐位一致。
 *
 * **已知代价（裁定记录）**：东八区用户在框里输入的「09:00」实际是 UTC 09:00
 * （本地 17:00），提醒会早 8 小时。这与模型抽取的待办、列表展示是**同一个**
 * 全局偏差。保持一致比局部修正重要：只在前端改这一处，会让「模型抽的 09:00」
 * 与「手填的 09:00」落到两个不同时刻；要修就整体修（把本地偏移显式传进 Rust，
 * 见 `parse_iso8601_ms` 的注释），不在这里打补丁。
 *
 * 空串 → `null`（待办但不提醒）。严格照 `parse_iso8601_ms` 的从严口径：
 * 月份/日期/时分越界一律 `null` —— 宁可这条待办不响，也不按一个猜出来的时刻
 * 设闹钟。`datetime-local` 的合法值形如 `YYYY-MM-DDTHH:MM`，部分浏览器补秒
 * （`:SS`），两种都容忍。
 */
const utcMsFromDatetimeLocal = (value: string): number | null => {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(value);
  if (m === null) return null;
  const [, ys, mos, ds, hs, mis, ss] = m;
  const year = Number(ys);
  const month = Number(mos);
  const day = Number(ds);
  const hour = Number(hs);
  const minute = Number(mis);
  const second = Number(ss ?? 0);
  // 越界检查与 Rust 侧同一套：`Date.UTC` 会把 13 月滚成次年 1 月、
  // 把 2 月 30 日滚成 3 月 2 日，静默接受等于替用户改了一个他没输入过的时间。
  if (month < 1 || month > 12) return null;
  // 当月天数用「下月第 0 天」取（`Date.UTC(y, m, 0)`），与 Rust `days_in_month` 同判据。
  const lastDay = new Date(Date.UTC(year, month, 0)).getUTCDate();
  if (day < 1 || day > lastDay) return null;
  if (hour > 23 || minute > 59 || second > 59) return null;
  // `Date.UTC` 对 0–99 年有一处历史遗留：它把年份当成 1900+year。逐位与 Rust
  // 对齐就得把字面年份写回去 —— 实际输入几乎不会落到那里，但「同一口径」这话
  // 要么成立、要么别写进注释。
  const d = new Date(Date.UTC(year, month - 1, day, hour, minute, second));
  if (year < 100) d.setUTCFullYear(year);
  return d.getTime();
};

export function SettingsPanel({
  initial,
  onSave,
  onSaveKey,
  onClearKey,
  onClose,
  onLoadMemories,
  onDeleteMemory,
  onClearMemories,
  onLoadProfile,
  onUpdateMemory,
  onAddMemory,
  onPickFolder,
  scrollToMemories = false,
}: SettingsPanelProps) {
  // 从 `initial` 拷一份本地草稿：面板活着的时候用户改的是这一份，
  // 保存失败不许把草稿也一起回滚掉（那等于让用户重打一遍）。
  const [form, setForm] = useState<Settings>(() => settingsOf(initial));
  const [rootsDraft, setRootsDraft] = useState(() => initial.roots.join("\n"));
  const [keyDraft, setKeyDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  // 记忆子区「清空全部」的确认态：与 API key 的 `confirmClear` 分开 —— 两把不同的刀，
  // 一把落下关闭面板（key 清了就没事了），一把落下留在原地（用户要亲眼看到列表空掉）。
  const [confirmClearMemories, setConfirmClearMemories] = useState(false);
  // 记忆列表只活在面板挂载期间：`null` = 还在拉取或从未拉成。**「还没有长期记忆」的提示
  // 必须等拉取确认过空才出**（`null` 一态都不渲染），否则拉取慢半拍会闪一帧假空，
  // 用户以为记忆没了 —— 跟「静默回落」是同一类谎言。
  const [memories, setMemories] = useState<MemoryView[] | null>(null);
  /**
   * P1 行内编辑：`null` = 没有任何一行在编辑；否则记着那一行的 id 与**草稿**。
   * 草稿必须从完整的 `m.body` 起，不能从列表里那个 `brief()` 起 —— 从 60 字截断串
   * 编辑等于用户按一次保存就亲手删掉自己记忆的后半截。
   * 一次只允许编辑一行：两行同时开着而其中一行的保存失败时，用户会分不清
   * 屏幕上哪一段文字属于哪一次写入。
   */
  const [editing, setEditing] = useState<{ id: number; draft: string } | null>(null);
  /**
   * P1 手动新增的草稿。`kind` 初值 `fact` 与 Rust `normalize_user_memory_kind` 的回落
   * 目标一致 —— 前端不校验名单（第二份会漂的真相），只给一个最中性的默认。
   */
  const [addKind, setAddKind] = useState("fact");
  const [addBody, setAddBody] = useState("");
  /**
   * P1 补充（spec §12.5）：待办的提醒时间草稿，存 `<input type="datetime-local">`
   * 的**原始字面值**而不是毫秒 —— 受控输入框要能原样回显用户刚敲的东西，
   * 换算只在提交那一刻发生（`utcMsFromDatetimeLocal`）。非待办时这个草稿保持
   * 空串（切换类型时清掉），所以「看不见的旧时间被带进下一次添加」不可能发生。
   */
  const [addDue, setAddDue] = useState("");
  // 「长期记忆」那一组的锚点：轻提示点击后要滚到这里（见 `scrollToMemories` 的说明）。
  const memoryGroupRef = useRef<HTMLFieldSetElement>(null);
  // T12：常驻画像正文。`""` 既是初值也是「后端没写过」的线上值 —— 两种都渲染同一个空，
  // 所以不需要 `null` 那一态（列表的 `null` 是为了「拉取中」与「确认为空」分开，
  // 而画像的空态渲染结果相同：什么都不显示）。
  const [profile, setProfile] = useState("");
  // 强调色控件的三态（任务 #64，计划 :942）：`null` = 跟随预设（复选框勾着、取色器不渲染），
  // `""` = 回到自选但**这一轮还没有值**（只有载入的 accent 本身是空串时才停在这里），
  // 其余才是用户真选过的 `#rrggbb`。
  // `""` 那一态不是多余的谨慎：`<input type="color">` 的 DOM 值**永远**是合法 hex（空白态被
  // 规范化成黑），载荷一旦取自 DOM，「我想看看自选长什么样」这一点击的必然结果就是存进一支
  // 用户从没选过的黑色 —— 症状是界面突然变黑底，而用户完全看不出为什么。
  // 反过来，取消勾选时把**载入的那支存色**恢复成待定值（种子来自设置本身，不是硬编码 hex）：
  // 否则「有存色 → 勾选跟随预设 → 再取消勾选」这条路上取色器显示 `#aa00ff` 而载荷发 `""`，
  // 同一个病的反面 —— 显示与载荷必须同源。
  // 这里**不校验形状**：`accent` 的合法值名单只有 Rust 的 `normalize_accent` 那一份（R123 的
  // 两处边界会漂移）；载入值已经过那一处归一，所以「非空即自选色」是可信的。
  const [customHex, setCustomHex] = useState<string | null>(() =>
    initial.accent === "" ? null : initial.accent,
  );
  const boxRef = useRef<HTMLDivElement>(null);
  const personaFileRef = useRef<HTMLInputElement>(null);

  // 打开即把焦点收进对话框：`aria-modal` 承诺了「这是当前唯一的操作面」，
  // 焦点却留在背后的界面上，键盘用户就看不见这个承诺；Escape 也才按得掉。
  useEffect(() => {
    boxRef.current?.focus();
  }, []);

  const setField = <K extends keyof Settings>(key: K, value: Settings[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

  const pickStorage = async () => {
    try {
      const picked = await onPickFolder();
      if (picked !== null) setField("storage_dir", picked);
    } catch (err) {
      setError(`选择默认储存位置失败：${String(err)}`);
    }
  };

  const importPersona = async (e: ChangeEvent<HTMLInputElement>) => {
    const input = e.currentTarget;
    const file = input.files?.[0];
    if (file === undefined) return;
    try {
      const raw = await file.text();
      const text = normalizePersonaText(raw);
      if (text.length > MAX_PERSONA_CHARS) {
        setError(
          `人设不能超过 ${MAX_PERSONA_CHARS.toLocaleString("en-US")} 字符（当前 ${text.length.toLocaleString("en-US")} 字符）`,
        );
        return;
      }
      setField("system_prompt", text);
      setError(null);
    } catch (err) {
      setError(`读取人设文件失败：${String(err)}`);
    } finally {
      input.value = "";
    }
  };

  // 记忆列表的一份实现、三个入口（挂载拉取 / 删除后重拉 / 清空后重拉，R117 的口径）。
  // 失败进**既有** error 通道：列表拉不到是面板级的事故，不该为它另开一个通知位。
  // 内部吞掉 rejection 是有意的：三个入口的调用点（effect / onClick）都不该再各自 catch。
  const reloadMemories = useCallback(async () => {
    try {
      setMemories(await onLoadMemories());
    } catch (e) {
      setError(String(e));
    }
  }, [onLoadMemories]);

  // 面板每次打开都是全新挂载（App 按 settingsView !== null 条件渲染），打开即拉一次；
  // `onLoadMemories` 在 App 侧是 `useCallback(…, [])`，依赖稳定 ⇒ 这个 effect 一生只跑一次。
  useEffect(() => {
    void reloadMemories();
  }, [reloadMemories]);

  // T12：画像与列表同一挂载期拉一次。`alive` 守卫是必须的 —— 面板可能在 promise 落地前
  // 被关掉（用户按了 Escape），那时 setState 会写在已卸载的组件上（React 18 不再警告，
  // 但状态更新本身没有意义，而且会把同一份失败重复记进一个已经不存在的面板）。
  // 失败进**既有** error 通道：画像拉不到与列表拉不到是同一级的事故，不为它另开通知位。
  useEffect(() => {
    let alive = true;
    onLoadProfile()
      .then((body) => {
        if (alive) setProfile(body);
      })
      .catch((e: unknown) => {
        if (alive) setError(String(e));
      });
    return () => {
      alive = false;
    };
  }, [onLoadProfile]);

  const removeMemory = async (id: number) => {
    try {
      await onDeleteMemory(id);
    } catch (e) {
      setError(String(e));
      return;
    }
    // 删除成功后的列表真值来自重拉，不在前端 splice：splice 是第二份真相，
    // 与 Rust 侧「这一刀实际删了哪几行」分叉时（并发写入、级联失效）界面就撒谎了。
    await reloadMemories();
  };

  const clearAllMemories = async () => {
    try {
      await onClearMemories();
    } catch (e) {
      setError(String(e));
      return;
    }
    setConfirmClearMemories(false);
    await reloadMemories();
  };

  /**
   * P1：从轻提示进来时把「长期记忆」这一组滚进视野。
   * `scrollIntoView?.` 的可选调用不是防御性装饰：jsdom 没有实现它，直接调用会让
   * 每一条「点 toast 打开面板」的用例当场抛 `not a function`，而真窗口里它一直在。
   */
  useEffect(() => {
    if (scrollToMemories) memoryGroupRef.current?.scrollIntoView?.({ block: "start" });
  }, [scrollToMemories]);

  const beginEdit = (m: MemoryView) => setEditing({ id: m.id, draft: m.body });

  const saveEdit = async () => {
    if (editing === null) return;
    // 前端只做「空正文不许点」这一层轻校验（按钮也一起禁用）；真校验是 Rust 的
    // `reject_empty_body`。这里再 `trim()` 是归一化而不是第二套判据：后端按 trim 后的
    // 结果判断空，但会把收到的原文落库 —— 不 trim 的话「前后各两个空格」会原样存进去。
    const body = editing.draft.trim();
    if (body === "") return;
    try {
      await onUpdateMemory(editing.id, body);
    } catch (e) {
      // 失败时**留在编辑态**：用户刚打的字还在框里，改完再存一次即可；关掉编辑框
      // 等于让他重打一遍（与 `submit` 失败不关面板同一条取舍）。
      setError(String(e));
      return;
    }
    setEditing(null);
    await reloadMemories();
  };

  const addMemory = async () => {
    const body = addBody.trim();
    if (body === "") return;
    // 到期时刻只对待办有意义。判据看 `addKind` 而不是「框里有没有字」：
    // 输入框在非待办时根本不渲染，但草稿可能在「切走」之前存在过 ——
    // 按可见性取载荷，「显示什么」与「发什么」才是同一个真相。
    const dueAt = addKind === "prospective" ? utcMsFromDatetimeLocal(addDue) : null;
    try {
      await onAddMemory(addKind, body, dueAt);
    } catch (e) {
      setError(String(e));
      return;
    }
    // 成功后只清正文与时间、保留类型：用户往往连续记同一类事（「记一下…、再记一下…」）。
    // 时间也必须清 —— 留着上一个待办的时刻，下一次「添加」会静默继承同一个闹钟。
    setAddBody("");
    setAddDue("");
    await reloadMemories();
  };

  // 数值行的一份实现、两个入口（模型组与长期记忆组共用同一渲染形状，R117 的口径）。
  const numericRow = ({ key, label }: { key: keyof Settings; label: string }) => (
    <label className="panel-row" key={key}>
      <span>{label}</span>
      <input
        type="number"
        step={1}
        value={Number.isFinite(form[key] as number) ? String(form[key]) : ""}
        onChange={(e) =>
          setField(key, e.target.valueAsNumber as Settings[typeof key])
        }
      />
    </label>
  );

  const submit = async () => {
    for (const { key, label } of NUMERIC_FIELDS) {
      const n = form[key];
      if (typeof n === "number" && !Number.isFinite(n)) {
        setError(`「${label}」必须是数字，现在是空的或不是数`);
        return;
      }
    }
    const typedKey = keyDraft.trim();
    const roots = rootsDraft
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line !== "");
    // `accent` 的唯一判据（见上面 `customHex` 那段）：`null`（勾选跟随预设）和 `""`（取消勾选
    // 不重新拼名单 —— `settingsOf` 的减法形状守的就是这件事（上面那条 carry-through 测试）。
    const payload: Settings = {
      ...form,
      roots,
      accent: customHex === null || customHex === "" ? "" : customHex,
    };
    try {
      await onSave(payload);
    } catch (e) {
      // 原文照贴，前端不解释文案（R195 口径）：解释一次就多一处与上游分叉的真相。
      setError(String(e));
      return;
    }
    if (typedKey === "") {
      onClose();
      return;
    }
    try {
      await onSaveKey(typedKey);
    } catch (e) {
      // 走到这里设置**已经存下了**，只有 key 没写进去。这句话必须说全，
      // 否则用户以为整次保存都失败，会去重打一遍已经存好的模型名。
      setError(`设置已保存，但 API key 写入失败：${String(e)}`);
      return;
    }
    setKeyDraft("");
    onClose();
  };

  const clearKey = async () => {
    try {
      await onClearKey();
    } catch (e) {
      setError(String(e));
      return;
    }
    setConfirmClear(false);
    onClose();
  };

  return (
    <div className="panel-backdrop">
      <div
        ref={boxRef}
        className="panel"
        role="dialog"
        aria-modal="true"
        aria-label="设置"
        tabIndex={-1}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h2 className="panel-title">设置</h2>

        {initial.corrupt_backup !== null ? (
          <p className="panel-note" data-testid="corrupt-backup">
            设置文件曾损坏，已备份为 {initial.corrupt_backup}；下面显示的是回落后的默认值。
          </p>
        ) : null}

        <fieldset className="panel-group">
          <legend>模型</legend>
          <label className="panel-row">
            <span>接口地址（base_url）</span>
            <input
              type="text"
              value={form.base_url}
              onChange={(e) => setField("base_url", e.target.value)}
            />
          </label>
          <label className="panel-row">
            <span>模型名</span>
            <input
              type="text"
              value={form.model}
              onChange={(e) => setField("model", e.target.value)}
            />
          </label>
          {MODEL_NUMERIC_FIELDS.map(numericRow)}
        </fieldset>

        <fieldset className="panel-group">
          <legend>API key</legend>
          <p className="panel-row panel-readonly">
            <span>当前</span>
            {/* 只有掩码，永远读不回值（spec:473）。`null` 是「未配置」，不是空字符串。 */}
            <code>{initial.api_key_masked ?? "未配置"}</code>
          </p>
          <label className="panel-row">
            <span>API key</span>
            <input
              type="password"
              autoComplete="off"
              value={keyDraft}
              onChange={(e) => setKeyDraft(e.target.value)}
            />
          </label>
          <p className="panel-hint">
            留空表示不动凭据；保存后这里的输入框立即清空，明文只走
            <code>settings_set_api_key</code> 这一条命令。
          </p>
          <div className="panel-actions">
            {confirmClear ? (
              <>
                <button type="button" onClick={() => void clearKey()}>
                  确认清除
                </button>
                <button type="button" onClick={() => setConfirmClear(false)}>
                  取消
                </button>
              </>
            ) : (
              <button
                type="button"
                disabled={initial.api_key_masked === null}
                onClick={() => setConfirmClear(true)}
              >
                清除已存的 key
              </button>
            )}
          </div>
        </fieldset>

        <fieldset className="panel-group">
          <legend>界面</legend>
          <label className="panel-row">
            <span>主题</span>
            <select
              value={form.theme}
              onChange={(e) => setField("theme", e.target.value)}
            >
              {THEME_NAMES.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </label>
          <label className="panel-row">
            <span>明暗</span>
            <select
              value={form.mode}
              onChange={(e) => setField("mode", e.target.value as Mode)}
            >
              <option value="light">浅色</option>
              <option value="dark">深色</option>
            </select>
          </label>
          {/* 这一行**不用**上面那几行的「`<label className="panel-row">` 套 span + 控件」写法：
              一行里有两个控件，而一个 `<label>` 只指向它第一个 labelable 后代。所以复选框自己带
              一支 `<label>`（可及名「跟随预设」），取色器用 `aria-label` 取同一个可见文案当名字。
              取色器是**非受控**的 `defaultValue`（裁定 2）：受控写法要么得写一支硬编码黑（R169 的
              病根 + 硬编码色闸），要么在第一次 onChange 时把控件从非受控翻成受控（控制台那条
              warning 本仓没有闸，会安静地留在产物里）。面板每次打开都是全新挂载，所以
              `defaultValue` 不需要「props 变了要重新同步」这件事。
              chip 恒定渲染且色值只写在 `styles.css` 里（`background-color: var(--accent)`）：
              它显示的就是界面当前**真的**在用的那支色（预设档来自预设块、自选档来自 inline 三件套），
              不需要 TS 知道它的值 —— 于是也没有第三份真相。 */}
          <div className="panel-row">
            <span>强调色</span>
            <span className="panel-accent">
              <label className="panel-follow">
                <input
                  type="checkbox"
                  checked={customHex === null}
                  onChange={() => setCustomHex(customHex === null ? form.accent : null)}
                />
                <span>跟随预设</span>
              </label>
              {customHex === null ? null : (
                <input
                  type="color"
                  aria-label="强调色"
                  defaultValue={form.accent}
                  onChange={(e) => setCustomHex(e.target.value)}
                />
              )}
              <span
                className="panel-swatch"
                data-testid="accent-swatch"
                aria-hidden="true"
              />
            </span>
          </div>
        </fieldset>

        <fieldset className="panel-group">
          <legend>文件访问</legend>
          <label className="panel-row panel-roots">
            <span>可访问目录</span>
            <textarea
              rows={4}
              spellCheck={false}
              placeholder={"C:\\Users\\你\\Documents\nD:\\notes"}
              value={rootsDraft}
              onChange={(e) => setRootsDraft(e.target.value)}
            />
          </label>
          <p className="panel-hint">
            一行一个绝对路径，只作为读取范围；写入/编辑只允许发生在当前工作文件夹或默认
            输出目录。留空 = 不允许读取任何目录。保存后下一条消息生效。
          </p>
        </fieldset>

        <fieldset className="panel-group">
          <legend>储存</legend>
          <label className="panel-row">
            <span>默认储存位置</span>
            <input
              type="text"
              value={form.storage_dir}
              placeholder="留空 = 文档\\LFTTDO"
              onChange={(e) => setField("storage_dir", e.target.value)}
            />
          </label>
          <div className="panel-actions">
            <button type="button" onClick={() => void pickStorage()}>
              选择文件夹
            </button>
            <button
              type="button"
              disabled={form.storage_dir === ""}
              onClick={() => setField("storage_dir", "")}
            >
              恢复默认
            </button>
          </div>
          <p className="panel-hint">
            会话没有设置工作文件夹时，生成的图片和文件会写到这里的会话子目录
            （视频尚未支持）。留空 = 系统文档目录下的 LFTTDO；工作文件夹优先。
            保存后下一条消息生效。
          </p>
        </fieldset>

        <fieldset className="panel-group">
          <legend>人设</legend>
          <label className="panel-row">
            <span>人设（系统提示词）</span>
            <textarea
              rows={7}
              maxLength={MAX_PERSONA_CHARS}
              value={form.system_prompt}
              onChange={(e) => setField("system_prompt", e.target.value)}
            />
          </label>
          <input
            ref={personaFileRef}
            type="file"
            accept=".txt,text/plain"
            aria-label="导入人设文件"
            style={{ display: "none" }}
            onChange={(e) => void importPersona(e)}
          />
          <p className="panel-hint">
            留空 = 使用内置提示词；导入按 UTF-8 读取。只保存在本机 settings.json，保存后下一条消息生效。
          </p>
          <div className="panel-actions">
            <button type="button" onClick={() => personaFileRef.current?.click()}>
              从 .txt 导入
            </button>
            <button
              type="button"
              disabled={form.system_prompt.trim() === ""}
              onClick={() => downloadPersonaText(form.system_prompt)}
            >
              导出 .txt
            </button>
            <button
              type="button"
              disabled={form.system_prompt === ""}
              onClick={() => {
                setError(null);
                setField("system_prompt", "");
              }}
            >
              恢复内置
            </button>
          </div>
        </fieldset>

        {/*
          长期记忆组（设计文档 §7）：总开关 + 五个数值旋钮 + 可审计的记忆管理子区。
          「记忆对用户必须可审计」是这一组的立组理由 —— 写入管线对用户透明（R69 的口径），
          唯一让用户看得见「它到底记了我什么」的地方就是这里。
          T12 补上了常驻画像那一行（`onLoadProfile` → `memory_profile`）以及每行的
          强度/待办/「已到期」标价；画像空串时不渲染，列表仍在（两者是独立的两块事实）。
        */}
        <fieldset className="panel-group" ref={memoryGroupRef}>
          <legend>长期记忆</legend>
          {/* 一行一个控件，所以「label 套 span + 控件」的写法在这里是合规的（可及名 =
              整行文字）；强调色那行为什么不用这个写法，见它自己的注释（一行两个控件）。 */}
          <label className="panel-row">
            <span>启用长期记忆</span>
            <input
              type="checkbox"
              checked={form.memory_enabled}
              onChange={(e) => setField("memory_enabled", e.target.checked)}
            />
          </label>
          {MEMORY_NUMERIC_FIELDS.map(numericRow)}
          {/* T12：常驻画像 —— 记忆列表**上方**一行只读正文（Memobase 的「画像是独立记忆类型、
              常驻注入」在设置页的对应物）。空串 = 后端从未写过画像，那一行整个不出现：
              渲染一行空的「用户画像」会让用户以为系统记了一片空白，与「没记过」是两件事。 */}
          {profile === "" ? null : (
            <p className="panel-hint" data-testid="memory-profile">
              用户画像：{profile}
            </p>
          )}
          {/* P1：手动新增。为什么入口必须存在：抽取是模型的事，模型没抽到的事
              （「记住我的快递柜密码是 0421」）此前只有「等下一轮再说」这一条路。
              类型用 select 而不是自由的 kind 输入框：白名单只有 Rust 那一处
              （`normalize_user_memory_kind`），界面把三个合法值摆出来就够了，
              多一个自由输入只会多一种「拼错 → 静默回落 fact」的可能。
              空正文禁用按钮是前端唯一的轻校验（真校验在 Rust）。 */}
          <div className="panel-memory-add" data-testid="memory-add">
            <select
              aria-label="记忆类型"
              value={addKind}
              onChange={(e) => {
                setAddKind(e.target.value);
                // 离开「待办」就把提醒时间草稿清掉：输入框已经消失，留着一份
                // 看不见的值，等用户切回来时又冒出来，等于替他记住了他没打算留的时刻。
                if (e.target.value !== "prospective") setAddDue("");
              }}
            >
              {/* 显示中文、value 用线上 kind：`value` 是要交给 Rust 的白名单成员，
                  一个字都不许改；只有「用户读到的那串」是展示层的事。
                  不把 kind 原文直接当标签还有一条实际理由 —— 列表行里也写着
                  `（preference）`，两处同样的字符串会让「读到哪一条」变得含混
                  （`SettingsPanel.test.tsx` 里那条按 `/preference/` 找行的旧断言
                  会一次命中两个元素，红的是旧用例而不是新功能）。 */}
              <option value="fact">事实</option>
              <option value="preference">偏好</option>
              <option value="prospective">待办</option>
            </select>
            <input
              aria-label="记忆正文"
              placeholder="手动记一件事"
              value={addBody}
              onChange={(e) => setAddBody(e.target.value)}
            />
            <button
              type="button"
              disabled={addBody.trim() === ""}
              onClick={() => void addMemory()}
            >
              添加
            </button>
          </div>
          {/* spec §12.5 补的缺口：待办必须能给到期时间，否则手动新增的待办永远
              等不到提醒。只在「待办」下渲染 —— 非待办没有提醒时间这个概念，
              摆一个灰框只会让用户以为「填了也会有用」。
              用「label 套 span + 控件」的既有 `panel-row` 形状，可及名就是「提醒时间」；
              **换算按 UTC**（见 `utcMsFromDatetimeLocal` 的裁定注释），
              代价是东八区用户看到的 09:00 实际提醒在本地 17:00 —— 与抽取侧同一偏差。 */}
          {addKind === "prospective" ? (
            <label className="panel-row" data-testid="memory-add-due">
              <span>提醒时间</span>
              <input
                type="datetime-local"
                value={addDue}
                onChange={(e) => setAddDue(e.target.value)}
              />
            </label>
          ) : null}
          {/* 子区：列表在上、清空在下。行结构与 `panel-row` 的两列网格对齐 ——
              正文列 + 按钮列，kind 与失效标记跟在正文后（按钮裹进既有 `panel-actions`，
              不为行结构新造一套网格）。 */}
          {memories === null
            ? null
            : memories.length === 0
              ? (
                  <p className="panel-hint" data-testid="memory-empty">
                    还没有长期记忆
                  </p>
                )
              : (
                  memories.map((m) =>
                    editing !== null && editing.id === m.id ? (
                      // 编辑态占同一行（`data-testid` 不变）：列表不会因为「有一行在编辑」
                      // 而重新编号，用户视线的位置也不跳。
                      <div className="panel-row" data-testid="memory-item" key={m.id}>
                        <textarea
                          aria-label={`编辑记忆 ${m.id}`}
                          rows={3}
                          value={editing.draft}
                          onChange={(e) =>
                            setEditing({ id: m.id, draft: e.target.value })
                          }
                        />
                        <div className="panel-actions">
                          <button
                            type="button"
                            disabled={editing.draft.trim() === ""}
                            onClick={() => void saveEdit()}
                          >
                            保存修改
                          </button>
                          <button type="button" onClick={() => setEditing(null)}>
                            取消
                          </button>
                        </div>
                      </div>
                    ) : (
                      <div className="panel-row" data-testid="memory-item" key={m.id}>
                        <span>
                          {brief(m.body)}（{m.kind}）
                          {/* P1：用户亲手改过的行带一个标记。它不只是装饰 ——
                              `user_edited` 同时意味着模型此后不会改写这一行，
                              用户看到它就知道「这条是我的说法，不会被悄悄覆盖」。
                              后端视图目前还没带这个字段，缺席时是 falsy ⇒ 不显示（见 types.ts）。 */}
                          {m.user_edited ? " · 已编辑" : null}
                          {/* T12：确认次数只在 > 0 时显示 —— 从未被确认过的记忆不带标价，
                              「确认 0 次」是噪声。 */}
                          {m.reinforcements > 0 ? ` · 确认 ${m.reinforcements} 次` : null}
                          {/* 待办日期只对待办显示：`due_at` 只有 `prospective` 才有值，
                              但视图是原样镜像（前端不复算业务口径），所以这里也要按 kind 把关。 */}
                          {m.kind === "prospective" && m.due_at !== null
                            ? ` · 待办 ${localDate(m.due_at)}`
                            : null}
                          {/* `is_due` 是 Rust 现算的（`due_at <= now`），这里只渲染，
                              不与 `Date.now()` 比第二遍。 */}
                          {m.is_due ? " · 已到期" : null}
                          {/* 失效不删除（Graphiti §3）：失效行照样列出、照样可删，但要明说。 */}
                          {m.valid ? null : <span>已失效</span>}
                        </span>
                        {/* 两个按钮裹进 `panel-actions`（flex），否则三列会掉进网格下一行。 */}
                        <div className="panel-actions">
                          <button type="button" onClick={() => beginEdit(m)}>
                            编辑
                          </button>
                          <button
                            type="button"
                            onClick={() => void removeMemory(m.id)}
                          >
                            删除
                          </button>
                        </div>
                      </div>
                    ),
                  )
                )}
          <div className="panel-actions">
            {confirmClearMemories ? (
              <>
                <button type="button" onClick={() => void clearAllMemories()}>
                  确认清空
                </button>
                <button
                  type="button"
                  onClick={() => setConfirmClearMemories(false)}
                >
                  取消
                </button>
              </>
            ) : (
              <button
                type="button"
                onClick={() => setConfirmClearMemories(true)}
              >
                清空全部记忆
              </button>
            )}
          </div>
        </fieldset>

        {error !== null ? (
          <p className="panel-error" role="alert" data-testid="panel-error">
            {error}
          </p>
        ) : null}

        <div className="panel-actions">
          <button type="button" className="panel-save" onClick={() => void submit()}>
            保存
          </button>
          <button type="button" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
