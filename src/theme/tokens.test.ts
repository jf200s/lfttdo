// `node:` 的导入由 `@types/node` 直接提供类型（R165：本仓库已装好它，并把 `"types": ["node"]`
// 写进 tsconfig.json —— 只装包不会自动进 program，实测那条 `node:fs` 仍然报 TS2591）。
// 这里**不许**再压 directive：`TS2578: Unused '@ts-expect-error' directive` 在本仓库是错误不是
// 警告（实测 rc=2），而 `npm run build` 是 `tsc && vite build`，一条多余的 directive 就是一次构建失败。
// 不用 Vite 的 `?raw`：Vitest 默认不处理 CSS，`import x from "./tokens.css?raw"` 在测试里拿到的是
// 空串（2026-10-21 实测 typeof === "string" 且 length === 0），断言会假绿。
// 也不直接把 `new URL(...)` 交给 fs：jsdom 环境里全局 `URL` 是 whatwg 实现，Node 的 fs 认不出它，
// 会抛 `The URL must be of scheme file`，所以先 `.href` 再 `fileURLToPath`。
import { readFileSync, readdirSync } from "node:fs";
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
// R203 NIT5 —— 这条闸**实际覆盖的记号形式**就是正则里那两类：hex 字面量，和
// `rgb()` / `rgba()` / `hsl()` / `hsla()` 函数记号。**CSS 具名色不在覆盖范围内**：
// 实测往 `src/styles.css` 写 `color: red`，`npx vitest run` 和 `npm run lint:css` 双双 rc=0
// （stylelint 那半边只有 `color-no-hex`，它同样不管具名色）。
// 不许为了让这句承诺成立去扩正则覆盖具名色 —— 那得先安置 `background: transparent`
// 这一类合法关键字，否则就是新的假阳，且超出本轮委托面。
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
 * F10 / R169 已经由 T16 执行完毕：`main.tsx` 里那行硬编码的启动期 `applyTheme` 调用
 * 和这个 `BOOT_DEFAULTS` 常量一起删掉了（"不许留成「有名单没消费者」的第二套真相"）。
 * 取而代之的是下面那条「main.tsx 里不许再出现 applyTheme」的守卫 —— 同一件事的反面，
 * 这次的判据是「它不在」而不是「它在且等于 Rust 的默认值」。
 */

function listCss(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const child = `${dir}/${entry.name}`;
    if (entry.isDirectory()) found.push(...listCss(child));
    else if (entry.name.endsWith(".css")) found.push(child);
  }
  return found;
}

/** 剥注释的 TS 版：行注释也要剥，否则注释里提到的 `applyTheme(` 会被当成一次调用（假阳）。 */
function stripCodeComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
}

/**
 * 收 `src/` 下全部**非测试**的 `.ts` / `.tsx`。测试文件豁免的理由与 CSS 闸豁免 `theme/`
 * 同一件事的两面：预设块与令牌名单的断言里满是不许当假阳处理的色值。
 */
function listCode(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const child = `${dir}/${entry.name}`;
    if (entry.isDirectory()) found.push(...listCode(child));
    else if (/\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name))
      found.push(child);
  }
  return found;
}

/** 收全部顶层规则：`[选择器串, 规则体]`。剥注释 —— 注释里的字面量不是证据（评审 M2）。 */
function rules(css: string): [string, string][] {
  return [...stripComments(css).matchAll(/([^{}]+)\{([^}]*)\}/g)].map((m) => [
    m[1],
    m[2],
  ]);
}

/** 预设块 → `token -> 原样值`。只给这条性质检查用，不参与上面那些双向集合断言。 */
function presetTokenValues(body: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of body.matchAll(/(--[a-z][a-z0-9-]*)\s*:\s*([^;]+);/g)) {
    out[m[1]] = m[2].trim();
  }
  return out;
}

/** 支持 `#rgb`/`#rgba`/`#rrggbb`/`#rrggbbaa` 与 `rgb()`/`rgba()`（含现代斜杠语法）。 */
function toRgb(value: string): [number, number, number] {
  const hex = /^#([0-9a-f]{3,8})$/i.exec(value.trim());
  if (hex) {
    const h = hex[1];
    const digits =
      h.length === 3 || h.length === 4
        ? [...h].map((c) => c + c)
        : [...h.match(/.{2}/g)!];
    return [
      parseInt(digits[0], 16),
      parseInt(digits[1], 16),
      parseInt(digits[2], 16),
    ];
  }
  const fn = /rgba?\(([^)]+)\)/i.exec(value.trim());
  if (fn) {
    const parts = fn[1].trim().split(/[\s,/]+/).map(Number);
    return [parts[0], parts[1], parts[2]];
  }
  throw new Error(`这条性质检查认不出的颜色写法：${value}`);
}

/** WCAG 相对亮度（和对比度用的是同一个 L）。 */
function relativeLuminance(value: string): number {
  const [r, g, b] = toRgb(value);
  const lin = (c: number): number => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/**
 * WCAG 对比度：亮度高的一支进分子，所以参数顺序无关（R201 用它守 hover 态的可辨识度）。
 * 只吃 `relativeLuminance` 认得出的写法 —— 预设块里那四支色值全是 `#rrggbb`。
 */
function contrast(first: string, second: string): number {
  const l1 = relativeLuminance(first);
  const l2 = relativeLuminance(second);
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
}

/** 组件样式里 `button` 静止态与 `:active` 各取哪个 token（按层叠顺序，后写的算）。 */
function buttonBackgrounds(css: string): { rest: string; active: string } {
  let rest = "";
  let active = "";
  for (const [selector, body] of rules(css)) {
    const bg = /background-color:\s*var\((--[a-z-]+)\)/.exec(body);
    if (!bg) continue;
    for (const s of selector.split(",").map((x) => x.trim())) {
      if (/^button:active\b/.test(s)) active = bg[1];
      else if (/^button$/.test(s)) rest = bg[1];
    }
  }
  return { rest, active };
}


describe("theme tokens", () => {
  // F2 / M3：完整性测试只问「CSS ⊇ 名单」，从不问「名单 ⊇ 实际声明」，于是从
  // `THEME_TOKENS` 里摘走一项（本任务自己就在 R124 下摘走过一项）会让漏项闸当场少守一个
  // token 而全绿。这三条把名单自己钉住：数量、内容、两份不相交。
  it("keeps the token lists themselves complete", () => {
    // T50 质感轮新增两项：`--radius-sm`（圆角只有一档时，72px 缩略图与整块面板共用同一个
    // 10px，是「没调过」最显眼的痕迹）与 `--font-code`（全仓当时没有任何等宽声明，
    // 工具调用 JSON 与 markdown 代码块都排在界面字体上，paper 下等于用 Georgia 排代码）。
    expect(THEME_TOKENS.length).toBe(17);
    expect(THEME_TOKENS).toContain("--radius-sm");
    expect(THEME_TOKENS).toContain("--font-code");
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
    // R199（评审 MIN2）：这块的集合相等是**裸 set diff**，而它守着的正则只认属性选择器的规范形
    // `[data-theme="x"][data-mode="y"]`（R171 刻意收紧，等价序 `[data-mode="y"][data-theme="x"]`
    // 在 CSS 里合法、渲染也一样，但会整块掉出集合）。掉出集合时红的是**拼写**，不是层叠也不是漏项 ——
    // 那条身份必须写进消息，否则改的人会去动名单或去放宽正则（两条都不许）。
    const expectedKeys = THEME_NAMES.flatMap((t) => [`${t}/dark`, `${t}/light`]).sort();
    const foundKeys = [...bodies.keys()].sort();
    expect(
      foundKeys,
      `预设块读到的键集合与 THEME_NAMES × {dark, light} 不等价：读到 ${JSON.stringify(foundKeys)}，` +
        `期望 ${JSON.stringify(expectedKeys)}。少的那一块若**在文件里看得见**，先查它的属性顺序 —— ` +
        `本用例只认规范形 [data-theme="…"][data-mode="…"]（R171 刻意收紧），` +
        `写成等价序 [data-mode="…"][data-theme="…"] 是合法 CSS、计算值一模一样，但会整块掉出集合。` +
        `红的是拼写不是漏项：把选择器改回规范形即可，**不许**为此放宽正则、也不许把等价序加进期望集合。`,
    ).toEqual(expectedKeys);

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
    expect(
      applyTheme.length,
      "applyTheme 的形参数量不再是 4 —— R122 明令删掉的第 5 个 `accent` 参数被加回来了" +
        "（哪怕声明成可选，arity 也会先红在这里）",
    ).toBe(4);

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

  // F9 / R120：层叠顺序零守卫。评审 M15 对调 `main.tsx` 的两行主题 import，实测 6 passed，
  // 而产物里顺序**真的翻了**（presets idx=0、:root idx=1973）。
  // 诚实附注：这两行互换今天不改变渲染（预设块特异度 0,2,0 恒高于 :root 的 0,1,0；唯一一对同特异度
  // 的 :root 规则声明的属性集合不相交），所以它是防御性约定。真正的风险是组件样式在 :root 上
  // 声明自定义属性，顺序立刻变承重 —— 下面那个 for-all 子句就是给那时准备的雷线，
  // 故意不硬编码第三个文件名（T16 新建的是 src/styles.css，将来还会再多）。
  // R197（评审 MAJ1）：for-all 循环只看得见 `main.tsx` 里**字面的 `.css` import**，看不见真正的
  // 层叠路径 —— 把 `import App from "./App"` 提到两份主题样式表之前，组件模块自己（或它将来任何
  // 一行）带来的样式就会排在主题声明之前，而实测该变异 rc=0 存活（评审 G1）。
  // 所以顺序现在钉在**模块导入**这一层，不再只钉在文件名这一层。
  it("imports tokens before presets before every other stylesheet", () => {
    const source = readFileSync(pathOf("../main.tsx"), "utf8");
    // 坐标系统是 main.tsx 里**全部** import 语句的出现顺序 —— 层叠由它决定，不是由 `.css` 子集决定。
    const importLines = [...source.matchAll(/^[ \t]*import[^\n]*/gm)].map((m) => m[0]);
    const at = (needle: string): number =>
      importLines.findIndex((line) => line.includes(needle));
    const tokensAt = at("theme/tokens.css");
    const presetsAt = at("theme/presets.css");
    const appAt = at('import App from "./App"');
    expect(tokensAt, "main.tsx 不再导入 theme/tokens.css").toBeGreaterThanOrEqual(0);
    expect(presetsAt, "main.tsx 不再导入 theme/presets.css").toBeGreaterThanOrEqual(0);
    expect(
      appAt,
      'main.tsx 不再以 `import App from "./App"` 的形式导入 App —— 层叠顺序闸丢掉了它的参照点（R197）',
    ).toBeGreaterThanOrEqual(0);
    expect(tokensAt).toBeLessThan(presetsAt);
    expect(
      presetsAt,
      `App 模块排在第 ${appAt} 位、theme/presets.css 排在第 ${presetsAt} 位：` +
        "组件模块的导入排在了 theme/presets.css 之前（R197）。这份文件里看不到它带进来的样式，" +
        "所以 for-all 那个循环根本看不见这条层叠路径 —— 它正是评审 G1 从两道闸中间穿过去的洞。",
    ).toBeLessThan(appAt);
    for (let i = 0; i < importLines.length; i += 1) {
      // 只判字面的 `.css` 导入；`theme/` 那两份是参照点本身。
      if (!/["'][^"']*\.css["']/.test(importLines[i]) || i === tokensAt || i === presetsAt) {
        continue;
      }
      expect(
        presetsAt,
        `组件样式 ${importLines[i]} 排在了 theme/presets.css 之前`,
      ).toBeLessThan(i);
    }
  });

  // R169 的另一半：硬编码那一行**必须不在**。注释里提到 applyTheme 不算调用，所以先剥注释。
  it("leaves settings as the only source of the applied theme", () => {
    const code = stripCodeComments(readFileSync(pathOf("../main.tsx"), "utf8"));
    expect(code, "main.tsx 又出现了启动期硬编码的 applyTheme 调用").not.toMatch(
      /applyTheme\s*\(/,
    );
    expect(code).not.toMatch(/resolveTheme\s*\(/);
    // 但第一帧仍然要有兜底字号：设置回来之前不许退回浏览器的 16px。
    // 这条不再钉死 15 —— 钉死数字就是再造一份默认值（R169 的根因）。
    expect(stripComments(tokens)).toMatch(/--font-size:\s*\d+px;/);
    // T15 评审 F11：`--font-size` 必须真的有人消费。`applyTheme` 把它写成 `<html>` 的
    // inline 变量，只有组件样式里出现 `font-size: var(--font-size)` 它才会变成字号；
    // 少了这一行，`settings.rs` 那套 12..=24 的钳位（R123）就是装饰。
    expect(
      stripComments(readFileSync(pathOf("../styles.css"), "utf8")),
      "styles.css 不再消费 --font-size（字号设置失效）",
    ).toMatch(/font-size:\s*var\(--font-size\)/);
  });

  // R133 / 评审 F9 的那道层叠纪律：**组件样式里不许声明任何 `--*`**。
  // 一旦声明，它和 tokens.css 的 `:root` 就是同特异度的两份真相，而唯一守着先后的是
  // import 顺序 —— 这条把「先后」这个变量本身消掉了。
  it("keeps every custom-property declaration out of component stylesheets", () => {
    const offenders: string[] = [];
    for (const file of listCss(srcDir)) {
      if (/[\\/]+theme[\\/]/.test(file)) continue;
      const body = stripComments(readFileSync(file, "utf8"));
      for (const m of body.matchAll(/(?:^|[{;])\s*(--[a-z][a-z0-9-]*)\s*:/g)) {
        offenders.push(`${file} -> ${m[1]}`);
      }
    }
    expect(offenders, offenders.join("\n")).toEqual([]);
  });

  // T50 质感轮新增的两项必须有人读它。R201 的教训是「token 被六块声明得整整齐齐，
  // 但没有一条闸问过它值对不对得起名字」——加进 `THEME_TOKENS` 只是付了六块 × 一行的税，
  // 判据在消费侧。这里不套对比度那种性质断言（圆角和字体族没有可比的大小关系），
  // 只闸最便宜的一条：组件样式里出现 `var(--…)`。
  // 变异：删掉 styles.css 里全部 `font-family: var(--font-code)` ⇒ 本用例红，
  //       而六块覆盖、名单长度、双向集合那几道闸一道都不响。
  it("keeps the new texture tokens consumed by the component stylesheet", () => {
    const css = stripComments(readFileSync(pathOf("../styles.css"), "utf8"));
    for (const token of ["--radius-sm", "--font-code"]) {
      expect(
        css,
        `styles.css 不再消费 ${token} —— 预设里那六行等于装饰：` +
          `${token} 进了名单却没有任何样式读它（R201 的「有名单没消费者」）`,
      ).toMatch(new RegExp(`var\\(${token}\\)`));
    }
    // 等宽不是「代码块排得好看」：改之前全仓没有任何等宽声明，工具卡的 JSON（`.io`）
    // 和 markdown 代码块都排在界面字体上，paper 下等于用 Georgia 排代码。
    expect(css, "markdown 代码语境必须走 --font-code，等宽名单只有预设里那一份").toMatch(
      /\.md pre,\s*\.md code\s*\{[^}]*font-family:\s*var\(--font-code\)/,
    );
  });

  // R181：按下态必须比静止态**暗**，六块全过。
  // 为什么做成性质检查而不是钉死某个值：`--bg-input` 的明暗方向本身随 mode 翻转
  // （三个 dark 块更亮、三个 light 块更暗），所以「这里用 --bg-input」在 light 三块看着对、
  // 在 dark 三块是反的 —— 那支 token 的语义是「输入区底色」，不是「按下凹陷」。
  // 变异：把 styles.css 的 `button:active` 改回 `--bg-input`，dark 那三块当场红。
  it("keeps the pressed state darker than the resting state in all six presets", () => {
    const css = readFileSync(pathOf("../styles.css"), "utf8");
    const { rest, active } = buttonBackgrounds(css);
    expect(active, "button:active 的底色没取 --bg-surface").toBe("--bg-surface");
    expect(rest, "button 静止态的底色没取 --bg-raised").toBe("--bg-raised");

    const blocks = [
      ...stripComments(presets).matchAll(
        /^\[data-theme="([a-z-]+)"\]\[data-mode="(light|dark)"\]\s*\{([^}]*)\}/gm,
      ),
    ];
    expect(blocks).toHaveLength(6);
    for (const m of blocks) {
      const key = `${m[1]}/${m[2]}`;
      const values = presetTokenValues(m[3]);
      expect(
        relativeLuminance(values[active]),
        `${key}：按下态(${active})不比静止态(${rest})暗，按下去看不出按下了`,
      ).toBeLessThan(relativeLuminance(values[rest]));
    }
  });

  // R201（评审 W1/W2）：token 被六块声明得整整齐齐，但没有一条闸问过它「值对不对得起它的名字」——
  // 双向集合闸只比**声明名**，所以把六块的 --accent-hover 全抄成同块的 --accent，集合断言、块数断言、
  // 上面那条按下去的亮度比较一道都不响（评审实测 W1–W5/P12/P13 全部存活）。
  // 本轮只做最便宜的一条消费判据：--accent-hover 对**同块**两支背景的 WCAG 对比度必须
  // **严格高于**同块 --accent 对同一支背景的对比度。评审已独立算过六块六个方向全部成立
  // ⇒ 今天不会假阳；用严格大于而不是「不低于」，正是为了「hover 被抄成 accent 同值」这种走样必须红。
  // W3/W4/W5（--shadow / --code-bg / --border 的消费）不在本轮委托面里。
  it("keeps --accent-hover more readable than --accent on both surfaces in all six presets", () => {
    const blocks = [
      ...stripComments(presets).matchAll(
        /^\[data-theme="([a-z-]+)"\]\[data-mode="(light|dark)"\]\s*\{([^}]*)\}/gm,
      ),
    ];
    expect(
      blocks,
      "预设块不是 6 块 —— 这条对比度判据的覆盖面会跟着少一块（R201）",
    ).toHaveLength(6);
    for (const m of blocks) {
      const key = `${m[1]}/${m[2]}`;
      const values = presetTokenValues(m[3]);
      for (const bg of ["--bg-surface", "--bg-raised"]) {
        const rest = contrast(values["--accent"], values[bg]);
        const hover = contrast(values["--accent-hover"], values[bg]);
        expect(
          hover,
          `${key}：--accent-hover 对 ${bg} 的对比度 ${hover.toFixed(3)} 不严格高于 ` +
            `--accent 的 ${rest.toFixed(3)} —— 悬停态和普通态在可读性上没有区别，` +
            `presets.css 顶部 R168 那句「hover 必须比同块的 --accent 更醒目」就是没有消费者的口头承诺（R201）`,
        ).toBeGreaterThan(rest);
      }
    }
  });


  // R126 ②：仓库没有 CI，`npm run lint:css` 只有「有人记得跑」才会跑，而 `npm test` 一定跑。
  // 两道闸各保一层：这条守住「hex 与函数记号形式的硬编码颜色不许进组件样式」（记号范围见
  // `HARDCODED_COLOR` 上面的 R203 NIT5 附注 —— 具名色两处都不守），stylelint 守住有人跑时的全量规则。
  // R200（评审 MIN3）：**豁免面两处不同源，各自独立维护，别再宣称「同一份」**。实测新建
  // `src/theme/legacy.css` 塞 `#ff0000` ⇒ stylelint rc=2 而本用例 rc=0。因为：
  //   · 本用例按**目录**豁免 —— `listCss` 收全 `src/**` 的 .css，命中 `theme/` 路径就整份跳过，
  //     将来任何新建于 src/theme/ 下的样式表都会自动被豁免掉；
  //   · `lint:css` 按**文件**豁免 —— `package.json` 的否定 glob 只点名 tokens.css 与 presets.css
  //     两个具名文件，theme/ 下的第三个文件照扫。
  // R170 明令不许新增 stylelint 规则，`package.json` 也不在本轮委托面里，所以这里只把注释改成实际情形。
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

  // 评审 #29 标准轴（前端与配置）：色值闸过去只扫 `.css`，于是 TSX 是唯一没人看的入口。
  // 组件里写 `style={{ color: "#f00" }}` 时三道闸全绿 —— vitest 的 CSS 闸不收 .tsx，
  // stylelint 的 `color-no-hex` 只管 CSS 文件，而 `MessageList.test.tsx` 那条按构造不可能红
  // （jsdom 从不把类规则内联进 DOM；React 的内联样式经 CSSOM 回来是 `rgb(255, 0, 0)`，
  // 连 `#` 那一支都抓不到）。主题纪律承诺的是「颜色只能来自 `var(--token)`」，
  // 那么判据就该落在**源码**上，不管它最终怎么渲染。
  // 覆盖面与 CSS 闸同一档：hex 字面量 + `rgb/rgba/hsl/hsla` 函数记号；具名色两处都不守
  // （R203 NIT5 的附注仍然成立）。先剥注释再扫，同一个理由：注释里的色值不参与渲染。
  it("keeps every hardcoded color out of component source too", () => {
    const offenders: string[] = [];
    for (const file of listCode(srcDir)) {
      const hits = stripCodeComments(readFileSync(file, "utf8")).match(
        HARDCODED_COLOR,
      );
      if (hits) offenders.push(`${file} -> ${[...new Set(hits)].join(", ")}`);
    }
    expect(offenders, offenders.join("\n")).toEqual([]);
  });
});
