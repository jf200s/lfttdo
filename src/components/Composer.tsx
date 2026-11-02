import { useEffect, useRef, useState } from "react";
import {
  ImageStrip,
  MAX_IMAGES_PER_MESSAGE,
  type PendingImage,
} from "./ImageStrip";
import type { ContentBlock, SendBlock } from "../types";

/**
 * 从粘贴事件里挑出图片文件（R145：实现只许读 `items[i].type` 与 `getAsFile()`，
 * **不许**读 `clipboardData.files` —— 那是 jsdom 与 Chrome 的语义差异面）。
 * 往输入框里粘一段文字时这一支返回空数组：那些项的 `type` 不是 `image/`。
 */
function imageFilesFromPaste(items: DataTransferItemList): File[] {
  const out: File[] = [];
  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    if (!item.type.startsWith("image/")) continue;
    const file = item.getAsFile();
    if (file) out.push(file);
  }
  return out;
}

/**
 * 要求 6：发送按钮在 `state.streaming` 时切换成「停止」，点击调 `onStop()`（→ `interrupt()`）
 * 并**禁用自身，直到 `done` 才恢复**。
 *
 * 「不做乐观恢复」落在下面那道 `useEffect` 上：`stopRequested` 只跟着 `streaming` 落地，
 * 而 `streaming` 唯一的置假来源是 `done` 事件（R112）。`interrupt()` 那个 promise 完成时
 * 界面什么都不改 —— 令牌一次性（R63），取消成功和这一轮结束是两件事。
 *
 * R205：T19 之后 `onSend` 收的是**要发出去的块**（图在前、文本在后），不再是纯文本。
 * 选「收窄 `onSend`」而不是「另加一条 `onSendBlocks`」：两条并存的发送入口就是两套真相，
 * 而 T19 之后不存在「只发文本」这条路（无图时产出的仍是单支 text 块，载荷和以前一字不差）。
 * `onSend` 返回 Promise 是有意的 —— 第 8 条要「发成功才清待发图」，没有可 await 的东西就做不到。
 */
export function Composer({
  streaming,
  onSend,
  onStop,
  onNormalize,
  seed,
}: {
  streaming: boolean;
  onSend: (blocks: SendBlock[]) => Promise<void>;
  onStop: () => void;
  /** 归一化由 prop 注入（Step 2 第 1 条）：这一层因此可测，`App` 接的是 `api.normalize`。 */
  onNormalize: (bytes: ArrayBuffer) => Promise<ContentBlock>;
  /** 技能面板“用这个技能”的预填；每次点击换一个新对象（id 递增），同一句话也能再填一次。 */
  seed?: { id: number; text: string } | null;
}) {
  const [text, setText] = useState("");
  const [stopRequested, setStopRequested] = useState(false);
  /** 本地发送锁：从按下发送到 turn_start（或失败返回）之间，拦住第二次 Enter。 */
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  /** 这一次已经交给 `onSend` 的草稿快照：turn_start 时按它清空，失败时保留。 */
  const sentRef = useRef<{ text: string; images: PendingImage[] } | null>(null);
  /** 已归一化、等着和这句话一起发出去的图。 */
  const [pending, setPending] = useState<PendingImage[]>([]);
  /** 逐文件失败：`<文件名>：<原文>`，绝不静默丢图（Step 2 第 5 条 / R195）。 */
  const [errors, setErrors] = useState<string[]>([]);
  /** 最近这几批里被数量上限挡掉的张数（Step 2 第 4 条那条可见提示）。 */
  const [ignored, setIgnored] = useState(0);
  /**
   * `pending` 的**同步影子**（`App.tsx` 的 `activeRef`/`bufRef` 同一手法）：
   * 归一化是异步的，只读 state 的话「连投两批」会把 4 张的额度算重。
   * 写这一支的地方只有 `pushPending` / `removePending` / `dropSent` 三处，所以可审。
   */
  const pendingRef = useRef<PendingImage[]>([]);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    if (!seed) return;
    setText(seed.text);
    const el = inputRef.current;
    if (el) {
      el.focus();
      el.setSelectionRange(seed.text.length, seed.text.length);
    }
  }, [seed]);

  const pushPending = (img: PendingImage) => {
    pendingRef.current = [...pendingRef.current, img];
    setPending(pendingRef.current);
  };

  /**
   * 只摘掉**这一次发出去的那几张**，不是「清空待发区」。
   *
   * 归一化是异步的，`submit` 在 `await onSend` 之后才清理；那段时间里回来的图（用户贴完
   * 图直接回车，`normalize_image` 的 promise 还挂着）既不在发出去的载荷里、也不该被算作
   * 已发。整区清空的症状和 0650036 修掉的「已忽略 N 张永不消失」同一族：界面先承诺缩略图，
   * 然后无声吞掉它 —— 用户只会以为那张图从没贴上。
   */
  const dropSent = (sent: PendingImage[]) => {
    pendingRef.current = pendingRef.current.filter((p) => !sent.includes(p));
    setPending(pendingRef.current);
  };

  /**
   * 回合被接受（turn_start / streaming=true）= 这份草稿已经交出去了，可以清空。
   * 失败路径在 `streaming` 变真之前就返回，因此「发失败保留草稿」的合同不受影响。
   * 比「等整个生成结束再清」早得多：那条路会让已发出去的句子在输入区挂几分钟。
   */
  useEffect(() => {
    if (!streaming) {
      setStopRequested(false);
      return;
    }
    const sent = sentRef.current;
    if (sent === null) return;
    // 快照**留到 onSend 结算**：落库失败时它负责把草稿和图恢复回来。
    sendingRef.current = false;
    setSending(false);
    setText((cur) => (cur === sent.text ? "" : cur));
    dropSent(sent.images);
    setErrors([]);
    setIgnored(0);
    // `dropSent` 每次渲染身份都新，但这里只在 streaming 翻转时执行一次；把它列进依赖
    // 会让「生成中每一次打字」都重跑一遍清空逻辑（把用户新输入也清掉）。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [streaming]);

  /** 移除第 `index` 张：同时腾出额度（`pendingRef` 是额度的读数来源）。 */
  const removePending = (index: number) => {
    pendingRef.current = pendingRef.current.filter((_, i) => i !== index);
    setPending(pendingRef.current);
  };

  /**
   * 三个入口汇成的那一条路径（R144）。
   *
   * 数量闸在这一层（Step 2 第 4 条）：**先处理前 `MAX_IMAGES_PER_MESSAGE - 已有` 个**，
   * 剩下的不碰归一化、只报一个「已忽略 N 张」的数 —— 不静默丢，也不假装收了。
   * 张数额度用 `pendingRef`（同步影子）而不是 `pending`：归一化是异步的，只看 state
   * 的话连投两批会把额度算重（第 4 张还没回来时 state 里还是 0）。
   * 光有影子还不够：影子只在 `pushPending` 落图那一刻更新，交叠批次在 await 之前读到
   * 的还是旧快照 —— 所以这一层必须整体串行（见 `addFiles` 的排队链）。
   */
  const addFilesImpl = async (files: File[]) => {
    const room = MAX_IMAGES_PER_MESSAGE - pendingRef.current.length;
    if (room <= 0) {
      setIgnored((n) => n + files.length);
      return;
    }
    if (files.length > room) setIgnored((n) => n + (files.length - room));
    for (const file of files.slice(0, room)) {
      try {
        // 整块 ArrayBuffer：不切 subarray，也不在这里包 `{ bytes }`（T3 / R147）。
        const block = await onNormalize(await file.arrayBuffer());
        if (block.type !== "image") {
          // 合同上 `normalize_image` 只会回 image 支。回来的不是它，就是 Rust 讲了另一件事
          // —— 上屏，不许静默丢图（Step 2 第 5 条）。
          setErrors((e) => [...e, `${file.name}：normalize_image 回来的不是图片块`]);
          continue;
        }
        pushPending({
          name: file.name,
          previewUrl: `data:image/${block.mime};base64,${block.data}`,
          block,
        });
      } catch (err) {
        // Step 2 第 5 条 / R195：`<文件名>：<原文>` 逐字上屏，前端不解释、不裁剪、不分类。
        // Tauri 把 `Err(String)` 原样 reject 出来（`ipc.rs:581` 的返回类型就是 `String`），
        // 取 `.message` 只是为了 `Error` 那种载体不顶着一句「Error: xxx」上屏 —— 不是解读内容。
        // 这里也**不做**任何本地大小预判：那道闸在 Rust，且它排在格式识别之前（R195）。
        const msg = err instanceof Error ? err.message : String(err);
        setErrors((prev) => [...prev, `${file.name}：${msg}`]);
      }
    }
  };

  /**
   * 排队链：各入口都是 fire-and-forget（`void addFiles(…)`），归一化又是异步的，
   * 两批粘贴/拖拽交叠时后一批会在前一批任何一张落定之前读 `pendingRef` —— 各算各的
   * `room`，4 张的额度被算两遍，上限被绕过。让后一批排在 `addFilesQueue` 尾部，
   * 前一批**落定**（该推的都推进了 `pendingRef`）之后才轮到自己算额度，影子才作数。
   * 链上存的是吞掉 rejection 的 promise：单批内部的失败已逐文件上屏（`errors`），
   * 不许把后面排队的批一起饿死。
   */
  const addFilesQueue = useRef<Promise<void>>(Promise.resolve());
  const addFiles = (files: File[]) => {
    // Step 2 第 9 条：`streaming` 期间三个入口都关掉，避免半途改图。闸在**入口这一刻**
    // （和旧代码一致：fire-and-forget 的 addFiles 本来也是提交即判）；排在队里、轮到时
    // streaming 才变真的批，与旧代码「await 挂着时回合开始」的同批一样放行。
    if (streaming) return;
    const run = addFilesQueue.current.then(() => addFilesImpl(files));
    addFilesQueue.current = run.catch(() => {});
    return run;
  };

  const stop = () => {
    setStopRequested(true);
    onStop();
  };

  /**
   * Step 2 第 8 条：`setText("")` / `dropSent(…)` 都移到 `await` **之后** ——
   * 发失败时用户准备好的图和那句话必须原样留在输入区（清空的症状是「只能重新贴一遍」）。
   * 这里的 `catch` 有意不显示任何东西：发送失败的原文由调用方（`App` 的 notice）上屏，
   * 在这一层再抄一遍就是同一件事两个来源。
   */
  /**
   * 整窗拖放：drop 只挂在输入框那一小块时，用户把图拖到气泡区或侧栏会直接被浏览器
   * 当成「打开文件」——在 Tauri 里等于把图丢给 WebView，界面什么都不发生。
   * 这里在 `document` 上兜住所有文件拖放；输入框自己的 handler 会 `stopPropagation`，
   * 所以两条路不会重复添加。
   * `addFiles` 每次渲染都是新函数，监听器只装一次 ⇒ 用 ref 取最新的那一份。
   */
  const addFilesRef = useRef(addFiles);
  useEffect(() => {
    addFilesRef.current = addFiles;
  });
  useEffect(() => {
    const hasFiles = (e: DragEvent) =>
      Array.from(e.dataTransfer?.types ?? []).includes("Files") ||
      (e.dataTransfer?.files?.length ?? 0) > 0;
    const onDragOver = (e: DragEvent) => {
      if (hasFiles(e)) e.preventDefault();
    };
    const onDrop = (e: DragEvent) => {
      if (e.defaultPrevented || !hasFiles(e)) return;
      e.preventDefault();
      addFilesRef.current(Array.from(e.dataTransfer?.files ?? []));
    };
    document.addEventListener("dragover", onDragOver);
    document.addEventListener("drop", onDrop);
    return () => {
      document.removeEventListener("dragover", onDragOver);
      document.removeEventListener("drop", onDrop);
    };
  }, []);

  const submit = async () => {
    const t = text.trim();
    // R216④：「有图无字」必须放行 —— 只贴一张图不打字是日常主用法，
    // 只判 `t === ""` 的可见症状是「贴图点发送没反应」。
    if ((t === "" && pending.length === 0) || streaming || sendingRef.current) return;
    sendingRef.current = true;
    setSending(true);
    // Step 2 第 7 条：顺序固定图在前、文本在最后；文本为空时**不产出**空的 text 块
    // （`App.test.tsx` 那十三条钉的就是无图发送仍是 `[{type:"text",text}]`，R216②）。
    // 取 `pendingRef`（同步影子）而不是 `pending`：这份快照同时是 `dropSent` 的删除依据，
    // 两者必须是同一个列表，否则「按身份移除」会移除成另一批。
    const sent = pendingRef.current;
    const draft = text;
    const blocks: SendBlock[] = sent.map((p) => p.block);
    if (t !== "") blocks.push({ type: "text", text: t });
    sentRef.current = { text: draft, images: sent };
    try {
      await onSend(blocks);
    } catch {
      // 失败：草稿与图留在输入区（Step 2 第 8 条），锁放开让用户重试。
      // 如果失败发生在 turn_start 之后，草稿已经被上面那条 effect 清掉过 ——
      // 这里按快照恢复；用户在这期间新敲的字/新贴的图优先，不覆盖。
      const sentDraft = sentRef.current;
      sentRef.current = null;
      sendingRef.current = false;
      setSending(false);
      if (sentDraft !== null) {
        setText((cur) => (cur === "" ? sentDraft.text : cur));
        const missing = sentDraft.images.filter(
          (img) => !pendingRef.current.includes(img),
        );
        if (missing.length > 0) {
          pendingRef.current = [...missing, ...pendingRef.current];
          setPending(pendingRef.current);
        }
      }
      return;
    }
    // 成功返回 = 这一轮已落库（含 turn_start 那条更早的清空）。只有内容还是同一条
    // 草稿时才清，避免把用户在最后几毫秒里新敲的字一起抹掉。
    sendingRef.current = false;
    setSending(false);
    sentRef.current = null;
    setText((cur) => (cur === draft ? "" : cur));
    dropSent(sent);
    // 「已忽略 N 张」和逐文件错误描述的是**这份草稿**，草稿发出去了它们就该一起走。
    // 少这两行的症状是真的：某一次投了 5 张被挡掉 1 张，此后每一条消息都挂着那句
    // 「已忽略 1 张」，用户会以为界面坏了 —— 而它连「这次到底成没成」都不再能说清。
    setErrors([]);
    setIgnored(0);
  };

  return (
    <form
      className="composer"
      data-testid="composer"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      // 粘贴挂在 form 这一层：真实浏览器里 paste 的 target 是获得焦点的 textarea，
      // 只会**向上**冒泡，不会向下进到子树 —— 挂在 `ImageStrip` 上收不到任何东西。
      onPaste={(e) => {
        const items = e.clipboardData?.items;
        if (!items) return;
        const files = imageFilesFromPaste(items);
        if (files.length > 0) void addFiles(files);
      }}
    >
      {/* 拖放与粘贴的落点：drop 挂在这一层，paste 挂在 form 上（真实浏览器里 paste 的
          target 是获得焦点的 textarea，只会向上冒泡到 form，不会向下进到子树）。 */}
      <div
        className="composer-main"
        data-testid="drop-zone"
        onDrop={(e) => {
          // 自己处理掉，别让下面那个「整窗可拖」的文档监听再收一遍（否则同一批图进两次）。
          e.preventDefault();
          e.stopPropagation();
          void addFiles(Array.from(e.dataTransfer.files ?? []));
        }}
        // 不拦 dragover 的话浏览器根本不把这次拖放交给 drop。
        onDragOver={(e) => e.preventDefault()}
      >
        <ImageStrip
          images={pending}
          errors={errors}
          ignored={ignored}
          disabled={streaming}
          onRemove={removePending}
          onPick={(files) => void addFiles(files)}
        />
        <div className="composer-row">
          <textarea
            ref={inputRef}
            aria-label="输入消息"
            className="composer-input"
            value={text}
            disabled={streaming}
            rows={3}
            placeholder="说点什么…（Enter 发送，Shift+Enter 换行）"
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              // 中文输入法的候选框里按 Enter 是**选词**，不是发送：`isComposing`
              // （以及老 WebView 的 keyCode 229）必须放过，否则打「你好」按回车就发出去半个词。
              if (e.key !== "Enter" || e.shiftKey) return;
              if (e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229) return;
              e.preventDefault();
              void submit();
            }}
          />
          {streaming ? (
            <button
              type="button"
              className="stop"
              disabled={stopRequested}
              onClick={stop}
            >
              {/* spec:475：点击后「立即禁用并显示停止中…」。文案与 `stopRequested` 同一个
                  真相，所以不需要第三个状态位 —— 按钮灰着而字还是「停止」，用户读到的是
                  「点不动」，不是「已经在路上了」。 */}
              {stopRequested ? "停止中…" : "停止"}
            </button>
          ) : (
            <button type="submit" className="send" disabled={sending}>
              {sending ? "发送中…" : "发送"}
            </button>
          )}
        </div>
      </div>
    </form>
  );
}
