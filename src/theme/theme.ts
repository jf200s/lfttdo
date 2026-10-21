export const THEME_NAMES = ["noir", "paper", "ocean"] as const;
export type ThemeName = (typeof THEME_NAMES)[number];
export type Mode = "light" | "dark";

/** 预设块负责拥有的 token。`tokens.test.ts` 逐块断言「每个预设覆盖这 15 个、一个不漏」，
 *  所以这里少列一个就等于漏项测试少守一个（R124）。
 *  `--accent-hover`（R168）与 `--shadow`（R170）是 T15 修复轮补进来的两项：前者是因为
 *  `a:hover` 曾经退回 `--text-primary`，让链接悬停比静止时**更不显眼**；后者是因为
 *  `App.css` 里剩了一条两道闸都看不见的 `rgb(0 0 0 / 20%)`，而 minifier 会把它写回成 `#0003`。 */
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
  "--shadow",
  "--font-family",
  "--bubble-max-w",
];

/** 由 `applyTheme` 在 `<html>` 的 inline style 上拥有的 token：字号是用户设置而不是配色。
 *  它只在 `tokens.css` 的 `:root` 留一份「applyTheme 还没跑时」的兜底，绝不进预设块（R124）。 */
export const USER_TOKENS = ["--font-size"];

export function applyTheme(
  el: HTMLElement,
  theme: string,
  mode: Mode,
  fontSize: number,
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
