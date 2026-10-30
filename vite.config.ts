/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [react()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    // R134：只允许为这一处改本文件 —— jest-dom 的匹配器要靠它注册，
    // 否则 `toBeInTheDocument` 一类断言全部报「invalid matcher」。
    setupFiles: "./src/setupTests.ts",
    // `github-ouput/` 是本地发布产物（gitignored），里面有一份完整的 src 副本。
    // 不排除的话本地 vitest 会把同一批测试跑两遍：读数与 CI 对不上，旧副本还会
    // 掩盖真实失败。显式列全默认排除项，因为一旦写了 `exclude` 就替换掉默认值。
    exclude: [
      "**/node_modules/**",
      "**/.git/**",
      "**/dist/**",
      "**/github-ouput/**",
      "**/src-tauri/**",
    ],
  },
}));
