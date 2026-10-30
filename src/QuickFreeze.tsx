import { useEffect, useRef, useState, type CSSProperties } from "react";
import { Pause, Play } from "lucide-react";
import { command } from "./bridge";
import type { GatewayFreeze } from "./bridge-contracts";
import { subscriptions } from "./subscriptions";
import { errorOf } from "./types";

const FREEZE_DURATION_MS = 1500;
const THAW_DURATION_MS = 860;
const CORNERS = ["nw", "ne", "se", "sw"] as const;

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
      motion === "freeze" ? FREEZE_DURATION_MS : THAW_DURATION_MS,
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
      <div className="freeze-pulse" />
      <div className="freeze-rime freeze-rime-top" />
      <div className="freeze-rime freeze-rime-bottom" />
      {CORNERS.map((corner, index) => (
        <svg
          key={corner}
          className={`freeze-crystal freeze-crystal-${corner}`}
          style={{ "--corner-index": index } as CSSProperties}
          viewBox="0 0 180 180"
          fill="none"
        >
          <path
            className="freeze-facet freeze-facet-deep"
            d="M0 0H176L142 11 151 28 118 24 123 49 92 38 97 71 69 60 73 96 47 85 50 126 24 110 28 149 10 136 0 176Z"
          />
          <path
            className="freeze-facet"
            d="M0 0H156L121 9 131 23 99 20 104 42 76 31 82 61 58 51 61 83 39 73 41 111 19 96 23 132 8 120 0 156Z"
          />
          <path
            className="freeze-facet freeze-facet-inner"
            d="M0 0H112L74 12 92 27 54 23 65 48 38 36 47 66 24 55 30 90 12 74 0 112Z"
          />
          <path
            className="freeze-vein"
            pathLength={100}
            d="M2 2 92 90M18 18 58 16 78 8M34 34 78 32 106 19M50 50 86 55 114 47M68 68 98 73 121 66M18 18 16 58 8 82M34 34 31 79 18 105M50 50 54 88 46 115M68 68 72 99 65 123M0 10 24 10 42 0M10 0 10 24 0 42M6 48 19 34M48 6 34 19M84 82 98 73M82 84 73 98"
          />
          <path
            className="freeze-shard freeze-shard-a"
            d="M118 7 136 12 124 20ZM96 28 114 35 101 44ZM68 58 86 67 73 76ZM32 95 45 112 29 107ZM8 118 17 137 6 129Z"
          />
          <path
            className="freeze-shard freeze-shard-b"
            d="M144 4 158 8 148 14ZM111 46 125 53 113 59ZM84 80 98 89 86 95ZM52 110 62 125 50 119ZM19 140 27 155 16 148Z"
          />
          <path
            className="freeze-glint"
            d="M26 14 30 10 34 14 30 18ZM58 26 62 21 66 26 62 31ZM26 58 31 54 36 58 31 62ZM74 52 77 48 80 52 77 56Z"
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
