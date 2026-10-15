import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  CommandName,
  CommandParams,
  CommandResult,
  EventMap,
} from "./bridge-contracts";
export const native = "__TAURI_INTERNALS__" in window;
export const preview = !native && import.meta.env.DEV;
export async function command<K extends CommandName>(
  name: K,
  ...params: CommandParams<K>
): Promise<CommandResult<K>> {
  const args = params[0] ?? {};
  if (native) return invoke<CommandResult<K>>(name, args);
  if (preview)
    return (await import("./preview")).run(name, args) as Promise<
      CommandResult<K>
    >;
  throw new Error("请在 lich13-switch 桌面客户端中打开");
}
export async function subscribe<K extends keyof EventMap>(
  event: K,
  fn: (payload: EventMap[K]) => void,
): Promise<() => void> {
  if (native) return listen<EventMap[K]>(event, (e) => fn(e.payload));
  if (preview) return (await import("./preview")).subscribe(event, fn);
  return () => {};
}
