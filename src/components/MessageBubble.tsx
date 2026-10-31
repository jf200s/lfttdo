import { memo, useState } from "react";
import Markdown from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";
import type {
  Decision,
  PermCard,
  Risk,
  ToolCallView,
  ToolCard,
  Turn,
  UiBlock,
} from "../types";

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

/**
 * 预览里那行「未能定界出正文…」是**给模型**的护栏（防止它把样板当正文引用），
 * 不是给用户看的报错。界面只显示正文：把这一行从展示文本里摘掉；
 * 如果整段预览就只有这行提示，预览区直接不渲染。
 */
export function readablePreview(text: string): string {
  const NL = String.fromCharCode(10);
  return text
    .split(NL)
    .filter((line) => !line.trimStart().startsWith("[未能定界出正文"))
    .join(NL)
    .trim();
}

function PathChip({
  path,
  onReveal,
}: {
  path: string;
  onReveal?: (path: string) => void;
}) {
  if (onReveal === undefined) {
    return (
      <span className="path-chip" title={path}>
        {path}
      </span>
    );
  }
  return (
    <button
      type="button"
      className="path-chip"
      title={`在资源管理器中显示：${path}`}
      onClick={() => onReveal(path)}
    >
      {path}
    </button>
  );
}

function toolPathOf(input: unknown): string | null {
  const o = asRecord(input);
  if (o === null || typeof o.path !== "string" || o.path.trim() === "") {
    return null;
  }
  return o.path;
}

function ToolCardView({
  card,
  onRevealPath,
}: {
  card: ToolCard;
  onRevealPath?: (path: string) => void;
}) {
  const path = toolPathOf(card.input);
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
      {path === null ? null : (
        <PathChip path={path} onReveal={onRevealPath} />
      )}
      <pre className="io">
        {JSON.stringify(card.input ?? {}, null, 2)}
      </pre>
      {(() => {
        const shown = card.preview ? readablePreview(card.preview) : "";
        return shown !== "" ? <pre className="io preview">{shown}</pre> : null;
      })()}
      {card.images.length > 0 ? (
        <div className="tool-images" data-testid={`tool-images-${card.callId}`}>
          {card.images.map((img, i) => (
            <figure className="tool-image" key={`${img.mime}-${i}`}>
              <img
                className="thumb"
                src={`data:image/${img.mime};base64,${img.data}`}
                alt="工具返回的图片"
              />
              {img.path === undefined ? null : (
                <PathChip path={img.path} onReveal={onRevealPath} />
              )}
            </figure>
          ))}
        </div>
      ) : null}
    </div>
  );
}

const DECISION_LABEL: Record<PermCard["state"], string> = {
  pending: "等待你的许可",
  allowed: "已放行",
  denied: "已拒绝",
};

type WriteInput = { path: string; content: string };
type EditInput = { path: string; old: string; new: string };

function asRecord(input: unknown): Record<string, unknown> | null {
  return typeof input === "object" && input !== null
    ? (input as Record<string, unknown>)
    : null;
}

function writeInputOf(input: unknown): WriteInput | null {
  const o = asRecord(input);
  if (o === null || typeof o.path !== "string" || typeof o.content !== "string") {
    return null;
  }
  return { path: o.path, content: o.content };
}

function editInputOf(input: unknown): EditInput | null {
  const o = asRecord(input);
  if (
    o === null ||
    typeof o.path !== "string" ||
    typeof o.old_string !== "string" ||
    typeof o.new_string !== "string"
  ) {
    return null;
  }
  return { path: o.path, old: o.old_string, new: o.new_string };
}

function CallPreview({ call }: { call: ToolCallView }) {
  const write = call.name === "write_file" ? writeInputOf(call.input) : null;
  const edit = call.name === "edit_file" ? editInputOf(call.input) : null;

  if (write !== null) {
    return (
      <div className="perm-preview" data-testid="write-preview">
        <p className="perm-path">
          <span>路径</span>
          <code>{write.path}</code>
        </p>
        <p className="perm-note">完整内容（{write.content.length} 字符）</p>
        <pre className="io preview" tabIndex={0} aria-label="写入内容预览">
          {write.content}
        </pre>
      </div>
    );
  }
  if (edit !== null) {
    return (
      <div className="perm-preview" data-testid="edit-preview">
        <p className="perm-path">
          <span>路径</span>
          <code>{edit.path}</code>
        </p>
        <p className="perm-note">替换前</p>
        <pre className="io preview" tabIndex={0} aria-label="替换前内容">
          {edit.old}
        </pre>
        <p className="perm-note">替换后</p>
        <pre className="io preview" tabIndex={0} aria-label="替换后内容">
          {edit.new}
        </pre>
      </div>
    );
  }
  return <pre className="io">{JSON.stringify(call.input ?? {}, null, 2)}</pre>;
}

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
        <CallPreview call={card.call} />
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
          {card.risk === "write" ? "本会话内该目录都允许" : "本会话内都允许"}
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

/**
 * 把 `<think>…</think>` 从正文里拆出来（R333）：Qwen3 系列把思考过程直接写在
 * content 里，原样渲染会在气泡里露出一大段 `<think>` + 推理文字，回答反而看不见。
 * 未闭合（流式进行中）时，余下全部算思考；结论先出、思考后到的模型也不会误判。
 */
export function splitThinking(text: string): { thinking: string | null; answer: string } {
  const open = text.indexOf("<think>");
  if (open < 0) return { thinking: null, answer: text };
  const rest = text.slice(open + "<think>".length);
  const close = rest.indexOf("</think>");
  const thinking = close < 0 ? rest : rest.slice(0, close);
  const after = close < 0 ? "" : rest.slice(close + "</think>".length);
  return { thinking, answer: (text.slice(0, open) + after).trim() };
}

function Thinking({ text, answer }: { text: string; answer: string }) {
  return (
    <details className="thinking" open={answer.trim() === ""}>
      <summary>思考过程</summary>
      <div className="thinking-body">{text}</div>
    </details>
  );
}

function BlockView({
  block,
  plain,
  onRespond,
  onOpenLink,
  onRevealPath,
}: {
  block: UiBlock;
  plain: boolean;
  onRespond: (id: string, d: Decision) => void;
  onOpenLink?: (url: string) => void;
  onRevealPath?: (path: string) => void;
}) {
  switch (block.kind) {
    case "text": {
      // 要求 2：正在流式的那一块按纯文本渲染 —— 反复解析半截语法就是闪烁的来源。
      // R333：思考过程单独收进可折叠区，正文只留回答。
      const { thinking, answer } = splitThinking(block.text);
      return (
        <>
          {thinking !== null && thinking.trim() !== "" ? (
            <Thinking text={thinking} answer={answer} />
          ) : null}
          {plain ? (
            <pre className="plain">{answer}</pre>
          ) : (
            <div className="md">
              <Markdown
                remarkPlugins={[remarkGfm]}
                rehypePlugins={[rehypeHighlight]}
                components={{
                  a: ({ href, children }) => (
                    <a
                      href={href}
                      rel="noreferrer noopener"
                      draggable={false}
                      onClick={(e) => {
                        // 不让 WebView 导航离开应用；能不能打开由 Rust 的
                        // `external_link::validate_external_url` 决定。
                        e.preventDefault();
                        if (href && /^https?:\/\//i.test(href)) {
                          onOpenLink?.(href);
                        }
                      }}
                      onAuxClick={(e) => {
                        // 中键/aux 点击同样不许开新窗口绕过校验。
                        e.preventDefault();
                        if (href && /^https?:\/\//i.test(href)) {
                          onOpenLink?.(href);
                        }
                      }}
                      onDragStart={(e) => e.preventDefault()}
                    >
                      {children}
                    </a>
                  ),
                }}
              >
                {answer}
              </Markdown>
            </div>
          )}
        </>
      );
    }
    case "image":
      return (
        <img
          className="thumb"
          // 线上的 `mime` 是裸子类型（`"png"` / `"jpeg"`，见 ImageMime 的小写 serde 表示），
          // 不是完整 MIME。少写这个 `image/` 前缀产出的 `data:png;base64,…` 是非法 data URL，
          // 浏览器直接画不出来：症状是「发图那一条气泡里只有文字、重开也还是没图」，
          // 而待发缩略图（Composer 那份拼对了）一切正常，所以极易被误判成后端没存图。
          src={`data:image/${block.mime};base64,${block.data}`}
          alt="图片"
        />
      );
    case "tool":
      return <ToolCardView card={block} onRevealPath={onRevealPath} />;
    case "perm":
      return <PermCardView card={block} onRespond={onRespond} />;
  }
}

function MessageBubbleView({
  turn,
  isLastTurn,
  streaming,
  onRespond,
  onOpenLink,
  onRevealPath,
}: {
  turn: Turn;
  isLastTurn: boolean;
  streaming: boolean;
  onRespond: (id: string, d: Decision) => void;
  onOpenLink?: (url: string) => void;
  onRevealPath?: (path: string) => void;
}) {
  const lastIndexOf = turn.blocks.length - 1;
  // R234：空的助手气泡有两种来路 —— 等首字（该留着并看得出在等）与这一轮已结束
  // （401、中断，该收起）。样式分不出「空且还在跑」和「空且已经死」，所以在这里打标。
  const inFlight = streaming && isLastTurn && turn.role === "assistant";
  return (
    <article
      className={`bubble bubble-${turn.role}`}
      data-testid={`bubble-${turn.role}`}
      data-role={turn.role}
      {...(inFlight ? { "data-streaming": "true" } : {})}
    >
      {turn.blocks.map((b, i) => (
        <BlockView
          key={i}
          block={b}
          plain={streaming && isLastTurn && i === lastIndexOf}
          onRespond={onRespond}
          onOpenLink={onOpenLink}
          onRevealPath={onRevealPath}
        />
      ))}
    </article>
  );
}

/// 历史气泡不随流式帧重渲染：`turn` 对象在 reducer 里只有被改的那一轮才换身份，
/// `onRespond` 是稳定的 `useCallback`。没有这一层，每个 rAF 都会把全部历史
/// Markdown 重新解析一遍（长会话掉帧的主因）。
export const MessageBubble = memo(MessageBubbleView);
