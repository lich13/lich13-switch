import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import ProviderControls from "./ProviderControls";
import { gatewayDemo } from "./gateway-preview";

const provider = gatewayDemo.providers[0];
it("validates caps, preserves edits through runtime refresh and retries conflicts explicitly", async () => {
  const user = userEvent.setup();
  const commit = vi.fn().mockRejectedValueOnce(new Error("设置已变化"));
  const report = vi.fn();
  const { rerender } = render(
    <ProviderControls
      provider={provider}
      revision="old"
      disabled={false}
      commit={commit}
      report={report}
    />,
  );
  await user.click(screen.getByRole("button", { name: /并发上限/ }));
  const input = screen.getByRole("spinbutton");
  for (const value of ["", "-1", "1.5", "100001"]) {
    fireEvent.change(input, { target: { value } });
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(screen.getByRole("alert")).toHaveTextContent("0–100000");
    expect(commit).not.toHaveBeenCalled();
  }
  fireEvent.change(input, { target: { value: "8" } });
  rerender(
    <ProviderControls
      provider={{ ...provider, activeRequests: 3, maxConcurrency: 6 }}
      revision="new"
      disabled={false}
      commit={commit}
      report={report}
    />,
  );
  expect(input).toHaveValue(8);
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(commit).toHaveBeenLastCalledWith(
    { op: "concurrencyProvider", id: provider.id, maxConcurrency: 8 },
    "old",
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("设置已变化");
  expect(input).toHaveValue(8);
  commit.mockResolvedValueOnce(undefined);
  fireEvent.change(input, { target: { value: "0" } });
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(commit).toHaveBeenLastCalledWith(
    { op: "concurrencyProvider", id: provider.id, maxConcurrency: 0 },
    null,
  );
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
  expect(screen.getByRole("button", { name: /并发上限/ })).toHaveFocus();
  expect(report).not.toHaveBeenCalled();
});

it("keeps authoritative queue state on failure and prevents duplicate submissions", async () => {
  let reject!: (e: Error) => void;
  const commit = vi.fn(
    () =>
      new Promise<void>((_, fail) => {
        reject = fail;
      }),
  );
  const report = vi.fn(),
    user = userEvent.setup();
  render(
    <ProviderControls
      provider={provider}
      revision="r1"
      disabled={false}
      commit={commit}
      report={report}
    />,
  );
  const toggle = screen.getByRole("button", { name: /移出队列/ });
  await user.dblClick(toggle);
  expect(commit).toHaveBeenCalledTimes(1);
  expect(commit).toHaveBeenCalledWith(
    { op: "queueProvider", id: provider.id, queued: false },
    "r1",
  );
  expect(toggle).toBeDisabled();
  reject(new Error("写入失败"));
  await waitFor(() => expect(report).toHaveBeenCalledWith("写入失败"));
  expect(toggle).toHaveAttribute("aria-pressed", "true");
  expect(toggle).toBeEnabled();
});

it("places takeover controls first and commits one actual field per click", async () => {
  const user = userEvent.setup();
  const commit = vi.fn().mockResolvedValue(undefined);
  const report = vi.fn();
  const { container, rerender } = render(
    <ProviderControls
      provider={{
        ...provider,
        handoffAfterCompaction: true,
        takeNewThreads: false,
      }}
      clientId="codex"
      revision="takeover-revision"
      disabled={false}
      commit={commit}
      report={report}
    />,
  );

  const handoff = screen.getByRole("button", {
    name: `${provider.name} 压缩后接管`,
  });
  const newThreads = screen.getByRole("button", {
    name: `${provider.name} 新线程接管`,
  });
  expect(handoff).toHaveAttribute("aria-pressed", "true");
  expect(newThreads).toHaveAttribute("aria-pressed", "false");
  expect(handoff).toHaveTextContent("压缩后");
  expect(newThreads).toHaveTextContent("新线程");
  const controls = container.querySelector(".provider-controls");
  expect(controls).not.toBeNull();
  expect(
    within(controls as HTMLElement)
      .getAllByRole("button")
      .slice(1, 3),
  ).toEqual([handoff, newThreads]);

  await user.click(handoff);
  expect(commit).toHaveBeenLastCalledWith(
    {
      op: "takeoverProvider",
      id: provider.id,
      handoffAfterCompaction: false,
    },
    "takeover-revision",
  );
  expect(commit.mock.calls.at(-1)?.[0]).not.toHaveProperty("takeNewThreads");
  expect(commit.mock.calls.at(-1)?.[0]).not.toHaveProperty("allowedModels");
  expect(commit.mock.calls.at(-1)?.[0]).not.toHaveProperty("supportsWebsocket");

  rerender(
    <ProviderControls
      provider={{
        ...provider,
        handoffAfterCompaction: false,
        takeNewThreads: false,
      }}
      clientId="codex"
      revision="takeover-revision-2"
      disabled={false}
      commit={commit}
      report={report}
    />,
  );
  await user.click(
    screen.getByRole("button", { name: `${provider.name} 新线程接管` }),
  );
  expect(commit).toHaveBeenLastCalledWith(
    { op: "takeoverProvider", id: provider.id, takeNewThreads: true },
    "takeover-revision-2",
  );
  expect(commit.mock.calls.at(-1)?.[0]).not.toHaveProperty(
    "handoffAfterCompaction",
  );
});

it("keeps the authoritative takeover state on failure and blocks duplicate clicks", async () => {
  let reject!: (error: Error) => void;
  const commit = vi.fn(
    () =>
      new Promise<void>((_, fail) => {
        reject = fail;
      }),
  );
  const report = vi.fn();
  const user = userEvent.setup();
  render(
    <ProviderControls
      provider={{ ...provider, handoffAfterCompaction: true }}
      clientId="codex"
      revision="takeover-failure"
      disabled={false}
      commit={commit}
      report={report}
    />,
  );
  const handoff = screen.getByRole("button", {
    name: `${provider.name} 压缩后接管`,
  });
  await user.dblClick(handoff);
  expect(commit).toHaveBeenCalledTimes(1);
  expect(commit).toHaveBeenCalledWith(
    {
      op: "takeoverProvider",
      id: provider.id,
      handoffAfterCompaction: false,
    },
    "takeover-failure",
  );
  expect(handoff).toBeDisabled();
  reject(new Error("接管设置失败"));
  await waitFor(() => expect(report).toHaveBeenCalledWith("接管设置失败"));
  expect(handoff).toHaveAttribute("aria-pressed", "true");
  expect(handoff).toBeEnabled();
});

it("hides takeover controls for Claude", () => {
  render(
    <ProviderControls
      provider={provider}
      clientId="claude"
      revision="claude-revision"
      disabled={false}
      commit={vi.fn()}
      report={vi.fn()}
    />,
  );
  expect(
    screen.queryByRole("button", { name: `${provider.name} 压缩后接管` }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: `${provider.name} 新线程接管` }),
  ).not.toBeInTheDocument();
});

it("edits the provider RPM cap in the shared control", async () => {
  const user = userEvent.setup();
  const commit = vi.fn().mockResolvedValue(undefined);
  render(
    <ProviderControls
      provider={{ ...provider, maxRpm: 60, rpmUsed: 12 }}
      revision="rpm-revision"
      disabled={false}
      commit={commit}
      report={vi.fn()}
    />,
  );
  expect(screen.getByRole("button", { name: /RPM 上限/ })).toHaveTextContent(
    "RPM 12/60",
  );
  await user.click(screen.getByRole("button", { name: /RPM 上限/ }));
  const input = screen.getByRole("spinbutton");
  await user.clear(input);
  await user.type(input, "120");
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(commit).toHaveBeenCalledWith(
    { op: "rpmProvider", id: provider.id, maxRpm: 120 },
    "rpm-revision",
  );
});

it("Escape closes only the editor, returns focus and hidden panels discard drafts", async () => {
  const commit = vi.fn(),
    report = vi.fn(),
    user = userEvent.setup();
  const props = { provider, revision: "r", disabled: false, commit, report };
  const { rerender } = render(<ProviderControls {...props} />);
  const anchor = screen.getByRole("button", { name: /并发上限/ });
  await user.click(anchor);
  await user.keyboard("{Escape}");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(anchor).toHaveFocus();
  await user.click(anchor);
  fireEvent.change(screen.getByRole("spinbutton"), { target: { value: "17" } });
  rerender(<ProviderControls {...props} visible={false} />);
  rerender(<ProviderControls {...props} visible />);
  await user.click(anchor);
  expect(screen.getByRole("spinbutton")).toHaveValue(provider.maxConcurrency);
  expect(commit).not.toHaveBeenCalled();
});

it("switches a Codex provider to search only and hides generation intake controls", async () => {
  const user = userEvent.setup();
  const commit = vi.fn().mockResolvedValue(undefined);
  const report = vi.fn();
  const props = {
    revision: "fixture-role",
    disabled: false,
    commit,
    report,
    clientId: "codex" as const,
  };
  const { rerender } = render(
    <ProviderControls {...props} provider={provider} />,
  );
  await user.click(
    screen.getByRole("button", { name: `${provider.name} 仅网络搜索` }),
  );
  expect(commit).toHaveBeenCalledWith(
    { op: "searchOnlyProvider", id: provider.id, searchOnly: true },
    "fixture-role",
  );
  rerender(
    <ProviderControls
      {...props}
      provider={{ ...provider, searchOnly: true }}
    />,
  );
  expect(
    screen.getByRole("button", { name: `${provider.name} 仅网络搜索` }),
  ).toHaveAttribute("aria-pressed", "true");
  expect(
    screen.queryByRole("button", { name: /压缩后接管/ }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: /新线程接管/ }),
  ).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: /并发上限/ })).toBeInTheDocument();
});
