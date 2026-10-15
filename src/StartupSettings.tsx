import { useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf, type StartupState } from "./types";

export default function StartupSettings() {
  const [state, setState] = useState<StartupState | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let disposed = false;
    void command("get_startup")
      .then((s) => {
        if (!disposed) setState(s);
      })
      .catch((e) => {
        if (!disposed) setError(errorOf(e).message);
      });
    return () => {
      disposed = true;
    };
  }, []);
  const update = async (enabled: boolean) => {
    if (!state) return;
    setBusy(true);
    setError("");
    try {
      setState(
        await command("set_startup", {
          enabled,
          expectedRevision: state.revision,
        }),
      );
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <fieldset className="startup-settings" disabled={busy || !state}>
      <legend>启动</legend>
      <label className="check-label">
        <input
          type="checkbox"
          checked={state?.launchOnBoot ?? false}
          onChange={(e) => void update(e.target.checked)}
        />
        开机静默启动
      </label>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
    </fieldset>
  );
}
