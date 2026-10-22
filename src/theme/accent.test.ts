// T44 Step 1（Plan C:716-831 + 简报 §3 裁定 3/4）：`accentTriplet` 是「一支自选色展开成
// 可读三件套」的唯一出口。为什么不是 `<input type="color">` 直接写 `--accent`：
// 预设里 `--accent-hover`/`--accent-text` 与 accent **配套**，只换一支的症状是
// 「按钮变亮黄、文字还是白」（R168 治过的同一族），而 `lint:css` 抓不到它。
// 本文件是测试夹具，hex 字面量是道具：`tokens.test.ts` 的两道「硬编码颜色」闸都豁免
// 测试文件（非测试源码里照样不许出现，所以 `accent.ts` 自己用拼接造黑白）。
import { describe, expect, it } from "vitest";
import {
  MIN_LUMA_DELTA,
  accentTriplet,
  normalizeHex,
  relativeLuminance,
} from "./accent";

/** WCAG 对比度，与 `tokens.test.ts` 自带的那份同式 —— 那边是预设块的闸，这里是本片的判据。 */
const contrast = (first: string, second: string): number => {
  const l1 = relativeLuminance(first);
  const l2 = relativeLuminance(second);
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
};

describe("accent triplet", () => {
  // 裁定 4：TS 是**解析器**，输入永远是 Rust 已经规范过的形状；对合法输入恒等返回，
  // 对任何其他形状返回 null。不许 trim —— 「trim 一下」就是替用户猜意图，这层的职责是判定形状。
  // 这与 Rust 侧 `normalize_accent`（会 trim）刻意不同：规则允许不同，方向只有一个
  // （Rust 的输出 TS 恒等收下；TS 认不出的东西本来就到不了它面前）。
  it("accepts only #rgb / #rrggbb and lowercases them, without guessing intent", () => {
    expect(normalizeHex("#ABC")).toBe("#aabbcc");
    expect(normalizeHex("#0aF")).toBe("#00aaff");
    expect(normalizeHex("#aabbcc")).toBe("#aabbcc");
    for (const bad of ["tomato", "#ff00", "#abcd", "#gggggg", "#00000000", "purple"]) {
      expect(normalizeHex(bad), `${bad} 不是 #rgb/#rrggbb 形状`).toBeNull();
    }
    // 带空格的输入**不 trim**（计划 Step 1 与裁定 4 在这条上一致）：Rust 会 trim 后发出
    // "#aabbcc"，TS 侧收到的永远是那份已规范的；这里出现空格本身就是「没经过 Rust」的证据。
    expect(normalizeHex("  #aabbcc "), "空白就是形状不对，不许替用户猜").toBeNull();
  });

  // 裁定 4 的跨语言契约（R242 的钉子）：Rust 的输出绝不允许被 TS 静默丢掉。
  // 这条红了就是真事故；「两边用例表同款」那种测法红了可能只是文档不同步。
  // 注意 `""` 在这里**恒等返回 ""**（合法值，不是 null）—— 计划 Step 1 把 `""` 列进
  // 非法名单，裁定 4 用这条契约把它改了：Rust 规范输出的两支里 `""` 是「跟随预设」的编码。
  it("never drops a value rust can emit", () => {
    for (const s of ["", "#000000", "#aabbcc", "#ffffff", "#00aaff"]) {
      expect(normalizeHex(s), `Rust 会原样发出 ${s}，TS 侧不许把它判成无效`).toBe(s);
    }
  });

  // 计划 Step 1 第一条 + 裁定 3：签名多收一个 `mode`（方向判据见 accent.ts 的注释）。
  it("keeps text legible on the picked accent instead of trusting the preset", () => {
    const { accent, hover, text } = accentTriplet("#aa00ff", "dark");
    expect(accent).toBe("#aa00ff"); // 自选色原样保留，三件套只有 accent 一支是用户挑的
    expect(hover, "hover 与静止同色 = 看不出可点（R168 治过的同一个病）").not.toBe(accent);
    expect(text, "深底必须白字").toBe("#ffffff");
    expect(accentTriplet("#ffff00", "light").text, "亮底必须黑字").toBe("#000000");
  });

  // 裁定 3 的硬要求：两个 mode 下 hover !== accent，且亮度位移至少 MIN_LUMA_DELTA。
  // 样本含两支触轨色（#000000/#ffffff），它们同时钉住「到边界反向走、不许返回同色」。
  it("always moves hover by at least MIN_LUMA_DELTA in either mode", () => {
    for (const hex of ["#2080ff", "#aa00ff", "#ffff00", "#7f7f7f", "#000000", "#ffffff"]) {
      for (const mode of ["light", "dark"] as const) {
        const { accent, hover } = accentTriplet(hex, mode);
        expect(hover, `${hex}/${mode}：hover 与静止同色`).not.toBe(accent);
        const delta = Math.abs(relativeLuminance(hover) - relativeLuminance(accent));
        expect(
          delta,
          `${hex}/${mode}：亮度位移 ${delta.toFixed(4)} 低于 MIN_LUMA_DELTA`,
        ).toBeGreaterThanOrEqual(MIN_LUMA_DELTA);
      }
    }
  });

  // 变异判据 (c) 的靶子：方向必须由 `mode` 决定。若实现改按「accent 自身亮度」分派，
  // 这两条至少红一条 —— #2080ff/亮：accent 暗 ⇒ 假规则往浅走，而亮表面上「更浅=更糊」；
  // #aaddff/暗：accent 亮 ⇒ 假规则往深走，对深色表面 hover 比静止**更不醒目**，
  // 正是 R168 治过的病（预设那条 hover 闸 tokens.test.ts 的判据同构于此）。
  it("takes the hover direction from the mode, not from the accent's own luminance", () => {
    const onLight = accentTriplet("#2080ff", "light");
    expect(
      relativeLuminance(onLight.hover),
      "亮模式的表面是亮的：醒目只能更暗，accent 自己的亮度不是方向判据",
    ).toBeLessThan(relativeLuminance(onLight.accent));
    const onDark = accentTriplet("#aaddff", "dark");
    expect(
      relativeLuminance(onDark.hover),
      "暗模式里选到近白的 accent 时，往深走会让 hover 比静止更不醒目（R168）",
    ).toBeGreaterThan(relativeLuminance(onDark.accent));
  });

  // 触轨（#ffffff 在暗模式、#000000 在亮模式）不许「返回同色了事」，也不许抛 ——
  // 反向走，位移判据仍然成立。上面一条的样本已覆盖 MIN_LUMA_DELTA，这里钉的是**方向**。
  it("flips direction at the rails instead of returning the same color", () => {
    const fromWhite = accentTriplet("#ffffff", "dark");
    expect(fromWhite.hover, "#ffffff 在暗模式不能再亮：必须反向（往深），不是原样返回").not.toBe("#ffffff");
    expect(
      relativeLuminance(fromWhite.hover),
      "反向之后应当更暗",
    ).toBeLessThan(relativeLuminance(fromWhite.accent));
    const fromBlack = accentTriplet("#000000", "light");
    expect(fromBlack.hover, "#000000 在亮模式不能再暗：必须反向（往浅）").not.toBe("#000000");
    expect(
      relativeLuminance(fromBlack.hover),
      "反向之后应当更亮",
    ).toBeGreaterThan(relativeLuminance(fromBlack.accent));
  });

  // text 判据的数学性质（写进 accent.ts 的注释，也测出来）：
  // `max(contrast(白,L), contrast(黑,L))` 的最小值在两支相等处取到 ——
  // (L+0.05)/0.05 = 1.05/(L+0.05) ⇒ L = √0.0525 − 0.05 ≈ 0.179，比值 ≈ 4.58 > 4.5。
  // （简报写的 4.65 略有出入，性质本身成立。）「两支都不到 4.5」的 accent 不存在，
  // 下面用采样点把它亮出来，含理论最差点 #767676 附近。
  it("always clears WCAG AA because it picks the better of black/white", () => {
    for (const hex of ["#7f7f7f", "#767677", "#767676", "#2080ff", "#aaddff", "#aa00ff", "#1a1a4a", "#ffff00", "#000000", "#ffffff"]) {
      const { accent, text } = accentTriplet(hex, "dark");
      expect(
        contrast(text, accent),
        `${hex} 选中的 ${text} 对底色不足 AA 的 4.5:1（黑/白择优的数学保证破了）`,
      ).toBeGreaterThanOrEqual(4.5);
    }
    // 中间灰的显式样本：黑 5.24 > 白 4.01 ⇒ 选黑。
    expect(accentTriplet("#7f7f7f", "dark").text).toBe("#000000");
  });
});
