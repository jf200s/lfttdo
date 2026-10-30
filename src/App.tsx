import { useCallback, useEffect, useRef, useState } from "react";
import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { SettingsPanel } from "./components/SettingsPanel";
import { McpPanel } from "./components/McpPanel";
import { SkillsPanel } from "./components/SkillsPanel";
import { Sidebar } from "./components/Sidebar";
import * as api from "./ipc/commands";
import { useTextBuffer } from "./ipc/batching";
import {
  appendUserTurn,
  dropUserTurn,
  initialState,
  reduce,
  replay,
} from "./state/reducer";
import { writeThemeCache } from "./theme/cache";
import { applyTheme, resolveTheme } from "./theme/theme";
import type {
  Decision,
  SendBlock,
  SessionRow,
  Settings,
  SettingsView,
  UiState,
} from "./types";

/** 侧栏那一行按 `updated_at` 取最新的一条（R128：键名是 snake_case，写错不会报错只会乱序）。 */
function newest(rows: SessionRow[]): SessionRow | undefined {
  return rows.reduce<SessionRow | undefined>(
    (acc, r) => (acc === undefined || r.updated_at > acc.updated_at ? r : acc),
    undefined,
  );
}

/**
 * P1 轻提示在屏上的毫秒数。8 秒的取法：短到不挡住下一句话（用户读到「已记住 3 条」就够了），
 * 长到够他看到并决定要不要点进设置页。它不是可配置项 —— 一个记忆写入的提示没有
 * 重要到值得占一个设置键。
 */
const MEMORY_TOAST_MS = 8_000;

export default function App() {
  const [rows, setRows] = useState<SessionRow[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [ui, setUi] = useState<UiState>(initialState);
  const [appNotices, setAppNotices] = useState<string[]>([]);
  /**
   * 面板打开时手里那份 `SettingsView`；`null` 就是「面板没开」。
   * 用「数据」而不是「布尔开关」表达可见性：布尔 + 另一份 view 状态会有两个真相
   * （开着但拿的是上次那份），而这里要的恰恰是**打开那一次读到的**那份。
   */
  const [settingsView, setSettingsView] = useState<SettingsView | null>(null);
  const [mcpOpen, setMcpOpen] = useState(false);
  const [skillsOpen, setSkillsOpen] = useState(false);
  const [composerSeed, setComposerSeed] = useState<{
    id: number;
    text: string;
  } | null>(null);
  const seedId = useRef(0);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  /**
   * P1：轻提示当前要显示的那一条（`null` = 没有）。只留一个槽 ——
   * 「同一轮连续事件只留最后一条」因此是免费的：新的一条覆盖旧的，并重置计时。
   */
  const [memoryToast, setMemoryToast] = useState<{ count: number } | null>(null);
  const memoryToastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  /**
   * 面板打开时要不要滚到「长期记忆」那一组。由轻提示的点击置真。
   * 它是**一次性的意图**而不是持久设置：关闭面板时归假，否则下一次从侧栏正常打开
   * 也会莫名其妙滚到记忆区。
   */
  const [scrollMemories, setScrollMemories] = useState(false);
  /**
   * 当前会话的**同步**影子。事件回调是 tauri 那边推进来的，读 `activeId` 这个 state
   * 会拿到旧值；这一支只有一处写（`openSession` / 启动那次），所以它是可审的。
   */
  const activeRef = useRef<string | null>(null);
  /**
   * 会话代际：每调一次 `openSession` +1。迟到的重放（A→B→A 时第一次 A 的那份）
   * 必须按代际丢弃，而不是只比 sessionId —— 只比 id 时它会被当成「当前会话」的
   * 合法快照，把更晚的那一次重放覆盖掉。
   */
  const genRef = useRef(0);

  /**
   * 界面自己的提示通道。和 `UiState.notices` 分开：那一支是** reducer 的事实**
   * （由事件驱动、可重放），这一支是「IPC 调用失败 / 设置没生效」这类**只属于这次会话**的事。
   * 混进 `UiState` 就等于让重放路径凭空多出几条没人发过的事件（R113/R117 的同一类错误）。
   */
  const notice = useCallback(
    (text: string) => setAppNotices((n) => [...n, text]),
    [],
  );

  /**
   * P1：弹「已记住 N 条」。两个来源汇到这里 ——
   * 1. `ui.memorySaved`：本会话抽取完成后后端发的 `memory_saved` 事件；
   * 2. `flushMemory` 的返回值：切走那一刻的收尾抽取，它发生在会话已经离开之后，
   *    事件通道里不会再有它的 `memory_saved`。
   *
   * 为什么 `count <= 0` 不弹：0 条不是「记住了 0 条」，是「这一轮没有新东西」。
   * 后端也刻意不发零条的事件（`ipc.rs` 的测试钉着），前端补一条「已记住 0 条」
   * 等于替它编了一次写入。
   */
  const showMemoryToast = useCallback((count: number) => {
    if (count <= 0) return;
    setMemoryToast({ count });
    // 连续第二条重置计时，而不是让第一条的 8 秒把第二条一起带走 ——
    // 否则快速两轮写入时，第二条只剩很短的存活时间。
    if (memoryToastTimer.current !== null) clearTimeout(memoryToastTimer.current);
    memoryToastTimer.current = setTimeout(() => {
      setMemoryToast(null);
      memoryToastTimer.current = null;
    }, MEMORY_TOAST_MS);
  }, []);

  // 卸载时清计时器：留着的 setTimeout 会在一个已经不存在的组件上 setState，
  // 症状是测试之间互相串（前一条的计时器把后一条的 toast 提前抹掉）。
  useEffect(
    () => () => {
      if (memoryToastTimer.current !== null) {
        clearTimeout(memoryToastTimer.current);
        memoryToastTimer.current = null;
      }
    },
    [],
  );

  /**
   * 事件 → 轻提示。依赖是 `ui.memorySaved` 的**对象身份**：reducer 每次 `memory_saved`
   * 都造一个新对象（`types.ts` 的 `memorySaved.at` 说明为什么），所以连续两次同 count
   * 也会重开这 8 秒。换成依赖 `ui.memorySaved?.count` 的话，同 count 的第二次就是 no-op。
   */
  useEffect(() => {
    // `memorySaved` 在类型上是可选的（见 types.ts 的说明）：外部手工构造的 `UiState`
    // 可能根本没有这一键，`undefined` 与 `null` 在这里读成同一件事 —— 没有新计数。
    if (!ui.memorySaved) return;
    showMemoryToast(ui.memorySaved.count);
  }, [ui.memorySaved, showMemoryToast]);

  // 帧合流：Rust 已经按 30ms 合过一帧，这里再按 rAF 合一次，渲染频率 ≤ 帧率。
  const buf = useTextBuffer((chunk) =>
    setUi((s) => reduce(s, { t: "text_delta", text: chunk })),
  );
  /**
   * `useTextBuffer` 每次渲染都返回一个新的字面量对象（方法只碰 ref，所以行为是稳定的，
   * 但**身份**不稳定）。这里必须走 ref，否则 `useCallback` 的依赖数组每次渲染都变，
   * 下面那个「启动时拉会话列表」的 effect 会被反复重跑 —— 每跑一次就 `setUi(initialState())`
   * 清一遍界面，实测是一条渲染死循环（切会话的历史刚进来就被下一次循环抹掉，
   * `App.test.tsx` 里那几条「事件之后断言界面」的用例全部红）。
   */
  const bufRef = useRef(buf);
  bufRef.current = buf;

  /**
   * 要求 9 = Step 0 第 9 条的下半场：`subscribeAgent` **不按会话过滤是有意为之**，
   * 所以这道闸门是 App 的义务。`UiState` 里没有 sessionId 可区分（`types.ts` 的注释就写在
   * 那一支上），B 会话的流式文本一旦画进 A 的气泡就再也补不回来 —— 只能丢在门口。
   */
  useEffect(() => {
    const off = api.subscribeAgent((sessionId, e) => {
      if (sessionId !== activeRef.current) return;
      if (e.t === "text_delta") {
        bufRef.current.push(e.text);
        return;
      }
      // 非文本事件之前必先冲刷，否则工具卡会插到半句话前面（时间线倒序）。
      bufRef.current.flushNow();
      setUi((s) => reduce(s, e));
    });
    return off;
    // 只订阅一次：会话切换靠 activeRef，不靠重新订阅。
  }, []);


  /**
   * 把设置里那四项落到 `documentElement` 上。**启动与保存共用这一份**（R117 的口径：
   * 一份实现、两个入口）—— 写两遍的话「保存后界面没跟着变」这种 bug 只会出现在没被
   * 抄到的那一边。回落必须出声：静默回落等于「设置没生效但界面说生效了」。
   * T44：`accent` 进必填第 5 参；成功后写主题缓存（冷启动播种的唯一数据源，
   * `main.tsx` + `theme/cache.ts`）。写缓存的时机判据与「成功后才应用」同一条：
   * 失败的保存不许把没生效的颜色/字号种进下一份启动（`AppSettings.test.tsx` 钉着）。
   */
  const applySettingsTheme = useCallback(
    (v: Pick<Settings, "theme" | "mode" | "font_size" | "accent">) => {
      const { theme, fellBack } = resolveTheme(v.theme);
      try {
        applyTheme(document.documentElement, theme, v.mode, v.font_size, v.accent);
      } catch (e) {
        notice(`主题未能应用：${String(e)}`);
        return;
      }
      writeThemeCache({
        theme,
        mode: v.mode,
        fontSize: v.font_size,
        accent: v.accent,
      });
      if (fellBack) notice(`未知主题「${v.theme}」，已回落到 noir`);
    },
    [notice],
  );

  /**
   * 第 12 条 / 要求 10：主题由真实 `settings_get` 驱动（R169 因此才能删掉 `main.tsx`
   * 那行硬编码；T44 之后 `main.tsx` 只从**缓存**播种，权威仍是这里 → `AppSettings` 保存）。
   * `applySettingsTheme` 兜主题名回落；`mode` 那一支的运行时防线在 `applyTheme` 里
   * （`cache.ts` 的播种路径共用同一份防线，R167），所以 try/catch 在那一处是**必需的**而不是装饰 ——
   * 磁盘上躺着未来版本写的值时，界面不许白屏。
   */
  useEffect(() => {
    let alive = true;
    api
      .getSettings()
      .then((v) => {
        if (!alive) return;
        applySettingsTheme(v);
      })
      .catch((e) => notice(`读取设置失败：${String(e)}`));
    return () => {
      alive = false;
    };
  }, [notice, applySettingsTheme]);

  /** 切会话：先打断正在跑的那一轮，再用 `replaySession` 重建界面（要求 5）。 */
  const openSession = useCallback(
    async (id: string, mustInterrupt: boolean) => {
      const gen = ++genRef.current;
      const previous = activeRef.current;
      // P1：切走前给**旧**会话一次收尾抽取（spec §4.1：「会话收尾」与空闲 worker 同源）。
      // 三条纪律，缺一条都会把主路径弄糟：
      // 1. `void` + 不 await —— 切换不该为一次后台抽取多等一个来回；
      // 2. `.catch(() => {})` —— 后端已把一切失败收成 Ok(0)，这里兜的是 IPC 层自身的错；
      // 3. 返回值 > 0 才弹提示 —— 它的 `memory_saved` 事件永远不会到（那时已经在别的会话），
      //    所以这条路径必须自己把「记住了几条」说出来。
      // `previous === id` 时不调：没有切换，谈不上收尾（也避免重复抽取同一段游标）。
      if (previous !== null && previous !== id) {
        void api
          .flushMemory(previous)
          .then((saved) => showMemoryToast(saved))
          .catch(() => {});
      }
      if (mustInterrupt) {
        try {
          await api.interrupt();
        } catch (e) {
          notice(`打断当前这一轮失败：${String(e)}`);
        }
      }
      // 等 interrupt 的这段时间里可能又切了一次：被取代的那次不许再改任何状态。
      if (genRef.current !== gen) return;
      activeRef.current = id;
      setActiveId(id);
      bufRef.current.flushNow();
      setUi(initialState());
      try {
        const events = await api.replaySession(id);
        // 竞态：慢回来的那次不许覆盖已经切过去的会话；A→B→A 靠代际拦下第一份 A。
        if (genRef.current !== gen || activeRef.current !== id) return;
        setUi(replay(events));
      } catch (e) {
        if (genRef.current === gen) notice(`读取会话历史失败：${String(e)}`);
      }
    },
    // 依赖里**不能有 `buf`**：见上面 `bufRef` 的注释 —— 它每次渲染都是新对象，
    // 放进依赖就等于让 `openSession`、进而让启动 effect 每次渲染都重跑一遍。
    [notice, showMemoryToast],
  );

  const refresh = useCallback(async () => {
    try {
      setRows(await api.sessions());
    } catch (e) {
      notice(`读取会话列表失败：${String(e)}`);
    }
  }, [notice]);

  // 启动：列表为空就先建一条（否则界面是一个没有会话的空壳，发送无处可去）。
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        let rs = await api.sessions();
        if (rs.length === 0) {
          await api.createSession();
          rs = await api.sessions();
        }
        if (!alive || rs.length === 0) return;
        setRows(rs);
        const first = newest(rs);
        if (first) await openSession(first.id, false);
      } catch (e) {
        notice(`读取会话失败：${String(e)}`);
      }
    })();
    return () => {
      alive = false;
    };
  }, [notice, openSession]);

  /**
   * R205/T19：这一层收到的已经是**要发出去的块**（图在前、文本在后，由 `Composer` 拼好），
   * App 只负责「乐观插入 + 上线」，不再自己造 `[{type:"text",text}]` —— 造第二份就等于
   * 让「发了什么」有两个答案（`Composer` 那份、和这里这份）。
   *
   * 失败时**必须把 rejection 传回去**（下面两处 `throw`）：`Composer` 靠它决定「待发图留不留」
   * （Step 2 第 8 条）。只 `notice(...)` 然后正常返回的话，Composer 会以为发送成功，
   * 把用户刚贴的四张图清空 —— 症状是「图没了、什么也没发出去」，比报错更糟。
   * `AppImages.test.tsx` 两条分别钉这两处（变异判据：删 `throw e`、或把 `throw` 换回 `return`）。
   */
  const send = useCallback(
    async (blocks: SendBlock[]) => {
      const sid = activeRef.current;
      const gen = genRef.current;
      if (sid === null) {
        notice("还没有可用的会话，先新建一条");
        throw new Error("没有可用的会话");
      }
      // R117：直播路径里事件流**从来没有** `user_message`（db.rs:380），这句话由这里
      // 乐观插入，用的就是重放那条同一个 `appendUserTurn` —— 一份实现、两个入口。
      setUi((s) => appendUserTurn(s, blocks));
      try {
        await api.sendMessage(sid, blocks);
      } catch (e) {
        // R332：乐观气泡必须跟着回滚 —— 否则屏幕上有一条「已发送」的消息，库里没有，
        // 切会话或重启后它凭空消失；用户当场重发还会多一条。
        // 但不能只删最后一个 user 气泡：失败可能发生在 turn_start 之后（落库失败），
        // 那时 assistant 轮已经在屏幕上。用重放把界面拉回持久化真相，
        // 与「messages 是唯一真相源」是同一条不变量。
        // 只对**发起这次发送的会话**做，切走之后不许改动别的会话。
        // 这里 `await` 掉这次重放：Composer 的发送锁要到下面 `throw` 才释放，
        // 所以用户不可能在「重放还没回来」时重发第二条，也就没有旧快照覆盖新消息的窗口。
        if (activeRef.current === sid && genRef.current === gen) {
          try {
            const events = await api.replaySession(sid);
            if (activeRef.current === sid && genRef.current === gen) {
              // 先冲刷 rAF 缓冲：迟到的增量不许在重建之后长出幽灵助手轮。
              bufRef.current.flushNow();
              setUi(replay(events));
            }
          } catch {
            // 连历史都读不回来时，至少把乐观气泡撤回（回合已开始的那种情况，
            // 这一步是 no-op，界面会停在 fatal 分支上，用户仍能从 notice 看到原文）。
            if (activeRef.current === sid && genRef.current === gen) {
              setUi((s) => dropUserTurn(s, blocks));
            }
          }
        }
        notice(`发送失败：${String(e)}`);
        throw e;
      }
      // R234：自动改名住在 Rust 侧（`auto_title`），而它不发事件 —— 不重新拉一次列表，
      // 侧栏那一行会一直写着「新对话」，直到用户下次新建/删除/重命名才更新。
      // 放在 `await` 之后是必须的：命令返回时那一轮的写入已经落库。不 `await` 它：
      // 草稿清空挂在 `onSend` 的 resolve 上，不该为一次列表刷新多等一个来回。
      // `refresh` 自己吞错并 `notice`，所以这里不存在悬空 rejection。
      void refresh();
    },
    [notice, refresh],
  );

  /**
   * R131：`respond` 的第二参名是 `requestId`，返回的 `bool` 意思是「pending 里确实有这一笔
   * 被闭合」。**这个返回值在这里被有意丢弃** —— 真也好、假也好，卡片的最终态只由
   * `permission_resolved` 事件驱动（`App.test.tsx` 两条都钉着：false 不动、true 也不动）。
   * 只有调用本身失败（serde 拒收、命令报错）才需要让用户看见，因为那时那一笔还挂着。
   */
  const onRespond = useCallback(
    (id: string, d: Decision) => {
      api.respond(id, d).catch((e) => notice(`提交许可决定失败：${String(e)}`));
    },
    [notice],
  );

  const stop = useCallback(() => {
    // 只打断，不改状态：`streaming` 的唯一出口是 `done`（R112），不做乐观恢复。
    api.interrupt().catch((e) => notice(`打断当前这一轮失败：${String(e)}`));
  }, [notice]);

  /**
   * Markdown 链接统一走这里：前端不直接导航 WebView（远程页会把应用页面替换掉），
   * 协议白名单与「不经过 shell」的落点都在 Rust 的 `open_url`。
   */
  const openLink = useCallback(
    (url: string) => {
      api.openUrl(url).catch((e) => notice(`打开链接失败：${String(e)}`));
    },
    [notice],
  );

  /**
   * T19：`Composer` 的归一化入口，接 `commands.ts` 平铺导出的 `normalize`（R206：不新造 `api` 对象）。
   *
   * 这里包一层闭包而不是直接传 `api.normalize`，是有意的：`import * as api` 的**属性取值**
   * 发生在渲染期，而 `App.test.tsx` 那十三条（R216②：一条都不许改）的 mock 工厂里没有
   * `normalize` 这一支 —— 直接传会让 vitest 抛「No "normalize" export is defined on the mock」，
   * 十三条全红，红的还是「不该动的测试」。包一层把取值推迟到**真的贴了图**才发生。
   * 身份由 `useCallback(…, [])` 保持稳定，且不进任何 effect 依赖（R208 的那条死循环）。
   */
  const normalize = useCallback(
    (bytes: ArrayBuffer) => api.normalize(bytes),
    [],
  );

  /**
   * T45：面板是 `settings_set` / `settings_set_api_key` 的第一个消费者。
   * 打开时**重新读一次**而不是复用启动那一份 —— `settings_get` 每次都读磁盘
   * （`ipc.rs:370` → `settings.rs:432`），用户在外面改了 `settings.json`，
   * 面板复用旧值就等于让他对着一个已经过期的模型名编辑。
   */
  const openSettingsAt = useCallback(
    (memories: boolean) => {
      // 先记下「要不要滚到记忆区」的意图，再读设置：`setSettingsView` 之后面板才挂载，
      // 面板的挂载 effect 会消费这个布尔。反过来的话，滚动发生在节点还不存在的时候。
      setScrollMemories(memories);
      api
        .getSettings()
        .then((v) => setSettingsView(v))
        .catch((e) => notice(`读取设置失败：${String(e)}`));
    },
    [notice],
  );
  /** 侧栏「设置」：从顶部开始看，不滚动。 */
  const openSettings = useCallback(() => openSettingsAt(false), [openSettingsAt]);
  /** P1 轻提示的点击目标：打开面板并滚到长期记忆区。 */
  const openMemorySettings = useCallback(() => openSettingsAt(true), [openSettingsAt]);

  const closeSettings = useCallback(() => {
    setSettingsView(null);
    // 一次性意图在这里收口：下一次从侧栏打开时不该再滚到记忆区。
    setScrollMemories(false);
  }, []);

  /** 技能面板的“用这个技能”：关面板，把一句引导填进输入框并聚焦（不替用户发送）。 */
  const useSkill = useCallback((name: string) => {
    seedId.current += 1;
    setComposerSeed({
      id: seedId.current,
      text: `使用「${name}」技能：`,
    });
    setSkillsOpen(false);
  }, []);

  /**
   * 这里**不许**把 rejection 吞成 resolved：`SettingsPanel` 靠它决定「面板关不关、
   * 错误原文显不显示」。理由与 `send` 那句 `throw e` 完全同构（`AppImages.test.tsx`
   * 的变异判据 c），判据在 `AppSettings.test.tsx` 那条「keeps the panel open …」。
   * 主题只在写盘成功之后应用一次 —— 失败的保存顺手改界面，是「没生效但看着生效了」。
   */
  const saveSettings = useCallback(
    async (s: Settings) => {
      await api.setSettings(s);
      applySettingsTheme(s);
    },
    [applySettingsTheme],
  );

  // 明文只许走 `settings_set_api_key` 这一条命令；空串是命令侧「清除」的唯一编码（R88），
  // 所以清除在 App 这一层也是一次 `setApiKey`，不另造删除路径。两处都不 catch：
  // 原文要交给面板显示。`async` + `await` 是为了把 `invoke` 的 `Promise<unknown>` 收成
  // 面板签名要的那个 `Promise<void>`（同 `commands.ts:31` 的 `send_message` 那一支）。
  const saveKey = useCallback(async (k: string) => {
    await api.setApiKey(k);
  }, []);
  const clearKey = useCallback(async () => {
    await api.setApiKey("");
  }, []);

  // 长期记忆子区的三条回调（设计文档 §7），转发 `commands.ts` 的命令，**都不 catch**：
  // 失败原文要交给面板自己的 error 通道（同 saveKey/clearKey 的口径）。记忆列表的状态
  // 只活在面板挂载期间（打开拉一次、删除/清空后重拉），App 不持有第二份 —— 这里
  // 只递命令、不递数据。三个都用 `useCallback` 钉住身份：面板的挂载 effect 以
  // `onLoadMemories` 为依赖，身份漂一次就重拉一次列表。
  const loadMemories = useCallback(() => api.listMemories(), []);
  const removeMemory = useCallback(async (id: number) => {
    await api.deleteMemory(id);
  }, []);
  const clearAllMemories = useCallback(async () => {
    await api.clearMemories();
  }, []);
  // P1：用户权威的两个写入口。与上面三条同一条线 —— 只转发命令、不持数据；
  // **不 catch**：失败原文交给面板自己的 error 通道（用户正在编辑框里，只有面板
  // 知道他刚才改的是哪一行）。成功后也不会在这里刷列表：面板自己重拉。
  const updateMemory = useCallback(async (id: number, body: string) => {
    await api.updateMemoryBody(id, body);
  }, []);
  const addMemory = useCallback(
    async (kind: string, body: string, dueAt: number | null) => {
      // `memory_add` 返回新行 id（面板当前不消费它，但签名要如实传递，免得将来
      // 「按 id 高亮新行」时有人以为命令不返回东西）。
      // `dueAt` 原样转发：字面值 → UTC 毫秒的换算归面板，App 不做第二次解释
      // （多一处换算就多一处会漂的口径）。
      return await api.addMemory(kind, body, dueAt);
    },
    [],
  );
  // T12：常驻画像。与上面三条同一条线 —— 只转发命令、不持数据，失败原文交给面板。
  // 身份必须稳定：面板的画像 effect 以它为依赖，漂一次就重拉一次。
  const loadProfile = useCallback(() => api.memoryProfile(), []);

  const usageText =
    ui.usage === null
      ? "本轮 token：尚未回报"
      : `本轮 token：输入 ${ui.usage.input} · 输出 ${ui.usage.output}`;

  const createSession = () => {
    void (async () => {
      try {
        const id = await api.createSession();
        await refresh();
        void openSession(id, true);
      } catch (e) {
        notice(`新建会话失败：${String(e)}`);
      }
    })();
  };

  return (
    <div className="app" data-sidebar={sidebarOpen ? "open" : "closed"}>
      <Sidebar
        collapsed={!sidebarOpen}
        onToggleCollapsed={() => setSidebarOpen((v) => !v)}
        onOpenSettings={openSettings}
        onOpenMcp={() => setMcpOpen(true)}
        onOpenSkills={() => setSkillsOpen(true)}
        rows={rows}
        activeId={activeId}
        onSelect={(id) => {
          if (id === activeRef.current) return;
          void openSession(id, true);
        }}
        onCreate={createSession}
        onDelete={(id) => {
          void (async () => {
            try {
              // 不在这里 interrupt()：`session_delete` 自己只取消**这个**会话那一轮
              // （ipc.rs:455-467），删 B 不该打断 A。
              await api.deleteSession(id);
              const rs = await api.sessions();
              setRows(rs);
              if (id !== activeRef.current) return;
              const next = newest(rs);
              if (next) {
                await openSession(next.id, false);
              } else {
                const created = await api.createSession();
                setRows(await api.sessions());
                await openSession(created, false);
              }
            } catch (e) {
              notice(`删除会话失败：${String(e)}`);
            }
          })();
        }}
        onRename={(id, title) => {
          void (async () => {
            try {
              await api.renameSession(id, title);
              await refresh();
            } catch (e) {
              notice(`改名失败：${String(e)}`);
            }
          })();
        }}
      />
      <main className="pane">
        {appNotices.length > 0 ? (
          <ul className="app-notices" data-testid="app-notices">
            {appNotices.map((n, i) => (
              <li className="app-notice" data-testid="app-notice" key={`${n}-${i}`}>
                {n}
              </li>
            ))}
          </ul>
        ) : null}
        <MessageList
          state={ui}
          onRespond={onRespond}
          onOpenLink={openLink}
        />
        <Composer
          streaming={ui.streaming}
          onSend={send}
          onStop={stop}
          onNormalize={normalize}
          seed={composerSeed}
        />
        <div className="statusbar" data-testid="usage">
          {usageText}
        </div>
      </main>
      {/* P1 轻提示。`role="status"` + `aria-live="polite"`：它**不该抢焦点**，
          也不该在用户正在打字时打断他，只排到下一句话之后读一次。
          区域常驻、内容条件渲染：live region 必须在内容变化之前就在 DOM 里，
          否则屏幕阅读器对「新挂载的 live region」通常不播报。
          两个来源都汇进同一个槽，所以屏幕上永远只有一条「已记住 N 条」。 */}
      <div className="toast-region" role="status" aria-live="polite">
        {memoryToast === null ? null : (
          <button
            type="button"
            className="toast"
            onClick={openMemorySettings}
          >
            已记住 {memoryToast.count} 条
          </button>
        )}
      </div>
      {/* 面板是**视图**，不是另一份状态：它开着的时候 `ui`、订阅、会话列表全都照旧跑
          （判据在 `AppSettings.test.tsx` 那条「keeps rendering the live turn …」）。
          `backdrop` 是 fixed 全屏，所以挂在 `.app` 这个 grid 的最后一个子位上不影响布局。 */}
      {settingsView !== null ? (
        <SettingsPanel
          initial={settingsView}
          onSave={saveSettings}
          onSaveKey={saveKey}
          onClearKey={clearKey}
          onClose={closeSettings}
          onLoadMemories={loadMemories}
          onDeleteMemory={removeMemory}
          onClearMemories={clearAllMemories}
          onLoadProfile={loadProfile}
          onUpdateMemory={updateMemory}
          onAddMemory={addMemory}
          scrollToMemories={scrollMemories}
        />
      ) : null}
      {mcpOpen ? <McpPanel onClose={() => setMcpOpen(false)} /> : null}
      {skillsOpen ? (
        <SkillsPanel
          onClose={() => setSkillsOpen(false)}
          onUseSkill={useSkill}
        />
      ) : null}
    </div>
  );
}
