let power = {
  helper: "notInstalled",
  supported: true,
  enabled: false,
  batterySleep: 0,
  revision: "power-preview",
};
export const powerCommands = [
  "get_startup_error",
  "get_clamshell_state",
  "install_power_helper",
  "remove_power_helper",
  "set_clamshell_awake",
  "force_quit_agent_clients",
];
export async function powerPreview(
  name: string,
  args: Record<string, unknown>,
  emit: (name: string, payload: unknown) => void,
): Promise<unknown> {
  switch (name) {
    case "get_startup_error":
      return null;
    case "get_clamshell_state":
      return { ...power };
    case "install_power_helper":
      power = { ...power, helper: "ready" };
      emit("clamshell-state", power);
      return power;
    case "remove_power_helper":
      power = { ...power, helper: "notInstalled", enabled: false };
      emit("clamshell-state", power);
      return power;
    case "set_clamshell_awake":
      power = {
        ...power,
        enabled: !!args.enabled,
        helper: "ready",
        revision: crypto.randomUUID(),
      };
      emit("clamshell-state", power);
      return power;
    case "force_quit_agent_clients":
      return { terminated: 0, failed: 0 };
  }
}
