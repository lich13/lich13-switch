import { useEffect, useState } from "react";
import { command } from "./bridge";
import { clients, clientName } from "./ClientSelection";
import { subscriptions } from "./subscriptions";
import { errorOf, type ClientId } from "./types";
export default function QuickPlatformSettings() {
  const [hidden, setHidden] = useState<ClientId[] | null>(null);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  useEffect(() => {
    let gone = false,
      updated = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("quick-state", (p) => {
      updated = true;
      setHidden(p.hiddenClients ?? []);
    });
    void command("get_quick")
      .then((p) => {
        if (!gone && !updated) setHidden(p.hiddenClients ?? []);
      })
      .catch((e) => {
        if (!gone) setError(errorOf(e).message);
      });
    return () => {
      gone = true;
      events.dispose();
    };
  }, []);
  return (
    <fieldset
      className="quick-platform-settings"
      disabled={busy || hidden === null}
    >
      <legend>快捷面板平台</legend>
      <div>
        {clients.map((id) => (
          <label key={id}>
            <span>{clientName(id)}</span>
            <input
              type="checkbox"
              checked={!hidden?.includes(id)}
              onChange={(e) => {
                const next = e.target.checked
                  ? (hidden ?? []).filter((c) => c !== id)
                  : [...(hidden ?? []), id];
                setBusy(true);
                setError("");
                void command("set_quick", { hiddenClients: next })
                  .then((p) => setHidden(p.hiddenClients ?? []))
                  .catch((e) => setError(errorOf(e).message))
                  .finally(() => setBusy(false));
              }}
            />
          </label>
        ))}
      </div>
      {error && (
        <div className="form-error" role="alert">
          {error}
        </div>
      )}
    </fieldset>
  );
}
