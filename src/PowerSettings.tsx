import { useEffect, useState } from "react";
import { command, subscribe } from "./bridge";
import { errorOf } from "./types";
export type PowerState = {
  supported: boolean;
  enabled: boolean;
  batterySleep: number;
  revision: string;
  helper: string;
  ownership?: "none" | "external" | "mixed" | "application";
  externalChanged?: boolean;
};
const labels: Record<string, string> = {
  ready: "已就绪",
  notInstalled: "未安装",
  needsRepair: "需要修复",
  isolated: "隔离运行",
  unsupported: "不支持",
};
export default function PowerSettings() {
  const [state, setState] = useState<PowerState | null>(null),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false);
  useEffect(() => {
    let disposed = false,
      clean = () => {};
    void command<PowerState>("get_clamshell_state")
      .then((s) => {
        if (!disposed) setState(s);
      })
      .catch((e) => {
        if (!disposed) setError(errorOf(e).message);
      });
    void subscribe<PowerState>("clamshell-state", (s) => {
      if (!disposed) setState(s);
    }).then((c) => (disposed ? c() : (clean = c)));
    return () => {
      disposed = true;
      clean();
    };
  }, []);
  const action = async (op: string) => {
    setBusy(true);
    setError("");
    try {
      setState(await command<PowerState>(op));
    } catch (e) {
      setError(errorOf(e).message);
      await command<PowerState>("get_clamshell_state")
        .then(setState)
        .catch(() => {});
    } finally {
      setBusy(false);
    }
  };
  if (state && !state.supported) return null;
  return (
    <fieldset
      className="startup-settings"
      disabled={busy || !state || state.helper === "isolated"}
    >
      <legend>电源助手</legend>
      <div className="power-helper-actions">
        <span role="status">
          {busy ? "正在处理…" : (labels[state?.helper ?? ""] ?? "正在读取…")}
        </span>
        <button
          type="button"
          onClick={() => void action("install_power_helper")}
        >
          {state?.helper === "notInstalled" ? "安装" : "修复"}
        </button>
        <button
          type="button"
          disabled={state?.helper === "notInstalled"}
          onClick={() => void action("remove_power_helper")}
        >
          移除
        </button>
      </div>
      {state?.externalChanged && <span role="status">外部已修改</span>}
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
    </fieldset>
  );
}
