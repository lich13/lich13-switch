import { useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import { errorOf, type ViewState } from "./types";

export default function QuotaRefreshSettings({
  seconds,
  onDirtyChange,
}: {
  seconds: number;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [enabled, setEnabled] = useState(seconds !== 0);
  const [input, setInput] = useState(String(seconds || 60));
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [retry, setRetry] = useState(false);
  const baseline = useRef(seconds);
  useEffect(() => {
    if (!editing) {
      setEnabled(seconds !== 0);
      setInput(String(seconds || 60));
      baseline.current = seconds;
    }
  }, [seconds, editing]);
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const change = () => {
    if (!editing) baseline.current = seconds;
    setEditing(true);
    onDirtyChange?.(true);
  };
  const save = async () => {
    const value = enabled ? Number(input) : 0;
    if (enabled && (!Number.isInteger(value) || value < 10 || value > 86400)) {
      setError("额度刷新间隔需为 10–86400 秒");
      return;
    }
    setBusy(true);
    setError("");
    try {
      await command<ViewState>("set_quota_refresh", {
        seconds: value,
        expectedSeconds: retry ? seconds : baseline.current,
      });
      baseline.current = value;
      setRetry(false);
      setEditing(false);
      onDirtyChange?.(false);
    } catch (e) {
      setError(errorOf(e).message);
      setRetry(true);
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="quota-refresh-settings"
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <label className="setting-row">
        <span>额度自动刷新</span>
        <input
          type="checkbox"
          checked={enabled}
          disabled={busy}
          onChange={(e) => {
            change();
            setEnabled(e.target.checked);
          }}
        />
      </label>
      {enabled && (
        <label className="setting-row">
          <span>间隔 / 秒</span>
          <input
            aria-label="额度刷新间隔"
            type="number"
            min={10}
            max={86400}
            step={1}
            value={input}
            disabled={busy}
            onChange={(e) => {
              change();
              setInput(e.target.value);
            }}
          />
        </label>
      )}
      {error && (
        <div role="alert" className="form-error">
          {error}
        </div>
      )}
      <button className="secondary" disabled={busy}>
        {busy ? "保存中…" : retry ? "重试" : "保存刷新设置"}
      </button>
    </form>
  );
}
