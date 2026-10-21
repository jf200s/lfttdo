import { describe, expect, it } from "vitest";
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
  Risk,
  ToolCallView,
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
const tool = (b: UiBlock | undefined): Extract<UiBlock, { kind: "tool" }> => {
  if (!b || b.kind !== "tool") throw new Error("期望一个 tool 卡片");
  return b;
};
const perm = (b: UiBlock | undefined): Extract<UiBlock, { kind: "perm" }> => {
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

// 逐字抄自 src-tauri/src/agent/events.rs:400-412
// （every_agent_event_variant_serializes_its_t_tag，R118）。
const WIRE_TAGS = [
  "turn_start",
  "text_delta",
  "user_message",
  "tool_call",
  "permission_request",
  "permission_resolved",
  "tool_result",
  "usage",
  "context_compacted",
  "error",
  "done",
] as const;

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
    expect(WIRE_TAGS).toHaveLength(11);
    const base = replay([start(1), d("在写正文"), toolCall("c1"), permRequest("p1")]);
    const probe: Record<(typeof WIRE_TAGS)[number], AgentEvent> = {
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
    const swallowed = WIRE_TAGS.filter((tag) => reduce(base, probe[tag]) === base);
    expect(swallowed).toEqual([]);
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

  it("replays the same events to the same state, so E2E can reproduce", () => {
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
    const a = replay(events);
    const b = replay(events);
    expect(a).toEqual(b);
    expect(JSON.stringify(a)).toBe(JSON.stringify(b)); // 连键序都稳定
  });

  // ==== ipc.rs 的 payload 镜像 ====

  it("mirrors the wire keys and the { sessionId, event } payload on the agent channel", () => {
    const e: AgentEvent = { t: "tool_result", call_id: "c", is_error: false, ms: 0, preview: "" };
    expect(Object.keys(e)).toEqual(["t", "call_id", "is_error", "ms", "preview"]);
    const b: ContentBlock = { type: "image", data: "AA", mime: "png" };
    expect(Object.keys(b)).toEqual(["type", "data", "mime"]);
    const p: AgentPayload = { sessionId: "s1", event: e };
    expect(Object.keys(p).sort()).toEqual(["event", "sessionId"]); // camelCase 只在这一层
    expect(AGENT_CHANNEL).toBe("agent");
    expect(p.event.t).toBe("tool_result"); // 信封换键，事件内部仍是 snake_case
    expect(Object.keys(p.event)).toEqual(["t", "call_id", "is_error", "ms", "preview"]);
  });
});

function toolCount(s: UiState): number {
  return s.turns.flatMap((t) => t.blocks).filter((b) => b.kind === "tool").length;
}
