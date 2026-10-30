import { useEffect, useRef, useState } from "react";
import { skillsList, skillsOpenFolder, skillsSetEnabled } from "../ipc/commands";
import type { SkillsView } from "../types";

interface SkillsPanelProps {
  /** 测试注入：省一次 IPC；生产不传，面板自己拉。 */
  initial?: SkillsView;
  onClose: () => void;
  load?: () => Promise<SkillsView>;
  openFolder?: () => Promise<void>;
  setEnabled?: (folder: string, enabled: boolean) => Promise<void>;
  /** 点「用这个技能」：由 App 关面板并把一句引导填进输入框。 */
  onUseSkill?: (name: string) => void;
}

/**
 * 技能面板。列表每次打开/刷新都从磁盘现扫（`skills_list`），选择表也由 Rust 落盘。
 * 启用/停用只影响后续消息；模型遇到已启用技能的相关任务时仍会自动读取。
 */
export function SkillsPanel({
  initial,
  onClose,
  load = skillsList,
  openFolder = skillsOpenFolder,
  setEnabled = skillsSetEnabled,
  onUseSkill,
}: SkillsPanelProps) {
  const [view, setView] = useState<SkillsView | null>(initial ?? null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  /**
   * 同步闸：真实浏览器里点一次复选框可能连发两次 change（label 转发），而 state 里的
   * `busy` 要等下一次渲染才挡住 —— 两次写盘会一个写停用一个写启用，读回来的列表还可能
   * 落在旧的那份上（实测：界面显示已停用、skills.json 却是空数组）。
   */
  const busyRef = useRef(false);

  const refresh = async () => {
    setBusy(true);
    try {
      setView(await load());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    if (initial === undefined) void refresh();
    // 只在挂载时拉一次；之后的刷新由按钮触发。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const open = async () => {
    setBusy(true);
    try {
      await openFolder();
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggle = async (folder: string, enabled: boolean) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      await setEnabled(folder, enabled);
      setView(await load());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  return (
    <div className="panel-backdrop" onClick={onClose}>
      <section
        className="panel"
        aria-label="技能"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="panel-title">技能</h2>
        <p className="panel-note">
          技能可以逐个启用/停用：启用后模型遇到相关任务会自动调用 use_skill 读取正文，
          也可以点「用这个技能」把它填进输入框。SKILL.md 里写了 keywords 的技能，
          你话里带到那个词就会自动加载，不用先点名。每个技能是一个文件夹，里面一份
          SKILL.md。
        </p>
        {error !== null ? (
          <p className="panel-note" role="alert">
            {error}
          </p>
        ) : null}
        {view === null ? (
          error === null ? <p className="panel-note">正在读取技能目录…</p> : null
        ) : (
          <>
            <p className="panel-note">
              技能目录：<code>{view.root}</code>
            </p>
            {view.skills.length === 0 ? (
              <p className="panel-note">
                还没有技能。在技能目录里新建一个文件夹，放一份 SKILL.md：开头写 name 和
                description，正文写步骤；保存后下一条消息就生效。
              </p>
            ) : (
              view.skills.map((s) => (
                <fieldset className="panel-group" key={`${s.folder}-${s.name}`}>
                  <legend>
                    {s.name}
                    {s.enabled ? "" : "（已停用）"}
                  </legend>
                  <p className="panel-row panel-readonly">
                    {s.description || "（未填写描述）"}
                  </p>
                  <p className="panel-note">文件夹：{s.folder}</p>
                  <div className="panel-row">
                    <label htmlFor={`skill-enable-${s.folder}`}>启用</label>
                    <input
                      id={`skill-enable-${s.folder}`}
                      type="checkbox"
                      aria-label={`启用 ${s.name}`}
                      checked={s.enabled}
                      disabled={busy}
                      onChange={(e) => void toggle(s.folder, e.target.checked)}
                    />
                  </div>
                  <div className="panel-actions">
                    <button
                      type="button"
                      disabled={busy || !s.enabled}
                      onClick={() => onUseSkill?.(s.name)}
                    >
                      用这个技能
                    </button>
                  </div>
                </fieldset>
              ))
            )}
          </>
        )}
        <div className="panel-actions">
          <button type="button" disabled={busy} onClick={() => void open()}>
            打开技能文件夹
          </button>
          <button type="button" disabled={busy} onClick={() => void refresh()}>
            刷新
          </button>
          <button type="button" onClick={onClose}>
            关闭
          </button>
        </div>
      </section>
    </div>
  );
}
