import { useEffect, useRef, useState } from "react";
import { MessageBubble } from "./MessageBubble";
import type { Decision, ErrorWhere, UiState } from "../types";

/** 距底多少像素以内算「还想跟着看」：太高会误粘、太低会在一次小增量后脱落。 */
const NEAR_BOTTOM_PX = 40;

/** `ErrorWhere`（types.rs 镜像 events.rs:103-111）只用来把「哪儿炸了」说成人话。 */
const WHERE_LABEL: Record<ErrorWhere, string> = {
  model: "模型端点",
  tool: "工具",
  mcp: "MCP",
  loop: "对话循环",
  store: "本地存储",
};

/**
 * 要求 11 = T14 评审 F5 的交接：`notices` **只增不减**，重放一整段历史可以上百条
 * （每个失败工具、每次压缩、每条非致命 error 都 push 一条），reducer 那边保持一份真相，
 * 折叠/截断全归渲染层。所以这里：
 *   ① 连续重复的合并成一条并显示 `×n`；
 *   ② 折叠后仍超过 `COLLAPSED_MAX` 条时只露最近的那几条，其余收进「展开」；
 *   ③ 不许每条都渲染成独立横幅。
 */
const COLLAPSED_MAX = 5;

interface Group {
  text: string;
  count: number;
}

function groupRuns(notices: string[]): Group[] {
  const out: Group[] = [];
  for (const n of notices) {
    const last = out[out.length - 1];
    if (last && last.text === n) last.count += 1;
    else out.push({ text: n, count: 1 });
  }
  return out;
}

function NoticeStrip({ notices }: { notices: string[] }) {
  const [open, setOpen] = useState(false);
  if (notices.length === 0) return null;
  const groups = groupRuns(notices);
  const collapsed = !open && groups.length > COLLAPSED_MAX;
  const shown = collapsed ? groups.slice(groups.length - COLLAPSED_MAX) : groups;
  const hidden = groups.length - shown.length;
  return (
    <ul className="notices" data-testid="notice-strip">
      {collapsed ? (
        <li className="notice-more">
          <button type="button" onClick={() => setOpen(true)}>
            展开（另有 {hidden} 条）
          </button>
        </li>
      ) : null}
      {open && groups.length > COLLAPSED_MAX ? (
        <li className="notice-more">
          <button type="button" onClick={() => setOpen(false)}>
            收起
          </button>
        </li>
      ) : null}
      {shown.map((g, i) => (
        <li className="notice" data-testid="notice" key={`${g.text}-${i}`}>
          {g.text}
          {g.count > 1 ? <span className="notice-count"> ×{g.count}</span> : null}
        </li>
      ))}
    </ul>
  );
}

/**
 * 要求 1：只收 `{ state, onRespond }`，**没有任何判断风险的 prop**。
 * 风险徽章归 `MessageBubble` 画，判定权在 Rust 的 PermissionGate（R114）。
 */
export function MessageList({
  state,
  onRespond,
  onOpenLink,
  onRevealPath,
}: {
  state: UiState;
  onRespond: (id: string, d: Decision) => void;
  /** Markdown 链接的出口（App 接 `open_url`）；测试与纯展示场景可以不传。 */
  onOpenLink?: (url: string) => void;
  onRevealPath?: (path: string) => void;
}) {
  const lastTurn = state.turns.length - 1;

  /**
   * 自动滚底：贴底时随新内容走，用户上翻之后**不再抢**滚动位置；
   * 回到底部附近（`NEAR_BOTTOM_PX` 内）重新黏住。
   * 判据用「距底距离」而不是「scrollTop 等于某值」：流式时内容高度一直在变。
   */
  const listRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);
  const onScroll = () => {
    const el = listRef.current;
    if (!el) return;
    stickRef.current =
      el.scrollHeight - el.scrollTop - el.clientHeight < NEAR_BOTTOM_PX;
  };
  // 无依赖：每次渲染（含流式增量）之后都检查一次；不贴底就什么都不做。
  useEffect(() => {
    const el = listRef.current;
    if (el && stickRef.current) el.scrollTop = el.scrollHeight;
  });

  return (
    <div
      className="message-list"
      data-testid="message-list"
      ref={listRef}
      onScroll={onScroll}
    >
      {/* 致命横幅排在所有东西之前：用户先看见「这一轮已经废了」，再读气泡。 */}
      {state.fatal ? (
        <div className="fatal" role="alert">
          <strong>致命错误（{WHERE_LABEL[state.fatal.where]}）</strong>
          <span>{state.fatal.message}</span>
        </div>
      ) : null}
      <NoticeStrip notices={state.notices} />
      <div className="turns" data-testid="bubble-list">
        {state.turns.map((t, i) => (
          <MessageBubble
            key={t.id}
            turn={t}
            isLastTurn={i === lastTurn}
            streaming={state.streaming}
            onRespond={onRespond}
            onOpenLink={onOpenLink}
            onRevealPath={onRevealPath}
          />
        ))}
      </div>
    </div>
  );
}
