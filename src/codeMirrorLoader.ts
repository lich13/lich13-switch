import { useEffect, useState, type ComponentType } from "react";
import type { CodeMirrorSurfaceProps } from "./CodeMirrorSurface";

export type CodeMirrorSurfaceComponent = ComponentType<CodeMirrorSurfaceProps>;
type Loader = () => Promise<{ default: CodeMirrorSurfaceComponent }>;

const defaultLoader: Loader = () => import("./CodeMirrorSurface");
let loader: Loader = defaultLoader;
let cached: CodeMirrorSurfaceComponent | null = null;
let pending: Promise<CodeMirrorSurfaceComponent> | null = null;

export function getCachedCodeMirrorSurface(): CodeMirrorSurfaceComponent | null {
  return cached;
}

export function loadCodeMirrorSurface(): Promise<CodeMirrorSurfaceComponent> {
  if (cached) return Promise.resolve(cached);
  if (!pending) {
    pending = loader()
      .then((mod) => {
        cached = mod.default;
        return cached;
      })
      .catch((error) => {
        pending = null;
        throw error;
      });
  }
  return pending;
}

export function useCodeMirrorSurface(
  enabled: boolean,
  onError?: (error: unknown) => void,
): CodeMirrorSurfaceComponent | null {
  const [Surface, setSurface] = useState<CodeMirrorSurfaceComponent | null>(
    () => (enabled ? cached : null),
  );
  useEffect(() => {
    if (!enabled) return;
    if (cached) {
      if (Surface !== cached) setSurface(() => cached);
      return;
    }
    let active = true;
    void loadCodeMirrorSurface()
      .then((Loaded) => {
        if (active) setSurface(() => Loaded);
      })
      .catch((error) => {
        if (active) onError?.(error);
      });
    return () => {
      active = false;
    };
  }, [enabled, Surface, onError]);
  return enabled ? Surface ?? cached : Surface;
}

export function resetCodeMirrorLoaderForTests(custom?: Loader): void {
  cached = null;
  pending = null;
  loader = custom ?? defaultLoader;
}
