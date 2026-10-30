import { accentTriplet, normalizeHex } from "./accent";

export const THEME_NAMES = ["noir", "paper", "ocean", "dusk"] as const;
export type ThemeName = (typeof THEME_NAMES)[number];
export type Mode = "light" | "dark";

/* 2026-10-25 追加 "dusk"（暮褐）：spec 443 原写「预设主题 3 套」，业主批准后加第 4 套 ——
 * 明暗两块住在 presets.css，玻璃层的主题级覆盖住在 tokens.css 末尾（dusk 有自己的暖褐底，
 * 不走 mode 级玻璃块那套统一深色）。tokens.test.ts 的块数闸跟着从 6 改成由名单推导。 */

/** 预设块负责拥有的 token。`tokens.test.ts` 逐块断言「每个预设覆盖这 17 个、一个不漏」，
 *  所以这里少列一个就等于漏项测试少守一个（R124）。
 *  `--accent-hover`（R168）与 `--shadow`（R170）是 T15 修复轮补进来的两项：前者是因为
 *  `a:hover` 曾经退回 `--text-primary`，让链接悬停比静止时**更不显眼**；后者是因为
 *  `styles.css` 的 `box-shadow` 曾经直接写死一条 `rgb(0 0 0 / 20%)`，两道 hex 闸都看不见它，
 *  而 minifier 会把它写回成 `#0003` 塞进产物 —— 所以它现在只能取 `var(--shadow)`，
 *  值由 `tokens.css` 给基座默认、`presets.css` 的六个预设块（三套 × 明/暗）各自覆盖。
 *  `--radius-sm` 与 `--font-code`（T50 质感轮）：前者是因为圆角只有一档时 72px 缩略图与整块
 *  面板共用同一个值；后者是因为全仓当时没有任何等宽声明，工具调用 JSON 与代码块都排在
 *  界面字体上。`--font-code` 进名单而不是落进 `styles.css`，是被文件头那条纪律逼的：
 *  组件样式表里**不许声明 `--*`**（R133，且有测试守着），而间距那一类永不变化的量走字面值，
 *  不该为五个常量付六块 × 一行的重复。 */
export const THEME_TOKENS = [
  "--bg-surface",
  "--bg-raised",
  "--bg-input",
  "--text-primary",
  "--text-muted",
  "--accent",
  "--accent-hover",
  "--accent-text",
  "--border",
  "--danger",
  "--code-bg",
  "--radius",
  "--radius-sm",
  "--shadow",
  "--font-family",
  "--font-code",
  "--bubble-max-w",
];

/** 由 `applyTheme` 在 `<html>` 的 inline style 上拥有的 token：字号是用户设置而不是配色。
 *  它只在 `tokens.css` 的 `:root` 留一份「applyTheme 还没跑时」的兜底，绝不进预设块（R124）。 */
export const USER_TOKENS = ["--font-size"];

/** T44 裁定 1：`--accent` 三件套是 inline **临时覆盖**，真相仍在预设 —— 与 `USER_TOKENS`
 *  的「inline **独占**、预设不许有」是两种语义，所以它们进 `ACCENT_TOKENS` 而不是
 *  `USER_TOKENS`（后者撞红的三条在跑的闸见 `tokens.test.ts` 名单完整性那条）。
 *  `ACCENT_TOKENS ⊆ THEME_TOKENS` 由那条测试钉住：预设块定义了它们，inline 被清除后
 *  层叠自然回落到那一份 —— 这就是「跟随预设」的结构保证。 */
export const ACCENT_TOKENS = ["--accent", "--accent-hover", "--accent-text"] as const;

/**
 * `accent` 是 T44 加的**必填**第 5 入参：可选意味着漏传合法，而漏传的症状是「强调色设了
 * 没反应」（计划 :734）。`""` 与认不出的形状都走「清除 inline、跟随预设」—— 不抛错、
 * 不出声：校验边界只有 Rust 一处（R123 同一条纪律），用户手改 `settings.json` 写进
 * `"tomato"` 时 `load()` 已经把它规范成 `""`，这里的宽容只是兜住直接调用方。
 * 这里翻面了 R122 的旧语义（「--accent 只由 presets.css 拥有」）：Plan C 之后真相是
 * 「预设 + 用户覆盖」，覆盖只能走这一个出口。
 */
export function applyTheme(
  el: HTMLElement,
  theme: string,
  mode: Mode,
  fontSize: number,
  accent: string,
): void {
  if (!THEME_NAMES.includes(theme as ThemeName)) {
    throw new Error(`未知主题 "${theme}"，可选：${THEME_NAMES.join(", ")}`);
  }
  if (mode !== "light" && mode !== "dark") {
    throw new Error(`未知模式 "${mode}"`);
  }
  el.setAttribute("data-theme", theme);
  el.setAttribute("data-mode", mode);
  el.style.setProperty("--font-size", `${fontSize}px`);
  const hex = normalizeHex(accent);
  if (hex === null || hex === "") {
    for (const t of ACCENT_TOKENS) el.style.removeProperty(t);
    return;
  }
  const triplet = accentTriplet(hex, mode);
  el.style.setProperty(ACCENT_TOKENS[0], triplet.accent);
  el.style.setProperty(ACCENT_TOKENS[1], triplet.hover);
  el.style.setProperty(ACCENT_TOKENS[2], triplet.text);
}

/** 面向设置的适配器（R125）。`Settings.theme` 是自由字符串（`settings.rs:243`，serde 不校验），
 *  磁盘上可能躺着未来版本写入的主题名；那种情况必须「回落 + 让人看见」，而不是启动即抛、白屏。
 *  `applyTheme` 保持抛错（程序员传错名的路径），设置加载侧走这里，名单只有这一份。 */
export function resolveTheme(name: string): {
  theme: ThemeName;
  fellBack: boolean;
} {
  if (THEME_NAMES.includes(name as ThemeName)) {
    return { theme: name as ThemeName, fellBack: false };
  }
  return { theme: "noir", fellBack: true };
}
