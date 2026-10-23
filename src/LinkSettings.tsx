import { subscriptions } from "./subscriptions";
import { useCallback, useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf } from "./types";
import type { LinkHandlers as Handlers } from "./bridge-contracts";
const SELF = "com.lich13.gpt-switch";
export default function LinkSettings() {
  const [state, setState] = useState<Handlers | null>(null),
    [selected, setSelected] = useState(""),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
    [cleanup, setCleanup] = useState(false);
  const accept = useCallback((s: Handlers) => {
    setState(s);
    setSelected(s.remembered ?? "");
  }, []);
  const refresh = useCallback(() => {
    void command("get_link_handler_state")
      .then(accept)
      .catch((e) => setError(errorOf(e).message));
  }, [accept]);
  useEffect(() => {
    refresh();
    window.addEventListener("focus", refresh);
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("app-visibility", (v) => {
      if (v) refresh();
    });
    void command("get_startup_error")
      .then((e) => {
        setCleanup(e?.code === "CLEANUP");
        if (e?.code === "CLEANUP") setError(e.message);
      })
      .catch(() => {});
    return () => {
      window.removeEventListener("focus", refresh);
      events.dispose();
    };
  }, [refresh]);
  const save = (appId: string | null) => {
    setBusy(true);
    setError("");
    void command("set_link_receiver", { appId })
      .then(accept)
      .catch((e) => setError(errorOf(e).message))
      .finally(() => setBusy(false));
  };
  return (
    <fieldset className="startup-settings link-settings">
      <legend>CC Switch 链接</legend>
      <label>
        接收应用
        <select
          aria-label="接收应用"
          disabled={busy || !state}
          value={selected}
          onChange={(e) => {
            if (state?.remembered) save(e.target.value);
            else setSelected(e.target.value);
          }}
        >
          <option value="" disabled>
            每次询问
          </option>
          {state?.remembered &&
            !state.apps.some((a) => a.id === state.remembered) && (
              <option value={state.remembered}>已记住的应用不可用</option>
            )}
          {state?.apps.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name}
            </option>
          ))}
        </select>
      </label>
      <label className="setting-row">
        <span>下次不再提示</span>
        <input
          type="checkbox"
          checked={!!state?.remembered}
          disabled={busy || !state || !selected}
          onChange={(e) => save(e.target.checked ? selected : null)}
        />
      </label>
      {state && state.current !== SELF && (
        <button
          type="button"
          className="secondary"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            setError("");
            void command("enable_link_picker")
              .then((s) => {
                accept(s);
                if (!s.systemPicker && s.current !== SELF)
                  setError("默认应用未更改，请重试");
              })
              .catch((e) => setError(errorOf(e).message))
              .finally(() => setBusy(false));
          }}
        >
          启用链接选择
        </button>
      )}
      {busy && <span role="status">正在设置…</span>}
      {error && (
        <div className="form-error" role="alert">
          {error}
        </div>
      )}
      {cleanup && (
        <button
          type="button"
          className="secondary"
          onClick={() =>
            void command("cleanup_retired_data")
              .then(() => {
                setCleanup(false);
                setError("");
              })
              .catch((e) => setError(errorOf(e).message))
          }
        >
          重试清理旧统计数据
        </button>
      )}
    </fieldset>
  );
}
