import { useEffect, useState } from "react";
import { Plus, RefreshCw, Search, X } from "lucide-react";
import { command } from "./bridge";
import type {
  GatewayModels as ModelView,
  ModelSearch,
} from "./bridge-contracts";
import { errorOf, type ClientId } from "./types";

const amount = (value: number | null | undefined) =>
  value == null
    ? "—"
    : value >= 1e6
      ? `${+(value / 1e6).toFixed(2)}M`
      : value >= 1e3
        ? `${+(value / 1e3).toFixed(1)}K`
        : String(value);
const price = (value: string | null | undefined) =>
  value == null ? "—" : value;
const priceSource = (source: string) =>
  ({ fixed: "固定价格", sub2api: "Wei-Shaw", models_dev: "models.dev" })[
    source
  ] ?? source;

export default function GatewayModels({ clientId }: { clientId: ClientId }) {
  const [view, setView] = useState<ModelView | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [search, setSearch] = useState("");
  const [manual, setManual] = useState("");
  const [hits, setHits] = useState<ModelSearch | null>(null);
  const [searching, setSearching] = useState(false);
  const [picker, setPicker] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  useEffect(() => {
    let gone = false;
    void command("get_gateway_models", { clientId })
      .then((value) => {
        if (!gone) setView(value);
      })
      .catch((e) => {
        if (!gone) setError(errorOf(e).message);
      });
    return () => {
      gone = true;
    };
  }, [clientId]);
  useEffect(() => {
    let gone = false;
    setHits(null);
    if (!adding && !picker) return;
    setSearching(true);
    const timer = setTimeout(() => {
      void command("search_models_dev", { query: search, limit: 50 })
        .then((value) => {
          if (!gone) setHits(value);
        })
        .catch((e) => {
          if (!gone) setHits({ hits: [], error: errorOf(e).message });
        })
        .finally(() => {
          if (!gone) setSearching(false);
        });
    }, 250);
    return () => {
      gone = true;
      clearTimeout(timer);
    };
  }, [search, adding, picker]);
  async function run(work: () => Promise<ModelView>) {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      setView(await work());
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  }
  const edit = (
    op: "add" | "remove" | "pin" | "unpin",
    id: string,
    source?: string,
  ) => run(() => command("edit_model_list", { clientId, op, id, source }));
  return (
    <section className="gateway-models" aria-label="网关模型列表">
      <div className="model-panel-heading">
        <div>
          <h3>模型列表</h3>
          <p className="muted">
            {view
              ? `${view.models.length} 个模型 · ${view.providers.length} 家在队供应商`
              : "读取模型列表…"}
          </p>
        </div>
        <div className="model-actions">
          <button
            disabled={busy || !view?.providers.length}
            onClick={() =>
              void run(() => command("refresh_gateway_models", { clientId }))
            }
          >
            <RefreshCw size={14} />
            刷新全部供应商
          </button>
          <button
            disabled={busy}
            onClick={() =>
              void run(async () => {
                await command("update_models_dev");
                return command("get_gateway_models", { clientId });
              })
            }
          >
            刷新 models.dev
          </button>
          {!view?.passthrough && (
            <button
              disabled={busy || !view}
              onClick={() => {
                setAdding(!adding);
                setPicker(null);
                setSearch("");
              }}
            >
              <Plus size={14} />
              添加模型
            </button>
          )}
        </div>
      </div>
      {view && <p className="model-hint">{view.hint}</p>}
      {(error || view?.error) && (
        <p className="banner error" role="alert">
          {error || view?.error}
        </p>
      )}
      {view && (
        <>
          <p className="model-index muted">
            models.dev：{view.modelsDev.providers} 家 /{" "}
            {view.modelsDev.models.toLocaleString()} 条
            {view.modelsDev.updatedAt
              ? ` · ${new Date(view.modelsDev.updatedAt).toLocaleString()}`
              : " · 尚无索引"}
            {view.modelsDev.error && ` · ${view.modelsDev.error}`}
          </p>
          <div className="model-table-scroll">
            <table className="model-table">
              <thead>
                <tr>
                  <th>模型 ID / 显示名</th>
                  <th>来源</th>
                  <th>上下文</th>
                  <th>推理档位</th>
                  <th>输入 $/1M</th>
                  <th>输出 $/1M</th>
                  <th>缓存读 $/1M</th>
                  <th>价源</th>
                  {!view.passthrough && (
                    <th>
                      <span className="sr-only">操作</span>
                    </th>
                  )}
                </tr>
              </thead>
              <tbody>
                {view.models.map((model) => (
                  <tr key={model.id}>
                    <td>
                      <strong title={model.id}>{model.id}</strong>
                      <span
                        className="muted"
                        title={model.metadata?.name ?? undefined}
                      >
                        {model.metadata?.name ?? "—"}
                      </span>
                    </td>
                    <td>
                      <div
                        className="model-sources"
                        title={[
                          ...model.providers.map((p) => p.name),
                          model.source,
                        ]
                          .filter(Boolean)
                          .join(" · ")}
                      >
                        {model.providers.map((p) => (
                          <span key={p.id} className="model-source">
                            {p.name}
                          </span>
                        ))}
                        {model.manual && (
                          <span className="model-source manual">手动</span>
                        )}
                      </div>
                      {model.source && (
                        <span className="model-source-ref" title={model.source}>
                          {model.source}
                        </span>
                      )}
                      {model.manual && !model.providers.length && (
                        <span
                          className="muted"
                          title="供应商尚未声明支持此模型，路由仍按原规则判断"
                        >
                          未声明支持
                        </span>
                      )}
                    </td>
                    <td>{amount(model.metadata?.context)}</td>
                    <td
                      title={Object.keys(
                        model.metadata?.reasoningEfforts ?? {},
                      ).join(" · ")}
                    >
                      {Object.keys(model.metadata?.reasoningEfforts ?? {}).join(
                        " · ",
                      ) || "—"}
                    </td>
                    <td>{price(model.price?.inputPerM)}</td>
                    <td>{price(model.price?.outputPerM)}</td>
                    <td>{price(model.price?.cacheReadPerM)}</td>
                    <td>
                      <span
                        className={
                          model.price?.deviation ? "model-deviation" : "muted"
                        }
                      >
                        {model.price ? priceSource(model.price.source) : "未知"}
                        {model.price?.deviation && " · 偏差≥25%"}
                      </span>
                    </td>
                    {!view.passthrough && (
                      <td>
                        <div className="model-row-actions">
                          <button
                            disabled={busy}
                            onClick={() => {
                              setPicker(model.id);
                              setAdding(false);
                              setSearch(model.id);
                            }}
                          >
                            指定来源
                          </button>
                          {model.pinned && (
                            <button
                              disabled={busy}
                              onClick={() => void edit("unpin", model.id)}
                            >
                              取消指定
                            </button>
                          )}
                          {model.manual && (
                            <button
                              disabled={busy}
                              onClick={() => void edit("remove", model.id)}
                            >
                              移除
                            </button>
                          )}
                        </div>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
            {!view.models.length && (
              <p className="model-empty">
                尚无模型。刷新供应商，或添加明确的模型 ID。
              </p>
            )}
          </div>
          <div className="model-provider-status">
            {view.providers.map((p) => (
              <button
                key={p.id}
                disabled={busy}
                title={p.error ?? "刷新此供应商"}
                onClick={() =>
                  void run(() =>
                    command("refresh_gateway_models", {
                      clientId,
                      providerIds: [p.id],
                    }),
                  )
                }
              >
                <span>{p.name}</span>
                <span
                  className={p.stale || p.error ? "model-deviation" : "muted"}
                >
                  {p.stale
                    ? "配置已变更，需刷新"
                    : p.error
                      ? "获取失败"
                      : p.checkedAt
                        ? `已获取 ${new Date(p.checkedAt * 1000).toLocaleTimeString()}`
                        : "尚未获取"}
                </span>
              </button>
            ))}
          </div>
        </>
      )}
      {(adding || picker) && (
        <div className="model-picker">
          <div className="model-picker-heading">
            <strong>{picker ? `指定来源 · ${picker}` : "添加模型"}</strong>
            <button
              className="icon-button"
              aria-label="关闭模型搜索"
              onClick={() => {
                setAdding(false);
                setPicker(null);
              }}
            >
              <X size={16} />
            </button>
          </div>
          {!picker && (
            <form
              className="model-input-row"
              onSubmit={(e) => {
                e.preventDefault();
                void edit("add", manual.trim());
              }}
            >
              <label>
                模型 ID
                <input
                  value={manual}
                  onChange={(e) => setManual(e.target.value)}
                maxLength={256}
                  placeholder="直接输入模型 ID"
                />
              </label>
              <button disabled={busy || !manual.trim()}>添加 ID</button>
            </form>
          )}
          <label className="model-search">
            <Search size={16} />
            <input
              aria-label="搜索 models.dev"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder="搜索模型 ID 或名称"
            />
          </label>
          <p className="muted">
            来源分别列出，最多 50 条。指定来源仅影响元数据和参考价格。
          </p>
          {hits?.error && (
            <p role="alert" className="form-error">
              {hits.error}
            </p>
          )}
          {searching ? (
            <p role="status">搜索中…</p>
          ) : hits && !hits.error && !hits.hits.length ? (
            <p>没有匹配模型</p>
          ) : (
            <ul className="model-search-results">
              {hits?.hits.map((hit) => (
                <li key={hit.source}>
                  <div>
                    <strong title={hit.model}>{hit.model}</strong>
                    <span title={hit.source}>
                      {hit.providerName || hit.provider} · {hit.name} ·{" "}
                      {amount(hit.context)}
                    </span>
                  </div>
                  <button
                    disabled={
                      busy ||
                      (!picker &&
                        view?.models.some(
                          (m) => m.id === hit.model && m.manual,
                        ))
                    }
                    onClick={() =>
                      void edit(
                        picker ? "pin" : "add",
                        picker ?? hit.model,
                        hit.source,
                      )
                    }
                  >
                    {picker
                      ? "使用此来源"
                      : view?.models.some((m) => m.id === hit.model && m.manual)
                        ? "已添加"
                        : "添加"}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}
