// 本仓库没有 @types/node，而 `npm run build` 是 `tsc && vite build`，所以 `node:` 的导入在
// 这份 tsconfig 下是编译期错误（同 R121 对 `vi` 的判据）；照 `vite.config.ts:4` 的既有先例，
// 每一行各自压一条 @ts-expect-error，运行时由 Node 提供。
// 不用 Vite 的 `?raw`：Vitest 默认不处理 CSS，`import x from "./tokens.css?raw"` 在测试里拿到的是
// 空串（2026-10-21 实测 typeof === "string" 且 length === 0），断言会假绿。
// 也不直接把 `new URL(...)` 交给 fs：jsdom 环境里全局 `URL` 是 whatwg 实现，Node 的 fs 认不出它，
// 会抛 `The URL must be of scheme file`，所以先 `.href` 再 `fileURLToPath`。
// @ts-expect-error type error without @types/node package
import { readFileSync, readdirSync } from "node:fs";
// @ts-expect-error type error without @types/node package
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  THEME_NAMES,
  THEME_TOKENS,
  USER_TOKENS,
  applyTheme,
  resolveTheme,
} from "./theme";
import type { Mode } from "./theme";

const pathOf = (relative: string): string =>
  fileURLToPath(new URL(relative, import.meta.url).href);

const srcDir = pathOf("../").replace(/[\\/]+$/, "");
const tokens = readFileSync(pathOf("./tokens.css"), "utf8");
const presets = readFileSync(pathOf("./presets.css"), "utf8");

// R126 ② + R170：组件样式里「硬编码颜色」要归零，而不只是「硬编码 hex」归零。
// 旧形态只扫 `#rrggbb`，于是 `box-shadow: 0 2px 2px rgb(0 0 0 / 20%)` 从两道闸中间穿过去，
// 而 minifier 还会把它写回成 `#0003` 塞进产物 —— 将来谁拿 hex 扫产物验收会得到一条无根的红。
// 带 `g`：测试名承诺「every」，就该把一个文件里的全部走样都报出来，而不是看见第一颗就收工。
const HARDCODED_COLOR =
  /#[0-9a-fA-F]{3,8}\b|\b(?:rgb|rgba|hsl|hsla)\s*\(/g;

/** 剥掉块注释：注释不参与任何渲染，绝不能拿它充当「这条规则存在」的证据（评审 M2 的入口）。 */
const stripComments = (css: string): string =>
  css.replace(/\/\*[\s\S]*?\*\//g, "");

/**
 * 取一条规则 body 里全部自定义属性**声明名**。保留重复项，所以「同一块里写两遍同一个 token」
 * 也会让下面的双向比较变红 —— 用 Set 去重就是把这一类漏掉了。
 */
function declaredCustomProperties(body: string): string[] {
  return [...body.matchAll(/(--[a-z][a-z0-9-]*)\s*:/g)].map((m) => m[1]);
}

/** applyTheme 的自证桩：把两次写入分别收进数组，`toEqual` 钉的是**顺序与内容**（R121）。 */
function makeSpyElement(): {
  el: HTMLElement;
  attrs: [string, string][];
  vars: [string, string][];
} {
  const attrs: [string, string][] = [];
  const vars: [string, string][] = [];
  const el = {
    setAttribute: (k: string, v: string): void => {
      attrs.push([k, v]);
    },
    style: {
      setProperty: (k: string, v: string): void => {
        vars.push([k, v]);
      },
    },
  } as unknown as HTMLElement;
  return { el, attrs, vars };
}

/**
 * F10 / R169：这是 `Settings::default()`（src-tauri/src/settings.rs:265-267 的
 * `theme: "noir"` / `mode: Mode::Dark` / `font_size: 15`）的**第三份拷贝**，
 * `tokens.css` 的 `--font-size: 15px` 是第四份。四处今天逐字相等 —— 这正是它危险的原因：
 * 任何一处单独改动都静默成立，而 cargo test / vitest / tsc 全绿。
 * ⚠️ T16 用真实 `settings_get` 驱动 `applyTheme` 之后，`main.tsx:12` 那一行必须消失，
 * 届时请连这条测试一起删（R169 把它写成 T16 的验收项，不是长期资产）。
 */
const BOOT_DEFAULTS = { theme: "noir", mode: "dark", fontSize: 15 } as const;

function listCss(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const child = `${dir}/${entry.name}`;
    if (entry.isDirectory()) found.push(...listCss(child));
    else if (entry.name.endsWith(".css")) found.push(child);
  }
  return found;
}

describe("theme tokens", () => {
  // F2 / M3：完整性测试只问「CSS ⊇ 名单」，从不问「名单 ⊇ 实际声明」，于是从
  // `THEME_TOKENS` 里摘走一项（本任务自己就在 R124 下摘走过一项）会让漏项闸当场少守一个
  // token 而全绿。这三条把名单自己钉住：数量、内容、两份不相交。
  it("keeps the token lists themselves complete", () => {
    expect(THEME_TOKENS.length).toBe(15);
    expect(USER_TOKENS).toEqual(["--font-size"]);
    expect(THEME_TOKENS.filter((t) => USER_TOKENS.includes(t))).toEqual([]);
    expect(new Set(THEME_TOKENS).size).toBe(THEME_TOKENS.length);
    // F16：spec 443 明写「预设主题 3 套」，块数期望值就是从这个数字推出来的，所以它自己也得有牙。
    expect(THEME_NAMES.length * 2).toBe(6);
  });

  // F2 + F5：双向集合相等，且**只在 :root 这一条规则里取**。
  // 评审 M4 把 `--code-bg` 从 `:root` 删掉、改在 `:root` 之外的 `.app-legacy {}` 里声明，
  // 旧断言 `expect(tokens).toContain(...)` 打的是全文 → 6 passed，而 :root 已经不再兜底那个 token。
  it("declares every token in :root and nothing besides", () => {
    const rootBlocks = [...stripComments(tokens).matchAll(/^:root\s*\{([^}]*)\}/gm)];
    expect(rootBlocks, "tokens.css 里必须恰好一条行首锚定的 :root 规则").toHaveLength(1);
    const declared = declaredCustomProperties(rootBlocks[0][1]);
    // 用带重复的 declared 比较（不是 Set），所以「少一项 / 多一项 / 重复声明」三种走样都会红。
    expect(declared.slice().sort()).toEqual([...THEME_TOKENS, ...USER_TOKENS].sort());
  });

  // F1 + F16：先用剥注释 + 行首锚定的 matchAll 收**全部**块，再断言块集合**恰好**等于
  // THEME_NAMES × {dark, light} 那 6 对 —— 少一块红、多一块（名单外的死 CSS）红、重复定义红。
  // 旧写法 `presets.match(非全局不锚行首的正则)` 只看「这串字符第一次出现在哪」，
  // 所以评审 M2 在文件顶部插一段注释诱饵、同时从真实块里删掉 `--danger`，6 passed。
  it("every preset overrides every theme token, not a subset", () => {
    const bodies = new Map<string, string>();
    for (const m of stripComments(presets).matchAll(
      /^\[data-theme="([a-z-]+)"\]\[data-mode="(light|dark)"\]\s*\{([^}]*)\}/gm,
    )) {
      const key = `${m[1]}/${m[2]}`;
      expect(bodies.has(key), `预设块 ${key} 被重复定义`).toBe(false);
      bodies.set(key, m[3]);
    }
    expect([...bodies.keys()].sort()).toEqual(
      THEME_NAMES.flatMap((t) => [`${t}/dark`, `${t}/light`]).sort(),
    );

    for (const theme of THEME_NAMES) {
      for (const mode of ["dark", "light"]) {
        const key = `${theme}/${mode}`;
        const body = bodies.get(key)!;
        const declared = declaredCustomProperties(body);
        for (const name of THEME_TOKENS) {
          expect(declared, `${key} 缺 ${name}`).toContain(name);
        }
        // R124：预设块的特异度高于 :root、又永远打不过 applyTheme 写在 <html> 上的 inline style，
        // 所以块里出现 --font-size 就是一行「读起来像设定、实际永不生效」的死 CSS。
        // 放在双向比较**之前**：两条都能拦住它，但只有这条会喊出「不该声明 --font-size」。
        expect(declared, `${key} 不该声明 --font-size`).not.toContain(
          "--font-size",
        );
        // 双向：块里也不许冒出名单外的死声明（和 F16 拦死块是同一条纪律的 token 版）。
        expect(declared.slice().sort()).toEqual([...THEME_TOKENS].sort());
      }
    }
  });

  it("writes attributes onto the element rather than swapping stylesheets", () => {
    const probe = makeSpyElement();
    applyTheme(probe.el, "noir", "dark", 17);
    expect(probe.attrs).toEqual([
      ["data-theme", "noir"],
      ["data-mode", "dark"],
    ]);
    expect(probe.vars).toEqual([["--font-size", "17px"]]);
  });

  // F3 / R122：`--accent` 只由 presets.css 拥有。评审 M16 把 R122 明令删掉的
  // `accent?: string` 参数与 `if (accent) el.style.setProperty("--accent", accent)` 原样加回
  // `theme.ts`，实测 vitest / tsc / lint:css 三道闸一道都不响 —— R122 是 R120-R127 里
  // 唯一「完全靠它自己不在」的裁定。下面两条是新绊线：
  //   ① arity：applyTheme 今天声明 4 个形参，加回第 5 个（哪怕可选）就变 5；
  //   ② 效果：多传一个颜色串，只要它真被写进 CSS 自定义属性，vars 就多一项。
  // 评审 §4.10 自己声明过修复片段没实跑，这里实跑了一遍：它给的
  // `applyTheme(el, "noir", "dark", 17, "#ff0000" as never)` 实测是
  // `error TS2554: Expected 4 arguments, but got 5`（`as never` 只放宽类型、不管实参个数），
  // 而本任务的判据要求 `npx tsc --noEmit` rc=0，所以改成通过放宽签名的引用传这个多余实参。
  it("has no hook for writing --accent inline", () => {
    expect(applyTheme.length).toBe(4);

    const probe = makeSpyElement();
    const callWithExtraAccent = applyTheme as unknown as (
      ...args: unknown[]
    ) => void;
    callWithExtraAccent(probe.el, "noir", "dark", 17, "#ff0000");
    expect(
      probe.vars,
      "applyTheme 把多余实参写进了 CSS 自定义属性（R122：--accent 不许走 inline）",
    ).toEqual([["--font-size", "17px"]]);
  });

  it("rejects unknown theme names instead of silently falling back", () => {
    expect(() =>
      applyTheme(document.documentElement, "bogus", "dark", 15),
    ).toThrow();
  });

  // F6 / R167：控制器裁定「留着并补测」，不是删。`theme.ts:41-43`（未知 mode 抛错那一支）今天不可达
  // （Mode 是 serde 校验过的 2 变体，settings.rs:233-236），但 T16 要把 settings_get 的
  // 动态返回值 `as Mode` 强转喂进来，那道分支从此是 JS 侧唯一的边界防线。
  // 评审 M14 实测：整支删掉后 6 passed + tsc rc=0（零覆盖）。
  it("rejects an unknown mode instead of writing an unusable attribute", () => {
    expect(() =>
      applyTheme(document.documentElement, "noir", "sepia" as Mode, 15),
    ).toThrow();
  });

  it("falls back with a visible flag instead of throwing for stored settings", () => {
    expect(resolveTheme("paper")).toEqual({ theme: "paper", fellBack: false });
    expect(resolveTheme("ocean")).toEqual({ theme: "ocean", fellBack: false });
    expect(resolveTheme("holographic-2099")).toEqual({
      theme: "noir",
      fellBack: true,
    });
    expect(resolveTheme("")).toEqual({ theme: "noir", fellBack: true });
  });

  // F9 / R120：层叠顺序零守卫。评审 M15 对调 `main.tsx:3/:4` 两行 import，实测 6 passed，
  // 而产物里顺序**真的翻了**（presets idx=0、:root idx=1973）。
  // 诚实附注：这条对调今天不改变渲染（预设块特异度 0,2,0 恒高于 :root 的 0,1,0；唯一一对同特异度
  // 的 :root 规则声明的属性集合不相交），所以它是防御性约定。真正的风险是 T16/T19 一旦有组件样式
  // 在 :root 上声明自定义属性，顺序立刻变承重 —— 下面那个 for-all 子句就是给那时准备的雷线，
  // 故意不硬编码第三个文件名（T16 会新建 src/styles.css）。
  it("imports tokens before presets before every other stylesheet", () => {
    const source = readFileSync(pathOf("../main.tsx"), "utf8");
    const cssImports = [
      ...source.matchAll(/^\s*import\s+["']([^"']+\.css)["']/gm),
    ].map((m) => m[1]);
    const at = (suffix: string): number =>
      cssImports.findIndex((p) => p.endsWith(suffix));
    const tokensAt = at("theme/tokens.css");
    const presetsAt = at("theme/presets.css");
    expect(tokensAt, "main.tsx 不再导入 theme/tokens.css").toBeGreaterThanOrEqual(0);
    expect(presetsAt, "main.tsx 不再导入 theme/presets.css").toBeGreaterThanOrEqual(0);
    expect(tokensAt).toBeLessThan(presetsAt);
    for (let i = 0; i < cssImports.length; i += 1) {
      if (i === tokensAt || i === presetsAt) continue;
      expect(
        presetsAt,
        `组件样式 ${cssImports[i]} 排在了 theme/presets.css 之前`,
      ).toBeLessThan(i);
    }
  });

  it("pins the boot-time applyTheme call until T16 replaces it", () => {
    const source = readFileSync(pathOf("../main.tsx"), "utf8");
    const call = source.match(
      /applyTheme\(\s*document\.documentElement\s*,\s*"([a-z-]+)"\s*,\s*"([a-z-]+)"\s*,\s*(\d+)\s*\)/,
    );
    // 匹配不到就是 T16 已经把这行换成真实设置驱动了 —— 那是好事，请连本条测试一起删（R169）。
    expect(
      call,
      "main.tsx 里找不到启动期 applyTheme(document.documentElement, ...) 硬编码调用",
    ).toBeTruthy();
    expect([call![1], call![2], Number(call![3])]).toEqual([
      BOOT_DEFAULTS.theme,
      BOOT_DEFAULTS.mode,
      BOOT_DEFAULTS.fontSize,
    ]);
    // 第四份拷贝也在：:root 的兜底字号必须和启动值一致，否则第一帧和首帧后不同字号。
    expect(tokens).toContain(`--font-size: ${BOOT_DEFAULTS.fontSize}px;`);
  });

  // R126 ②：仓库没有 CI，`npm run lint:css` 只有「有人记得跑」才会跑，而 `npm test` 一定跑。
  // 两道闸各保一层：这条守住「硬编码颜色不许进组件样式」，stylelint 守住有人跑时的全量规则。
  // 豁免面就是 src/theme/ 那两个具名文件（它们**是**色值表），与 package.json:11 的否定 glob
  // recipe 是同一份豁免，两边都要能审。
  // 和上面收预设块时同一个纪律、只是方向相反：**先剥注释再扫**。注释里的色值不参与渲染，
  // 拿它充当「这里有硬编码颜色」的证据就是假阳（本轮实跑就被自己写的注释绊了一次），
  // 而拿它充当「这里没有硬编码颜色」的证据就是 M2 那一类假阴。规则体才是判据。
  it("keeps every hardcoded color out of component stylesheets", () => {
    const offenders: string[] = [];
    for (const file of listCss(srcDir)) {
      if (/[\\/]+theme[\\/]/.test(file)) continue;
      const hits = stripComments(readFileSync(file, "utf8")).match(
        HARDCODED_COLOR,
      );
      if (hits) offenders.push(`${file} -> ${[...new Set(hits)].join(", ")}`);
    }
    expect(offenders, offenders.join("\n")).toEqual([]);
  });
});
