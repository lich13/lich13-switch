import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { GatewayListener } from "./types";

import Advanced from "./GatewayAdvanced";
import { gatewayDemo } from "./gateway-preview";

function listener(runningClients: GatewayListener["runningClients"]): GatewayListener {
  return {
    port: gatewayDemo.settings.port,
    revision: "fixture-listener-revision",
    runningClients,
  };
}

it("disables the shared port while any gateway client is running", () => {
  render(
    <Advanced
      clientId="codex"
      settings={structuredClone(gatewayDemo.settings)}
      listener={listener(["claude"])}
      revision="fixture-gateway-revision"
      busy={false}
      save={vi.fn(async () => {})}
    />,
  );

  expect(screen.getByRole("spinbutton", { name: "共享本地端口" })).toBeDisabled();
});

it("allows editing the shared port when no gateway client is running", async () => {
  const user = userEvent.setup();
  render(
    <Advanced
      clientId="codex"
      settings={structuredClone(gatewayDemo.settings)}
      listener={listener([])}
      revision="fixture-gateway-revision"
      busy={false}
      save={vi.fn(async () => {})}
    />,
  );

  const port = screen.getByRole("spinbutton", { name: "共享本地端口" });
  expect(port).toBeEnabled();
  await user.clear(port);
  await user.type(port, "15723");
  expect(port).toHaveValue(15723);
});
