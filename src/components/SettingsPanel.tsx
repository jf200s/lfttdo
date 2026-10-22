import { useEffect, useRef, useState } from "react";
import { THEME_NAMES, type Mode } from "../theme/theme";
import type { Settings, SettingsView } from "../types";

interface SettingsPanelProps {
  initial: SettingsView;
  onSave: (s: Settings) => Promise<void>;
  onSaveKey: (k: string) => Promise<void>;
  onClearKey: () => Promise<void>;
  onClose: () => void;
}

/** 面板只渲染这 8 个键 —— 它们就是 `Settings` 今天的全部（`settings.rs:245-252` / `types.ts:109-118`）。
 *  `api_key_masked` 与 `corrupt_backup` 是 `SettingsView` 多出来的两只**只读**键，
 *  写回 `settings_set` 等于把掩码和备份路径当设置存盘（`commands.ts:62` 的注释就写着这件事）。 */
const settingsOf = (v: SettingsView): Settings => ({
  base_url: v.base_url,
  model: v.model,
  reserve_output: v.reserve_output,
  budget_tokens: v.budget_tokens,
  window_messages: v.window_messages,
  theme: v.theme,
  mode: v.mode,
  font_size: v.font_size,
});

/** 数值项的名称表：`NaN` 必须在提交前被拦下并**报出是哪一项**。
 *  serde 收到 NaN 会直接拒收，症状是「点了保存什么也没发生」，而用户看不出是哪个框。 */
const NUMERIC_FIELDS: { key: keyof Settings; label: string }[] = [
  { key: "reserve_output", label: "输出预留" },
  { key: "budget_tokens", label: "上下文预算" },
  { key: "window_messages", label: "窗口条数" },
  { key: "font_size", label: "字号" },
];

export function SettingsPanel({
  initial,
  onSave,
  onSaveKey,
  onClearKey,
  onClose,
}: SettingsPanelProps) {
  // 从 `initial` 拷一份本地草稿：面板活着的时候用户改的是这一份，
  // 保存失败不许把草稿也一起回滚掉（那等于让用户重打一遍）。
  const [form, setForm] = useState<Settings>(() => settingsOf(initial));
  const [keyDraft, setKeyDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);

  // 打开即把焦点收进对话框：`aria-modal` 承诺了「这是当前唯一的操作面」，
  // 焦点却留在背后的界面上，键盘用户就看不见这个承诺；Escape 也才按得掉。
  useEffect(() => {
    boxRef.current?.focus();
  }, []);

  const setField = <K extends keyof Settings>(key: K, value: Settings[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

  const submit = async () => {
    for (const { key, label } of NUMERIC_FIELDS) {
      const n = form[key];
      if (typeof n === "number" && !Number.isFinite(n)) {
        setError(`「${label}」必须是数字，现在是空的或不是数`);
        return;
      }
    }
    const typedKey = keyDraft.trim();
    try {
      await onSave(form);
    } catch (e) {
      // 原文照贴，前端不解释文案（R195 口径）：解释一次就多一处与上游分叉的真相。
      setError(String(e));
      return;
    }
    if (typedKey === "") {
      onClose();
      return;
    }
    try {
      await onSaveKey(typedKey);
    } catch (e) {
      // 走到这里设置**已经存下了**，只有 key 没写进去。这句话必须说全，
      // 否则用户以为整次保存都失败，会去重打一遍已经存好的模型名。
      setError(`设置已保存，但 API key 写入失败：${String(e)}`);
      return;
    }
    setKeyDraft("");
    onClose();
  };

  const clearKey = async () => {
    try {
      await onClearKey();
    } catch (e) {
      setError(String(e));
      return;
    }
    setConfirmClear(false);
    onClose();
  };

  return (
    <div className="panel-backdrop">
      <div
        ref={boxRef}
        className="panel"
        role="dialog"
        aria-modal="true"
        aria-label="设置"
        tabIndex={-1}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h2 className="panel-title">设置</h2>

        {initial.corrupt_backup !== null ? (
          <p className="panel-note" data-testid="corrupt-backup">
            设置文件曾损坏，已备份为 {initial.corrupt_backup}；下面显示的是回落后的默认值。
          </p>
        ) : null}

        <fieldset className="panel-group">
          <legend>模型</legend>
          <label className="panel-row">
            <span>接口地址（base_url）</span>
            <input
              type="text"
              value={form.base_url}
              onChange={(e) => setField("base_url", e.target.value)}
            />
          </label>
          <label className="panel-row">
            <span>模型名</span>
            <input
              type="text"
              value={form.model}
              onChange={(e) => setField("model", e.target.value)}
            />
          </label>
          {NUMERIC_FIELDS.map(({ key, label }) => (
            <label className="panel-row" key={key}>
              <span>{label}</span>
              <input
                type="number"
                step={1}
                value={Number.isFinite(form[key] as number) ? String(form[key]) : ""}
                onChange={(e) =>
                  setField(key, e.target.valueAsNumber as Settings[typeof key])
                }
              />
            </label>
          ))}
        </fieldset>

        <fieldset className="panel-group">
          <legend>API key</legend>
          <p className="panel-row panel-readonly">
            <span>当前</span>
            {/* 只有掩码，永远读不回值（spec:473）。`null` 是「未配置」，不是空字符串。 */}
            <code>{initial.api_key_masked ?? "未配置"}</code>
          </p>
          <label className="panel-row">
            <span>API key</span>
            <input
              type="password"
              autoComplete="off"
              value={keyDraft}
              onChange={(e) => setKeyDraft(e.target.value)}
            />
          </label>
          <p className="panel-hint">
            留空表示不动凭据；保存后这里的输入框立即清空，明文只走
            <code>settings_set_api_key</code> 这一条命令。
          </p>
          <div className="panel-actions">
            {confirmClear ? (
              <>
                <button type="button" onClick={() => void clearKey()}>
                  确认清除
                </button>
                <button type="button" onClick={() => setConfirmClear(false)}>
                  取消
                </button>
              </>
            ) : (
              <button
                type="button"
                disabled={initial.api_key_masked === null}
                onClick={() => setConfirmClear(true)}
              >
                清除已存的 key
              </button>
            )}
          </div>
        </fieldset>

        <fieldset className="panel-group">
          <legend>界面</legend>
          <label className="panel-row">
            <span>主题</span>
            <select
              value={form.theme}
              onChange={(e) => setField("theme", e.target.value)}
            >
              {THEME_NAMES.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </label>
          <label className="panel-row">
            <span>明暗</span>
            <select
              value={form.mode}
              onChange={(e) => setField("mode", e.target.value as Mode)}
            >
              <option value="light">浅色</option>
              <option value="dark">深色</option>
            </select>
          </label>
        </fieldset>

        {error !== null ? (
          <p className="panel-error" role="alert" data-testid="panel-error">
            {error}
          </p>
        ) : null}

        <div className="panel-actions">
          <button type="button" className="panel-save" onClick={() => void submit()}>
            保存
          </button>
          <button type="button" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
