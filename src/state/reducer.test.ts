import { describe, expect, it, vi } from "vitest";
import { AGENT_CHANNEL } from "../types";
import {
  appendUserTurn,
  initialState,
  reduce,
  replay,
  selectActiveTurn,
} from "./reducer";
import type {
  AgentEvent,
  AgentPayload,
  ContentBlock,
  PermCard,
  Risk,
  ToolCallView,
  ToolCard,
  UiBlock,
  UiState,
} from "../types";

const d = (text: string): AgentEvent => ({ t: "text_delta", text });
const start = (turn: number): AgentEvent => ({ t: "turn_start", turn });
const done = (stop_reason: "end_turn" | "max_turns" | "interrupted" | "error" = "end_turn"): AgentEvent => ({
  t: "done",
  stop_reason,
});
const call = (id: string, risk: Risk = "read"): ToolCallView => ({
  id,
  name: risk === "read" ? "fs.read" : "fs.write",
  risk,
  input: { path: `/${id}` },
});
const toolCall = (id: string, risk: Risk = "read"): AgentEvent => ({
  t: "tool_call",
  risk,
  call: call(id, risk),
});
const toolResult = (
  call_id: string,
  preview: string,
  is_error = false,
  ms = 12,
): AgentEvent => ({ t: "tool_result", call_id, is_error, ms, preview });
const permRequest = (id: string, risk: Risk = "write"): AgentEvent => ({
  t: "permission_request",
  id,
  risk,
  call: call(id, risk),
});
// 收窄断言用 `types.ts` 的具名别名，不再在本文件重抄 `Extract<UiBlock, ...>`（评审 F3）。
const tool = (b: UiBlock | undefined): ToolCard => {
  if (!b || b.kind !== "tool") throw new Error("期望一个 tool 卡片");
  return b;
};
const perm = (b: UiBlock | undefined): PermCard => {
  if (!b || b.kind !== "perm") throw new Error("期望一个 perm 卡片");
  return b;
};
function deepFreeze<T>(value: T): T {
  if (value !== null && typeof value === "object") {
    for (const key of Object.getOwnPropertyNames(value)) {
      deepFreeze((value as Record<string, unknown>)[key]);
    }
  }
  return value;
}

/**
 * 读仓库源码用的最小 `node:fs` 具名。本仓库**没有** `@types/node`，也不许安装，
 * 于是静态 `import { readFileSync } from "node:fs"` 会报 TS2591 并打断 `npm run build`（实测）。
 * 把说明符宽化成 `string` 让 tsc 不去解析它（结果是 `Promise<any>`），再在本地把用到的
 * 那一个函数标注出来 —— 类型仍然在本文件里闭环，运行期由 node 正常解析 `node:fs`。
 */
async function readSource(relative: string): Promise<string> {
  const fs = (await import("node:fs" as string)) as {
    readFileSync(path: URL, encoding: "utf8"): string;
  };
  // 相对起点是本文件 src/state/reducer.test.ts ⇒ 仓库根是 **../../**，不是 ../../../。
  return fs.readFileSync(new URL(relative, import.meta.url), "utf8");
}

/**
 * 剥掉注释、只留正文（评审 F2② 的坑）：`reducer.ts` 的纪律注释里就**明写**着
 * 「不用 Date.now()、不用 Math.random()」，naive 的 `not.toContain` 会在干净代码上直接红。
 * 逐字符走一遍，行注释丢到行尾（换行保留，免得把两行并成一行造出假 token）、
 * 块注释整段丢掉，字符串与模板字面量原样保留。
 */
function stripComments(src: string): string {
  let out = "";
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const n = src[i + 1];
    if (c === "/" && n === "/") {
      while (i < src.length && src[i] !== "\n") i++;
      continue;
    }
    if (c === "/" && n === "*") {
      i += 2;
      while (i < src.length && !(src[i] === "*" && src[i + 1] === "/")) i++;
      i += 2;
      continue;
    }
    if (c === '"' || c === "'" || c === "`") {
      out += c;
      i++;
      while (i < src.length) {
        const s = src[i];
        if (s === "\\") {
          out += s + (src[i + 1] ?? "");
          i += 2;
          continue;
        }
        out += s;
        i++;
        if (s === c) break;
      }
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

/**
 * 表驱动用例的输入：`AgentEvent` 的每一支至少给一条能推进状态的事件。
 *
 * 类型写成 `Record<AgentEvent["t"], AgentEvent>` 是这套守卫的核心（评审 F1③）：
 * 名单**不再手抄**，而是 `Object.keys(probe)` 从镜像自己推出来。于是
 * `types.ts` 的 union 多了一支而这里忘了补键 ⇒ 缺键直接 TS2741，`tsc` 红。
 * 键的插入顺序按 events.rs:400-412 排，`Object.keys` 保序，所以下面那条跨语言用例
 * 拿到的就是「按 Rust 名单顺序的镜像标签表」。
 */
const probe: Record<AgentEvent["t"], AgentEvent> = {
  turn_start: start(3),
  text_delta: d("继续"),
  user_message: { t: "user_message", blocks: [{ type: "text", text: "说一句" }] },
  tool_call: toolCall("c2"),
  permission_request: permRequest("p2"),
  permission_resolved: { t: "permission_resolved", id: "p1", decision: { allow: true, scope: "once" } },
  tool_result: toolResult("c1", "结果"),
  usage: { t: "usage", input_tokens: 1, output_tokens: 2 },
  context_compacted: { t: "context_compacted", dropped: 4, summary_chars: 120 },
  error: { t: "error", where: "loop", message: "循环了", fatal: false },
  done: done(),
};

/** 由联合类型派生的标签名单（不是抄来的字面量）；与 `events.rs` 的一致性由机器检查保证。 */
const WIRE_TAGS = Object.keys(probe) as AgentEvent["t"][];

/**
 * `ContentBlock` 同法（评审 F1②/F1③ 的姊妹手法）：四支由 `ContentBlock["type"]` 派生，
 * `types.rs` 加第 5 支时这里缺键就 TS2741，而 `reducer.ts` 的 `userBlocks` 守卫同时 TS2322。
 */
const contentProbe: Record<ContentBlock["type"], ContentBlock> = {
  text: { type: "text", text: "甲" },
  image: { type: "image", data: "AA", mime: "png" },
  tool_call: { type: "tool_call", id: "c1", name: "fs.read", input: {} },
  tool_result: {
    type: "tool_result",
    call_id: "c1",
    is_error: false,
    output: { kind: "text", text: "乙" },
  },
};
const CONTENT_TAGS = Object.keys(contentProbe) as ContentBlock["type"][];

describe("reducer", () => {
  // ==== brief Step 1 的 11 条 ====

  it("merges consecutive text deltas into one block", () => {
    let s = initialState();
    s = reduce(s, start(1));
    for (const ch of "你好世界") s = reduce(s, d(ch));
    const turn = selectActiveTurn(s)!;
    expect(turn.blocks).toHaveLength(1);
    expect(turn.blocks[0]).toEqual({ kind: "text", text: "你好世界" });
  });

  it("starts a new text block after a tool card so timeline order survives", () => {
    let s = initialState();
    s = reduce(s, start(1));
    s = reduce(s, d("前半"));
    s = reduce(s, toolCall("c1"));
    s = reduce(s, d("后半"));
    const kinds = selectActiveTurn(s)!.blocks.map((b) => b.kind);
    expect(kinds).toEqual(["text", "tool", "text"]);
    expect(s.toolIndex.c1).toBeDefined();
  });

  it("backfills tool result by call_id into the existing card", () => {
    let s = initialState();
    s = reduce(s, start(1));
    s = reduce(s, toolCall("c1"));
    s = reduce(s, toolResult("c1", "hi", false, 12));
    expect(tool(selectActiveTurn(s)!.blocks[0]).status).toBe("ok");
    expect(tool(selectActiveTurn(s)!.blocks[0]).preview).toBe("hi");
    expect(tool(selectActiveTurn(s)!.blocks[0]).ms).toBe(12);
  });

  it("marks an error tool result without dropping the card", () => {
    let s = reduce(initialState(), start(1));
    s = reduce(s, toolCall("x"));
    s = reduce(s, toolResult("x", "拒绝", true, 3));
    expect(tool(selectActiveTurn(s)!.blocks[0]).status).toBe("error");
    expect(s.notices.some((n) => n.includes("拒绝"))).toBe(true);
  });

  it("shows a pending permission card and resolves it by id", () => {
    let s = reduce(initialState(), start(1));
    s = reduce(s, permRequest("p1"));
    expect(selectActiveTurn(s)!.blocks[0].kind).toBe("perm");
    s = reduce(s, { t: "permission_resolved", id: "p1", decision: { allow: true, scope: "session" } });
    expect(perm(selectActiveTurn(s)!.blocks[0]).state).toBe("allowed");
  });

  it("refuses to open the composer on done, whatever the reason", () => {
    for (const r of ["end_turn", "max_turns", "interrupted", "error"] as const) {
      let s = reduce(initialState(), start(1));
      expect(s.streaming).toBe(true);
      s = reduce(s, { t: "done", stop_reason: r });
      expect(s.streaming).toBe(false);
    }
  });

  it("surfaces max_turns as an explicit notice, not a silent stop", () => {
    const s = reduce(initialState(), done("max_turns"));
    expect(s.notices.join()).toContain("轮数上限");
    // R116：25 是 Rust 的 SessionConfig 常量，不许在前端复述。
    expect(s.notices.join()).not.toMatch(/25/);
  });

  it("separates fatal errors onto a banner, non-fatal inside the turn", () => {
    const s = reduce(initialState(), { t: "error", where: "model", message: "429", fatal: true });
    expect(s.fatal?.message).toBe("429");
    const s2 = reduce(initialState(), { t: "error", where: "tool", message: "越界", fatal: false });
    expect(s2.fatal).toBeNull();
    expect(s2.notices.join()).toContain("越界");
  });

  it("records usage for the status bar", () => {
    const s = reduce(initialState(), { t: "usage", input_tokens: 120, output_tokens: 30 });
    expect(s.usage).toEqual({ input: 120, output: 30 });
  });

  it("replays a persisted history into the same shape a live session produced", () => {
    const live: AgentEvent[] = [
      start(0),
      d("查一下"),
      start(0), // R112：每个模型往返发一次 turn_start，必须各起一个 turn
      d("好的"),
      toolCall("c"),
      toolResult("c", "结果", false, 0),
      done(),
    ];
    const s = replay(live);
    expect(s.turns).toHaveLength(2);
    expect(s.turns[1].blocks.map((b) => b.kind)).toEqual(["text", "tool"]);
    expect(tool(s.turns[1].blocks[1]).preview).toBe("结果");
    expect(s.turns.every((t) => t.role === "assistant")).toBe(true);
  });

  it("ignores an unknown event shape instead of crashing the app", () => {
    const before = initialState();
    const s = reduce(before, { t: "brand_new_event" } as unknown as AgentEvent);
    expect(s.turns).toEqual([]);
    // R118：default 只许兜住「线上还不存在」的变体，且要原样返回。
    expect(s).toBe(before);
  });

  // ==== R111：user_message 与 role ====

  it("gives user_message its own user turn and keeps assistant text out of it", () => {
    let s = reduce(initialState(), {
      t: "user_message",
      blocks: [{ type: "text", text: "帮我读文件" }],
    });
    expect(s.turns).toHaveLength(1);
    expect(s.turns[0].role).toBe("user");
    expect(s.turns[0].blocks).toEqual([{ kind: "text", text: "帮我读文件" }]);
    s = reduce(s, start(1));
    expect(s.turns).toHaveLength(2);
    expect(s.turns[1].role).toBe("assistant");
    s = reduce(s, d("好的"));
    expect(s.turns[0].blocks).toHaveLength(1); // 用户气泡里不会长出助手文本
    expect(s.turns[1].blocks).toEqual([{ kind: "text", text: "好的" }]);
  });

  it("routes assistant content away from a user bubble even while still streaming", () => {
    // R112 的另一半：归位闸门要看「最后一格是不是助手气泡」，不能看 streaming。
    // 重放整段历史时中途从没有 done（db.rs:419 只在末尾发一次），所以 user_message
    // 到达时 streaming 一直是真；按 streaming 归位就会把后面的助手文本并进用户气泡。
    const s = replay([
      start(1),
      d("第一轮回答"),
      { t: "user_message", blocks: [{ type: "text", text: "再查一个" }] },
      start(2),
      toolCall("c1"),
      toolResult("c1", "答案"),
    ]);
    expect(s.streaming).toBe(true);
    expect(s.turns.map((t) => t.role)).toEqual(["assistant", "user", "assistant"]);
    expect(s.turns[1].blocks).toEqual([{ kind: "text", text: "再查一个" }]);
    expect(tool(s.turns[2].blocks[0]).preview).toBe("答案");
    // 异常次序（用户气泡之后没有 turn_start 就来了 text_delta）也不许污染那条用户气泡
    const midStream = replay([
      start(1),
      { t: "user_message", blocks: [{ type: "text", text: "只说一句" }] },
    ]);
    expect(midStream.streaming).toBe(true); // streaming 是 turn_start 留的，user_message 没碰它
    expect(midStream.turns.map((t) => t.role)).toEqual(["assistant", "user"]);
    const odd = reduce(midStream, d("没人发 turn_start 的回答"));
    expect(odd.turns).toHaveLength(3);
    expect(odd.turns[2].role).toBe("assistant");
    expect(odd.turns[2].blocks).toEqual([{ kind: "text", text: "没人发 turn_start 的回答" }]);
    expect(odd.turns[1].blocks).toEqual([{ kind: "text", text: "只说一句" }]);
  });

  it("maps only the text and image ContentBlock arms into a user turn", () => {
    const blocks: ContentBlock[] = [
      { type: "text", text: "看这张图" },
      { type: "image", data: "iVBOR", mime: "png" },
      // R111：User 行里的 tool_call 块 Plan A 不可达，前端不渲染、也不为它发明分支。
      { type: "tool_call", id: "c9", name: "fs.read", input: {} },
      {
        type: "tool_result",
        call_id: "c9",
        is_error: false,
        output: { kind: "text", text: "不该出现在用户气泡里" },
      },
    ];
    const s = appendUserTurn(initialState(), blocks);
    expect(s.turns[0].blocks).toEqual([
      { kind: "text", text: "看这张图" },
      { kind: "image", data: "iVBOR", mime: "png" },
    ]);
    expect(s.toolIndex.c9).toBeUndefined();
  });

  // ==== R112/R113：turn 边界与身份 ====

  it("opens a second turn for the second turn_start even mid-stream", () => {
    let s = reduce(initialState(), start(1));
    s = reduce(s, d("第一段"));
    expect(s.streaming).toBe(true);
    s = reduce(s, start(2)); // 直播时 streaming 一直是真，这里必须新起一 turn
    expect(s.turns).toHaveLength(2);
    s = reduce(s, d("第二段"));
    expect(s.turns.map((t) => t.blocks.map((b) => (b.kind === "text" ? b.text : "")))).toEqual([
      ["第一段"],
      ["第二段"],
    ]);
  });

  it("treats e.turn as data, never as an identifier", () => {
    // 两次 run() 都从 turn:1 起（db.rs:385 每次真人发言后重置）。
    const events: AgentEvent[] = [
      { t: "user_message", blocks: [{ type: "text", text: "第一轮" }] },
      start(1),
      toolCall("c1"),
      toolResult("c1", "一"),
      done(),
      { t: "user_message", blocks: [{ type: "text", text: "第二轮" }] },
      start(1),
      toolCall("c2"),
      toolResult("c2", "二"),
      done(),
    ];
    const s = replay(events);
    const assistants = s.turns.filter((t) => t.role === "assistant");
    expect(assistants).toHaveLength(2);
    expect(assistants[0].id).not.toBe(assistants[1].id);
    expect(tool(assistants[0].blocks[0]).preview).toBe("一");
    expect(tool(assistants[1].blocks[0]).preview).toBe("二");
    // 索引存的是 reducer 自己的 turnSeq，不是 e.turn
    expect(s.toolIndex.c1.turnId).toBe(assistants[0].id);
    expect(s.toolIndex.c2.turnId).toBe(assistants[1].id);
  });

  it("produces the same shape whatever numbers e.turn happens to carry", () => {
    // R113 的正向说法：`e.turn` 每次真人发言后从 1 重启（db.rs:385）、每次 run() 也从 1 起
    // （session.rs:119），所以它只能是展示用的数据。改这里让两条历史只差在 `turn` 的取值上，
    // 状态必须一模一样 —— reducer 一旦拿它当标识符，这两条就会分叉。
    const a = replay([start(1), d("甲"), start(1), d("乙"), toolCall("c"), toolResult("c", "果"), done()]);
    const b = replay([start(7), d("甲"), start(1), d("乙"), toolCall("c"), toolResult("c", "果"), done()]);
    expect(b).toEqual(a);
    expect(JSON.stringify(b)).toBe(JSON.stringify(a));
  });

  it("resolves a permission into its own turn, not into the last one", () => {
    let s = replay([
      start(1),
      permRequest("p1"),
      done(),
      { t: "user_message", blocks: [{ type: "text", text: "算了，先不" }] },
      start(1),
      d("新的助手回合"),
    ]);
    const untouched = structuredClone(s.turns[s.turns.length - 1]);
    expect(s.turns).toHaveLength(3);
    s = reduce(s, { t: "permission_resolved", id: "p1", decision: { allow: false, reason: "用户拒绝" } });
    expect(perm(s.turns[0].blocks[0]).state).toBe("denied");
    expect(perm(s.turns[0].blocks[0]).reason).toBe("用户拒绝");
    expect(s.turns[s.turns.length - 1]).toEqual(untouched);
  });

  // ==== R117：parity ====

  it("replays user_message into exactly what the optimistic insert produces", () => {
    const b: ContentBlock[] = [
      { type: "text", text: "你好" },
      { type: "image", data: "AAAA", mime: "jpeg" },
    ];
    expect(replay([{ t: "user_message", blocks: b }])).toEqual(appendUserTurn(initialState(), b));
  });

  // ==== R118：11 个标签表驱动 ====

  it("handles every one of the 11 wire t tags without falling through to default", () => {
    // 名单来自 `Object.keys(probe)`，probe 的类型是 `Record<AgentEvent["t"], AgentEvent>`：
    // 镜像有第 12 支而 probe 没补 → tsc 红；probe 补了而 reducer 没写臂 → reducer 的
    // never 守卫红（tsc）+ 这里的 swallowed 断言红。三层都在（评审 F1③）。
    expect(WIRE_TAGS).toHaveLength(11); // 这个数字锚在 events.rs:358 那条 (11, 11) 断言上
    const base = replay([start(1), d("在写正文"), toolCall("c1"), permRequest("p1")]);
    const swallowed = WIRE_TAGS.filter((tag) => reduce(base, probe[tag]) === base);
    expect(swallowed).toEqual([]);
  });

  it("still mirrors the tag list that events.rs actually serializes", async () => {
    // 评审 F1④：这条才是「Rust 加第 12 支而前端镜像没跟上」的唯一机器判据。
    // 之前 WIRE_TAGS 是手抄字面量，全仓库没有任何一条测试把它和 events.rs 连起来 ——
    // Rust 加一支、镜像不动 ⇒ 测试与 tsc 双双全绿，那条事件在 reduce 的 default 里被静默吞掉。
    const src = await readSource("../../src-tauri/src/agent/events.rs");
    const anchor = "every_agent_event_variant_serializes_its_t_tag";
    // 锚点在文件里必须唯一，否则切片起点就是在赌其中哪一个。
    expect(src.split(anchor).length - 1).toBe(1);

    // 切片**必须有界**：只取 `let tags = [` 到**下一个** `];` 之间的那一段。
    // 从锚点一路切到文件末尾会把后续计划里新增的测试字符串一起吞进来，
    // 于是这条用例会越来越宽，最后谁都读不出它到底在钉什么。
    const openMarker = "let tags = [";
    const open = src.indexOf(openMarker);
    expect(open, "events.rs 里找不到 `let tags = [` —— 名单的锚点被改名了").toBeGreaterThanOrEqual(0);
    const bodyStart = open + openMarker.length;
    const close = src.indexOf("];", bodyStart);
    expect(close, "`let tags = [` 之后找不到 `];` —— 切片没有收口").toBeGreaterThan(bodyStart);

    const slice = src.slice(bodyStart, close);
    const rust = [...slice.matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);
    // 两条都要：只比集合的话，切片一旦被收紧成空列表就会「两边都缺同一支」地悄悄通过。
    expect(rust).toHaveLength(11); // 这个数字锚在 events.rs:358 的 (11, 11) 断言上
    expect(WIRE_TAGS).toHaveLength(11);
    // 集合相等，**不**比顺序：Rust 那边把 tags 数组重新排序不影响线格式，
    // 断言顺序只会造出一个假红。
    expect(new Set(rust)).toEqual(new Set(WIRE_TAGS));
  });

  // ==== R114/R115/R116：不许替线格式发明真相 ====

  it("passes ms through untouched so replayed 0 still means 未记录", () => {
    let s = reduce(initialState(), start(1));
    s = reduce(s, toolCall("c1"));
    expect(tool(selectActiveTurn(s)!.blocks[0]).ms).toBe(0); // 卡片初值：未记录
    s = reduce(s, toolResult("c1", "ok", false, 0));
    const card = tool(selectActiveTurn(s)!.blocks[0]);
    expect(card.ms).toBe(0);
    expect(card.ms).not.toBeNull(); // R115：reducer 不把 0 换成 null
    expect(card.status).toBe("ok");
  });

  it("does not fabricate permission cards or usage when replaying a history", () => {
    const s = replay([
      { t: "user_message", blocks: [{ type: "text", text: "读一下" }] },
      start(1),
      d("结果是这样"),
      toolCall("c1"),
      toolResult("c1", "内容"),
      done(),
    ]);
    const kinds = s.turns.flatMap((t) => t.blocks.map((b) => b.kind));
    expect(kinds).not.toContain("perm");
    expect(s.permIndex).toEqual({});
    expect(s.usage).toBeNull(); // R114：usage 是瞬时事件，重开即消失
    expect(s.streaming).toBe(false);
  });

  // ==== 归位失败的兜底 ====

  it("turns an unattributable tool result into a notice instead of guessing a card", () => {
    let s = replay([start(1), d("正文"), toolResult("ghost", "没人认识这次调用")]);
    expect(toolCount(s)).toBe(0);
    expect(s.notices.join()).toContain("ghost");
    // 最后一个 turn 的文本块没有被改写成工具卡
    expect(selectActiveTurn(s)!.blocks).toEqual([{ kind: "text", text: "正文" }]);
  });

  it("ignores a permission decision for an unknown id without touching any card", () => {
    const s = replay([start(1), permRequest("p1"), done(), start(1), d("下一轮")]);
    const before = structuredClone(s);
    const next = reduce(s, {
      t: "permission_resolved",
      id: "ghost",
      decision: { allow: false, reason: "过期" },
    });
    expect(next.turns).toEqual(before.turns);
    expect(next.notices.join()).toContain("ghost");
  });

  // ==== streaming 门与 R112 的分工 ====

  it("lets turn_start, text_delta and tool_call own streaming, but not user_message", () => {
    expect(reduce(initialState(), { t: "user_message", blocks: [] }).streaming).toBe(false);
    expect(reduce(initialState(), start(1)).streaming).toBe(true);
    // 下面两条钉的是 **R112 那句枚举的字面推论**（置真只有三支、done 置假），不是业务规则。
    // 真机出口产不出「`done` 之后的迟到 text_delta / tool_call」这种帧：
    // `ipc.rs:133-138` 的 `other => { flush(); send(other) }` 臂保证任何非 TextDelta 事件之前
    // 先落帧（`Done` 也走这一臂），`session.rs:263-267` 的 `finish()` 是**唯一出口**、
    // 致命错误之后必定跟着一条 `Done` 收尾。⇒ 这两行断言的是不可达分支上的 reducer 语义。
    // 如果 T16 要做「回合结束后不许再锁输入框」的加固，改的是渲染层怎么解读 `streaming`，
    // 别把这个断言当成「产品期望输入框二次锁死」（评审 F6）。
    expect(reduce(replay([done()]), d("迟到的一帧")).streaming).toBe(true);
    expect(reduce(replay([done()]), toolCall("late")).streaming).toBe(true);
  });

  // ==== 纯函数与可复现（R119） ====

  it("never mutates the state it is handed", () => {
    const base = deepFreeze(replay([start(1), d("在写正文"), toolCall("c1"), permRequest("p1")]));
    const snapshot = structuredClone(base);
    const probes: AgentEvent[] = [
      start(2),
      d("续"),
      { t: "user_message", blocks: [{ type: "text", text: "说一句" }] },
      toolCall("c2"),
      permRequest("p2"),
      { t: "permission_resolved", id: "p1", decision: { allow: false, reason: "不要" } },
      toolResult("c1", "结果", true, 4),
      { t: "usage", input_tokens: 7, output_tokens: 8 },
      { t: "context_compacted", dropped: 1, summary_chars: 2 },
      { t: "error", where: "store", message: "写库失败", fatal: true },
      done("interrupted"),
    ];
    for (const e of probes) {
      const next = reduce(base, e);
      expect(next).not.toBe(base);
      expect(base).toEqual(snapshot);
    }
  });

  it("replays the same events to the same state even across a clock jump, so E2E can reproduce", () => {
    const events: AgentEvent[] = [
      { t: "user_message", blocks: [{ type: "text", text: "第一轮" }] },
      start(1),
      d("甲"),
      toolCall("c1"),
      toolResult("c1", "结果甲"),
      start(2),
      d("乙"),
      permRequest("p1"),
      { t: "permission_resolved", id: "p1", decision: { allow: true, scope: "once" } },
      { t: "error", where: "mcp", message: "超时", fatal: false },
      { t: "usage", input_tokens: 3, output_tokens: 4 },
      { t: "context_compacted", dropped: 9, summary_chars: 30 },
      done("max_turns"),
    ];
    // 评审 F2①：这条以前是 R119「同输入同输出」的唯一守卫，而两次 replay 落在**同一毫秒**里
    // —— 往 notice() 插一个 `Date.now()` 实测仍然 28 条全绿。现在自己把时钟推开。
    // `vi` 从 "vitest" 显式导入：R121 的坑是不导入会让 `tsc && vite build` 编译不过。
    vi.useFakeTimers();
    try {
      vi.setSystemTime(0);
      // 先自证时钟真的被推开了。桩哪天失效时这条会喊，而不是悄悄退回「两次同一毫秒」那个假绿样子。
      expect(Date.now()).toBe(0);
      const a = replay(events);
      vi.setSystemTime(5_000);
      expect(Date.now()).toBe(5_000);
      const b = replay(events);
      expect(b).toEqual(a);
      expect(JSON.stringify(b)).toBe(JSON.stringify(a)); // 连键序都稳定
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps reducer.ts itself free of clock, randomness and global reads", async () => {
    // 评审 F2②：上一条用「推开时钟」抓时钟读取，这一条从**源码**层面把 R119 那句
    // 「请保持纯函数」钉成机器判据。两者互补：事件级断言受攻击面选择影响
    // （notice() 只是可达位置之一，把时钟当单调 id 塞进 Turn.id 同样能骗过事件级比较），
    // 静态扫描不看可达性，正文里出现一次就红。
    //
    // 坑（必须剥注释）：`reducer.ts` 的纪律注释里**明写**着「不用 Date.now()、不用 Math.random()」，
    // 所以拿原文做 `not.toContain` 会在干净代码上直接红。
    const code = stripComments(await readSource("./reducer.ts"));
    for (const read of ["Date.now(", "Math.random(", "new Date(", "performance.now(", "globalThis"]) {
      expect(code, `reducer.ts 的正文里出现了 ${read} —— replay 不再可复现（R119）`).not.toContain(read);
    }
    // 剥注释不许把正文一起剥没：扫描范围必须仍然看得见真实代码，
    // 否则 stripComments 一旦退化成返回空串，上面那五条就全成了空断言。
    expect(code).toContain("export function replay");
    expect(code.length).toBeGreaterThan(1_000);
  });

  // ==== ipc.rs 的信封与通道名 ====

  it("keeps the { sessionId, event } payload envelope and the channel name mirrored", () => {
    // 改名说明（评审 F4）：旧名字「mirrors the wire keys ...」承诺过头 —— 它 `Object.keys` 的是
    // 本用例自己手写的字面量，Rust 把 `ms` 改名成 `elapsed_ms` 时它照样绿。
    // 线格式的真判据不在这儿：键名由 `probe`/`contentProbe` 上的 `AgentEvent`、`ContentBlock`
    // 标注（tsc）+ 上面那条跨语言用例（读 events.rs 的 tags 表）承担。
    // 本用例只留下**有独立价值**的两件事：信封形状（ipc.rs:24-32 那唯一一层 camelCase）
    // 和通道名（ipc.rs:146）。键序断言全部改指镜像产物 `probe`/`contentProbe`。
    const p: AgentPayload = { sessionId: "s1", event: probe.tool_result };
    expect(Object.keys(p).sort()).toEqual(["event", "sessionId"]); // camelCase 只在这一层
    expect(AGENT_CHANNEL).toBe("agent");
    expect(p.event.t).toBe("tool_result"); // 信封换键，事件内部仍是 snake_case
    expect(Object.keys(probe.tool_result)).toEqual(["t", "call_id", "is_error", "ms", "preview"]);
    expect(Object.keys(probe.usage)).toEqual(["t", "input_tokens", "output_tokens"]);
    expect(Object.keys(probe.error)).toEqual(["t", "where", "message", "fatal"]);
    expect(Object.keys(contentProbe.image)).toEqual(["type", "data", "mime"]);
    expect(CONTENT_TAGS).toHaveLength(4); // 对齐 types.rs:4-24 的四支
  });
});

function toolCount(s: UiState): number {
  return s.turns.flatMap((t) => t.blocks).filter((b) => b.kind === "tool").length;
}
