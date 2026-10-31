import { render, screen, fireEvent } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MessageList } from "./MessageList";
import { readablePreview } from "./MessageBubble";
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
  // 真窗口实测（R234）：发一条消息后 `.bubble-assistant` 立刻存在而且是空的，
  // 质感层给它加了内边距与投影，于是屏幕左侧浮出一个没有任何含义的小胶囊。
  // 同一个空元素在「正在生成」和「这一轮已经死了（401/中断）」两种状态下长得一模一样，
  // CSS 分不开，所以由这里打一个标记，样式才有依据只留前者、藏后者。
  it("marks the in-flight empty assistant bubble as streaming", () => {
    const s = state({
      streaming: true,
      turns: [{ id: 1, role: "assistant", blocks: [] }],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    const bubble = screen.getByTestId("bubble-assistant");
    expect(bubble.textContent).toBe("");
    expect(bubble).toHaveAttribute("data-streaming", "true");
  });

  // 链接点击必须交给外部打开命令，而不是让 WebView 导航走（远程页会替换应用页面）。
  it("routes markdown links through onOpenLink instead of navigating", () => {
    const onOpenLink = vi.fn();
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            { kind: "text", text: "见 [链接](https://example.com/doc)" },
          ],
        },
      ],
    });
    render(
      <MessageList
        state={s}
        onRespond={() => {}}
        onOpenLink={onOpenLink}
      />,
    );
    const notPrevented = fireEvent.click(screen.getByText("链接"));
    expect(notPrevented).toBe(false); // preventDefault 确实被调用：WebView 不会导航
    expect(onOpenLink).toHaveBeenCalledWith("https://example.com/doc");
  });

  // 非 http(s) 链接不交给 Rust 的 open_url（那里只认 http/https）；
  // 在这里就拦下，免得每点一次 `#锚点`/`mailto:` 都弹一条「打开链接失败」。
  it("does not hand non-http links to the opener", () => {
    const onOpenLink = vi.fn();
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [{ kind: "text", text: "写信：[邮件](mailto:a@b.c)" }],
        },
      ],
    });
    render(
      <MessageList
        state={s}
        onRespond={() => {}}
        onOpenLink={onOpenLink}
      />,
    );
    fireEvent.click(screen.getByText("邮件"));
    expect(onOpenLink).not.toHaveBeenCalled();
  });

  it("drops the streaming mark when the turn is no longer running", () => {
    const s = state({
      streaming: false,
      turns: [{ id: 1, role: "assistant", blocks: [] }],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByTestId("bubble-assistant")).not.toHaveAttribute(
      "data-streaming",
    );
  });

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
              images: [],
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
              images: [],
              ms: 123,
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText("hi")).toBeInTheDocument();
    expect(screen.getByText(/123\s*ms/)).toBeInTheDocument();
    expect(screen.getByTestId("tool-c1").querySelector("img")).toBeNull();
  });

  // T27（更正 2）：卡片本体 Plan A T16 就交付了，这里只补**没人钉过**的那一半 ——
  // 失败态。需求本身（「marks an errored result without hiding the message」）是对的，
  // 组件也早就做对了，缺的只是证据。
  it("labels an errored tool card 失败 and keeps the error message visible", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "c1",
              name: "fetch_url",
              risk: "read",
              input: { url: "https://a.test" },
              status: "error",
              preview: "上游返回 403，内容可能受限",
              images: [],
              ms: 812,
            },
          ],
        },
      ],
    });
    render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText(/fetch_url/)).toBeInTheDocument();
    expect(screen.getByText("失败")).toBeInTheDocument();
    // 这条的重点在后半句：错误态最容易被人顺手写成 `!isError && preview`，把「红在哪儿」
    // 一起藏掉 —— 一张说不出原因的红卡比不显示更糟。逐字断整句，不放宽成 /403/。
    expect(screen.getByText("上游返回 403，内容可能受限")).toBeInTheDocument();
    // 「完成」不许同时出现：状态标签只有一支，出现两支就是状态投影错了。
    expect(screen.queryByText("完成")).toBeNull();
  });

  // 第 9 条（R161）在**失败卡**这一头：重放路径的 `ms` 恒为 0（`db.rs:370` 没记这一列），
  // 而用户最常看到失败卡的时刻恰恰是重放 —— 上一轮炸了、重开会话来查看结果。
  // 上面 `renders an unknown duration as a dash instead of inventing zero milliseconds`
  // 钉的是**成功**卡，这一条钉的是「失败 + 没有测量」这个真实组合。
  // 账面要写清（报告 §6）：`fmtMs` 的主变异（改成 `${ms} ms`）会让两条**一起**红，
  // 所以这一条的增量在那个组合，而不是「破折号」这条判据第一次被钉住。
  it("shows a replayed failed card with a dash instead of 0 ms", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "c2",
              name: "read_file",
              risk: "read",
              input: { path: "D:\\notes\\a.md" },
              status: "error",
              preview: "只接受绝对路径",
              images: [],
              ms: 0,
            },
          ],
        },
      ],
    });
    const { container } = render(<MessageList state={s} onRespond={() => {}} />);
    expect(screen.getByText("失败")).toBeInTheDocument();
    expect(container.textContent).toContain("—");
    expect(container.textContent).not.toMatch(/0\s*ms/);
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
              images: [],
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
              images: [],
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

  it("renders images returned by a tool inside the tool card", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "c1",
              name: "mcp__shot__capture",
              risk: "read",
              input: {},
              status: "ok",
              preview: "[图片 png]\n[另有 1 张图片未在卡片中显示]",
              ms: 12,
              images: [
                {
                  data: "QUJD",
                  mime: "png",
                  path: "C:\\cache\\pic.png",
                },
              ],
            },
          ],
        },
      ],
    });
    const onRevealPath = vi.fn();
    const { container } = render(
      <MessageList
        state={s}
        onRespond={() => {}}
        onRevealPath={onRevealPath}
      />,
    );
    const img = container.querySelector(
      '[data-testid="tool-c1"] img',
    ) as HTMLImageElement | null;
    expect(img).not.toBeNull();
    expect(img?.src).toBe("data:image/png;base64,QUJD");
    expect(img?.alt).toBe("工具返回的图片");
    expect(container.textContent).toContain("另有 1 张图片未在卡片中显示");
    fireEvent.click(screen.getByRole("button", { name: "C:\\cache\\pic.png" }));
    expect(onRevealPath).toHaveBeenCalledWith("C:\\cache\\pic.png");
  });

  it("shows a clickable path for tools with a file argument", () => {
    const onRevealPath = vi.fn();
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "tool",
              callId: "c1",
              name: "write_file",
              risk: "write",
              input: { path: "C:\\notes\\周报.md", content: "x" },
              status: "ok",
              preview: "已写入",
              images: [],
              ms: 5,
            },
          ],
        },
      ],
    });
    render(
      <MessageList
        state={s}
        onRespond={() => {}}
        onRevealPath={onRevealPath}
      />,
    );
    fireEvent.click(
      screen.getByRole("button", { name: "C:\\notes\\周报.md" }),
    );
    expect(onRevealPath).toHaveBeenCalledWith("C:\\notes\\周报.md");
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

  it("previews the path and full content of a write_file request", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "perm",
              id: "w1",
              risk: "write",
              state: "pending",
              call: {
                id: "c",
                name: "write_file",
                risk: "write",
                input: {
                  path: "C:\\notes\\周报.md",
                  content: "第一行\n第二行",
                },
              },
            },
          ],
        },
      ],
    });
    const { container } = render(
      <MessageList state={s} onRespond={() => {}} />,
    );
    expect(screen.getByTestId("write-preview").textContent).toContain(
      "C:\\notes\\周报.md",
    );
    expect(container.textContent).toContain("第一行");
    expect(container.textContent).toContain("第二行");
    expect(container.textContent).not.toContain('"content"');
    expect(screen.getByRole("button", { name: "仅本次" })).toBeEnabled();
    expect(
      screen.getByRole("button", { name: "本会话内该目录都允许" }),
    ).toBeEnabled();
    expect(screen.getByLabelText("写入内容预览")).toHaveAttribute(
      "tabindex",
      "0",
    );
  });

  it("previews both sides of an edit_file request", () => {
    const s = state({
      turns: [
        {
          id: 1,
          role: "assistant",
          blocks: [
            {
              kind: "perm",
              id: "e1",
              risk: "write",
              state: "pending",
              call: {
                id: "c",
                name: "edit_file",
                risk: "write",
                input: {
                  path: "C:\\notes\\周报.md",
                  old_string: "旧的一行",
                  new_string: "新的一行",
                },
              },
            },
          ],
        },
      ],
    });
    const { container } = render(
      <MessageList state={s} onRespond={() => {}} />,
    );
    expect(container.textContent).toContain("C:\\notes\\周报.md");
    expect(container.textContent).toContain("替换前");
    expect(container.textContent).toContain("旧的一行");
    expect(container.textContent).toContain("替换后");
    expect(container.textContent).toContain("新的一行");
    expect(container.textContent).not.toContain('"old_string"');
    expect(screen.getByLabelText("替换前内容")).toHaveAttribute("tabindex", "0");
    expect(screen.getByLabelText("替换后内容")).toHaveAttribute("tabindex", "0");
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

describe("MessageList 自动滚底", () => {
  const withTurn = (id: number) =>
    state({ turns: [{ id, role: "assistant", blocks: [] }] });

  it("贴底时跟随新内容，用户上翻后不再抢滚动，回到底部又黏住", () => {
    const { rerender } = render(<MessageList state={withTurn(1)} onRespond={() => {}} />);
    const list = screen.getByTestId("message-list");
    Object.defineProperty(list, "scrollHeight", { value: 1000, configurable: true });
    Object.defineProperty(list, "clientHeight", { value: 400, configurable: true });

    // 首次渲染之后的那次检查把它带到最底（jsdom 不做布局，这里是纯赋值判据）。
    rerender(<MessageList state={withTurn(1)} onRespond={() => {}} />);
    expect(list.scrollTop).toBe(1000);

    // 用户往上翻到距底 500px：再来内容也不许把他拽回去。
    list.scrollTop = 100;
    fireEvent.scroll(list);
    rerender(<MessageList state={withTurn(2)} onRespond={() => {}} />);
    expect(list.scrollTop).toBe(100);

    // 自己回到底部（距底 -300 < 40 ⇒ 算贴底）之后恢复跟随。
    list.scrollTop = 900;
    fireEvent.scroll(list);
    rerender(<MessageList state={withTurn(3)} onRespond={() => {}} />);
    expect(list.scrollTop).toBe(1000);
  });
});

describe("工具卡预览（R339）", () => {
  it("hides the extraction notice from the UI but keeps the body text", () => {
    const NL = String.fromCharCode(10);
    const raw = [
      "[未能定界出正文，以下 156 字可能是站点样板或反爬提示，别当正文引用]",
      "This domain is for use in documentation examples.",
    ].join(NL);
    expect(readablePreview(raw)).toBe("This domain is for use in documentation examples.");
    expect(
      readablePreview("[未能定界出正文，以下 0 字可能是站点样板或反爬提示，别当正文引用]"),
    ).toBe("");
  });
});
