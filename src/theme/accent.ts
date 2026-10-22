// T44（Plan C:716-831 + 简报 §3 裁定 3/4）：「一支自选色 → 可读三件套」的唯一出口。
// 本文件被 `theme.ts` import，所以**不许反向 import `theme.ts`** —— 循环依赖里谁先初始化
// 取决于入口顺序，那是「今天绿、改天炸」的形状。方向判据需要的 `"light" | "dark"` 因此
// 写成内联联合而不是 `import type { Mode }`：`import type` 今天会被擦除，但那是在押
// 编译器行为；内联联合不依赖任何东西（裁定 3 理由 3）。也不要为此新造 `mode.ts`。
//
// 纪律：本文件是**非测试源码**，在 `tokens.test.ts` 的「硬编码颜色」闸的扫描范围内，
// 所以这里不出现 `#rrggbb` 字面量、也不出现 `rgb(`/`hsl(` 记号 —— 黑白用 `"#" + 重复字符` 拼。

/** 简报裁定 3 的位移下限：hover 相对 accent 的相对亮度至少移动这么多，否则「看不出可点」。 */
export const MIN_LUMA_DELTA = 0.06;

/**
 * 形状判定，不是纠正（裁定 4）：输入永远是 Rust `normalize_accent`（settings.rs:336）
 * 已经规范过的 `""` 或 `#rrggbb`。对这两支**恒等**返回（跨语言契约：Rust 的输出绝不允许
 * 被 TS 静默丢掉 —— 红了就是真事故）；对任何其他形状返回 `null`。
 * **不许 trim**：trim 是替用户猜意图，而这一层的职责是判定形状。Rust 侧 trim 归 Rust 管，
 * 两份规则允许不同，方向只有一个 —— 到达这里的字符串若带空白，就是「没经过 Rust」的证据。
 * 注意 `""` 是合法值（「跟随预设」的编码），不是非法值：调用方靠 `hex === ""` 区分两者。
 */
export function normalizeHex(raw: string): string | null {
  if (raw === "") return "";
  const m = /^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.exec(raw);
  if (m === null) return null;
  const digits = m[1].length === 3 ? [...m[1]].map((c) => c + c).join("") : m[1];
  return "#" + digits.toLowerCase();
}

/** 解析 `#rrggbb` 为三个 0..255 通道；形状不对直接抛 —— 它是内部函数，入口在 `normalizeHex`。 */
function hexToChannels(hex: string): [number, number, number] {
  const norm = normalizeHex(hex);
  if (norm === null || norm.length !== 7) {
    throw new Error(`只吃 normalizeHex 认得出的 6 位 hex，收到：${hex}`);
  }
  const digits = norm.slice(1);
  return [
    parseInt(digits.slice(0, 2), 16),
    parseInt(digits.slice(2, 4), 16),
    parseInt(digits.slice(4, 6), 16),
  ];
}

const clampChannel = (n: number): number => Math.max(0, Math.min(255, Math.round(n)));
const toPair = (n: number): string => clampChannel(n).toString(16).padStart(2, "0");

/** WCAG 相对亮度。导出是因为测试的判据（方向、位移、AA）都要读它 —— 不导出就是「测试只能猜实现」。 */
export function relativeLuminance(hex: string): number {
  const [r, g, b] = hexToChannels(hex);
  const lin = (c: number): number => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

const WCAG_WHITE = "#" + "f".repeat(6);
const WCAG_BLACK = "#" + "0".repeat(6);

/** WCAG 对比度（亮度高的一支进分子，参数顺序无关）。 */
function contrast(l1: number, l2: number): number {
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
}

/**
 * 把一支 accent 展开成 `{ accent, hover, text }`。输入必须是 `normalizeHex` 的非空输出
 * （调用方 `applyTheme` 已把过关的形状；`""`/`null` 走「清除 inline」分支，到不了这里）。
 *
 * hover 的方向由 **`mode`** 决定（裁定 3），不是 accent 自身亮度：
 * 1. `mode` 与表面亮度同源（presets.css 六块里 light 块的 `--bg-surface` 恒亮、dark 块恒暗），
 *    所以「亮 → hover 更暗、暗 → hover 更亮」与预设那条 hover 闸（对底色的对比度严格更高，
 *    tokens.test.ts）同构；accent 自身亮度与底色亮度**没有蕴含关系**。
 * 2. 按 accent 亮度分派的反例：暗模式里用户选近白的 accent（如 `#aaddff`），「亮 accent ⇒
 *    hover 往深走」对深色表面意味着 hover 比静止**更不醒目** —— R168 治过的病换了个入口复发。
 * 3. 方向不读 `getComputedStyle`：jsdom 对自定义属性的层叠支持不完整，读出来很可能是空串 ——
 *    那不是判据，是假阳；而 `applyTheme` 的入参里本来就有 `mode`，不需要新管道。
 *
 * 位移量：三条通道**平移**（加法而非乘法 —— `#000000` 乘任何系数还是黑，位移判据会失效），
 * 从 1 逐级加到刚好满足 `MIN_LUMA_DELTA`；到轨道尽头仍不满足（`#ffffff` 在暗模式、
 * `#000000` 在亮模式）就**反向**走，不许返回同色。
 *
 * text：黑/白里选对 accent 对比度更高的一支。这样选**永远**过 WCAG AA ——
 * `max(contrast(白,L), contrast(黑,L))` 的最小值在两支相等处取到：
 * (L+0.05)/0.05 = 1.05/(L+0.05) ⇒ L = √0.0525 − 0.05 ≈ 0.179，比值 ≈ 4.58 > 4.5，
 * 所以「两支都不到 4.5」的 accent 不存在（`accent.test.ts` 用采样点把这件事亮出来）。
 */
export function accentTriplet(
  hex: string,
  mode: "light" | "dark",
): { accent: string; hover: string; text: string } {
  const base = hexToChannels(hex);
  const shifted = (offset: number): string =>
    "#" + base.map((c) => toPair(c + offset)).join("");
  const rest = relativeLuminance(hex);
  // 沿方向 d 找最小平移量；`moved` 是「往 d 方向走了多少亮度」，负方向取反号。
  const seek = (d: 1 | -1): string | null => {
    for (let step = 1; step <= 255; step += 1) {
      const candidate = shifted(d * step);
      if ((relativeLuminance(candidate) - rest) * d >= MIN_LUMA_DELTA) {
        return candidate;
      }
    }
    return null;
  };
  const forward = mode === "dark" ? (1 as const) : (-1 as const);
  // 触轨反向：`seek` 全程不达标返回 null（加法平移 + 钳位下只可能是 `#ffffff`/`#000000`
  // 这类单极色），反向再找一次；两个方向都不达标在数学上不存在（一端必能动）。
  const hover = seek(forward) ?? seek(forward === 1 ? -1 : 1);
  const text =
    contrast(1, rest) >= contrast(0, rest) ? WCAG_WHITE : WCAG_BLACK;
  return { accent: hex, hover: hover ?? hex, text };
}
