import { beforeEach, describe, expect, it, vi } from "vitest";
import type { EventMap } from "./bridge-contracts";

const subscribeMock = vi.hoisted(() => vi.fn());
vi.mock("./bridge", () => ({ subscribe: subscribeMock }));

import { subscriptions } from "./subscriptions";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

async function nextTurn() {
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
}

describe("subscriptions lifetime", () => {
  beforeEach(() => {
    subscribeMock.mockReset();
  });

  it("routes a rejected registration through the existing error handler", async () => {
    const error = new Error("fixture registration failure");
    const registration = deferred<() => void>();
    const errors: unknown[] = [];
    const onError = (value: unknown) => errors.push(value);
    subscribeMock.mockReturnValue(registration.promise);

    subscriptions(onError).watch("switch-notice", vi.fn());
    registration.reject(error);

    await nextTurn();
    expect(subscribeMock).toHaveBeenCalledOnce();
    expect(errors).toEqual([error]);
  });

  it("unlistens when registration finishes after disposal", async () => {
    const registration = deferred<() => void>();
    const unlisten = vi.fn();
    const onError = vi.fn();
    subscribeMock.mockReturnValue(registration.promise);

    const lifetime = subscriptions(onError);
    lifetime.watch("switch-notice", vi.fn());
    lifetime.dispose();
    registration.resolve(unlisten);

    await vi.waitFor(() => expect(unlisten).toHaveBeenCalledOnce());
    expect(onError).not.toHaveBeenCalled();
  });

  it("ignores late events and async failures after disposal", async () => {
    let deliver: ((payload: string) => void) | undefined;
    const pending = deferred<void>();
    const onError = vi.fn();
    const handler = vi.fn(() => pending.promise);
    const unlisten = vi.fn();
    subscribeMock.mockImplementation((_event: keyof EventMap, callback) => {
      deliver = callback;
      return Promise.resolve(unlisten);
    });

    const lifetime = subscriptions(onError);
    lifetime.watch("switch-notice", handler);
    await nextTurn();

    deliver?.("before-disposal");
    lifetime.dispose();
    deliver?.("after-disposal");
    pending.reject(new Error("fixture async failure"));
    await nextTurn();

    expect(handler).toHaveBeenCalledTimes(1);
    expect(unlisten).toHaveBeenCalledOnce();
    expect(onError).not.toHaveBeenCalled();
  });

  it("cleans up every registered subscription", async () => {
    const unlisteners = [vi.fn(), vi.fn(), vi.fn()];
    let registration = 0;
    subscribeMock.mockImplementation(() =>
      Promise.resolve(unlisteners[registration++]),
    );
    const lifetime = subscriptions(vi.fn());

    lifetime.watch("switch-notice", vi.fn());
    lifetime.watch("switch-error", vi.fn());
    lifetime.watch("login-state", vi.fn());
    await nextTurn();
    lifetime.dispose();

    for (const unlisten of unlisteners) {
      expect(unlisten).toHaveBeenCalledOnce();
    }
  });

  it("catches synchronous throws and asynchronous callback rejections", async () => {
    const listeners: Array<(payload: unknown) => void> = [];
    const syncError = new Error("fixture sync failure");
    const asyncError = new Error("fixture async failure");
    const onError = vi.fn();
    subscribeMock.mockImplementation(
      (_event: keyof EventMap, callback: (payload: unknown) => void) => {
        listeners.push(callback);
        return Promise.resolve(vi.fn());
      },
    );

    const lifetime = subscriptions(onError);
    lifetime.watch("switch-notice", () => {
      throw syncError;
    });
    lifetime.watch("switch-notice", async () => {
      throw asyncError;
    });
    listeners[0]("fixture");
    listeners[1]("fixture");

    await vi.waitFor(() => expect(onError).toHaveBeenCalledTimes(2));
    expect(onError.mock.calls).toEqual([[syncError], [asyncError]]);
    lifetime.dispose();
  });
});
