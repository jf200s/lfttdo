import { useCallback, useEffect, useRef, useState } from "react";
import { Composer } from "./components/Composer";
import { MessageList } from "./components/MessageList";
import { Sidebar } from "./components/Sidebar";
import * as api from "./ipc/commands";
import { useTextBuffer } from "./ipc/batching";
import { appendUserTurn, initialState, reduce, replay } from "./state/reducer";
import { applyTheme, resolveTheme } from "./theme/theme";
import type {
  Decision,
  SendBlock,
  SessionRow,
  UiState,
} from "./types";

/** 侧栏那一行按 `updated_at` 取最新的一条（R128：键名是 snake_case，写错不会报错只会乱序）。 */
function newest(rows: SessionRow[]): SessionRow | undefined {
  return rows.reduce<SessionRow | undefined>(
    (acc, r) => (acc === undefined || r.updated_at > acc.updated_at ? r : acc),
    undefined,
  );
}

export default function App() {
  const [rows, setRows] = useState<SessionRow[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [ui, setUi] = useState<UiState>(initialState);
  const [appNotices, setAppNotices] = useState<string[]>([]);
  /**
   * 当前会话的**同步**影子。事件回调是 tauri 那边推进来的，读 `activeId` 这个 state
   * 会拿到旧值；这一支只有一处写（`openSession` / 启动那次），所以它是可审的。
   */
  const activeRef = useRef<string | null>(null);

  /**
   * 界面自己的提示通道。和 `UiState.notices` 分开：那一支是** reducer 的事实**
   * （由事件驱动、可重放），这一支是「IPC 调用失败 / 设置没生效」这类**只属于这次会话**的事。
   * 混进 `UiState` 就等于让重放路径凭空多出几条没人发过的事件（R113/R117 的同一类错误）。
   */
  const notice = useCallback(
    (text: string) => setAppNotices((n) => [...n, text]),
    [],
  );

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
   * 第 12 条 / 要求 10：主题由真实 `settings_get` 驱动（R169 因此才能删掉 `main.tsx`
   * 那行硬编码）。`resolveTheme` 只兜主题名；`mode` 那一支的运行时防线在 `applyTheme` 里
   * （T16 是它唯一的调用方，R167），所以 try/catch 在这里是**必需的**而不是装饰 ——
   * 磁盘上躺着未来版本写的值时，界面不许白屏。
   */
  useEffect(() => {
    let alive = true;
    api
      .getSettings()
      .then((v) => {
        if (!alive) return;
        const { theme, fellBack } = resolveTheme(v.theme);
        try {
          applyTheme(document.documentElement, theme, v.mode, v.font_size);
        } catch (e) {
          notice(`主题未能应用：${String(e)}`);
          return;
        }
        // 静默回落等于「设置没生效但界面说生效了」—— 必须让用户看见。
        if (fellBack) notice(`未知主题「${v.theme}」，已回落到 noir`);
      })
      .catch((e) => notice(`读取设置失败：${String(e)}`));
    return () => {
      alive = false;
    };
  }, [notice]);

  /** 切会话：先打断正在跑的那一轮，再用 `replaySession` 重建界面（要求 5）。 */
  const openSession = useCallback(
    async (id: string, mustInterrupt: boolean) => {
      if (mustInterrupt) {
        try {
          await api.interrupt();
        } catch (e) {
          notice(`打断当前这一轮失败：${String(e)}`);
        }
      }
      activeRef.current = id;
      setActiveId(id);
      bufRef.current.flushNow();
      setUi(initialState());
      try {
        const events = await api.replaySession(id);
        // 竞态：慢回来的那次不许覆盖已经切过去的会话。
        if (activeRef.current !== id) return;
        setUi(replay(events));
      } catch (e) {
        notice(`读取会话历史失败：${String(e)}`);
      }
    },
    // 依赖里**不能有 `buf`**：见上面 `bufRef` 的注释 —— 它每次渲染都是新对象，
    // 放进依赖就等于让 `openSession`、进而让启动 effect 每次渲染都重跑一遍。
    [notice],
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

  const send = useCallback(
    async (text: string) => {
      const sid = activeRef.current;
      if (sid === null) {
        notice("还没有可用的会话，先新建一条");
        return;
      }
      const blocks: SendBlock[] = [{ type: "text", text }];
      // R117：直播路径里事件流**从来没有** `user_message`（db.rs:380），这句话由这里
      // 乐观插入，用的就是重放那条同一个 `appendUserTurn` —— 一份实现、两个入口。
      setUi((s) => appendUserTurn(s, blocks));
      try {
        await api.sendMessage(sid, blocks);
      } catch (e) {
        notice(`发送失败：${String(e)}`);
      }
    },
    [notice],
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

  const usageText =
    ui.usage === null
      ? "本轮 token：尚未回报"
      : `本轮 token：输入 ${ui.usage.input} · 输出 ${ui.usage.output}`;

  return (
    <div className="app">
      <Sidebar
        rows={rows}
        activeId={activeId}
        onSelect={(id) => {
          if (id === activeRef.current) return;
          void openSession(id, true);
        }}
        onCreate={() => {
          void (async () => {
            try {
              const id = await api.createSession();
              await refresh();
              void openSession(id, true);
            } catch (e) {
              notice(`新建会话失败：${String(e)}`);
            }
          })();
        }}
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
        <MessageList state={ui} onRespond={onRespond} />
        <Composer streaming={ui.streaming} onSend={send} onStop={stop} />
        <div className="statusbar" data-testid="usage">
          {usageText}
        </div>
      </main>
    </div>
  );
}
