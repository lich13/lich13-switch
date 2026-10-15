import { useCallback, useState } from "react";
import type { ClientId } from "./types";
import { confirmAction } from "./confirmation";

export const clientName = (client: ClientId) =>
  client === "claude" ? "Claude Code" : client === "grok" ? "Grok Build" : "Codex";
export function useClientSelection(surface: "main" | "quick" | "config") {
  const key = `lich13-switch.${surface}.client`;
  const [client, setClient] = useState<ClientId>(() => {
    try {
      const stored = localStorage.getItem(key);
      return stored === "claude" || stored === "grok" ? stored : "codex";
    } catch {
      return "codex";
    }
  });
  const select = useCallback(
    async (next: ClientId, dirty = false) => {
      if (next === client) return true;
      if (
        (dirty || document.querySelector("[data-provider-draft]")) &&
        !(await confirmAction("切换客户端会丢弃未保存的修改。"))
      )
        return false;
      try {
        localStorage.setItem(key, next);
      } catch {
        /* Selection remains usable without storage. */
      }
      setClient(next);
      return true;
    },
    [client, key],
  );
  return [client, select] as const;
}
export default function ClientSelection({
  client,
  select,
  disabled = false,
}: {
  client: ClientId;
  select: (id: ClientId) => void;
  disabled?: boolean;
}) {
  return (
    <nav className="segmented client-selection" aria-label="客户端">
      {(["codex", "claude", "grok"] as const).map((id) => (
        <button
          key={id}
          data-client-switch
          disabled={disabled}
          aria-pressed={id === client}
          onClick={() => select(id)}
        >
          {clientName(id)}
        </button>
      ))}
    </nav>
  );
}
