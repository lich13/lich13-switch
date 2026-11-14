import { useEffect, useState, type ComponentType } from "react";
import type { CodeMirrorSurfaceProps } from "./CodeMirrorSurface";

export type CodeMirrorSurfaceComponent = ComponentType<CodeMirrorSurfaceProps>;

let cached: CodeMirrorSurfaceComponent | null = null;
let pending: Promise<CodeMirrorSurfaceComponent> | null = null;

function loadCodeMirrorSurface(): Promise<CodeMirrorSurfaceComponent> {
  pending ??= import("./CodeMirrorSurface").then((mod) => {
    cached = mod.default;
    return cached;
  });
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
