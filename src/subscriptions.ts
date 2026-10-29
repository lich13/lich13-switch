import { subscribe } from "./bridge";
import type { EventMap } from "./bridge-contracts";

// One lifetime for registrations, deliveries and late registration results.
// Callers route failures through their existing error presentation.
export function subscriptions(onError: (error: unknown) => void) {
  let disposed = false;
  const cleaners = new Set<() => void>();
  const failed = (error: unknown) => {
    if (!disposed) onError(error);
  };
  return {
    watch<K extends keyof EventMap>(
      event: K,
      fn: (value: EventMap[K]) => void | Promise<void>,
    ) {
      return subscribe(event, (value) => {
        if (disposed) return;
        try {
          void Promise.resolve(fn(value)).catch(failed);
        } catch (error) {
          failed(error);
        }
      })
        .then((clean) => {
          if (disposed) clean();
          else cleaners.add(clean);
        })
        .catch(failed);
    },
    dispose() {
      disposed = true;
      for (const clean of cleaners) clean();
      cleaners.clear();
    },
  };
}
