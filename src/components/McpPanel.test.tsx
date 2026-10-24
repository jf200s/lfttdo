// T35 的组件层判据：状态点、工具数、一键信任、失败原因就地显示（零弹窗）。
// 三条都是**视图**的事，命令层由 `ipc.rs` 的内联测试管（mcp_servers_of / parse_trust /
// mcp_clear_tools），这里一律注入假实现，不碰 IPC。
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { McpPanel } from "./McpPanel";
import type { McpServerView } from "../types";

const ready: McpServerView = {
  name: "search",
  status: "ready",
  tools: 4,
  trust: "ask",
  reason: null,
};

describe("McpPanel", () => {
  it("announces the tool count and offers one-click Auto on first connect", async () => {
    const setTrust = vi.fn().mockResolvedValue(undefined);
    const load = vi.fn().mockResolvedValue([ready]);
    render(
      <McpPanel
        initial={[ready]}
        onClose={() => {}}
        load={load}
        setTrust={setTrust}
      />,
    );

    expect(screen.getByText("search 提供 4 个工具")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "本服务器全部允许" }));
    await waitFor(() => expect(setTrust).toHaveBeenCalledWith("search", "auto"));
  });

  it("shows the failure reason for a disconnected server without a popup", () => {
    const alertSpy = vi.spyOn(window, "alert").mockImplementation(() => {});
    render(
      <McpPanel
        initial={[
          {
            name: "dead",
            status: "disconnected",
            tools: 0,
            trust: "ask",
            reason: "spawn 失败：program not found",
          },
        ]}
        onClose={() => {}}
      />,
    );
    expect(screen.getByText(/program not found/)).toBeInTheDocument();
    expect(alertSpy).not.toHaveBeenCalled();
    alertSpy.mockRestore();
  });

  it("re-reads the server list after a reconnect instead of keeping a stale count", async () => {
    const load = vi
      .fn()
      .mockResolvedValueOnce([{ ...ready, status: "disconnected", tools: 0 }])
      .mockResolvedValueOnce([{ ...ready, status: "ready", tools: 4 }]);
    const reconnect = vi.fn().mockResolvedValue(4);
    // 不传 `initial`：让挂载时的那次拉取落到第一格（disconnected），重连后的刷新落到第二格（ready）。
    render(
      <McpPanel onClose={() => {}} load={load} reconnect={reconnect} />,
    );
    await waitFor(() =>
      expect(screen.getByText("search 提供 0 个工具")).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("button", { name: "重连" }));
    await waitFor(() => expect(reconnect).toHaveBeenCalledWith("search"));
    await waitFor(() =>
      expect(screen.getByText("search 提供 4 个工具")).toBeInTheDocument(),
    );
  });

  it("grants the front end no process or filesystem capability", () => {
    // 走 cwd 而不是 `new URL(..., import.meta.url)`：jsdom 的全局 URL 是 whatwg 实现，
    // `fileURLToPath` 对它会抛「The URL must be of scheme file」。vitest 的 cwd 是仓库根。
    const path = resolve(process.cwd(), "src-tauri/capabilities/default.json");
    const caps = JSON.parse(readFileSync(path, "utf8")) as {
      permissions: string[];
      description: string;
    };
    const flat = JSON.stringify(caps.permissions);
    expect(flat).not.toMatch(/shell:allow-spawn|fs:allow-read|fs:allow-write/);
    expect(flat).toMatch(/core:event/);
    // 说明与权限必须一致（plan :1343）：MCP 的进程执行在 Rust，前端没有 shell。
    expect(caps.description).toMatch(/mcp_\*/);
  });
});
