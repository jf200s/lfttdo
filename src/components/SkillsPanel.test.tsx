import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SkillsPanel } from "./SkillsPanel";
import type { SkillsView } from "../types";

const enabledView: SkillsView = {
  root: "E:\\glmai\\aidox\\skills",
  skills: [
    {
      name: "翻译润色",
      description: "中英互译",
      folder: "translate",
      enabled: true,
    },
  ],
};

const disabledView: SkillsView = {
  ...enabledView,
  skills: [{ ...enabledView.skills[0], enabled: false }],
};

describe("SkillsPanel", () => {
  it("lists the skills with a per-skill switch and the directory", () => {
    render(<SkillsPanel initial={enabledView} onClose={() => {}} />);
    expect(screen.getByText("翻译润色")).toBeInTheDocument();
    expect(screen.getByText("中英互译")).toBeInTheDocument();
    expect(screen.getByText("文件夹：translate")).toBeInTheDocument();
    expect(screen.getByText(/技能可以逐个启用/)).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "启用 翻译润色" })).toBeChecked();
    expect(screen.getByText(/^技能目录：/)).toHaveTextContent(
      "E:\\glmai\\aidox\\skills",
    );
  });

  it("shows the directory and how to add one when empty", () => {
    render(
      <SkillsPanel initial={{ root: "E:\\skills", skills: [] }} onClose={() => {}} />,
    );
    expect(screen.getByText(/^技能目录：/)).toHaveTextContent("E:\\skills");
    expect(screen.getByText(/还没有技能/)).toBeInTheDocument();
    expect(screen.queryByText(/文件夹：/)).toBeNull();
    expect(screen.queryByRole("checkbox")).toBeNull();
  });

  it("toggles a skill off through the injected command and reloads", async () => {
    const setEnabled = vi.fn().mockResolvedValue(undefined);
    const load = vi.fn().mockResolvedValue(disabledView);
    render(
      <SkillsPanel
        initial={enabledView}
        onClose={() => {}}
        setEnabled={setEnabled}
        load={load}
      />,
    );
    fireEvent.click(screen.getByRole("checkbox", { name: "启用 翻译润色" }));
    await waitFor(() =>
      expect(setEnabled).toHaveBeenCalledWith("translate", false),
    );
    await waitFor(() => expect(load).toHaveBeenCalledTimes(1));
    expect(await screen.findByText("翻译润色（已停用）")).toBeInTheDocument();
    expect(
      screen.getByRole("checkbox", { name: "启用 翻译润色" }),
    ).not.toBeChecked();
  });

  it("does not offer a disabled skill for use, but still lets it be enabled", async () => {
    const setEnabled = vi.fn().mockResolvedValue(undefined);
    render(
      <SkillsPanel
        initial={disabledView}
        onClose={() => {}}
        setEnabled={setEnabled}
        load={vi.fn().mockResolvedValue(enabledView)}
      />,
    );
    expect(
      screen.getByRole("button", { name: "用这个技能" }),
    ).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: "启用 翻译润色" }));
    await waitFor(() =>
      expect(setEnabled).toHaveBeenCalledWith("translate", true),
    );
    await waitFor(() =>
      expect(
        screen.getByRole("checkbox", { name: "启用 翻译润色" }),
      ).toBeChecked(),
    );
  });

  it("ignores a second change while the first toggle is still saving", async () => {
    let release: () => void = () => {};
    const pending = new Promise<void>((r) => {
      release = r;
    });
    const setEnabled = vi.fn().mockReturnValue(pending);
    const load = vi.fn().mockResolvedValue(disabledView);
    render(
      <SkillsPanel
        initial={enabledView}
        onClose={() => {}}
        setEnabled={setEnabled}
        load={load}
      />,
    );
    const box = screen.getByRole("checkbox", { name: "启用 翻译润色" });
    fireEvent.click(box);
    fireEvent.click(box);
    expect(setEnabled).toHaveBeenCalledTimes(1);
    release();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(1));
  });

  it("hands the chosen skill name to the composer hook", () => {
    const onUseSkill = vi.fn();
    render(
      <SkillsPanel
        initial={enabledView}
        onClose={() => {}}
        onUseSkill={onUseSkill}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "用这个技能" }));
    expect(onUseSkill).toHaveBeenCalledWith("翻译润色");
  });

  it("opens the folder through the injected command", async () => {
    const openFolder = vi.fn().mockResolvedValue(undefined);
    render(
      <SkillsPanel initial={enabledView} onClose={() => {}} openFolder={openFolder} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "打开技能文件夹" }));
    await waitFor(() => expect(openFolder).toHaveBeenCalledTimes(1));
  });

  it("shows a load error instead of a fake empty list, and still closes", async () => {
    const load = vi.fn().mockRejectedValue(new Error("目录读不了"));
    const onClose = vi.fn();
    render(<SkillsPanel onClose={onClose} load={load} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("目录读不了");
    expect(screen.queryByText("正在读取技能目录…")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "关闭" }));
    expect(onClose).toHaveBeenCalled();
  });

  it("refreshes from disk when the refresh button is pressed", async () => {
    const load = vi.fn().mockResolvedValue(enabledView);
    render(<SkillsPanel onClose={() => {}} load={load} />);
    await waitFor(() => expect(load).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: "刷新" }));
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
  });
});
