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

const pathOf = (relative: string): string =>
  fileURLToPath(new URL(relative, import.meta.url).href);

const srcDir = pathOf("../").replace(/[\\/]+$/, "");
const tokens = readFileSync(pathOf("./tokens.css"), "utf8");
const presets = readFileSync(pathOf("./presets.css"), "utf8");

const HEX = /#[0-9a-fA-F]{3,8}\b/;

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
  it("declares every token in :root", () => {
    for (const name of [...THEME_TOKENS, ...USER_TOKENS]) {
      expect(tokens).toContain(`${name}:`);
    }
  });

  it("every preset overrides every theme token, not a subset", () => {
    for (const theme of THEME_NAMES) {
      for (const mode of ["light", "dark"]) {
        const block = new RegExp(
          `\\[data-theme="${theme}"\\]\\[data-mode="${mode}"\\]\\s*\\{([^}]*)\\}`,
        );
        const m = presets.match(block);
        expect(m, `缺少 ${theme}/${mode} 预设块`).toBeTruthy();
        const body = m![1];
        for (const name of THEME_TOKENS) {
          expect(body, `${theme}/${mode} 缺 ${name}`).toContain(`${name}:`);
        }
        // R124：预设块的特异度高于 :root、又永远打不过 applyTheme 写在 <html> 上的 inline style，
        // 所以块里出现 --font-size 就是一行「读起来像设定、实际永不生效」的死 CSS。
        expect(body, `${theme}/${mode} 不该声明 --font-size`).not.toContain(
          "--font-size:",
        );
      }
    }
  });

  it("writes attributes onto the element rather than swapping stylesheets", () => {
    const set: [string, string][] = [];
    const vars: [string, string][] = [];
    const el = {
      setAttribute: (k: string, v: string) => set.push([k, v]),
      style: { setProperty: (k: string, v: string) => vars.push([k, v]) },
    } as unknown as HTMLElement;
    applyTheme(el, "noir", "dark", 17);
    expect(set).toEqual([
      ["data-theme", "noir"],
      ["data-mode", "dark"],
    ]);
    expect(vars).toEqual([["--font-size", "17px"]]);
  });

  it("rejects unknown theme names instead of silently falling back", () => {
    expect(() =>
      applyTheme(document.documentElement, "bogus", "dark", 15),
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

  // R126 ②：仓库没有 CI，`npm run lint:css` 只有「有人记得跑」才会跑，而 `npm test` 一定跑。
  // 两道闸各保一层：这条守住「硬编码颜色不许进组件样式」，stylelint 守住有人跑时的全量规则。
  it("keeps every hardcoded hex color out of component stylesheets", () => {
    const offenders: string[] = [];
    for (const file of listCss(srcDir)) {
      if (/[\\/]+theme[\\/]/.test(file)) continue;
      const hit = readFileSync(file, "utf8").match(HEX);
      if (hit) offenders.push(`${file} -> ${hit[0]}`);
    }
    expect(offenders, offenders.join("\n")).toEqual([]);
  });
});
