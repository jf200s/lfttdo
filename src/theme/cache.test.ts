// T44 Step 3（Plan C:791-817 + 简报裁定 2/4、§4.5/§4.6）：冷启动主题缓存。
// 裁定：这是**缓存**不是第二份真相 —— `seedFromCache` 只走 `resolveTheme` + `applyTheme`
// （一份实现、两个入口，R117 口径），坏缓存的处置是「什么都不做」，第一帧仍是 `:root` 兜底。
// 测试收的是**注入元素**（简报 §4.5）：不动 `document.documentElement`，那是全局态，会串味。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { THEME_CACHE_KEY, seedFromCache, writeThemeCache } from "./cache";

describe("theme cache", () => {
  beforeEach(() => {
    localStorage.clear();
  });
  afterEach(() => {
    // `vi.spyOn(Storage.prototype, ...)` 的还原在各用例自己的 finally 里，
    // 这里再兜一道 reset：漏还原会污染同文件后续用例（简报 §5 的 mock 纪律）。
    vi.restoreAllMocks();
  });

  // 计划 Step 3 第一条（参数化改写：`seedFromCache(el)` 收注入元素）。
  it("seeds the theme synchronously from a cache so the first paint is not the default", () => {
    localStorage.setItem(
      THEME_CACHE_KEY,
      JSON.stringify({ theme: "ocean", mode: "light", fontSize: 17, accent: "" }),
    );
    const el = document.createElement("html");
    seedFromCache(el);
    expect(el.getAttribute("data-theme")).toBe("ocean");
    expect(el.getAttribute("data-mode")).toBe("light");
    expect(el.style.getPropertyValue("--font-size")).toBe("17px");
  });

  // 计划 Step 3 第二条：坏缓存不许在启动路径上抛。
  it("ignores a corrupt cache instead of throwing during startup", () => {
    localStorage.setItem(THEME_CACHE_KEY, "{not json");
    const el = document.createElement("html");
    expect(() => seedFromCache(el)).not.toThrow();
    expect(el.getAttribute("data-theme"), "坏缓存的处置是什么都不做").toBeNull();
  });

  // 计划 Step 3 第三条。简报 §4.6：原稿在 `.toBeNull()` 后跟逗号再写字符串是语法噪声，
  // 消息必须走 `expect(value, msg)` 第二参。名单校验只有 `resolveTheme` 这一份（R125），
  // 缓存侧对「名单外的名字」的处置是**不种**：写 noir 是替用户做主（R169 的病根），
  // 什么都不写才是「首帧和没有缓存时一模一样」。
  it("refuses to seed an unknown theme name from the cache", () => {
    localStorage.setItem(
      THEME_CACHE_KEY,
      JSON.stringify({ theme: "matrix", mode: "light", fontSize: 15, accent: "" }),
    );
    const el = document.createElement("html");
    seedFromCache(el);
    expect(
      el.getAttribute("data-theme"),
      "缓存里躺着未来版本/手改的主题名时不许把 data-theme 写上去，也不许擅自回落",
    ).toBeNull();
  });

  // 形状校验的另外两支：mode 不可用（applyTheme 会抛，这里吞掉＝不种）、字段类型不对。
  // 「种一半」比不种更糟：data-theme 写了、data-mode 没写，六块预设一块都不匹配。
  it("does not seed half a theme when the cached entry is unusable", () => {
    localStorage.setItem(
      THEME_CACHE_KEY,
      JSON.stringify({ theme: "paper", mode: "sepia", fontSize: 15, accent: "" }),
    );
    const badMode = document.createElement("html");
    expect(() => seedFromCache(badMode)).not.toThrow();
    expect(badMode.getAttribute("data-theme")).toBeNull();

    localStorage.setItem(
      THEME_CACHE_KEY,
      JSON.stringify({ theme: "paper", mode: "dark", fontSize: "15", accent: "" }),
    );
    const badFontSize = document.createElement("html");
    seedFromCache(badFontSize);
    expect(
      badFontSize.getAttribute("data-theme"),
      "fontSize 不是数字 ⇒ 整个条目按坏缓存处置，不许把 \"15\" 拼成 --font-size",
    ).toBeNull();
  });

  // 无缓存 = 静默跳过：这是「缓存缺失时第一帧今天什么样还是什么样」（裁定 2）的正体。
  it("does nothing when no cache exists yet", () => {
    const el = document.createElement("html");
    expect(() => seedFromCache(el)).not.toThrow();
    expect(el.getAttribute("data-theme")).toBeNull();
  });

  // 往返：accent 也要种得回来 —— 「上次挑的强调色」和主题/模式/字号是同一件事。
  it("round-trips the accent through the cache", () => {
    writeThemeCache({ theme: "ocean", mode: "light", fontSize: 16, accent: "#2080ff" });
    const el = document.createElement("html");
    seedFromCache(el);
    expect(el.style.getPropertyValue("--accent")).toBe("#2080ff");
    expect(el.style.getPropertyValue("--accent-hover")).not.toBe("");
    expect(el.style.getPropertyValue("--font-size")).toBe("16px");
  });

  // API key 红线（简报 §6）：缓存的字段集合是**白名单**而不是 `Settings` 的转储。
  // 这条用一个「什么都敢往里塞」的调用方钉死：即使传的对象带 base_url/模型名/key 形状，
  // 落盘的 JSON 里也只准有那四键。夹具值是假的，红线判据是真的。
  it("persists only the whitelisted four fields, never the whole Settings", () => {
    const leaky = {
      theme: "noir",
      mode: "dark" as const,
      fontSize: 15,
      accent: "#aabbcc",
      base_url: "https://internal.example/v1",
      model: "qwen-plus",
      api_key: "sk-THIS-IS-NOT-A-REAL-KEY",
    };
    writeThemeCache(leaky);
    const raw = localStorage.getItem(THEME_CACHE_KEY);
    expect(raw, "writeThemeCache 没落盘？").not.toBeNull();
    expect(Object.keys(JSON.parse(raw as string)).sort()).toEqual([
      "accent",
      "fontSize",
      "mode",
      "theme",
    ]);
    expect(raw).not.toMatch(/sk-|api_key|base_url|example\.invalid|internal\.example|qwen-plus/);
  });

  // 简报 §5：jsdom 不会自己抛配额错，只能把 `Storage.prototype.setItem` 临时换掉。
  // 写失败的失败模式必须是「这次没缓存成，下次首帧走兜底」，不是白屏 —— 吞异常是设计。
  it("survives a storage that refuses to write (quota / private mode)", () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    try {
      expect(() =>
        writeThemeCache({ theme: "noir", mode: "dark", fontSize: 15, accent: "" }),
      ).not.toThrow();
    } finally {
      spy.mockRestore();
    }
  });

  // 读侧同理：某些 WebView 隐私态连 getItem 都能抛，启动路径上不许有任何一条缝能炸。
  it("survives a storage that refuses to read", () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError");
    });
    try {
      expect(() => seedFromCache(document.createElement("html"))).not.toThrow();
    } finally {
      spy.mockRestore();
    }
  });
});
