import { render, screen } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import type { ModelCatalog } from "./types";

const mock = vi.hoisted(() => ({ command: vi.fn() }));

vi.mock("./bridge", () => ({ command: mock.command }));

import ProviderSettings from "./ProviderSettings";
import { gatewayDemo } from "./gateway-preview";

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockResolvedValue({
    providerId: gatewayDemo.providers[0].id,
    version: "fixture-v0212-catalog",
    models: ["fixture-model"],
    checkedAt: null,
    stale: false,
    error: null,
    retryAt: null,
  } satisfies ModelCatalog);
});

it("names the provider policy editor and its mode as a whitelist", async () => {
  const provider = structuredClone(gatewayDemo.providers[0]);
  render(
    <ProviderSettings
      provider={provider}
      runtime={provider}
      clientId="codex"
      revision="fixture-v0212-revision"
      save={vi.fn().mockResolvedValue(undefined)}
      close={() => {}}
    />,
  );

  expect(
    await screen.findByRole("dialog", { name: `${provider.name} · 白名单` }),
  ).toBeInTheDocument();
  expect(screen.queryByText("供应商设置")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "白名单" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
});
