import type { ComponentType } from "react";
import type { CodeMirrorSurfaceProps } from "./CodeMirrorSurface";

export type CodeMirrorSurfaceComponent = ComponentType<CodeMirrorSurfaceProps>;

let cached: CodeMirrorSurfaceComponent | null = null;
let pending: Promise<CodeMirrorSurfaceComponent> | null = null;

export function getCachedCodeMirrorSurface(): CodeMirrorSurfaceComponent | null {
  return cached;
}

export function loadCodeMirrorSurface(): Promise<CodeMirrorSurfaceComponent> {
  if (cached) return Promise.resolve(cached);
  if (!pending) {
    pending = import("./CodeMirrorSurface").then((mod) => {
      cached = mod.default;
      return cached;
    });
  }
  return pending;
}
