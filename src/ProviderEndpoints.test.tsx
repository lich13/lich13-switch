import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { GatewayState, Provider } from "./types";

const mock = vi.hoisted(() => ({ command: vi.fn() }));

vi.mock("./bridge", () => ({ command: mock.command }));

import ProviderEndpoints from "./ProviderEndpoints";
import { gatewayDemo } from "./gateway-preview";

function fixtureProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    ...structuredClone(gatewayDemo.providers[0]),
    id: "fixture-provider",
    name: "Fixture Provider",
    baseUrl: "https://primary.fixture.invalid/v1",
    extraBaseUrls: ["https://secondary.fixture.invalid/v1"],
    endpointMeasurements: undefined,
    preferredBaseUrl: undefined,
    ...overrides,
  };
}

function props(provider: Provider) {
  return {
    provider,
    runtime: provider,
    clientId: "codex" as const,
    revision: "fixture-revision",
    save: vi.fn().mockResolvedValue(undefined),
    update: vi.fn(),
    close: vi.fn(),
  };
}

beforeEach(() => {
  mock.command.mockReset();
});

it("saves an added auxiliary URL with the provider edit payload", async () => {
  const user = userEvent.setup();
  const provider = fixtureProvider({ extraBaseUrls: [] });
  const harness = props(provider);
  render(<ProviderEndpoints {...harness} />);

  await user.click(screen.getByRole("button", { name: "添加" }));
  await user.type(
    screen.getByRole("textbox", { name: "辅助 base_url 1" }),
    "https://auxiliary.fixture.invalid/v1",
  );
  await user.click(screen.getByRole("button", { name: "保存" }));

  await waitFor(() => expect(harness.close).toHaveBeenCalledOnce());
  expect(harness.save).toHaveBeenCalledExactlyOnceWith(
    {
      op: "endpointsProvider",
      id: provider.id,
      baseUrls: ["https://auxiliary.fixture.invalid/v1"],
    },
    "fixture-revision",
  );
});

it("submits a one-click swap and reflects the returned runtime endpoints", async () => {
  const user = userEvent.setup();
  const provider = fixtureProvider();
  const harness = props(provider);
  const { rerender } = render(<ProviderEndpoints {...harness} />);

  await user.click(
    screen.getByRole("button", { name: "辅助端点 1 设为主端点" }),
  );
  await waitFor(() => expect(harness.save).toHaveBeenCalledOnce());
  expect(harness.save).toHaveBeenCalledWith(
    {
      op: "swapEndpoint",
      id: provider.id,
      baseUrl: "https://secondary.fixture.invalid/v1",
    },
    "fixture-revision",
  );
  expect(await screen.findByRole("status")).toHaveTextContent("主副端点已互换");

  const swapped = fixtureProvider({
    baseUrl: "https://secondary.fixture.invalid/v1",
    extraBaseUrls: ["https://primary.fixture.invalid/v1"],
  });
  rerender(
    <ProviderEndpoints
      {...harness}
      runtime={swapped}
      revision="fixture-after-swap"
    />,
  );

  await waitFor(() =>
    expect(screen.getByText("https://secondary.fixture.invalid/v1")).toBeInTheDocument(),
  );
  expect(
    screen.getByRole("textbox", { name: "辅助 base_url 1" }),
  ).toHaveValue("https://primary.fixture.invalid/v1");
});

it("keeps the endpoint draft after a conflict and retries with a null baseline", async () => {
  const user = userEvent.setup();
  const provider = fixtureProvider();
  const save = vi
    .fn()
    .mockRejectedValueOnce({ code: "CONFLICT", message: "网关设置已变化" })
    .mockResolvedValueOnce(undefined);
  const harness = { ...props(provider), save };
  const { rerender } = render(<ProviderEndpoints {...harness} />);
  const draft = "https://draft.fixture.invalid/v1";
  const input = screen.getByRole("textbox", { name: "辅助 base_url 1" });
  await user.clear(input);
  await user.type(input, draft);
  await user.click(screen.getByRole("button", { name: "保存" }));

  expect(await screen.findByRole("alert")).toHaveTextContent("网关设置已变化");
  expect(harness.save).toHaveBeenLastCalledWith(
    {
      op: "endpointsProvider",
      id: provider.id,
      baseUrls: [draft],
    },
    "fixture-revision",
  );
  expect(input).toHaveValue(draft);
  expect(harness.close).not.toHaveBeenCalled();

  const authoritative = fixtureProvider({
    extraBaseUrls: ["https://authoritative.fixture.invalid/v1"],
  });
  rerender(
    <ProviderEndpoints
      {...harness}
      runtime={authoritative}
      revision="fixture-external-revision"
    />,
  );
  expect(screen.getByRole("textbox", { name: "辅助 base_url 1" })).toHaveValue(draft);

  await user.click(screen.getByRole("button", { name: "重试" }));
  await waitFor(() => expect(harness.close).toHaveBeenCalledOnce());
  expect(harness.save).toHaveBeenLastCalledWith(
    {
      op: "endpointsProvider",
      id: provider.id,
      baseUrls: [draft],
    },
    null,
  );
});

it("keeps unsaved endpoint input through a probe update and scopes probe arguments", async () => {
  const user = userEvent.setup();
  const provider = fixtureProvider();
  const harness = props(provider);
  let resolveProbe!: (state: GatewayState) => void;
  mock.command.mockReturnValueOnce(
    new Promise<GatewayState>((resolve) => {
      resolveProbe = resolve;
    }),
  );
  const { rerender } = render(<ProviderEndpoints {...harness} />);

  await user.click(screen.getByRole("button", { name: "测速" }));
  const draft = "https://draft-while-probing.fixture.invalid/v1";
  const input = screen.getByRole("textbox", { name: "辅助 base_url 1" });
  await user.clear(input);
  await user.type(input, draft);

  const probedProvider = fixtureProvider({
    extraBaseUrls: ["https://probe.fixture.invalid/v1"],
    endpointMeasurements: [
      {
        baseUrl: "https://primary.fixture.invalid/v1",
        latencyMs: 42,
        status: 401,
        error: null,
        checkedAt: 1,
      },
    ],
  });
  const probedState: GatewayState = {
    ...structuredClone(gatewayDemo),
    clientId: "codex",
    revision: "fixture-probed-revision",
    providers: [probedProvider],
  };
  resolveProbe(probedState);
  await waitFor(() => expect(harness.update).toHaveBeenCalledWith(probedState));

  rerender(
    <ProviderEndpoints
      {...harness}
      runtime={probedProvider}
      revision="fixture-probed-revision"
    />,
  );
  expect(screen.getByRole("textbox", { name: "辅助 base_url 1" })).toHaveValue(draft);
  expect(mock.command).toHaveBeenCalledExactlyOnceWith(
    "probe_provider_endpoints",
    { clientId: "codex", providerId: provider.id },
  );
});
