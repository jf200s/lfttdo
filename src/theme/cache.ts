// T44（Plan C:791-817 + 简报 §3 裁定 2）：冷启动主题缓存。
// 这是**缓存**，不是第二份真相：
// - `writeThemeCache` 只在 `applyTheme` 成功之后由 App 调用；`settings_get` 回来后无条件覆盖；
// - `seedFromCache` 内部只走 `resolveTheme` + `applyTheme`（一份实现、两个入口，R117 口径），
//   这里**不许**自己写 `setAttribute`/`setProperty` —— 第二份写法必然与 `applyTheme` 漂移，
//   而漂移的症状（accent 设了没反应）没人测得出来；
// - 它不违反 R169：当年删掉的是 `Settings::default()` 的第 N 份硬编码拷贝，这里装的是
//   **用户上一次的真值**；缓存缺失/坏掉时什么都不做，第一帧仍是 `tokens.css` 的 `:root` 兜底。
//
// API key 红线（简报 §6）：缓存的字段集合是**白名单**而不是 `Settings` 的转储 ——
// 只有 theme/mode/fontSize/accent 四键，绝不把 base_url、模型名或任何 key 相关字段
// 顺手塞进缓存。落盘时逐键显式构造（不是 stringify 整个入参对象），白名单因此是结构性的。
import { applyTheme, resolveTheme } from "./theme";
import type { Mode } from "./theme";

export const THEME_CACHE_KEY = "qwen-do.theme";

/** 缓存条形的形状（键名与 `settings_get` 的 snake_case 无关 —— 这是我们自己的存储，不是线协议）。 */
export interface ThemeCacheEntry {
  theme: string;
  mode: Mode;
  fontSize: number;
  accent: string;
}

/**
 * 首帧前同步播种：任何一步不合期望（无缓存、非 JSON、字段类型不对、名单外的主题名、
 * 不可用的 mode、storage 连读都抛）都**什么都不做**地返回。吞掉异常是设计而不是偷懒：
 * 首帧优化的失败模式应该是「看到默认主题」，不是白屏。
 */
export function seedFromCache(el: HTMLElement): void {
  let raw: string | null;
  try {
    raw = localStorage.getItem(THEME_CACHE_KEY);
  } catch {
    return;
  }
  if (raw === null) return;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return;
  const entry = parsed as Partial<ThemeCacheEntry>;
  if (
    typeof entry.theme !== "string" ||
    typeof entry.mode !== "string" ||
    typeof entry.fontSize !== "number" ||
    !Number.isFinite(entry.fontSize) ||
    typeof entry.accent !== "string"
  ) {
    return;
  }
  // 名单校验只有 `resolveTheme` 这一份（R125）。但缓存侧对「回落到 noir」的处置是**不种**：
  // 未来版本/手改的名字说明这份缓存不属于今天的名单，替用户写一个 noir 正是 R169 的病根。
  const { theme, fellBack } = resolveTheme(entry.theme);
  if (fellBack) return;
  try {
    // mode 不在两支联合里 ⇒ applyTheme 抛（R167 的运行时防线），这里吞成「不种」：
    // 「种一半」比不种更糟 —— data-theme 写了、data-mode 没写，六块预设一块都不匹配。
    applyTheme(el, theme, entry.mode, entry.fontSize, entry.accent);
  } catch {
    /* 坏缓存的全集都在这里：静默，首帧走 :root 兜底 */
  }
}

/**
 * 只在 `applyTheme` 成功之后调用（写早了会把「没生效的那次尝试」种进下一份启动）。
 * 写失败（隐私模式、配额）不许影响启动 ⇒ 吞异常，理由与 `seedFromCache` 同一句。
 */
export function writeThemeCache(entry: ThemeCacheEntry): void {
  try {
    localStorage.setItem(
      THEME_CACHE_KEY,
      JSON.stringify({
        theme: entry.theme,
        mode: entry.mode,
        fontSize: entry.fontSize,
        accent: entry.accent,
      }),
    );
  } catch {
    /* 吞掉它是设计：下次冷启动回到 :root 兜底，不是白屏 */
  }
}
