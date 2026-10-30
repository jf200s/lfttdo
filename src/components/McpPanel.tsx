import { useEffect, useState } from "react";
import {
  mcpConfigRead,
  mcpConfigSave,
  mcpConfigStatus,
  mcpDisconnect,
  mcpReconnect,
  mcpServers,
  mcpSetTrust,
} from "../ipc/commands";
import type {
  McpConfigStatus,
  McpConfigText,
  McpServerView,
  McpTrust,
} from "../types";
import { McpConfigEditor } from "./McpConfigEditor";

interface McpPanelProps {
  /** 测试注入：省一次 IPC；生产不传，面板自己拉。 */
  initial?: McpServerView[];
  onClose: () => void;
  load?: () => Promise<McpServerView[]>;
  setTrust?: (name: string, trust: McpTrust) => Promise<void>;
  reconnect?: (name: string) => Promise<number>;
  disconnect?: (name: string) => Promise<void>;
  /** 配置状态（不含正文）；测试注入，生产走默认命令。 */
  loadConfigStatus?: () => Promise<McpConfigStatus>;
  /** 只在用户点开编辑器时调用，会把含密钥的原文带到前端。 */
  readConfig?: () => Promise<McpConfigText>;
  saveConfig?: (text: string) => Promise<void>;
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
 * 应用内配置编辑（2026-10-30）：在同一面板里切出 JSON 编辑器。保存只写文件，
 * 重启后由 `mcp_bootstrap` 生效；当前会话的连接与工具不变。
 */
export function McpPanel({
  initial,
  onClose,
  load = mcpServers,
  setTrust = mcpSetTrust,
  reconnect = mcpReconnect,
  disconnect = mcpDisconnect,
  loadConfigStatus = mcpConfigStatus,
  readConfig = mcpConfigRead,
  saveConfig = mcpConfigSave,
}: McpPanelProps) {
  const [servers, setServers] = useState<McpServerView[] | null>(initial ?? null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [configStatus, setConfigStatus] = useState<McpConfigStatus | null>(null);
  const [editing, setEditing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const refresh = async () => {
    try {
      setServers(await load());
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const refreshStatus = async () => {
    try {
      setConfigStatus(await loadConfigStatus());
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

  useEffect(() => {
    void refreshStatus();
    // 状态只在挂载与保存成功后刷新。
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

  const finishEditing = async (saved: boolean) => {
    setEditing(false);
    if (!saved) return;
    setNotice(
      "配置已保存，重启应用后生效。当前会话仍按启动时的连接与工具运行。",
    );
    await refresh();
    await refreshStatus();
  };

  return (
    <div className="panel-backdrop" onClick={editing ? undefined : onClose}>
      <section
        className={editing ? "panel panel-wide" : "panel"}
        aria-label={editing ? "编辑 MCP 配置" : "MCP 服务器"}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="panel-title">{editing ? "编辑 mcp.json" : "MCP 服务器"}</h2>
        {error !== null ? <p className="panel-note">{error}</p> : null}
        {editing ? (
          <McpConfigEditor
            load={readConfig}
            save={saveConfig}
            onDone={(saved) => void finishEditing(saved)}
          />
        ) : (
          <>
            {notice !== null ? (
              <p className="panel-note" data-testid="mcp-config-saved">
                {notice}
              </p>
            ) : null}
            {configStatus !== null &&
            (configStatus.parseError !== null ||
              configStatus.skipped.length > 0) ? (
              <div className="panel-note" data-testid="mcp-config-banner">
                {configStatus.parseError !== null ? (
                  <p>配置文件无法解析：{configStatus.parseError}</p>
                ) : null}
                {configStatus.skipped.length > 0 ? (
                  <>
                    <p>有 {configStatus.skipped.length} 条配置被跳过：</p>
                    <ul className="panel-issues">
                      {configStatus.skipped.map((s, i) => (
                        <li key={`${i}-${s.name}`}>
                          {s.name}（{s.reason}）
                        </li>
                      ))}
                    </ul>
                  </>
                ) : null}
                <p>点击「编辑配置」修复。</p>
              </div>
            ) : null}
            {servers === null ? (
              <p className="panel-note">正在读取 mcp.json…</p>
            ) : servers.length === 0 ? (
              <p className="panel-note">
                {configStatus !== null &&
                (configStatus.parseError !== null ||
                  configStatus.skipped.length > 0)
                  ? "没有可用的服务器配置。"
                  : "还没有配置服务器。点击「编辑配置」直接写 mcp.json。"}
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
                  {s.reason !== null ? (
                    <p className="panel-note">{s.reason}</p>
                  ) : null}
                  <p className="panel-row">
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(s.name, () => setTrust(s.name, "auto"))
                      }
                    >
                      本服务器全部允许
                    </button>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(s.name, () => setTrust(s.name, "ask"))
                      }
                    >
                      每次询问
                    </button>
                    <button
                      type="button"
                      disabled={busy !== null}
                      onClick={() =>
                        void run(s.name, () => setTrust(s.name, "block"))
                      }
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
            <div className="panel-actions">
              <button
                type="button"
                onClick={() => {
                  setError(null);
                  setNotice(null);
                  setEditing(true);
                }}
              >
                编辑配置
              </button>
              <button type="button" onClick={onClose}>
                关闭
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
