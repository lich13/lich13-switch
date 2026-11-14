import { useEffect, useRef, useState } from "react";
import { subscriptions } from "./subscriptions";
import { errorOf } from "./types";

/**
 * Whether the surface is on screen: the document is not hidden and the native
 * side has not reported the window or panel as hidden.
 */
export function useVisible(
  event: "app-visibility" | "panel-visibility",
  onError: (message: string) => void,
) {
  const failure = useRef(onError);
  failure.current = onError;
  const [shown, setShown] = useState(document.visibilityState !== "hidden");
  const [nativeShown, setNativeShown] = useState(true);
  useEffect(() => {
    const changed = () => setShown(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", changed);
    const events = subscriptions((e) => failure.current(errorOf(e).message));
    events.watch(event, setNativeShown);
    return () => {
      document.removeEventListener("visibilitychange", changed);
      events.dispose();
    };
  }, [event]);
  return shown && nativeShown;
}
