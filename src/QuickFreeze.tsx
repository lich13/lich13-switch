import { useEffect, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { command } from "./bridge";
import type { GatewayFreeze } from "./bridge-contracts";
import { subscriptions } from "./subscriptions";
import { errorOf } from "./types";

function useGatewayFreeze(surface: "quick" | "main") {
  const [state, setState] = useState<GatewayFreeze | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [refresh, setRefresh] = useState(0);
  const [motion, setMotion] = useState<"still" | "freeze" | "thaw">("still");
  const previous = useRef<GatewayFreeze | null>(null);
  const visible = useRef(!document.hidden);
  const accept = (next: GatewayFreeze, animate: boolean) => {
    const wasReady =
      previous.current?.frozen && previous.current.draining === 0;
    const ready = next.frozen && next.draining === 0;
    const reduced = window.matchMedia?.(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    if (
      animate &&
      previous.current &&
      visible.current &&
      !reduced &&
      !!wasReady !== ready
    ) {
      setMotion(ready ? "freeze" : "thaw");
    } else if (!animate || !visible.current || reduced) {
      setMotion("still");
    }
    previous.current = next;
    setState(next);
  };
  useEffect(() => {
    if (motion === "still") return;
    const timer = setTimeout(
      () => setMotion("still"),
      motion === "freeze" ? 1500 : 480,
    );
    return () => clearTimeout(timer);
  }, [motion]);
  const version = useRef(0);
  const locked = useRef(false);
  useEffect(() => {
    let disposed = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    const ready = events.watch("gateway-freeze", (next) => {
      version.current++;
      accept(next, true);
      setError("");
    });
    const read = () => {
      if (disposed) return;
      const initial = ++version.current;
      void command("get_gateway_freeze")
        .then((next) => {
          if (!disposed && initial === version.current) accept(next, false);
        })
        .catch((e) => {
          if (!disposed && initial === version.current)
            setError(errorOf(e).message);
        });
    };
    const visibility = (shown: boolean) => {
      visible.current = shown;
      version.current++;
      setMotion("still");
      if (shown) void ready.then(read);
    };
    events.watch(
      surface === "quick" ? "panel-visibility" : "app-visibility",
      visibility,
    );
    const documentVisibility = () => visibility(!document.hidden);
    document.addEventListener("visibilitychange", documentVisibility);
    void ready.then(read);
    return () => {
      disposed = true;
      events.dispose();
      document.removeEventListener("visibilitychange", documentVisibility);
    };
  }, [refresh, surface]);
  const draining = !!state?.frozen && state.draining > 0;
  const tone = error
    ? "red"
    : !state
      ? "neutral"
      : !state.frozen
        ? "green"
        : draining
          ? "yellow"
          : "ice";
  const label = error
    ? "状态异常"
    : !state
      ? "读取状态"
      : !state.frozen
        ? "未冻结"
        : draining
          ? "等待收尾"
          : "可休眠";
  const toggle = async () => {
    if (locked.current || draining) return;
    if (!state || error) {
      setState(null);
      setError("");
      setRefresh((value) => value + 1);
      return;
    }
    locked.current = true;
    setBusy(true);
    setError("");
    const current = version.current;
    try {
      const next = await command("set_gateway_frozen", {
        frozen: !state.frozen,
      });
      if (current === version.current) accept(next, true);
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      locked.current = false;
      setBusy(false);
    }
  };
  return { state, busy, error, draining, tone, label, toggle, motion };
}

function IceLayer({
  ready,
  motion,
}: {
  ready: boolean;
  motion: "still" | "freeze" | "thaw";
}) {
  if (!ready && motion !== "thaw") return null;
  return (
    <div className={`freeze-effect freeze-effect-${motion}`} aria-hidden="true">
      {["nw", "ne", "se", "sw"].map((corner) => (
        <svg
          key={corner}
          className={`freeze-crystal freeze-crystal-${corner}`}
          viewBox="0 0 160 160"
          fill="none"
        >
          <path
            className="freeze-facet"
            d="M0 0H160L123 9 133 23 101 20 105 41 78 31 83 60 59 51 62 82 40 73 42 110 20 96 24 132 8 120 0 160Z"
          />
          <path
            className="freeze-facet freeze-facet-inner"
            d="M0 0H110L72 12 91 27 53 23 64 47 37 36 46 65 24 55 29 89 12 73 0 111Z"
          />
          <path
            className="freeze-vein"
            d="M2 2 82 80M20 20 56 18 74 10M35 35 76 33 102 21M50 50 82 55 108 48M18 18 17 58 8 81M34 34 31 78 19 102M50 50 54 85 47 110M0 9 22 9 40 0M9 0 9 22 0 40M5 46 18 33M46 5 33 18M78 76 90 68M76 78 68 90"
          />
          <path
            className="freeze-splinter"
            d="m105 6 12 4-10 4Zm-99 99 4 12 4-10Zm72-37 9 12-4-18Zm-10 10 12 9-18-4Z"
          />
        </svg>
      ))}
    </div>
  );
}

export default function QuickFreeze({
  surface = "quick",
}: {
  surface?: "quick" | "main";
}) {
  const { state, busy, error, draining, tone, label, toggle, motion } =
    useGatewayFreeze(surface);
  const stages =
    state &&
    [
      ["接收中", state.receiving],
      ["等待发送", state.waiting],
      ["等待响应", state.headers],
      ["传输中", state.streaming],
      ["收尾中", state.finishing],
      ["暂存", state.queued],
    ].filter(([, count]) => Number(count) > 0);
  return (
    <>
      <IceLayer ready={!!state?.frozen && !draining} motion={motion} />
      <section
        className={`quick-freeze quick-freeze-${tone}${surface === "main" ? " quick-freeze-compact" : ""}`}
        aria-label="全局冻结"
      >
        <div className="quick-freeze-main">
          <strong role="status">
            <span className="quick-freeze-light" aria-hidden="true" />
            {label}
          </strong>
          <button
            type="button"
            disabled={(!state && !error) || busy || draining}
            onClick={() => void toggle()}
          >
            {state?.frozen ? <Play size={16} /> : <Pause size={16} />}
            {error ? "重试" : state?.frozen ? "解冻" : "全局冻结"}
          </button>
        </div>
        {surface === "quick" && !!stages?.length && (
          <div className="quick-freeze-stages">
            {stages.map(([name, count]) => (
              <span key={name}>
                {name}
                <b>{count}</b>
              </span>
            ))}
          </div>
        )}
        {error && (
          <div className="quick-freeze-error" role="alert">
            {error}
          </div>
        )}
      </section>
    </>
  );
}
