import { useEffect, useRef, useState } from "react";
import { Pause, Play } from "lucide-react";
import { command } from "./bridge";
import type { GatewayFreeze } from "./bridge-contracts";
import { subscriptions } from "./subscriptions";
import { errorOf } from "./types";

export default function QuickFreeze() {
  const [state, setState] = useState<GatewayFreeze | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [refresh, setRefresh] = useState(0);
  const version = useRef(0);
  const locked = useRef(false);
  useEffect(() => {
    let disposed = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    const ready = events.watch("gateway-freeze", (next) => {
      version.current++;
      setState(next);
      setError("");
    });
    const read = () => {
      if (disposed) return;
      const initial = ++version.current;
      void command("get_gateway_freeze")
        .then((next) => {
          if (!disposed && initial === version.current) setState(next);
        })
        .catch((e) => {
          if (!disposed && initial === version.current)
            setError(errorOf(e).message);
        });
    };
    events.watch("panel-visibility", (visible) => {
      if (visible) void ready.then(read);
    });
    void ready.then(read);
    return () => {
      disposed = true;
      events.dispose();
    };
  }, [refresh]);
  const draining = !!state?.frozen && state.draining > 0;
  const tone = !state?.frozen || error ? "red" : draining ? "yellow" : "green";
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
      if (current === version.current) setState(next);
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      locked.current = false;
      setBusy(false);
    }
  };
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
    <section
      className={`quick-freeze quick-freeze-${tone}`}
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
      {!!stages?.length && (
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
  );
}
