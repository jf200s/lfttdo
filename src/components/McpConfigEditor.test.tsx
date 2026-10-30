// 编辑器组件的独立判据：载入、模板、保存失败保文本、脏文本二次确认、读失败重试。
// 所有 IPC 都注入假实现，组件本身不碰 Tauri。
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { McpConfigEditor } from "./McpConfigEditor";
import type { McpConfigText } from "../types";

const snapshot = (over: Partial<McpConfigText> = {}): McpConfigText => ({
  path: "C:\\data\\mcp.json",
  exists: true,
  text: '{\n  "mcpServers": {}\n}\n',
  parseError: null,
  skipped: [],
  ...over,
});

describe("McpConfigEditor", () => {
  it("loads the file into a textarea, shows the path and the stdio hint", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByLabelText("mcp.json 内容")).toHaveValue(
      snapshot().text,
    );
    expect(screen.getByText("C:\\data\\mcp.json")).toBeInTheDocument();
    expect(screen.getByText(/仅支持 stdio/)).toBeInTheDocument();
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("prefills the template and says the file will be created", async () => {
    const load = vi
      .fn()
      .mockResolvedValue(snapshot({ exists: false, text: '{\n  "mcpServers": {}\n}\n' }));
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByText(/保存后创建/)).toBeInTheDocument();
  });

  it("shows load-time parse and skipped issues", async () => {
    const load = vi.fn().mockResolvedValue(
      snapshot({
        parseError: "expected value at line 3 column 2",
        skipped: [{ name: "x", reason: "字段类型不对" }],
      }),
    );
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByText(/载入时无法解析/)).toBeInTheDocument();
    expect(screen.getByText(/x（字段类型不对）/)).toBeInTheDocument();
  });

  it("keeps the text and shows the backend error when saving is rejected", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const save = vi.fn().mockRejectedValue("mcp.json 未保存：bad：字段类型不对");
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} save={save} onDone={onDone} />);

    const box = await screen.findByLabelText("mcp.json 内容");
    fireEvent.change(box, { target: { value: "{ nope" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByTestId("mcp-config-error")).toHaveTextContent(
      "字段类型不对",
    );
    expect(box).toHaveValue("{ nope");
    expect(onDone).not.toHaveBeenCalled();
  });

  it("returns saved=true after a successful save", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const save = vi.fn().mockResolvedValue(undefined);
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} save={save} onDone={onDone} />);

    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => expect(onDone).toHaveBeenCalledWith(true));
    expect(save).toHaveBeenCalledWith(snapshot().text);
  });

  it("asks before discarding edits and can keep editing", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} onDone={onDone} />);

    const box = await screen.findByLabelText("mcp.json 内容");
    fireEvent.change(box, { target: { value: "changed" } });
    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    expect(screen.getByText("放弃修改？")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "继续编辑" }));
    expect(screen.getByLabelText("mcp.json 内容")).toHaveValue("changed");

    fireEvent.click(screen.getByRole("button", { name: "返回" }));
    fireEvent.click(screen.getByRole("button", { name: "放弃" }));
    expect(onDone).toHaveBeenCalledWith(false);
  });

  it("routes Escape through the dirty check", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    const box = await screen.findByLabelText("mcp.json 内容");
    fireEvent.change(box, { target: { value: "changed" } });
    fireEvent.keyDown(box, { key: "Escape" });

    expect(screen.getByText("放弃修改？")).toBeInTheDocument();
  });

  it("returns without a prompt when nothing changed", async () => {
    const load = vi.fn().mockResolvedValue(snapshot());
    const onDone = vi.fn();
    render(<McpConfigEditor load={load} onDone={onDone} />);

    await screen.findByLabelText("mcp.json 内容");
    fireEvent.click(screen.getByRole("button", { name: "返回" }));

    expect(onDone).toHaveBeenCalledWith(false);
    expect(screen.queryByText("放弃修改？")).toBeNull();
  });

  it("shows a retry path when the file cannot be read", async () => {
    const load = vi
      .fn()
      .mockRejectedValueOnce("读取 mcp.json 失败：拒绝访问")
      .mockResolvedValueOnce(snapshot());
    render(<McpConfigEditor load={load} onDone={() => {}} />);

    expect(await screen.findByText(/拒绝访问/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(await screen.findByLabelText("mcp.json 内容")).toBeInTheDocument();
  });
});
