import { useState } from "react";
import type { PendingQuestion } from "../types";

/**
 * G20 提问面板：渲染一道挂起的模型提问 —— 问题文本 + 每个候选一个按钮 +
 * 一个「自定义回答」输入框（后端 `question_respond` 接受任意字符串，自由文本
 * 天然支持；候选为空时输入框就是唯一的应答入口）。
 *
 * 应答本身不在组件里发命令 —— 与 `PermCardView` 同一条分工：组件只调 `onAnswer`，
 * 命令、清除面板与消息流里的本地记录都归 App（只有那里的 `setUi` 能同时动
 * reducer 的状态）。组件是受控的展示件：提交后输入框清空，面板的去留由
 * `pendingQuestion` 这一份数据真相决定。
 */
export function QuestionCard({
  pending,
  onAnswer,
}: {
  pending: PendingQuestion;
  onAnswer: (id: string, choice: string) => void;
}) {
  const [custom, setCustom] = useState("");
  // 空白回答不许提交：后端接受任意字符串，但没有「空回答」这个语义 ——
  // 空串送过去，模型只会收到一句「用户选择了：（空）」。
  const trimmed = custom.trim();
  const submitCustom = () => {
    if (trimmed === "") return;
    onAnswer(pending.id, trimmed);
    setCustom("");
  };
  return (
    <div className="card question" data-testid="question-card">
      <div className="card-head">
        <span className="question-title">模型提问</span>
      </div>
      <p className="question-text">{pending.question}</p>
      {pending.options.length > 0 ? (
        <div className="question-options">
          {pending.options.map((opt) => (
            <button key={opt} type="button" onClick={() => onAnswer(pending.id, opt)}>
              {opt}
            </button>
          ))}
        </div>
      ) : null}
      <div className="question-custom">
        <label htmlFor="question-custom-input">自定义回答</label>
        <input
          id="question-custom-input"
          value={custom}
          onChange={(e) => setCustom(e.target.value)}
          placeholder="也可自由输入回答"
        />
        <button type="button" disabled={trimmed === ""} onClick={submitCustom}>
          提交
        </button>
      </div>
    </div>
  );
}
