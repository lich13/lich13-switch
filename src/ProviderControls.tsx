import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, Gauge, LoaderCircle, Plus } from "lucide-react";
import { errorOf, type Provider, type ClientId } from "./types";
import type { ProviderCommit } from "./SortableProviders";

export default function ProviderControls({
  provider,
  clientId = "codex",
  revision,
  disabled,
  visible = true,
  commit,
  report,
}: {
  provider: Provider;
  clientId?: ClientId;
  revision: string;
  disabled: boolean;
  visible?: boolean;
  commit: ProviderCommit;
  report: (message: string) => void;
}) {
  const [draft, setDraft] = useState<{
    kind: "concurrency" | "rpm";
    value: string;
    revision: string | null;
  } | null>(null);
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const pending = useRef(false);
  const focusOnClose = useRef(false);
  const anchor = useRef<HTMLButtonElement>(null),
    popover = useRef<HTMLDivElement>(null),
    input = useRef<HTMLInputElement>(null);
  const [position, setPosition] = useState({ top: 0, left: 0 });
  const close = (focus = true) => {
    focusOnClose.current = focus;
    setDraft(null);
    setError("");
  };
  useEffect(() => {
    if (!draft && !saving && !disabled && focusOnClose.current) {
      focusOnClose.current = false;
      anchor.current?.focus({ preventScroll: true });
    }
  }, [draft, saving, disabled]);
  useEffect(() => {
    if (!visible) close(false);
  }, [visible]);
  useEffect(() => {
    if (!draft) return;
    input.current?.focus();
    input.current?.select();
    const outside = (event: PointerEvent) => {
      if (
        (event.target as Element)?.closest?.(
          "[data-client-switch], [data-confirmation]",
        )
      )
        return;
      if (
        !pending.current &&
        !popover.current?.contains(event.target as Node) &&
        !anchor.current?.contains(event.target as Node)
      )
        close(false);
    };
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [Boolean(draft)]);
  useLayoutEffect(() => {
    if (!draft) return;
    const reposition = () => {
      const a = anchor.current?.getBoundingClientRect(),
        p = popover.current?.getBoundingClientRect();
      if (!a || !p) return;
      setPosition({
        left: Math.max(
          8,
          Math.min(a.right - p.width, window.innerWidth - p.width - 8),
        ),
        top: Math.max(
          8,
          a.bottom + p.height + 14 <= window.innerHeight
            ? a.bottom + 6
            : a.top - p.height - 6,
        ),
      });
    };
    reposition();
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);
    const observer = new ResizeObserver(reposition);
    if (popover.current) observer.observe(popover.current);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", reposition);
      window.removeEventListener("scroll", reposition, true);
    };
  }, [Boolean(draft)]);
  const update = async (queue: boolean) => {
    if (pending.current || disabled) return;
    if (
      !queue &&
      (!draft || !/^\d+$/.test(draft.value) || Number(draft.value) > 100000)
    ) {
      setError("请输入 0–100000 的整数");
      return;
    }
    pending.current = true;
    setSaving(true);
    setError("");
    try {
      await commit(
        queue
          ? { op: "queueProvider", id: provider.id, queued: !provider.queued }
          : draft!.kind === "rpm"
            ? { op: "rpmProvider", id: provider.id, maxRpm: Number(draft!.value) }
            : {
                op: "concurrencyProvider",
                id: provider.id,
                maxConcurrency: Number(draft!.value),
              },
        queue ? revision : draft!.revision,
      );
      if (!queue) close();
    } catch (e) {
      if (queue) report(errorOf(e).message);
      else setError(errorOf(e).message);
      // The parent refreshes authoritative state on failure. Retain the typed value;
      // only an explicit subsequent save may retry against that current revision.
      if (!queue) setDraft((d) => (d ? { ...d, revision: null } : null));
    } finally {
      pending.current = false;
      setSaving(false);
    }
  };
  const togglePolicy = async (field: "handoffAfterCompaction" | "takeNewThreads") => {
    if (pending.current || disabled) return;
    pending.current = true;
    setSaving(true);
    try {
      await commit({ op: "takeoverProvider", id: provider.id, [field]: !provider[field] }, revision);
    } catch (e) {
      report(errorOf(e).message);
    } finally {
      pending.current = false;
      setSaving(false);
    }
  };
  return (
    <div className="provider-controls">
      {clientId === "codex" && <>
        <button type="button" className="provider-takeover" aria-label={`${provider.name} 压缩后接管`}
          title="压缩后接管" aria-pressed={Boolean(provider.handoffAfterCompaction)} disabled={disabled || saving}
          onClick={() => void togglePolicy("handoffAfterCompaction")}><span className="policy-dot" />压缩后</button>
        <button type="button" className="provider-takeover" aria-label={`${provider.name} 新线程接管`}
          title="新线程接管" aria-pressed={Boolean(provider.takeNewThreads)} disabled={disabled || saving}
          onClick={() => void togglePolicy("takeNewThreads")}><span className="policy-dot" />新线程</button>
      </>}
      <button
        ref={anchor}
        type="button"
        className="provider-concurrency"
        disabled={disabled || saving}
        aria-label={`${provider.name} 并发上限`}
        aria-expanded={draft?.kind === "concurrency"}
        aria-haspopup="dialog"
        onClick={() => {
          setError("");
          setDraft(
            draft
              ? null
              : { kind: "concurrency", value: String(provider.maxConcurrency), revision },
          );
        }}
      >
        并发 {provider.activeRequests}/{provider.maxConcurrency || "∞"}
      </button>
      <button
        type="button"
        className="provider-concurrency provider-rpm"
        disabled={disabled || saving}
        aria-label={`${provider.name} RPM 上限`}
        aria-expanded={draft?.kind === "rpm"}
        aria-haspopup="dialog"
        onClick={() => {
          setError("");
          setDraft(
            draft
              ? null
              : { kind: "rpm", value: String(provider.maxRpm), revision },
          );
        }}
      >
        <Gauge size={12} /> RPM {provider.rpmUsed}/{provider.maxRpm || "∞"}
      </button>
      <button
        type="button"
        className="provider-queue"
        disabled={disabled || saving}
        aria-label={`${provider.name} ${provider.queued ? "移出队列" : "加入队列"}`}
        aria-pressed={provider.queued}
        onClick={() => void update(true)}
      >
        {saving && !draft ? (
          <LoaderCircle size={12} className="spin" />
        ) : provider.queued ? (
          <Check size={12} />
        ) : (
          <Plus size={12} />
        )}
        {provider.queued ? "已入队" : "加入队列"}
      </button>
      {draft &&
        createPortal(
          <div
            ref={popover}
            role="dialog"
            data-provider-draft
            aria-label={`${provider.name} ${draft.kind === "rpm" ? "RPM 上限" : "并发上限"}`}
            className="concurrency-popover"
            style={position}
            onKeyDown={(event) => {
              if (event.key === "Escape" && !event.nativeEvent.isComposing) {
                event.preventDefault();
                event.stopPropagation();
                if (!saving) close();
              }
              if (event.key === "Tab") {
                const buttons = [
                  ...(popover.current?.querySelectorAll<HTMLElement>(
                    "input:not(:disabled), button:not(:disabled)",
                  ) ?? []),
                ];
                const first = buttons[0],
                  last = buttons[buttons.length - 1];
                if (event.shiftKey && document.activeElement === first) {
                  event.preventDefault();
                  last?.focus();
                } else if (!event.shiftKey && document.activeElement === last) {
                  event.preventDefault();
                  first?.focus();
                }
              }
            }}
          >
            <form
              noValidate
              onSubmit={(event) => {
                event.preventDefault();
                void update(false);
              }}
            >
              <label>
                上限（0 不限）
                <input
                  ref={input}
                  type="number"
                  min={0}
                  max={100000}
                  step={1}
                  value={draft.value}
                  disabled={saving}
                  onChange={(event) =>
                    setDraft({ ...draft, value: event.target.value })
                  }
                />
              </label>
              {error && (
                <div className="form-error" role="alert">
                  {error}
                </div>
              )}
              <div className="concurrency-actions">
                <button
                  type="button"
                  className="secondary"
                  disabled={saving}
                  onClick={() => close()}
                >
                  取消
                </button>
                <button type="submit" className="primary" disabled={saving}>
                  {saving ? "保存中…" : "保存"}
                </button>
              </div>
            </form>
          </div>,
          document.body,
        )}
    </div>
  );
}
