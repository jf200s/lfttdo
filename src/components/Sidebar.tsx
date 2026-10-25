import { useState } from "react";
import type { SessionRow } from "../types";

interface SidebarProps {
  /** 收起态：只留一条 56px 窄栏（R337）。 */
  collapsed?: boolean;
  onToggleCollapsed?: () => void;
  rows: SessionRow[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onCreate: () => void;
  onDelete: (id: string) => void;
  onRename: (id: string, title: string) => void;
}

/**
 * 会话侧栏。
 *
 * 排序键必须是 `updated_at`（R128：`db.rs:239-246` 的 `SessionRow` 没有 `rename_all`，
 * 线上就是 snake_case）。写成 `updatedAt` 的话这里读到 `undefined`，`b.updatedAt - a.updatedAt`
 * 全是 `NaN`，比较函数恒假 ⇒ 顺序变成「碰巧」而**一声不响**，所以 `Sidebar.test.tsx` 里
 * 那两条顺序断言是这一支的唯一守卫。`[...rows].sort` 而不是 `rows.sort`：不许改调用方的数组。
 *
 * `updated_at` 是毫秒（R72），前端只拿它排序，不格式化、不显示 —— 没有消费者就不许编一份本地时间。
 */
export function Sidebar({
  collapsed = false,
  onToggleCollapsed,
  rows,
  activeId,
  onSelect,
  onCreate,
  onDelete,
  onRename,
}: SidebarProps) {
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const sorted = [...rows].sort((a, b) => b.updated_at - a.updated_at);

  const submitRename = (id: string) => {
    const title = draft.trim();
    // 空标题不许提交：那是把侧栏那一行变成点不动的空白，而 Rust 侧只会照单收下。
    if (title !== "") onRename(id, title);
    setEditing(null);
    setDraft("");
  };

  return (
    <nav
      className="sidebar"
      aria-label="会话列表"
      data-collapsed={collapsed}
    >
      <div className="sidebar-head">
        <button
          type="button"
          className="sidebar-toggle"
          aria-label={collapsed ? "展开侧栏" : "收起侧栏"}
          onClick={onToggleCollapsed}
        >
          ☰
        </button>
        {collapsed ? (
          <button
            type="button"
            className="sidebar-plus"
            aria-label="新建对话"
            onClick={onCreate}
          >
            ＋
          </button>
        ) : null}
      </div>
      {collapsed ? null : (
        <button type="button" className="new-session" onClick={onCreate}>
          新建对话
        </button>
      )}
      <ul className="session-list">
        {sorted.map((r) => (
          <li
            key={r.id}
            className="session-item"
            data-testid={`session-row-${r.id}`}
            data-session-id={r.id}
            data-active={r.id === activeId}
          >
            <button
              type="button"
              className="session-title"
              onClick={() => onSelect(r.id)}
            >
              {r.title}
            </button>
            <button
              type="button"
              className="session-rename"
              onClick={() => {
                setEditing(r.id);
                setDraft(r.title);
              }}
            >
              重命名
            </button>
            <button
              type="button"
              className="session-delete"
              onClick={() => onDelete(r.id)}
            >
              删除
            </button>
            {editing === r.id ? (
              <div className="rename-form">
                <input
                  aria-label={`重命名 ${r.title}`}
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                />
                <button
                  type="button"
                  onClick={() => submitRename(r.id)}
                >
                  提交重命名
                </button>
              </div>
            ) : null}
          </li>
        ))}
      </ul>
    </nav>
  );
}
