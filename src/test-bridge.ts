import { vi } from "vitest";

/** What a test drives: the `command` stub and the listeners the page registered. */
export const mock = {
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
};

/**
 * Stands in for `./bridge`:
 * `vi.mock("./bridge", async () => (await import("./test-bridge")).bridge)`.
 */
export const bridge = {
  preview: false,
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
};
