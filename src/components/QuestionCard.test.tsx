import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { QuestionCard } from "./QuestionCard";
import type { PendingQuestion } from "../types";

const q = (p: Partial<PendingQuestion> = {}): PendingQuestion => ({
  id: "q1",
  question: "选哪个方案？",
  options: ["方案一", "方案二"],
  ...p,
});

// 组件层的分工：只渲染 + 只调 `onAnswer`，命令与状态清除归 App（AppQuestion.test.tsx）。
describe("QuestionCard", () => {
  it("renders the question text and one button per option", () => {
    render(<QuestionCard pending={q()} onAnswer={vi.fn()} />);
    expect(screen.getByText("选哪个方案？")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "方案一" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "方案二" })).toBeInTheDocument();
  });

  it("answers with the clicked option", () => {
    const onAnswer = vi.fn();
    render(<QuestionCard pending={q()} onAnswer={onAnswer} />);
    fireEvent.click(screen.getByRole("button", { name: "方案二" }));
    expect(onAnswer).toHaveBeenCalledTimes(1);
    expect(onAnswer).toHaveBeenCalledWith("q1", "方案二");
  });

  it("submits a custom answer trimmed, and only when it is non-empty", () => {
    const onAnswer = vi.fn();
    render(<QuestionCard pending={q({ options: [] })} onAnswer={onAnswer} />);
    const input = screen.getByLabelText("自定义回答");
    // 空白回答不许提交：后端接受任意字符串，但没有「空回答」这个语义。
    expect(screen.getByRole("button", { name: "提交" })).toBeDisabled();
    fireEvent.change(input, { target: { value: "  都行，按你的来  " } });
    expect(screen.getByRole("button", { name: "提交" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "提交" }));
    expect(onAnswer).toHaveBeenCalledTimes(1);
    expect(onAnswer).toHaveBeenCalledWith("q1", "都行，按你的来");
    // 受控行为：提交后输入框清空。面板真正消失由 `pendingQuestion` 决定（App 那侧）。
    expect(input).toHaveValue("");
  });

  it("offers only the custom input when the question carries no options", () => {
    render(<QuestionCard pending={q({ options: [] })} onAnswer={vi.fn()} />);
    expect(screen.getByLabelText("自定义回答")).toBeInTheDocument();
    // 除了「提交」之外没有任何候选按钮。
    expect(screen.getAllByRole("button")).toHaveLength(1);
  });
});
