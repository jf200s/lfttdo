import type {
  AgentEvent,
  BlockLocation,
  ContentBlock,
  Decision,
  PermCard,
  ToolCard,
  Turn,
  TurnRole,
  UiBlock,
  UiState,
} from "../types";

/**
 * 事件 → 界面状态的纯映射。TS 侧唯一的线格式镜像在 `src/types.ts`（冲突时以 Rust 为准）。
 *
 * 三条纪律（裁定 R112/R113/R117，都优先于 brief 的片段）：
 * 1. `turn_start` 与 `user_message` **无条件**新起一个 turn，不看 `streaming`。
 *    真实节奏是 `session.rs:119` 的 `for turn in 1..=max_turns`：每个模型往返发一次
 *    `turn_start`，用它当闸门会把整轮压成一条气泡。
 * 2. `e.turn` 不是标识符（每次真人发言后从 1 重启）。turn 身份用下面的单调 `turnSeq`，
 *    `toolIndex`/`permIndex` 存 `Turn.id`；任何地方都不拿 `e.turn` 当键或数组下标。
 * 3. 不用 `Date.now()`、不用 `Math.random()`、不修改入参 —— `replay` 同输入同输出，
 *    T17 的端到端断言靠它复现。
 *    这一条以前只是愿望（评审 F2 实测：往 `notice()` 里插 `Date.now()` 时全测试集 28 条全绿，
 *    因为两次 replay 落在同一毫秒）。现在有两道机器判据：`reducer.test.ts` 里
 *    一条剥注释后的静态扫描 + 一条自己推时钟的复现用例。
 */

/** 归位失败时交给界面的那句话。留调用方（notices）而不是抛错：前端不许因为一条陌生事件崩。 */
function notice(s: UiState, text: string): UiState {
  return { ...s, notices: [...s.notices, text] };
}

export function initialState(): UiState {
  return {
    turnSeq: 0,
    turns: [],
    streaming: false,
    usage: null,
    fatal: null,
    notices: [],
    toolIndex: {},
    permIndex: {},
  };
}

/** 最后一条气泡（直播渲染「本轮」用的那一格）。 */
export function selectActiveTurn(s: UiState): Turn | undefined {
  return s.turns[s.turns.length - 1];
}

/** 开一条新气泡。返回新状态和新气泡的下标（后续都按下标改，不靠「最后一个」去找）。 */
function startTurn(
  s: UiState,
  role: TurnRole,
): { state: UiState; index: number } {
  const id = s.turnSeq + 1;
  const turns = [...s.turns, { id, role, blocks: [] }];
  return { state: { ...s, turnSeq: id, turns }, index: turns.length - 1 };
}

/**
 * 找到可以承接**助手**内容的那一格：最后一格是助手气泡就复用它，
 * 否则（没有气泡，或最后一格是用户发言）开一条新的助手气泡。
 * 这里没有 `streaming` 闸门 —— 那是输入框的事，不是 turn 边界的事（R112）。
 */
function ensureTurn(s: UiState): { state: UiState; index: number } {
  const last = s.turns.length - 1;
  if (last >= 0 && s.turns[last].role === "assistant") return { state: s, index: last };
  return startTurn(s, "assistant");
}

/** 按下标替换一条气泡，其余原样。 */
function mapTurn(s: UiState, index: number, fn: (t: Turn) => Turn): UiState {
  return { ...s, turns: s.turns.map((t, i) => (i === index ? fn(t) : t)) };
}

/** 往一条气泡尾部追加块；文本块连续时并入同一块（流式合并），否则新起一块。 */
function pushBlock(t: Turn, block: UiBlock): Turn {
  const last = t.blocks[t.blocks.length - 1];
  if (block.kind === "text" && last && last.kind === "text") {
    const merged: UiBlock = { kind: "text", text: last.text + block.text };
    return { ...t, blocks: [...t.blocks.slice(0, -1), merged] };
  }
  return { ...t, blocks: [...t.blocks, block] };
}

/**
 * 按 `BlockLocation` 交出那条气泡的下标；找不到返回 -1（R113：定位认 turnId，不认 `e.turn`）。
 *
 * 为什么必须 `findIndex` 而不是 `return turnId - 1`（评审 F10）：今天 reducer 从不裁剪 `turns`，
 * 所以 `Turn.id === 下标 + 1` 恒成立，两种写法是**等价变异** —— 实现者在 `task-14-report.md` §4 M5
 * 自报过这一点，评审复现确认「换成下标写法后 28 条全绿」。正因为它分辨不出来，才要把理由写在这里：
 * 一旦 T16 给历史做虚拟化/裁剪，下标写法会**静默把结果归到别的气泡上**，而测试不会红。
 * `findIndex` 今天不多做事，将来不塌方 —— 它不是冗余。
 */
function locate(s: UiState, turnId: number): number {
  return s.turns.findIndex((t) => t.id === turnId);
}

/** `user_message` 的 blocks 只映射 `text`/`image` 两支（R111）。 */
function userBlocks(blocks: ContentBlock[]): UiBlock[] {
  const out: UiBlock[] = [];
  for (const b of blocks) {
    switch (b.type) {
      case "text":
        out.push({ kind: "text", text: b.text });
        break;
      case "image":
        out.push({ kind: "image", data: b.data, mime: b.mime });
        break;
      // User 行里的 tool_call / tool_result 在 Plan A 不可达（db.rs:383），
      // 显示与否由前端决定 ⇒ 不渲染、也不为它发明分支。库里原样留着。
      case "tool_call":
      case "tool_result":
        break;

      default: {
        // 编译期穷尽守卫（评审 F1②）：理由和 reduce 那张 switch 完全同构，但这条更硬 ——
        // db.rs:388 把 `m.blocks.clone()` **原样**上交，Rust 给 `ContentBlock` 加第 5 支时
        // 它会自动流到这里，而 `out` 不 push 就等于静默丢数据。
        // 于是「ContentBlock 多了一支而这里没跟上」直接是 TS2322，不许靠 T17 手测发现。
        const _exhaustive: never = b;
        void _exhaustive; // 本仓库 noUnusedLocals=true：用 void 消费掉才不报 TS6133
        break; // 运行期行为与今天一致：未知 tag 不崩、也不渲染
      }
    }
  }
  return out;
}

/**
 * 真人发言入界面：**一份实现、两个入口**（R117）。
 * - 重放路径：`reduce` 的 `user_message` 臂调它（`db.rs:379-388` 发的事件）。
 * - 直播路径：事件流里从来没有 `user_message`（`db.rs:380`），由 T16 在用户按发送时乐观插入，同样调它。
 * 两条路必须给出同一个形状，所以 `reducer.test.ts` 里有一条深相等断言钉着。
 *
 * 注意它**不动 `streaming`**：R112 把 streaming 的归属写死为
 * `turn_start`/`text_delta`/`tool_call` 置真、`done` 置假。
 */
export function appendUserTurn(state: UiState, blocks: ContentBlock[]): UiState {
  const { state: opened, index } = startTurn(state, "user");
  return mapTurn(opened, index, (t) => ({ ...t, blocks: userBlocks(blocks) }));
}

/**
 * 落卡的公共部分：复用/新开助手气泡，追加块，并交回登记索引要用的坐标。
 * 索引记 `{ turnId: 这条气泡的 Turn.id, block: 落卡时的下标 }`，不记 `e.turn`（R113）。
 */
function placeCard(state: UiState, block: UiBlock): { next: UiState; loc: BlockLocation } {
  const { state: base, index } = ensureTurn(state);
  const at = base.turns[index].blocks.length;
  const next = mapTurn(base, index, (t) => pushBlock(t, block));
  return { next, loc: { turnId: next.turns[index].id, block: at } };
}

function openToolCard(state: UiState, block: ToolCard, callId: string): UiState {
  const { next, loc } = placeCard(state, block);
  return { ...next, streaming: true, toolIndex: { ...next.toolIndex, [callId]: loc } };
}

function openPermCard(state: UiState, block: PermCard, id: string): UiState {
  const { next, loc } = placeCard(state, block);
  // 不动 `streaming`：R112 把置真的事件写死为 turn_start/text_delta/tool_call 三支，
  // 权限请求在直播里一定发生在已流式的回合内，这里再置一次是多余的第二次真相。
  //
  // 这条前提的真机依据（评审 F8）：权限门只在 `session.rs:119` 的
  // `for turn in 1..=self.cfg.max_turns` 循环体**内部**被调用，而 `:124` 每轮开头就发了
  // `TurnStart{turn}` ⇒ 恒有前置 `turn_start`，`placeCard` 里的 `ensureTurn` 只会复用那一格。
  // 将来若有人把权限门挪到 `run()` 之前做预检，这条就不成立了：界面会出现
  // 「有待决权限卡 + 输入框可用」。挪 gate 的人请回来看这一行。
  return { ...next, permIndex: { ...next.permIndex, [id]: loc } };
}

/** 把决定写进 perm 卡。`allow` 是布尔真相（R98），不靠「哪个键存在」猜变体。 */
function resolvedCard(block: UiBlock, decision: Decision): PermCard | undefined {
  if (block.kind !== "perm") return undefined;
  return decision.allow
    ? { ...block, state: "allowed", scope: decision.scope, reason: undefined }
    : { ...block, state: "denied", reason: decision.reason, scope: undefined };
}

export function reduce(state: UiState, e: AgentEvent): UiState {
  switch (e.t) {
    // 无条件新起一条助手气泡（R112）。这一支**读都不读** `e.turn`：它每次真人发言后从 1
    // 重启（db.rs:385）、每次 run() 也从 1 起（session.rs:119），当键或下标必错位（R113）。
    // 想显示「第几轮」的话，渲染层自己数 assistant turn 的序号，别拿线格式那个数用。
    case "turn_start":
      return { ...startTurn(state, "assistant").state, streaming: true };

    case "user_message":
      return appendUserTurn(state, e.blocks);

    case "text_delta": {
      const { state: base, index } = ensureTurn(state);
      return {
        ...mapTurn(base, index, (t) => pushBlock(t, { kind: "text", text: e.text })),
        streaming: true,
      };
    }

    case "tool_call":
      return openToolCard(
        state,
        {
          kind: "tool",
          callId: e.call.id,
          name: e.call.name,
          risk: e.risk, // 与 e.call.risk 相等（events.rs:34 的不变量）
          input: e.call.input,
          status: "running",
          preview: "",
          ms: 0, // 0 = 还没记录，别替线格式发明区分（R115）
        },
        e.call.id,
      );

    case "permission_request":
      return openPermCard(
        state,
        { kind: "perm", id: e.id, call: e.call, risk: e.risk, state: "pending" },
        e.id,
      );

    case "tool_result": {
      const loc = state.toolIndex[e.call_id];
      const index = loc ? locate(state, loc.turnId) : -1;
      const block = loc && index >= 0 ? state.turns[index].blocks[loc.block] : undefined;
      if (!loc || !block || block.kind !== "tool") {
        // 归位不了就承认归位不了：不猜卡、不静默丢（brief:337 说的歧义由这条守卫兜住）。
        return notice(state, `工具结果无法归位：${e.call_id}`);
      }
      const updated: ToolCard = {
        ...block,
        status: e.is_error ? "error" : "ok",
        preview: e.preview,
        ms: e.ms, // 原样透传，重放里的 0 意思是「库里没记」（R115）
      };
      const next = mapTurn(state, index, (t) => ({
        ...t,
        blocks: t.blocks.map((b, i) => (i === loc.block ? updated : b)),
      }));
      return e.is_error
        ? notice(next, `工具 ${block.name} 失败：${e.preview}`)
        : next;
    }

    case "permission_resolved": {
      const loc = state.permIndex[e.id];
      const index = loc ? locate(state, loc.turnId) : -1;
      const block = loc && index >= 0 ? state.turns[index].blocks[loc.block] : undefined;
      // 按 loc 定位，绝不写进「最后一个」turn（R113：done 之后用户又发一句、
      // 或取消臂先收尾时，最后一格已经不是那张待决卡片了）。
      if (!loc || !block) return notice(state, `权限决定无法归位：${e.id}`);
      const updated = resolvedCard(block, e.decision);
      if (!updated) return notice(state, `权限决定落到了非权限卡上：${e.id}`);
      return mapTurn(state, index, (t) => ({
        ...t,
        blocks: t.blocks.map((b, i) => (i === loc.block ? updated : b)),
      }));
    }

    case "usage":
      return { ...state, usage: { input: e.input_tokens, output: e.output_tokens } };

    case "context_compacted":
      return notice(
        state,
        `已压缩早期对话：省略 ${e.dropped} 条，摘要 ${e.summary_chars} 字`,
      );

    case "error":
      return e.fatal
        ? { ...state, fatal: { message: e.message, where: e.where } }
        : notice(state, e.message);

    case "done": {
      const notes = [...state.notices];
      // R116：上限数字住在 Rust 的 SessionConfig，事件里不带，前端不许复述它。
      if (e.stop_reason === "max_turns") notes.push("已达到单轮最大工具轮数上限，已停止");
      if (e.stop_reason === "interrupted") notes.push("已停止");
      return { ...state, streaming: false, notices: notes };
    }

    default: {
      // 前向兼容：Rust 侧新增事件时前端不许崩（brief:339）——运行期仍然原样返回。
      //
      // 但这一支**不许**兜住任何已经存在于线格式的变体（R111 漏 `user_message` 就是活例子），
      // 所以把它做成编译期穷尽守卫（评审 F1①）：`AgentEvent` 加了第 12 支而这里没写臂，
      // `e` 在 default 上就不再是 `never`，`tsc` 直接 TS2322 —— 「镜像有臂、reducer 没写」
      // 从「只靠测试」升级成「编译不过」。
      const _exhaustive: never = e;
      void _exhaustive; // 本仓库 noUnusedLocals=true：用 void 消费掉才不报 TS6133
      return state;
    }
  }
}

/** 把一段持久化历史（`events_from_messages` 的输出）换成界面状态。纯映射，可复现。 */
export function replay(events: AgentEvent[]): UiState {
  return events.reduce(reduce, initialState());
}
