import { subscriptions } from "./subscriptions";
import { useCallback, useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf } from "./types";
import type { LinkHandlers as Handlers } from "./bridge-contracts";
export default function LinkSettings() {
  const [state, setState] = useState<Handlers | null>(null),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
    [cleanup, setCleanup] = useState(false);
  const refresh = useCallback(() => {
    void command("get_link_handler_state")
      .then(setState)
      .catch((e) => setError(errorOf(e).message));
  }, []);
  useEffect(() => {
    refresh();
    window.addEventListener("focus", refresh);
    let disposed = false;
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
      disposed = true;
      window.removeEventListener("focus", refresh);
      events.dispose();
    };
  }, [refresh]);
  return (
    <fieldset className="startup-settings link-settings">
      <legend>CC Switch 链接</legend>
      <label>
        默认接收应用
        <select
          aria-label="默认接收应用"
          disabled={busy || !state}
          value={state?.current ?? ""}
          onChange={(e) => {
            const id = e.target.value;
            setBusy(true);
            setError("");
            void command("set_link_handler", { appId: id })
              .then((s) => {
                setState(s);
                if (!s.systemPicker && s.current !== id)
                  setError("默认应用未更改");
              })
              .catch((e) => setError(errorOf(e).message))
              .finally(() => setBusy(false));
          }}
        >
          {!state?.apps.some((a) => a.id === state.current) && (
            <option value={state?.current ?? ""}>
              {state?.current ? "其他应用" : "未设置"}
            </option>
          )}
          {state?.apps.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name}
            </option>
          ))}
        </select>
      </label>
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
