import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { Sidebar } from "./Sidebar";
import type { SessionRow } from "../types";

// R128：线上键是 `updated_at`（db.rs:239-246 无 rename_all）。这一支 fixture 故意写成
// 「数组顺序 = 插入顺序、updated_at = 相反」，这样「忘了排序」和「按 updatedAt(undefined)
// 排序 ⇒ 比较函数恒假、顺序碰巧」两种坏法都会红。（写成 `updatedAt` 的话 TS 直接报错，
// 因为镜像里没有那个键 —— 这正是补镜像的意义。）
const rows: SessionRow[] = [
  { id: "old", title: "昨天那条", updated_at: 1_700_000_000_000 },
  { id: "new", title: "刚才那条", updated_at: 1_700_000_999_999 },
  { id: "mid", title: "中午那条", updated_at: 1_700_000_500_000 },
];

// 每一行的 testid 都带 id（`session-row-<id>`），排序断言只看**文档顺序**，
// 所以这里必须用正则取全量再按 `data-session-id` 还原成 id 序列。
const order = (): string[] =>
  screen
    .getAllByTestId(/session-row-/)
    .map((li) => li.getAttribute("data-session-id") as string);

function setup(over: Partial<Parameters<typeof Sidebar>[0]> = {}) {
  const props = {
    rows,
    activeId: "new",
    onSelect: vi.fn(),
    onCreate: vi.fn(),
    onDelete: vi.fn(),
    onRename: vi.fn(),
    ...over,
  };
  render(<Sidebar {...props} />);
  return props;
}

describe("Sidebar", () => {
  it("orders sessions by updated_at descending, not by arrival order", () => {
    setup();
    expect(order()).toEqual(["new", "mid", "old"]);
  });

  it("keeps the ordering stable when the list arrives sorted the other way", () => {
    setup({ rows: [...rows].reverse() });
    expect(order()).toEqual(["new", "mid", "old"]);
  });

  it("marks the active session and hands the id over on click", () => {
    const props = setup();
    const active = screen.getByTestId("session-row-new");
    expect(active).toHaveAttribute("data-active", "true");
    expect(screen.getByTestId("session-row-old")).toHaveAttribute(
      "data-active",
      "false",
    );
    fireEvent.click(screen.getByRole("button", { name: /昨天那条/ }));
    expect(props.onSelect).toHaveBeenCalledWith("old");
  });

  it("creates a session through the button only", () => {
    const props = setup();
    fireEvent.click(screen.getByRole("button", { name: /新建对话/ }));
    expect(props.onCreate).toHaveBeenCalled();
  });

  // 要求 3 的同一道红线在侧栏这一侧：改名必须是行内 <input>。
  it("renames through an inline input", () => {
    const props = setup();
    expect(screen.queryByRole("textbox")).toBeNull();
    const row = screen.getByTestId("session-row-old");
    fireEvent.click(within(row).getByRole("button", { name: /重命名/ }));
    const box = within(row).getByRole("textbox") as HTMLInputElement;
    expect(box.value).toBe("昨天那条");
    fireEvent.change(box, { target: { value: "改名了" } });
    fireEvent.click(within(row).getByRole("button", { name: /提交重命名/ }));
    expect(props.onRename).toHaveBeenCalledWith("old", "改名了");
  });

  it("does not rename to a blank title", () => {
    const props = setup();
    const row = screen.getByTestId("session-row-old");
    fireEvent.click(within(row).getByRole("button", { name: /重命名/ }));
    fireEvent.change(
      within(row).getByRole("textbox"),
      { target: { value: "   " } },
    );
    fireEvent.click(within(row).getByRole("button", { name: /提交重命名/ }));
    expect(props.onRename).not.toHaveBeenCalled();
  });

  it("deletes the session the button belongs to", () => {
    const props = setup();
    const target = screen.getByTestId("session-row-mid");
    fireEvent.click(within(target).getByRole("button", { name: /删除/ }));
    expect(props.onDelete).toHaveBeenCalledWith("mid");
  });
});
