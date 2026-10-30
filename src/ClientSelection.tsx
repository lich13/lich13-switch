import { flushTestPrompt } from "./provider-test-prompt";
import { useCallback, useState } from "react";
import type { ClientId, ConfigClientId } from "./types";
import { confirmAction } from "./confirmation";

export const clients: ClientId[] = ["codex", "claude", "grok", "dsh"];
export const clientName = (client: ConfigClientId) =>
  ({
    codex: "Codex",
    claude: "Claude Code",
    grok: "Grok Build",
    dsh: "DSH",
    antigravity: "Antigravity app",
  })[client];
export function useClientSelection(
  surface: "config",
): readonly [
  ConfigClientId,
  (next: ConfigClientId, dirty?: boolean) => Promise<boolean>,
];
export function useClientSelection(
  surface: "main" | "quick",
): readonly [
  ClientId,
  (next: ConfigClientId, dirty?: boolean) => Promise<boolean>,
];
export function useClientSelection(surface: "main" | "quick" | "config") {
  const key = `lich13-switch.${surface}.client`;
  const [client, setClient] = useState<ConfigClientId>(() => {
    try {
      const stored = localStorage.getItem(key);
      return stored === "claude" ||
        stored === "grok" ||
        stored === "dsh" ||
        (surface === "config" && stored === "antigravity")
        ? stored
        : "codex";
    } catch {
      return "codex";
    }
  });
  const select = useCallback(
    async (next: ConfigClientId, dirty = false) => {
      if (next === client) return true;
      if (!(await flushTestPrompt())) return false;
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
export default function ClientSelection<C extends ConfigClientId = ClientId>({
  client,
  select,
  disabled = false,
  includeDsh = true,
  visibleClients = clients as unknown as readonly C[],
}: {
  client: C;
  select: (id: C) => void;
  disabled?: boolean;
  includeDsh?: boolean;
  visibleClients?: readonly C[];
}) {
  return (
    <nav className="segmented client-selection" aria-label="客户端">
      {visibleClients
        .filter((id) => includeDsh || id !== "dsh")
        .map((id) => (
          <button
            key={id}
            type="button"
            data-client={id}
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
