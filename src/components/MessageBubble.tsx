import { useState } from "react";
import Markdown from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import type { Decision, PermCard, Risk, ToolCard, Turn, UiBlock } from "../types";

/** `risk` 只用来画徽章，不参与任何决策（R114：判定权在 Rust 的 PermissionGate）。 */
const RISK_LABEL: Record<Risk, string> = { read: "只读", write: "写入", exec: "执行" };

const STATUS_LABEL: Record<ToolCard["status"], string> = {
  running: "执行中",
  ok: "完成",
  error: "失败",
};

/**
 * 第 9 条 = 硬性要求 8：`ms === 0` 来自 `db.rs:370` 的「库里没记这一列」，重放路径恒为 0。
 * 把它显示成「0 ms」是替用户编一次测量。reducer 原样透传（R115），归位到界面上就是这个破折号。
 */
const fmtMs = (ms: number): string => (ms === 0 ? "—" : `${ms} ms`);

function ToolCardView({ card }: { card: ToolCard }) {
  return (
    <div
      className="card tool"
      data-testid={`tool-${card.callId}`}
      data-status={card.status}
    >
      <div className="card-head">
        <span className="tool-name">{card.name}</span>
        <span className={`badge risk-${card.risk}`}>{RISK_LABEL[card.risk]}</span>
        <span className="tool-status">{STATUS_LABEL[card.status]}</span>
        <span className="tool-ms">{fmtMs(card.ms)}</span>
      </div>
      <pre className="io">
        {JSON.stringify(card.input ?? {}, null, 2)}
      </pre>
      {card.preview ? <pre className="io preview">{card.preview}</pre> : null}
    </div>
  );
}

const DECISION_LABEL: Record<PermCard["state"], string> = {
  pending: "等待你的许可",
  allowed: "已放行",
  denied: "已拒绝",
};

function PermCardView({
  card,
  onRespond,
}: {
  card: PermCard;
  onRespond: (id: string, d: Decision) => void;
}) {
  // 拒绝理由必须是**行内** input：第 8 条红线。`window.prompt` 在 WebView2 里返回 null，
  // 理由发不出去 ⇒ `respond` 收不到参数 ⇒ 那一笔永远 pending ⇒ 整轮静默挂住。
  // `setupTests.ts` 已经把三个弹窗函数换成一调用就抛，这里连提都不该提。
  const [denying, setDenying] = useState(false);
  const [reason, setReason] = useState("");
  const pending = card.state === "pending";

  const allow = (scope: "once" | "session") =>
    onRespond(card.id, { allow: true, scope });
  const submitDeny = () => {
    const trimmed = reason.trim();
    onRespond(card.id, { allow: false, reason: trimmed === "" ? "用户拒绝" : trimmed });
    setDenying(false);
    setReason("");
  };

  return (
    <div
      className="card perm"
      data-testid={`perm-${card.id}`}
      data-state={card.state}
    >
      <div className="card-head">
        <span className="perm-title">请求许可</span>
        <span className={`badge risk-${card.risk}`}>{RISK_LABEL[card.risk]}</span>
        <span className="perm-state">{DECISION_LABEL[card.state]}</span>
      </div>
      <div className="perm-call">
        <span className="tool-name">{card.call.name}</span>
        <pre className="io">{JSON.stringify(card.call.input ?? {}, null, 2)}</pre>
      </div>

      {card.state === "allowed" && card.scope ? (
        <p className="perm-note">
          {card.scope === "session" ? "本会话内都已放行" : "仅本次放行"}
        </p>
      ) : null}
      {card.state === "denied" ? (
        <p className="perm-note">理由：{card.reason ?? "用户拒绝"}</p>
      ) : null}

      <div className="perm-actions">
        <button type="button" disabled={!pending} onClick={() => allow("once")}>
          仅本次
        </button>
        <button
          type="button"
          disabled={!pending}
          onClick={() => allow("session")}
        >
          本会话内都允许
        </button>
        <button
          type="button"
          disabled={!pending}
          onClick={() => setDenying((v) => !v)}
        >
          拒绝
        </button>
      </div>
      {denying && pending ? (
        <div className="deny-reason">
          <label htmlFor={`deny-${card.id}`}>拒绝理由（可留空）</label>
          <input
            id={`deny-${card.id}`}
            value={reason}
            onChange={(e) => setReason(e.target.value)}
          />
          <button type="button" onClick={submitDeny}>
            提交拒绝理由
          </button>
        </div>
      ) : null}
    </div>
  );
}

function BlockView({
  block,
  plain,
  onRespond,
}: {
  block: UiBlock;
  plain: boolean;
  onRespond: (id: string, d: Decision) => void;
}) {
  switch (block.kind) {
    case "text":
      // 要求 2：正在流式的那一块按纯文本渲染 —— 反复解析半截语法就是闪烁的来源。
      return plain ? (
        <pre className="plain">{block.text}</pre>
      ) : (
        <div className="md">
          <Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
            {block.text}
          </Markdown>
        </div>
      );
    case "image":
      return (
        <img
          className="thumb"
          src={`data:${block.mime};base64,${block.data}`}
          alt="图片"
        />
      );
    case "tool":
      return <ToolCardView card={block} />;
    case "perm":
      return <PermCardView card={block} onRespond={onRespond} />;
  }
}

export function MessageBubble({
  turn,
  isLastTurn,
  streaming,
  onRespond,
}: {
  turn: Turn;
  isLastTurn: boolean;
  streaming: boolean;
  onRespond: (id: string, d: Decision) => void;
}) {
  const lastIndexOf = turn.blocks.length - 1;
  return (
    <article
      className={`bubble bubble-${turn.role}`}
      data-testid={`bubble-${turn.role}`}
      data-role={turn.role}
    >
      {turn.blocks.map((b, i) => (
        <BlockView
          key={i}
          block={b}
          plain={streaming && isLastTurn && i === lastIndexOf}
          onRespond={onRespond}
        />
      ))}
    </article>
  );
}
