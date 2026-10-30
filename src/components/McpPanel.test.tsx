// T35 的组件层判据：状态点、工具数、一键信任、失败原因就地显示（零弹窗）。
// 应用内配置编辑的判据：入口、状态横幅、保存后重启提示、编辑时遮罩不关闭。
// 命令层由 `ipc.rs` 的内联测试管，这里一律注入假实现，不碰 IPC。
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { McpPanel } from "./McpPanel";
import type { McpConfigStatus, McpServerView } from "../types";

const ready: McpServerView = {
  name: "search",
  status: "ready",
  tools: 4,
  trust: "ask",
  reason: null,
};

const cleanStatus = (): McpConfigStatus => ({
  path: "C:\\data\\mcp.json",
  exists: true,
  parseError: null,
  skipped: [],
});

const statusOf = () => vi.fn().mockResolvedValue(cleanStatus());

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
        loadConfigStatus={statusOf()}
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
        loadConfigStatus={statusOf()}
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
    render(
      <McpPanel
        onClose={() => {}}
        load={load}
        reconnect={reconnect}
        loadConfigStatus={statusOf()}
      />,
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
    const path = resolve(process.cwd(), "src-tauri/capabilities/default.json");
    const caps = JSON.parse(readFileSync(path, "utf8")) as {
      permissions: string[];
      description: string;
    };
    const flat = JSON.stringify(caps.permissions);
    expect(flat).not.toMatch(/shell:allow-spawn|fs:allow-read|fs:allow-write/);
    expect(flat).toMatch(/core:event/);
    expect(caps.description).toMatch(/mcp_\*/);
  });

  it("opens the raw config editor and shows the file text", async () => {
    const readConfig = vi.fn().mockResolvedValue({
      ...cleanStatus(),
      text: '{\n  "mcpServers": {}\n}\n',
    });
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        loadConfigStatus={statusOf()}
        readConfig={readConfig}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑配置" }));
    expect(await screen.findByLabelText("mcp.json 内容")).toHaveValue(
      '{\n  "mcpServers": {}\n}\n',
    );
    expect(readConfig).toHaveBeenCalledTimes(1);
  });

  it("shows the restart notice after a successful save and refreshes the list", async () => {
    const load = vi.fn().mockResolvedValue([]);
    const status = statusOf();
    const saveConfig = vi.fn().mockResolvedValue(undefined);
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        load={load}
        loadConfigStatus={status}
        readConfig={vi.fn().mockResolvedValue({ ...cleanStatus(), text: "{}" })}
        saveConfig={saveConfig}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑配置" }));
    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByTestId("mcp-config-saved")).toHaveTextContent(
      "重启应用后生效",
    );
    expect(saveConfig).toHaveBeenCalledWith("{}");
    await waitFor(() => expect(load).toHaveBeenCalled());
    await waitFor(() => expect(status).toHaveBeenCalledTimes(2));
  });

  it("banners a broken config and keeps the editor reachable", async () => {
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        loadConfigStatus={vi.fn().mockResolvedValue({
          ...cleanStatus(),
          parseError: "expected value at line 1 column 2",
        })}
      />,
    );

    expect(await screen.findByTestId("mcp-config-banner")).toHaveTextContent(
      "配置文件无法解析",
    );
    expect(screen.getByText("没有可用的服务器配置。")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "编辑配置" })).toBeInTheDocument();
  });

  it("lists skipped entries with their reasons", async () => {
    render(
      <McpPanel
        initial={[]}
        onClose={() => {}}
        loadConfigStatus={vi.fn().mockResolvedValue({
          ...cleanStatus(),
          skipped: [{ name: "bad", reason: "字段类型不对" }],
        })}
      />,
    );

    expect(await screen.findByTestId("mcp-config-banner")).toHaveTextContent(
      "bad（字段类型不对）",
    );
  });

  it("does not close the panel when the backdrop is clicked while editing", async () => {
    const onClose = vi.fn();
    const { container } = render(
      <McpPanel
        initial={[]}
        onClose={onClose}
        loadConfigStatus={statusOf()}
        readConfig={vi.fn().mockResolvedValue({ ...cleanStatus(), text: "{}" })}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "编辑配置" }));
    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(container.querySelector(".panel-backdrop")!);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("points the empty state at the in-app editor", () => {
    render(
      <McpPanel initial={[]} onClose={() => {}} loadConfigStatus={statusOf()} />,
    );
    expect(
      screen.getByText(/点击「编辑配置」直接写 mcp.json/),
    ).toBeInTheDocument();
  });
});
