import { render, screen, fireEvent } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MessageList } from "./MessageList";
import type { UiState } from "../types";

function state(partial: Partial<UiState>): UiState {
  return {
    turnSeq: 1,
    turns: [],
    streaming: false,
    usage: null,
    fatal: null,
    notices: [],
    toolIndex: {},
    permIndex: {},
    ...partial,
  };
}

describe("MessageList", () => {
  it("renders streaming text without markdown-parsing the in-flight block", () => {
    const s = state({
      streaming: true,
      turnSeq: 1,
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [{ kind: "text", text: "## 半截标题\n未闭合的 **粗" }],
        },
      ],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    // 正在流式的块必须按纯文本渲染，否则反复解析半截语法造成闪烁
    expect(screen.getByText(/半截标题/).textContent).toContain("##");
  });

  it("renders a running tool card with the tool name", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "c1",
              name: "echo",
              risk: "read",
              input: {},
              status: "running",
              preview: "",
              ms: 0,
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/echo/)).toBeInTheDocument();
    expect(screen.getByText(/执行中/)).toBeInTheDocument();
  });

  it("shows elapsed ms and preview once the result lands", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "c1",
              name: "echo",
              risk: "read",
              input: {},
              status: "ok",
              preview: "hi",
              ms: 123,
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText("hi")).toBeInTheDocument();
    expect(screen.getByText(/123\s*ms/)).toBeInTheDocument();
  });

  it("offers allow / allow-for-session / deny on a pending permission card", () => {
    const calls: unknown[] = [];
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "perm",
              id: "p1",
              risk: "write",
              state: "pending",
              call: {
                id: "c",
                name: "write_file",
                risk: "write",
                input: { path: "/x" },
              },
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={(id, d) => calls.push([id, d])} />);
    // 一律用 `fireEvent.*`：原生 `element.click()` 也在 React 19 里更新 state，
    // 但不进 `act()` 批次，后面那句「再查一次 DOM」会在重渲染之前跑（实测：警告
    // "An update to PermCardView inside a test was not wrapped in act(...)"，断言拿到 null）。
    fireEvent.click(screen.getByRole("button", { name: /仅本次/ }));
    expect(calls[0]).toEqual(["p1", { allow: true, scope: "once" }]);
  });

  it("disables permission buttons once resolved and shows the reason when denied", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "perm",
              id: "p1",
              risk: "write",
              state: "denied",
              reason: "不想让你写",
              call: { id: "c", name: "write_file", risk: "write", input: {} },
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/不想让你写/)).toBeInTheDocument();
    for (const b of screen.getAllByRole("button")) expect(b).toBeDisabled();
  });

  it("renders a fatal banner above everything else", () => {
    const s = state({ fatal: { message: "无法连接模型端点", where: "model" } });
    const { container } = render(<MessageList state={s} onRespond={() => {}} />);
    const alert = screen.getByRole("alert");
    expect(alert).toHaveTextContent("无法连接模型端点");
    // 「哪儿炸了」要说成人话：`where` 是 `ErrorWhere` 镜像（events.rs:103-111），
    // 直接把 `model` 印出来等于让用户读枚举。
    expect(alert).toHaveTextContent("模型端点");
    // 排在最前不是装饰：先看见「这一轮已经废了」，再读气泡。
    const list = container.querySelector('[data-testid="message-list"]');
    expect(list?.firstElementChild).toBe(alert);
  });

  it("renders notices as inline separators, including the compacted one", () => {
    const s = state({ notices: ["已压缩早期对话：省略 12 条，摘要 300 字"] });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/已压缩早期对话/)).toBeInTheDocument();
  });

  it("renders no inline style attribute, the only place a component could hide a color", () => {
    // 评审 #29：这条过去断的是「`container.outerHTML` 里没有 `#rrggbb`」，而它按构造不可能红 ——
    // 类规则不会被 jsdom 内联进 DOM，把 `styles.css` 的 `var()` 全换成 hex 它照样绿；
    // 组件真写了 `style={{ color: "#f00" }}`，CSSOM 序列化回来是 `rgb(255, 0, 0)`，`#` 也抓不到。
    // 现在钉的是结构：这条渲染路径上不产出 `style` 属性，颜色一律来自样式表里的 token。
    // 样式表与 TSX 源码那两层由 `tokens.test.ts` 的两条源码闸守着（CSS 一条、TS 一条）。
    const s = state({
      turns: [
        { id: 1, role: "assistant", blocks: [{ kind: "text", text: "x" }] },
      ],
    });
    const { container } = render(<MessageList state={s} onRespond={() => {}} />);
    expect(container.querySelector("[style]")).toBeNull();
  });

  // ---- 以下是 Step 0 裁定要求补的断言（第 9 条 / 第 10 条 / 要求 3、7、11） ----

  // 第 9 条 = 硬性要求 8：`ms === 0` 来自 `db.rs:370` 的「库里没记这一列」，
  // 重放路径恒为 0。显示成「0 ms」是替用户编一个测量值。
  it("renders an unknown duration as a dash instead of inventing zero milliseconds", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "call-unknown",
              name: "echo",
              risk: "read",
              input: {},
              status: "ok",
              preview: "done",
              ms: 0,
            },
          ],
        },
      ],
    });
    const { container } = render(
      <MessageList state={s} onRespond={() => {}} />,
    );
    expect(container.textContent).toContain("—");
    expect(container.textContent).not.toMatch(/0\s*ms/);
    // 「不许出现 0」这条要能被 123 那一支反证，所以同一轮里再喂一次有值的卡。
    const withMs = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "call-known",
              name: "echo",
              risk: "read",
              input: {},
              status: "ok",
              preview: "done",
              ms: 7,
            },
          ],
        },
      ],
    });
    const { container: c2 } = render(
      <MessageList state={withMs} onRespond={() => {}} />,
    );
    expect(c2.textContent).toContain("7 ms");
    expect(c2.textContent).not.toContain("—");
  });

  // 第 10 条 = 硬性要求 7 的渲染侧：助手文本不许落进用户气泡。
  it("keeps an assistant text block out of the user bubble", () => {
    const s = state({
      turnSeq: 2,
      turns: [
        { id: 1, role: "user", blocks: [{ kind: "text", text: "我说的话" }] },
        {
          id: 2,
          role: "assistant",
          blocks: [{ kind: "text", text: "模型的回答" }],
        },
      ],
    });
    const { container } = render(
      <MessageList state={s} onRespond={() => {}} />,
    );
    const user = container.querySelector('[data-role="user"]');
    const assistant = container.querySelector('[data-role="assistant"]');
    expect(user?.textContent).toBe("我说的话");
    expect(assistant?.textContent).toBe("模型的回答");
  });

  // 要求 3：拒绝必须能在卡片里填理由，而且用的是行内 <input>（第 8 条红线：绝不用 prompt()）。
  it("collects a deny reason through an inline input instead of a popup", () => {
    const calls: unknown[] = [];
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "perm",
              id: "p1",
              risk: "write",
              state: "pending",
              call: { id: "c", name: "write_file", risk: "write", input: {} },
            },
          ],
        },
      ],
    });
    const { container } = render(
      <MessageList state={s} onRespond={(id, d) => calls.push([id, d])} />,
    );
    expect(container.querySelector("input")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /拒绝/ }));
    const input = container.querySelector("input") as HTMLInputElement;
    expect(input).not.toBeNull();

    fireEvent.change(input, { target: { value: "别删我的文件" } });
    fireEvent.click(screen.getByRole("button", { name: /提交拒绝理由/ }));
    expect(calls).toEqual([["p1", { allow: false, reason: "别删我的文件" }]]);
  });

  it("falls back to the default deny wording when the reason is left blank", () => {
    const calls: unknown[] = [];
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "perm",
              id: "p9",
              risk: "exec",
              state: "pending",
              call: { id: "c", name: "run", risk: "exec", input: {} },
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={(id, d) => calls.push([id, d])} />);
    fireEvent.click(screen.getByRole("button", { name: /拒绝/ }));
    fireEvent.click(screen.getByRole("button", { name: /提交拒绝理由/ }));
    expect(calls).toEqual([["p9", { allow: false, reason: "用户拒绝" }]]);
  });

  // 要求 11：`notices` 只增不减（T14 评审 F5 的交接），折叠归渲染层。
  it("collapses repeated notices into one line with a count", () => {
    const s = state({ notices: ["工具炸了", "工具炸了", "工具炸了"] });
    const { container } = render(
      <MessageList state={s} onRespond={() => {}} />,
    );
    expect(screen.getAllByText(/工具炸了/)).toHaveLength(1);
    expect(container.textContent).toContain("×3");
  });

  it("collapses a long notice list behind an expander instead of stacking banners", () => {
    const notices = Array.from({ length: 9 }, (_, i) => `提示 ${i + 1}`);
    const { container } = render(
      <MessageList state={state({ notices })} onRespond={() => {}} />,
    );
    // 折叠状态只露最近 5 条，其余必须真的不在 DOM 里（不是靠 CSS 藏起来）。
    expect(screen.queryByText(/提示 1\b/)).toBeNull();
    expect(screen.getByText(/提示 9/)).toBeInTheDocument();
    const shown = container.querySelectorAll('[data-testid="notice"]');
    expect(shown).toHaveLength(5);
    fireEvent.click(screen.getByRole("button", { name: /展开/ }));
    expect(
      container.querySelectorAll('[data-testid="notice"]'),
    ).toHaveLength(9);
  });

  // 第 8 条红线：`prompt` 在 WebView2 里返回 null ⇒ 理由永远发不出去，那一笔权限卡死、
  // 整轮静默挂住；`alert`/`confirm` 同理。判据不是「扫源码里的字」—— 扫源码会被注释里的
  // 字面量绊倒（本仓的注释恰好就在讨论这几个函数），所以 `setupTests.ts` 把三个函数换成
  // **一调用就抛**，任何组件在任何用例里弹窗都会当场红。这一条钉的是那道绊线本身还在：
  // 把 setupTests.ts 里那段删掉，这一条立刻红。
  it("fails loudly the moment a component tries to open a popup", () => {
    const w = window as unknown as {
      alert: () => void;
      confirm: () => void;
      prompt: () => void;
    };
    expect(() => w.alert()).toThrow(/零弹窗|不许/);
    expect(() => w.confirm()).toThrow(/零弹窗|不许/);
    expect(() => w.prompt()).toThrow(/零弹窗|不许/);
  });
});
