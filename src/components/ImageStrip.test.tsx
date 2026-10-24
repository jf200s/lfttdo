// T19「图片输入三入口」的测试文件。两份职责（都写在文件名能找到的地方）：
// ① 直接断言 `ImageStrip` —— R146 裁定「至少一条 `ImageStrip` 自己的用例」，
//    否则这个文件只是挂着别的名字的 Composer 测试。
// ② 本仓**第一份直接测 `Composer` 的文件**（R216①：此前 Composer 只被 `App.test.tsx` 间接跑到）。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import { ImageStrip, MAX_IMAGES_PER_MESSAGE, type PendingImage } from "./ImageStrip";
import type { ContentBlock, SendBlock } from "../types";

/** `normalize_image` 成功回来的那一支（线格式的 image 臂）。 */
const okBlock: ContentBlock = { type: "image", data: "AAAA", mime: "png" };
/** 三个入口共用的成功归一化桩（把参数类型写出来，`mock.calls[0][0]` 才是 `ArrayBuffer`）。 */
const okNormalize = () => vi.fn((_bytes: ArrayBuffer) => Promise.resolve(okBlock));
/** `onSend` 的合同是 Promise（成功与否决定 pending 清不清），桩因此必须是 async 的。 */
const okSend = () => vi.fn((_blocks: SendBlock[]) => Promise.resolve());

const png = (n: number, name = "a.png") =>
  new File([new Uint8Array([n])], name, { type: "image/png" });

const dropOn = (files: File[]) =>
  fireEvent.drop(screen.getByTestId("drop-zone"), {
    dataTransfer: { files } as unknown as DataTransfer,
  });

const stub = (name: string, mime: "png" | "jpeg"): PendingImage => ({
  name,
  previewUrl: `data:image/${mime};base64,AAAA`,
  block: { type: "image", data: "AAAA", mime },
});

describe("ImageStrip", () => {
  it("names each thumbnail by its file name and reports the index it removes", () => {
    const onRemove = vi.fn();
    render(
      <ImageStrip
        images={[stub("第一张.png", "png"), stub("第二张.jpeg", "jpeg")]}
        errors={[]}
        ignored={0}
        disabled={false}
        onRemove={onRemove}
        onPick={() => {}}
      />,
    );
    // 可访问名必须是**文件名**：读屏用户要能在四张缩略图里认出移除的是哪一张。
    expect(screen.getByTestId("thumb-0")).toHaveAccessibleName("第一张.png");
    expect(screen.getByTestId("thumb-1")).toHaveAccessibleName("第二张.jpeg");
    // 按名字点第二张的移除键：`onRemove` 必须收到 1。写死 0 的实现在这里红，
    // 而不是在「想删第三张、结果删掉第一张」的用户面前红。
    fireEvent.click(screen.getByRole("button", { name: "移除 第二张.jpeg" }));
    expect(onRemove).toHaveBeenCalledWith(1);
  });
});

describe("Composer 图片入口", () => {
  it("normalizes a dropped file with its own bytes and lists it as pending", async () => {
    const onNormalize = okNormalize();
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([png(7, "截图.png")]);
    await screen.findByTestId("thumb-0");
    expect(screen.getByTestId("thumb-0")).toHaveAccessibleName("截图.png");
    expect(onNormalize).toHaveBeenCalledTimes(1);
    // 交出去的是**整块 ArrayBuffer**、而且就是这张图的字节（T3 的 subarray `.buffer` 陷阱
    // 与「上一轮残留」都在这里红；`commands.ts` 那层另外钉 invoke 的第二参形状）。
    const bytes = onNormalize.mock.calls[0][0];
    expect(bytes).toBeInstanceOf(ArrayBuffer);
    expect(new Uint8Array(bytes)).toEqual(new Uint8Array([7]));
  });

  // R216④：`text.trim()===""` 那道闸在「有图无字」时必须放行 —— 只贴一张图不打字是日常主用法。
  // 这条红了的用户可见症状是「贴图不打字，点发送没反应」。
  it("sends an image-only message with an empty textarea", async () => {
    const onSend = okSend();
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={okNormalize()}
      />,
    );
    dropOn([png(1)]);
    await screen.findByTestId("thumb-0");
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));
    expect(onSend.mock.calls[0][0]).toEqual([okBlock]);
  });

  // Step 2 第 7 条：顺序固定「图在前、文本在最后」，且文本为空时**不产出空的 text 块**。
  it("sends text plus images as ordered blocks with the text last", async () => {
    const onSend = okSend();
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={okNormalize()}
      />,
    );
    dropOn([png(1)]);
    await screen.findByTestId("thumb-0");
    fireEvent.change(screen.getByRole("textbox", { name: "输入消息" }), {
      target: { value: "这张图里是什么" },
    });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));
    expect(onSend.mock.calls[0][0]).toEqual([
      okBlock,
      { type: "text", text: "这张图里是什么" },
    ]);
  });

  // Step 2 第 8 条上半场：发成功之后待发图与输入框都要清空（下一条钉的是下半场「失败不许清」）。
  it("clears the pending images and the textarea after a successful send", async () => {
    const onSend = okSend();
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={okNormalize()}
      />,
    );
    dropOn([png(1)]);
    await screen.findByTestId("thumb-0");
    const box = screen.getByRole("textbox", { name: "输入消息" });
    fireEvent.change(box, { target: { value: "看图" } });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(screen.queryByTestId("thumb-0")).toBeNull());
    expect(box).toHaveValue("");
  });

  // Step 2 第 8 条下半场：发送失败**保留**待发图。清空的用户可见症状是「图没发出去、
  // 输入框也空了，只能重新贴一遍」—— 所以这一条的变异判据就是那句 `throw e`（见报告）。
  it("keeps the pending images when the send fails", async () => {
    const onSend = vi.fn((_blocks: SendBlock[]) =>
      Promise.reject("尚未配置 API key"),
    );
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={okNormalize()}
      />,
    );
    dropOn([png(3, "合同扫描件.png")]);
    await screen.findByTestId("thumb-0");
    fireEvent.change(screen.getByRole("textbox", { name: "输入消息" }), {
      target: { value: "看一下这份合同" },
    });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));
    // 图还在，文本也还在：失败的那一次不许把用户已经准备好的东西吃掉。
    expect(await screen.findByTestId("thumb-0")).toHaveAccessibleName(
      "合同扫描件.png",
    );
    expect(screen.getByRole("textbox", { name: "输入消息" })).toHaveValue(
      "看一下这份合同",
    );
  });

  // Step 2 第 4 条：超过 4 张时**先处理前 4 个**，再给一条说得清「忽略了几张」的提示。
  // 变异判据：把 `slice` 的额度算错（或不提示）—— 前 4 张照常出缩略图、用户以为四张都发出去了。
  it("refuses the fifth image with a visible count, after normalizing the first four", async () => {
    const onNormalize = okNormalize();
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn(
      Array.from({ length: MAX_IMAGES_PER_MESSAGE + 1 }, (_, i) =>
        png(i, `i${i}.png`),
      ),
    );
    expect(await screen.findByTestId("image-overflow")).toHaveTextContent(
      `一次最多 ${MAX_IMAGES_PER_MESSAGE} 张，已忽略 1 张`,
    );
    // 被拒的是第 5 张，不是前 4 张：归一化恰好跑了 4 次、缩略图恰好 4 张。
    await waitFor(() => expect(onNormalize).toHaveBeenCalledTimes(4));
    expect(screen.getByTestId("thumb-3")).toBeInTheDocument();
    expect(screen.queryByTestId("thumb-4")).toBeNull();
  });

  // R195：错误行只做「文件名 + Rust 原文逐字上屏」两件事，前端不解释文案。
  // fixture 用的是 T18 裁定（R184/R185）真实可达的那句超限文案。
  it("names the file and echoes the rejection verbatim when a normalize fails", async () => {
    // 这一句故意**不含** "MB"：断言若还像原 brief 那样钉 `toContain("5MB")`，这里必红 ——
    // 红的是断言而不是实现，这才证明前端是中性的（R195 的变异判据）。
    const rejected = "图片太大（当前 47185920 字节），请先裁剪或缩小后再发送";
    const onNormalize = vi.fn((_bytes: ArrayBuffer) => Promise.reject(rejected));
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([png(1, "超大截图.png")]);
    const msg = await screen.findByTestId("image-error");
    expect(msg.textContent).toContain(rejected);
    expect(msg.textContent).toContain("超大截图.png");
    // 绝不静默丢图的另一半：失败的那张不许留下一张打不开的缩略图。
    expect(screen.queryByTestId("thumb-0")).toBeNull();
  });

  // R145：jsdom 里没有 `DataItem` / `DataTransferItem` 全局，所以用最小字面量桩。
  // 实现侧只许读 `clipboardData.items[i].type` 与 `getAsFile()`（真实浏览器形状），
  // 不许读 `clipboardData.files`（jsdom 与 Chrome 的语义差异面）—— 桩里特意放了 files 那一支，
  // 读它的话这条会「过」得毫无疑义，所以换成只喂 items 的桩来钉住（见下一条）。
  it("accepts paste as well as drop", async () => {
    const onNormalize = okNormalize();
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    const pasted = new File([new Uint8Array([9])], "粘贴的图.png", {
      type: "image/png",
    });
    fireEvent.paste(screen.getByTestId("composer"), {
      clipboardData: {
        items: [
          // 文本那一支必须被跳过：往输入框里粘一段字不该多出一张图。
          { kind: "string", type: "text/plain", getAsFile: () => null },
          { kind: "file", type: "image/png", getAsFile: () => pasted },
        ],
        files: [],
      } as unknown as DataTransfer,
    });
    await screen.findByTestId("thumb-0");
    // 只出现缩略图不够：必须证明归一化收到的就是这张图的字节，而不是上一轮的残留。
    expect(onNormalize).toHaveBeenCalledTimes(1);
    expect(new Uint8Array(onNormalize.mock.calls[0][0])).toEqual(
      new Uint8Array([9]),
    );
  });

  // R144：第三个入口是 `<input type="file" multiple accept="image/*">`，不是 dialog 返回的
  // 路径（capabilities 里没有任何 fs 读权限，拿到路径也读不出字节，为此放权违反安全红线）。
  // 断言它汇入**同一个** addFiles 路径：产物同样是 File，同样走 arrayBuffer → onNormalize。
  it("accepts the file picker as well as drop and paste", async () => {
    const onNormalize = okNormalize();
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    const input = screen.getByTestId("file-input");
    expect(input).toHaveAttribute("type", "file");
    expect(input).toHaveAttribute("multiple");
    expect(input).toHaveAttribute("accept", "image/*");
    // jsdom 的 file input 读 `value` 恒为 `""`（它不接受程序赋值的那份文件名），所以原先
    // 「选完之后 value 是空」这条断言对生产代码是**聋的**：删掉 `e.target.value = ""`
    // 也不会红，而那正是「第二次选同一个文件没反应」的真 bug。
    // 把 `value` 做成 own 可写属性、初值写成浏览器真实会填进去的 `C:\fakepath\…`，
    // 这条断言才开始描述实现（变异判据：删掉 ImageStrip.tsx 的那一行 ⇒ 这条红）。
    Object.defineProperty(input, "value", {
      writable: true,
      value: "C:\\fakepath\\选取.png",
    });
    fireEvent.change(input, { target: { files: [png(11, "选取.png")] } });
    await screen.findByTestId("thumb-0");
    expect(onNormalize).toHaveBeenCalledTimes(1);
    expect(new Uint8Array(onNormalize.mock.calls[0][0])).toEqual(
      new Uint8Array([11]),
    );
    // 选完必须把 value 清掉：不清的话「连续选取同一个文件」第二次连 change 都不会触发。
    expect((input as HTMLInputElement).value).toBe("");
  });

  // Step 1 原文的「removes a pending image before sending」：移除之后额度要**真的腾出来**，
  // 所以这里补一张新的 —— 只断「不见了」的测试放得过「清空整个列表」那种错实现。
  it("removes one pending image and frees its slot", async () => {
    const onNormalize = okNormalize();
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([png(1, "留着的.png"), png(2, "要删的.png")]);
    await screen.findByTestId("thumb-1");
    fireEvent.click(screen.getByRole("button", { name: "移除 要删的.png" }));
    expect(screen.queryByTestId("thumb-1")).toBeNull();
    expect(screen.getByTestId("thumb-0")).toHaveAccessibleName("留着的.png");
    // 腾出额度：删掉一张之后还可以再贴一张（第 5 张那条用例的上半场）。
    dropOn([png(3, "补上的.png")]);
    await screen.findByTestId("thumb-1");
    expect(screen.getByTestId("thumb-1")).toHaveAccessibleName("补上的.png");
    expect(onNormalize).toHaveBeenCalledTimes(3);
  });

  // Step 2 第 9 条：`streaming` 期间三个入口一起关掉 —— 半途改图会让「发出去的那批」和
  // 界面上看到的那批不是同一批，而那正是 vision 会话里最难自查的一种错位。
  it("closes all three entries while streaming", async () => {
    const onNormalize = okNormalize();
    render(
      <Composer
        streaming
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    const input = screen.getByTestId("file-input");
    expect(input).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "输入消息" })).toBeDisabled();

    dropOn([png(1)]);
    fireEvent.paste(screen.getByTestId("composer"), {
      clipboardData: {
        items: [{ kind: "file", type: "image/png", getAsFile: () => png(2) }],
      } as unknown as DataTransfer,
    });
    fireEvent.change(input, { target: { files: [png(3)] } });
    // 三个入口都点过一遍。等**一个宏任务**而不是一个微任务：`addFiles` 里第一个 await 是
    // `file.arrayBuffer()`，只等 `Promise.resolve()` 的话断言会抢在链跑完之前跑，
    // 于是「闸门被删掉」也能绿（实测过：变异后仍绿，就是这里）。
    await new Promise((r) => setTimeout(r, 10));
    expect(onNormalize).not.toHaveBeenCalled();
    expect(screen.queryByTestId("thumb-0")).toBeNull();
  });

  // `normalize_image` 的合同只会回 image 支，但 `ContentBlock` 是四支联合（types.ts:62），
  // 所以 TS 侧必须收窄一次。收窄失败那一路**也得可见**：静默 `continue` 的症状是
  // 「贴了图、没缩略图、没报错、模型答非所问」，正是 Step 2 第 5 条禁的事。
  it("surfaces a normalize that answers with something other than an image block", async () => {
    const onNormalize = vi.fn((_bytes: ArrayBuffer) =>
      Promise.resolve({ type: "text", text: "这不是图" } as ContentBlock),
    );
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([png(1, "怪图.png")]);
    const msg = await screen.findByTestId("image-error");
    expect(msg.textContent).toContain("怪图.png");
    expect(screen.queryByTestId("thumb-0")).toBeNull();
  });

  // 修复轮 F1：提示与错误描述的是**这份草稿**，草稿发出去了它们必须跟着走。
  // 少那两行 reset 的真实症状是「某一次投了 5 张被挡掉 1 张，此后每一条消息都挂着
  // 已忽略 1 张」—— 界面从此不再能说清这一次到底成没成。
  it("drops the overflow note once a send succeeds", async () => {
    const onNormalize = okNormalize();
    const onSend = okSend();
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([1, 2, 3, 4, 5].map((n) => png(n, `第${n}张.png`)));
    await screen.findByTestId("thumb-3");
    expect(screen.getByTestId("image-overflow")).toHaveTextContent("已忽略 1 张");
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));
    expect(screen.queryByTestId("image-overflow")).toBeNull();
  });

  it("drops a per-file error note once a later send succeeds", async () => {
    // 第一次归一化失败、第二次成功：界面上应当同时出现「一条错误」和「一张待发图」，
    // 发送成功后错误消失、图也发出去了。
    let n = 0;
    const onNormalize = vi.fn((_bytes: ArrayBuffer) => {
      n += 1;
      return n === 1
        ? Promise.reject(new Error("图片太大"))
        : Promise.resolve(okBlock);
    });
    const onSend = okSend();
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([png(1, "坏.png")]);
    await screen.findByTestId("image-error");
    dropOn([png(2, "好.png")]);
    await screen.findByTestId("thumb-0");
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));
    expect(screen.queryByTestId("image-error")).toBeNull();
    // 发出去的仍是那四元组形状的图片块（清提示不许顺手改载荷）。
    expect(onSend.mock.calls[0][0]).toEqual([okBlock]);
  });

  // 同一条规则的另一半：发**失败**时不许清 —— 那正是用户还要再看一眼错误的时候。
  it("keeps the notes when the send fails", async () => {
    const onNormalize = okNormalize();
    const onSend = vi.fn(() => Promise.reject("网络断了"));
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={vi.fn()}
        onNormalize={onNormalize}
      />,
    );
    dropOn([1, 2, 3, 4, 5].map((n) => png(n, `第${n}张.png`)));
    await screen.findByTestId("thumb-3");
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));
    expect(screen.getByTestId("image-overflow")).toBeInTheDocument();
    expect(screen.getByTestId("thumb-0")).toHaveAccessibleName("第1张.png");
  });
});

describe("Composer 键盘与整窗拖放", () => {
  it("Enter 发送、Shift+Enter 换行、输入法组合期不发送", async () => {
    const onSend = okSend();
    render(
      <Composer
        streaming={false}
        onSend={onSend}
        onStop={() => {}}
        onNormalize={okNormalize()}
      />,
    );
    const ta = screen.getByLabelText("输入消息");
    fireEvent.change(ta, { target: { value: "你好" } });
    // 换行与输入法选词都不许触发发送（中文用户按 Enter 选词是日常）。
    fireEvent.keyDown(ta, { key: "Enter", shiftKey: true });
    fireEvent.keyDown(ta, { key: "Enter", isComposing: true });
    expect(onSend).not.toHaveBeenCalled();
    fireEvent.keyDown(ta, { key: "Enter" });
    await waitFor(() =>
      expect(onSend).toHaveBeenCalledWith([{ type: "text", text: "你好" }]),
    );
  });

  it("把图片拖到窗口任意位置也进待发区", async () => {
    const normalize = okNormalize();
    render(
      <Composer
        streaming={false}
        onSend={okSend()}
        onStop={() => {}}
        onNormalize={normalize}
      />,
    );
    // 不在 `drop-zone` 上：整窗兜底才算「拖进窗里就有用」。
    fireEvent.drop(document, {
      dataTransfer: { files: [png(9, "全窗.png")] } as unknown as DataTransfer,
    });
    await waitFor(() => expect(normalize).toHaveBeenCalledTimes(1));
  });
});
