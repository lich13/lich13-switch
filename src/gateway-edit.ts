import type { GatewayEdit } from "./bridge-contracts";
import { command } from "./bridge";
import type { ClientId, GatewayState } from "./types";

// null is an explicit retry: fetch an authoritative version before another write.
// Background events never advance an open form's original editing baseline.
export type EditRevision = string | null;

export async function saveGatewayEdit(
  clientId: ClientId,
  current: GatewayState,
  edit: GatewayEdit,
  expected: EditRevision | undefined,
  update: (state: GatewayState) => void,
): Promise<void> {
  if (expected === null) {
    current = await command("get_gateway", { clientId });
    update(current);
  }
  try {
    update(await command("update_gateway", {
      clientId,
      edit,
      expectedRevision: expected ?? current.revision,
      ...(edit.op === "select" && !current.running
        ? { expectedConfigRevision: current.configRevision }
        : {}),
    }));
  } catch (error) {
    await command("get_gateway", { clientId }).then(update).catch(() => {});
    throw error;
  }
}
