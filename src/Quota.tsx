import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import Modal from "./Modal";
import { command, subscribe } from "./bridge";
import { compact as compactNumber } from "./usage-types";
import {
  errorOf,
  type ClientId,
  type Provider,
  type ProviderQuota,
  type QuotaPlan,
} from "./types";

export function useProviderQuota(
  providers: Provider[],
  active: boolean,
  visibilityEvent = "app-visibility",
  clientId: ClientId = "codex",
  refreshSeconds = 60,
) {
  const latest = useRef(providers);
  latest.current = providers;
  const [values, setValues] = useState<Record<string, ProviderQuota>>({});
  const [nativeVisible, setNativeVisible] = useState(true);
  const [visible, setVisible] = useState(document.visibilityState !== "hidden");
  const flights = useRef(new Set<string>());
  const snapshots = useRef<Record<string, ProviderQuota>>({});
  const mounted = useRef(true);
  const accept = useCallback((q: ProviderQuota) => {
    if (
      !mounted.current ||
      !q ||
      !latest.current.some(
        (p) => p.id === q.providerId && p.quotaVersion === q.version,
      )
    )
      return;
    snapshots.current = {...snapshots.current, [q.providerId]: q};
    setValues((v) => ({ ...v, [q.providerId]: q }));
  }, []);
  useEffect(() => {
    mounted.current = true;
    const clean: (() => void)[] = [];
    let disposed = false;
    for (const promise of [
      subscribe<{ clientId: ClientId; quota: ProviderQuota }>(
        "provider-quota",
        (e) => {
          if (e.clientId === clientId) accept(e.quota);
        },
      ),
      subscribe<boolean>(visibilityEvent, setNativeVisible),
    ]) {
      void promise.then((fn) => (disposed ? fn() : clean.push(fn)));
    }
    const onVisibility = () =>
      setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      disposed = true;
      mounted.current = false;
      clean.forEach((fn) => fn());
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [accept, visibilityEvent, clientId]);
  const refresh = useCallback(
    async (id: string, force = true) => {
      const provider = latest.current.find((p) => p.id === id);
      if (!provider) return;
      const flight = id + provider.quotaVersion;
      if (flights.current.has(flight)) return;
      flights.current.add(flight);
      try {
        accept(
          await command<ProviderQuota>("query_provider_quota", {
            clientId,
            providerId: id,
            force,
          }),
        );
      } catch (e) {
        const err = errorOf(e);
        if (
          err.code !== "STALE" &&
          mounted.current &&
          latest.current.some(
            (p) => p.id === id && p.quotaVersion === provider.quotaVersion,
          )
        ) {
          setValues((v) => ({
            ...v,
            [id]: {
              ...(v[id] ?? provider.quota ?? emptyQuota(provider)),
              state: "error",
              stale: Boolean((v[id] ?? provider.quota)?.successAt),
              error: err.message,
              nextRefreshAt: refreshSeconds > 0 ? Date.now() / 1000 + refreshSeconds : null,
            },
          }));
        }
      } finally {
        flights.current.delete(flight);
      }
    },
    [accept, clientId, refreshSeconds],
  );
  const refreshAll = useCallback(
    (force = true) => {
      for (const p of latest.current) void refresh(p.id, force);
    },
    [refresh],
  );
  const identity = providers.map((p) => p.id + p.quotaVersion).join("|");
  const quotaFor = (p: Provider) => {
    const q = values[p.id];
    return q?.version === p.quotaVersion ? q : (p.quota ?? undefined);
  };
  useEffect(() => {
    if (!active || !nativeVisible || !visible || !identity || refreshSeconds === 0) return;
    let cancelled = false;
    let timer: number;
    const tick = async () => {
      const now = Date.now() / 1000;
      let next = now + refreshSeconds;
      const due: Promise<void>[] = [];
      for (const p of latest.current) {
        const cached = snapshots.current[p.id];
        const q = cached?.version === p.quotaVersion ? cached : p.quota;
        const at = Math.max(q?.nextRefreshAt ?? (q?.checkedAt ? q.checkedAt + refreshSeconds : 0), q?.retryAt ?? 0);
        if (at <= now) due.push(refresh(p.id, false));
        else next = Math.min(next, at);
      }
      await Promise.all(due);
      if (!cancelled) {
        next = Date.now()/1000 + refreshSeconds;
        for (const p of latest.current) {
          const cached=snapshots.current[p.id];
          const q=cached?.version===p.quotaVersion ? cached : p.quota;
          if(q?.nextRefreshAt) next=Math.min(next, Math.max(q.nextRefreshAt,q.retryAt??0));
        }
        timer=window.setTimeout(tick,Math.max(1000,(next-Date.now()/1000)*1000));
      }
    };
    void tick();
    return () => { cancelled = true; window.clearTimeout(timer); };
  }, [active, nativeVisible, visible, identity, refreshSeconds, refresh]);
  return { quotaFor, refresh, refreshAll };
}
function emptyQuota(p: Provider): ProviderQuota {
  return {
    providerId: p.id,
    version: p.quotaVersion,
    state: "idle",
    source: null,
    checkedAt: null,
    successAt: null,
    retryAt: null,
    stale: false,
    error: null,
    keyStatus: null,
    plans: [],
    expiresAt: null,
    expiresAtUnix: null,
    today: null,
    totalUsage: null,
  };
}
const number = (n: number) =>
  n.toLocaleString(undefined, { maximumFractionDigits: 3 });
const amount = (p: QuotaPlan) =>
  p.unlimited
    ? "无限制"
    : p.remaining == null
      ? "未返回剩余额度"
      : `${number(p.remaining)} ${p.unit}`;
const date = (value: string | number) => {
  const d = new Date(value);
  return Number.isNaN(d.valueOf()) ? "" : d.toLocaleString();
};
const shortDate = (value: string | number) => {
  const d = new Date(value);
  return Number.isNaN(d.valueOf()) ? "" : d.toLocaleDateString();
};
const samePlan = (a: QuotaPlan, b: QuotaPlan) =>
  a.name === b.name && a.unit === b.unit && a.remaining === b.remaining &&
  a.used === b.used && a.total === b.total && a.unlimited === b.unlimited &&
  a.resetAt === b.resetAt;

function QuotaPlans({ plans, label = "周期" }: { plans: QuotaPlan[]; label?: string }) {
  if (!plans.length) return null;
  const unit = plans.every((p) => p.unit === plans[0].unit) ? plans[0].unit : null;
  const value = (n: number | null, p: QuotaPlan) =>
    n == null ? "—" : `${number(n)}${unit ? "" : ` ${p.unit}`}`;
  return <table className="quota-plan-table">
    <thead><tr><th scope="col">{label}{unit && <span className="quota-unit">{unit}</span>}</th><th scope="col">剩余</th><th scope="col">已用 / 上限</th></tr></thead>
    <tbody>{plans.map((p, i) => <tr key={i}>
      <th scope="row"><span>{p.name}</span>{p.resetAt && <time title={date(p.resetAt) || p.resetAt}>重置 {shortDate(p.resetAt) || p.resetAt}</time>}</th>
      <td>{p.unlimited ? "无限制" : value(p.remaining, p)}</td>
      <td>{value(p.used, p)}<span className="quota-separator"> / </span>{p.unlimited ? "∞" : value(p.total, p)}</td>
    </tr>)}</tbody>
  </table>;
}
export function QuotaInfo({
  provider,
  quota,
  refresh,
  compact = false,
  details,
}: {
  provider: Provider;
  quota?: ProviderQuota;
  refresh: () => void;
  compact?: boolean;
  details?: () => void;
}) {
  const [showDetails, setShowDetails] = useState(false);
  const loading = quota?.state === "loading";
  const stale =
    quota?.stale ||
    Boolean(quota?.successAt && quota.nextRefreshAt && Date.now() / 1000 >= quota.nextRefreshAt);
  const headline = quota?.plans[0]
    ? `${quota.plans[0].unlimited ? "" : "剩余 "}${amount(quota.plans[0])}`
    : (quota?.keyStatus ??
      (quota?.state === "unsupported"
        ? "不可查询"
        : quota?.state === "error"
          ? "查询失败"
          : loading
            ? "查询中…"
            : "未查询"));
  const source =
    quota?.source === "sub2api"
      ? "Sub2API"
      : quota?.source === "newapi"
        ? "New API"
        : "";
  const expiry = quota?.expiresAtUnix
    ? date(quota.expiresAtUnix * 1000)
    : quota?.expiresAt
      ? date(quota.expiresAt)
      : "";
  const subscriptions = quota?.subscriptions ?? [];
  const activeSubscriptions = subscriptions.filter((s) => s.active === true);
  const currentSubscription = activeSubscriptions.length === 1 ? activeSubscriptions[0] : subscriptions.length === 1 ? subscriptions[0] : null;
  const subscriptionLabel = currentSubscription?.name || (subscriptions.length > 1 ? `${subscriptions.length} 个套餐` : null);
  const repeatedSubscriptionSummary = quota?.source === "sub2api" && subscriptions.length === 1 &&
    subscriptions[0].plans.length > 0 && quota.plans[0]?.name === (subscriptions[0].name ?? "订阅");
  const standalonePlans = (quota?.plans ?? []).filter((plan, index) => {
    // Sub2API's single-subscription summary combines the most restrictive
    // remaining amount with a different window's usage. It belongs only in the
    // headline, never as a second, misleading cycle row.
    if (!repeatedSubscriptionSummary) return true;
    return index !== 0 && !subscriptions[0].plans.some((p) => samePlan(p, plan));
  });
  if (compact)
    return (
      <>
      <div className="quota-compact" aria-label={`${provider.name} 额度`}>
        {subscriptionLabel && <span className="quota-subscription" title={subscriptionLabel}>{subscriptionLabel}</span>}
          <button className="quota-value" title={headline} onClick={details ?? (() => setShowDetails(true))}>
            {headline}
          </button>
        {stale && <span className="quota-warning">已过期</span>}
        {quota?.error &&
          quota.state !== "unsupported" &&
          quota.plans.length > 0 && (
            <button className="quota-warning text-button" onClick={details ?? (() => setShowDetails(true))}>
              查询失败
            </button>
          )}
        <button
          className="icon-button"
          disabled={
            loading ||
            Boolean(quota?.retryAt && quota.retryAt > Date.now() / 1000)
          }
          onClick={refresh}
          aria-label={`刷新 ${provider.name} 额度`}
        >
          <RefreshCw size={13} className={loading ? "quota-spin" : ""} />
        </button>
      </div>
      {showDetails && <Modal compact title={`${provider.name} 额度`} close={() => setShowDetails(false)}>
        <QuotaInfo provider={provider} quota={quota} refresh={refresh} />
      </Modal>}
      </>
    );
  return (
    <div className="quota-area" aria-label={`${provider.name} 额度`}>
      <div className="quota-summary">
        <strong className="quota-headline">{headline}</strong>
        <div className="quota-freshness">
        {source && <span>{source}</span>}
        {quota?.successAt != null && (
          <time dateTime={new Date(quota.successAt * 1000).toISOString()} title={`更新于 ${date(quota.successAt * 1000)}`}>
            {new Date(quota.successAt * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
          </time>
        )}
        <button
          className="icon-button"
          disabled={loading || Boolean(quota?.retryAt && quota.retryAt > Date.now() / 1000)}
          onClick={refresh}
          aria-label={`刷新 ${provider.name} 额度`}
          title="刷新额度"
        >
          <RefreshCw size={14} className={loading ? "quota-spin" : ""} />
        </button>
        </div>
      </div>
      {((quota?.keyStatus && quota.plans.length > 0) || stale) && <div className="quota-notices">
        {quota?.keyStatus && quota.plans.length > 0 && (
          <span className="quota-warning">{quota.keyStatus}</span>
        )}
        {stale && <span className="quota-warning">已过期</span>}
      </div>}
      {quota?.error && (
        <div className="quota-error" role="status">
          {quota.error}
          {quota.retryAt ? ` · ${date(quota.retryAt * 1000)} 后重试` : ""}
        </div>
      )}
      {subscriptions.map((subscription, i) => <section className="quota-subscription-detail" key={i} aria-label={subscription.name ?? "未识别套餐"}>
        <div className="quota-subscription-heading">
          <strong>{subscription.name ?? "未识别套餐"}</strong>
          {subscription.status && subscription.status !== "active" && <span className="quota-warning">{({ expired: "已到期", disabled: "已禁用", cancelled: "已取消", canceled: "已取消" } as Record<string, string>)[subscription.status] ?? subscription.status}</span>}
          {subscription.expiresAt && <time title={`套餐到期 ${date(subscription.expiresAt)}`}>到期 {shortDate(subscription.expiresAt)}</time>}
        </div>
        <QuotaPlans plans={subscription.plans} />
      </section>)}
      {standalonePlans.length > 0 && <section className="quota-subscription-detail" aria-label="额度窗口"><QuotaPlans plans={standalonePlans} label="额度" /></section>}
      {expiry && <div className="quota-key-expiry">Key 到期 <time>{expiry}</time></div>}
      {quota && (quota.today || quota.totalUsage) && <details className="quota-usage">
        <summary tabIndex={0}>用量统计</summary>
        <table><thead><tr><th scope="col">时间</th><th scope="col">请求</th><th scope="col">Token</th><th scope="col">费用</th></tr></thead>
          <tbody>{([["今日", quota.today], ["累计", quota.totalUsage]] as const).map(([label, usage]) => usage && <tr key={label}>
            <th scope="row">{label}</th>
            {[usage.requests, usage.tokens, usage.cost].map((n, index) => <td key={index}><span tabIndex={n == null ? undefined : 0} title={n == null ? undefined : number(n)}>{index === 2 ? n == null ? "—" : number(n) : compactNumber(n)}</span></td>)}
          </tr>)}</tbody>
        </table>
      </details>}
    </div>
  );
}
