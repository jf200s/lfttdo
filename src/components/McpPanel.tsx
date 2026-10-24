import { useEffect, useState } from "react";
import {
  mcpDisconnect,
  mcpReconnect,
  mcpServers,
  mcpSetTrust,
} from "../ipc/commands";
import type { McpServerView, McpTrust } from "../types";

interface McpPanelProps {
  /** 测试注入：省一次 IPC；生产不传，面板自己拉。 */
  initial?: McpServerView[];
  onClose: () => void;
  load?: () => Promise<McpServerView[]>;
  setTrust?: (name: string, trust: McpTrust) => Promise<void>;
  reconnect?: (name: string) => Promise<number>;
  disconnect?: (name: string) => Promise<void>;
}

const STATUS_TEXT: Record<McpServerView["status"], string> = {
  connecting: "连接中…",
  ready: "已连接",
  disconnected: "未连接",
};

const TRUST_TEXT: Record<McpTrust, string> = {
  auto: "全部允许",
  ask: "每次询问",
  block: "禁止",
};

/**
 * MCP 服务器面板（T35）。**视图即真相**：每次动作都重新拉一遍列表，
 * 不在前端维护第二份「连接状态」——Rust 的桥才是唯一的状态源（`ServerStatus`）。
 *
 * 没有弹窗（R133）：失败原因直接渲染在条目下面，`reason` 已经在 Rust 侧脱敏且截断到 200 字符。
 */
export function McpPanel({
  initial,
  onClose,
  load = mcpServers,
  setTrust = mcpSetTrust,
  reconnect = mcpReconnect,
  disconnect = mcpDisconnect,
}: McpPanelProps) {
  const [servers, setServers] = useState<McpServerView[] | null>(initial ?? null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setServers(await load());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    if (initial === undefined) {
      void refresh();
    }
    // 只在挂载时拉一次；之后的刷新由每个动作自己触发。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = async (name: string, action: () => Promise<unknown>) => {
    setBusy(name);
    try {
      await action();
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      await refresh();
    }
  };

  return (
    <div className="panel-backdrop" onClick={onClose}>
      <section
        className="panel"
        aria-label="MCP 服务器"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="panel-title">MCP 服务器</h2>
        {error !== null ? <p className="panel-note">{error}</p> : null}
        {servers === null ? (
          <p className="panel-note">正在读取 mcp.json…</p>
        ) : servers.length === 0 ? (
          <p className="panel-note">
            还没有配置服务器。在应用数据目录里放一份 mcp.json（mcpServers 字段）后重开这个面板。
          </p>
        ) : (
          servers.map((s) => (
            <fieldset className="panel-group" key={s.name}>
              <legend>
                {s.name} 提供 {s.tools} 个工具
              </legend>
              <p className="panel-row panel-readonly">
                {STATUS_TEXT[s.status]} · 信任：{TRUST_TEXT[s.trust]}
              </p>
              {s.reason !== null ? <p className="panel-note">{s.reason}</p> : null}
              <p className="panel-row">
                <button
                  type="button"
                  disabled={busy !== null}
                  onClick={() => void run(s.name, () => setTrust(s.name, "auto"))}
                >
                  本服务器全部允许
                </button>
                <button
                  type="button"
                  disabled={busy !== null}
                  onClick={() => void run(s.name, () => setTrust(s.name, "ask"))}
                >
                  每次询问
                </button>
                <button
                  type="button"
                  disabled={busy !== null}
                  onClick={() => void run(s.name, () => setTrust(s.name, "block"))}
                >
                  禁止
                </button>
                <button
                  type="button"
                  disabled={busy !== null}
                  onClick={() => void run(s.name, () => reconnect(s.name))}
                >
                  重连
                </button>
                <button
                  type="button"
                  disabled={busy !== null}
                  onClick={() => void run(s.name, () => disconnect(s.name))}
                >
                  断开
                </button>
              </p>
            </fieldset>
          ))
        )}
        <p className="panel-row">
          <button type="button" onClick={onClose}>
            关闭
          </button>
        </p>
      </section>
    </div>
  );
}
