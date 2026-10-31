import { useEffect, useState } from "react";
import { mcpConfigRead, mcpConfigSave } from "../ipc/commands";
import type { McpConfigText } from "../types";

interface McpConfigEditorProps {
  /** 测试注入；生产默认走 IPC。 */
  load?: () => Promise<McpConfigText>;
  save?: (text: string) => Promise<void>;
  /** saved = true 表示已写盘；面板据此提示「重启后生效」。 */
  onDone: (saved: boolean) => void;
}

/**
 * 应用内 mcp.json 编辑器。
 *
 * 只在挂载期间持有配置正文；关闭即丢弃，不写任何 store。
 * 保存成功由父组件负责关闭本视图并刷新列表，所以成功路径不在这里改父状态。
 */
export function McpConfigEditor({
  load = mcpConfigRead,
  save = mcpConfigSave,
  onDone,
}: McpConfigEditorProps) {
  const [snapshot, setSnapshot] = useState<McpConfigText | null>(null);
  const [text, setText] = useState("");
  const [original, setOriginal] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let alive = true;
    load()
      .then((c) => {
        if (!alive) return;
        setSnapshot(c);
        setText(c.text);
        setOriginal(c.text);
        setError(null);
      })
      .catch((e: unknown) => {
        if (alive) setError(String(e));
      });
    return () => {
      alive = false;
    };
  }, [load, reload]);

  const dirty = text !== original;

  const requestClose = () => {
    if (saving) return;
    if (dirty) {
      setConfirmDiscard(true);
      return;
    }
    onDone(false);
  };

  const submit = async () => {
    setSaving(true);
    try {
      await save(text);
    } catch (e) {
      setError(String(e));
      setSaving(false);
      return;
    }
    setSaving(false);
    onDone(true);
  };

  if (snapshot === null) {
    return (
      <div className="panel-group" data-testid="mcp-config-loading">
        {error === null ? (
          <p className="panel-note">正在读取 mcp.json…</p>
        ) : (
          <>
            <p className="panel-error" role="alert">
              {error}
            </p>
            <div className="panel-actions">
              <button
                type="button"
                onClick={() => {
                  setError(null);
                  setReload((n) => n + 1);
                }}
              >
                重试
              </button>
              <button type="button" onClick={() => onDone(false)}>
                返回
              </button>
            </div>
          </>
        )}
      </div>
    );
  }

  return (
    <div
      className="panel-group mcp-config-editor"
      data-testid="mcp-config-editor"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          requestClose();
        }
      }}
    >
      <p className="panel-readonly" data-testid="mcp-config-path">
        <span className="panel-hint">文件：</span>
        <code>{snapshot.path}</code>
      </p>
      <p className="panel-hint">
        配置中的 env/headers 会以明文显示。文件根必须是对象、mcpServers 必须是对象。
        可配置多个 server，名称不能重复：stdio 用 command/args/env；HTTP（streamable-http）
        用 url/headers。type 字段当前不参与判定；disabled 字段当前不识别。
      </p>
      {snapshot.exists ? null : (
        <p className="panel-hint">文件尚不存在，保存后创建。</p>
      )}
      {snapshot.parseError !== null ? (
        <p className="panel-error" role="alert">
          载入时无法解析：{snapshot.parseError}
        </p>
      ) : null}
      {snapshot.skipped.length > 0 ? (
        <div className="panel-note">
          <p>载入时有 {snapshot.skipped.length} 条配置被跳过：</p>
          <ul className="panel-issues">
            {snapshot.skipped.map((s, i) => (
              <li key={`${i}-${s.name}`}>
                {s.name}（{s.reason}）
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      <textarea
        className="mcp-config-text"
        aria-label="mcp.json 内容"
        autoFocus
        spellCheck={false}
        rows={16}
        readOnly={saving}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setError(null);
        }}
      />
      {error !== null ? (
        <p className="panel-error" role="alert" data-testid="mcp-config-error">
          {error}
        </p>
      ) : null}
      {confirmDiscard ? (
        <div className="panel-actions">
          <span className="panel-hint">放弃修改？</span>
          <button type="button" onClick={() => onDone(false)}>
            放弃
          </button>
          <button type="button" onClick={() => setConfirmDiscard(false)}>
            继续编辑
          </button>
        </div>
      ) : (
        <div className="panel-actions">
          <button
            type="button"
            className="panel-save"
            disabled={saving}
            onClick={() => void submit()}
          >
            保存
          </button>
          <button type="button" disabled={saving} onClick={requestClose}>
            返回
          </button>
        </div>
      )}
    </div>
  );
}
