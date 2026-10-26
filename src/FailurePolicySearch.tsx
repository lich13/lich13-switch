import { useEffect, useState, type ReactNode } from "react";
import { command } from "./bridge";
import { errorOf, type ClientId } from "./types";
import type {
  FailureCategory,
  FailureFocus,
  FailureInput,
  FailureMatch,
  FailureParameter,
  FailureRule,
} from "./failure-policy-types";
import FailurePolicyCatalog from "./FailurePolicyCatalog";
import FailureRuleDialog from "./FailureRuleDialog";

const sources = {
  builtin_code: "内置错误码",
  builtin_type: "内置错误类型",
  builtin_message: "内置关键词",
  builtin_condition: "内置判断条件",
  custom_code: "自定义错误码",
  custom_keyword: "自定义关键词",
  status: "HTTP 状态或业务状态",
};

export default function FailurePolicySearch({
  clientId,
  rules,
  revision,
  focus,
  consumed,
  fields,
  allOpen,
}: {
  clientId: ClientId;
  rules: FailureRule[];
  revision: string;
  focus?: FailureFocus | null;
  consumed?: () => void;
  fields: (keys: FailureParameter[]) => ReactNode;
  allOpen: boolean;
}) {
  const [input, setInput] = useState<FailureInput>({});
  const [selected, setSelected] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const [recorded, setRecorded] = useState<string | null>(null);
  const [result, setResult] = useState<FailureMatch | null>(null);
  const [catalog, setCatalog] = useState<FailureCategory[]>([]);
  const [error, setError] = useState("");
  const [catalogError, setCatalogError] = useState("");
  const [loading, setLoading] = useState(false);
  const [adding, setAdding] = useState(false);
  const [ruleChange, setRuleChange] = useState(0);
  useEffect(() => {
    let active = true;
    void command("get_failure_policy_catalog")
      .then((items) => {
        if (active) setCatalog(items ?? []);
      })
      .catch((e) => {
        if (active) setCatalogError(errorOf(e).message);
      });
    return () => {
      active = false;
    };
  }, []);
  useEffect(() => {
    if (!focus) return;
    setInput(focus.input);
    setSelected(null);
    setStatus(focus.input.status == null ? "" : String(focus.input.status));
    setRecorded(focus.recordedCategory ?? null);
    consumed?.();
  }, [focus, consumed]);
  useEffect(() => {
    let active = true;
    setResult(null);
    setError("");
    setLoading(false);
    if (status && (!/^\d{3}$/.test(status) || +status < 100 || +status > 599)) {
      setError("HTTP 状态应为 100–599");
      return;
    }
    if (
      !recorded &&
      ![
        input.query,
        input.rawResponse,
        input.eventId,
        input.code,
        input.message,
        input.upstreamType,
        input.upstreamReason,
        status,
      ].some((v) => v?.trim())
    )
      return;
    setLoading(true);
    const timer = setTimeout(() => {
      void command("match_failure_policy", {
        clientId,
        input: { ...input, status: status ? +status : null },
      })
        .then((match) => {
          if (active) setResult(match);
        })
        .catch((e) => {
          if (active) setError(errorOf(e).message);
        })
        .finally(() => {
          if (active) setLoading(false);
        });
    }, 300);
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [clientId, input, status, revision, ruleChange, recorded]);
  const change = (
    key: "code" | "message" | "upstreamType" | "upstreamReason" | "parameter",
    value: string,
  ) => {
    setInput((old) => ({
      ...result?.parsed,
      ...old,
      eventId: undefined,
      [key]: value,
    }));
    setSelected(null);
    setRecorded(null);
    setResult(null);
  };
  const displayed =
    result?.candidates?.find((c) => c.category === selected) ?? result;
  const category = catalog.find((c) => c.category === displayed?.category);
  const label = (key: string) =>
    catalog.find((c) => c.category === key)?.label ?? key;
  const hasParameters = !!(
    displayed?.parameters.length || displayed?.commonParameters.length
  );
  return (
    <div className="failure-search">
      <label className="failure-search-message">
        搜索错误
        <textarea
          rows={3}
          value={
            input.query ??
            input.rawResponse ??
            input.message ??
            input.code ??
            result?.parsed?.message ??
            result?.parsed?.code ??
            ""
          }
          placeholder="输入 429、错误码、报错原文或 JSON"
          onChange={(e) => {
            setInput({ query: e.target.value });
            setStatus("");
            setRecorded(null);
            setResult(null);
            setSelected(null);
          }}
        />
      </label>
      <details className="failure-common">
        <summary>结构化字段</summary>
        <div className="failure-search-inputs">
          <label>
            HTTP 状态
            <input
              inputMode="numeric"
              value={
                status ||
                (result?.parsed?.status == null
                  ? ""
                  : String(result.parsed.status))
              }
              maxLength={3}
              placeholder="例如 200 或 429"
              onChange={(e) => {
                setStatus(e.target.value);
                setInput((old) => ({
                  ...result?.parsed,
                  ...old,
                  eventId: undefined,
                }));
                setRecorded(null);
                setSelected(null);
              }}
            />
          </label>
          {(
            [
              ["code", "上游错误码"],
              ["message", "报错原文"],
              ["upstreamType", "上游错误类型"],
              ["upstreamReason", "上游原因"],
              ["parameter", "参数"],
            ] as const
          ).map(([key, label]) => (
            <label key={key}>
              {label}
              <input
                value={input[key] ?? result?.parsed?.[key] ?? ""}
                onChange={(e) => change(key, e.target.value)}
              />
            </label>
          ))}
        </div>
      </details>
      <button
        type="button"
        className="text-button"
        onClick={() => {
          setInput({});
          setSelected(null);
          setStatus("");
          setRecorded(null);
          setResult(null);
          setError("");
        }}
        disabled={!Object.values(input).some(Boolean) && !status}
      >
        清空
      </button>
      {(error || catalogError) && (
        <div role="alert" className="form-error">
          {error || catalogError}
        </div>
      )}
      {loading && <div role="status">正在匹配…</div>}
      {recorded && recorded !== result?.category && !loading && (
        <div className="failure-recorded">日志记录分类：{label(recorded)}</div>
      )}
      {result && (
        <section className="failure-match" aria-label="匹配结果">
          {result.conflict && (
            <div role="alert" className="form-error">
              自定义规则冲突，保留当前兜底结果
            </div>
          )}
          <div className="failure-recorded">
            当前判定：
            {result.category ? label(result.category) : "信息不足，尚未匹配"}
          </div>
          {!!result.candidates?.length && (
            <details
              className="failure-common"
              open={!result.parsed?.code && !result.parsed?.message}
            >
              <summary>可能原因（{result.candidates.length}）</summary>
              <div className="failure-candidates">
                {result.candidates.map((candidate) => (
                  <button
                    type="button"
                    className="secondary"
                    key={candidate.category}
                    aria-pressed={selected === candidate.category}
                    onClick={() => setSelected(candidate.category)}
                  >
                    {candidate.label} · {candidate.action}
                  </button>
                ))}
              </div>
            </details>
          )}
          {displayed?.category ? (
            <>
              <h3>
                {selected ? "候选策略：" : "网关分类："}
                {category?.label ?? label(displayed.category)}
              </h3>
              <dl className="failure-match-evidence">
                <div>
                  <dt>实际命中依据</dt>
                  <dd>
                    {result.source ? sources[result.source] : "—"}
                    {result.evidence && (
                      <>
                        {" "}
                        · <code>{result.evidence}</code>
                      </>
                    )}
                  </dd>
                </div>
                <div>
                  <dt>处理策略</dt>
                  <dd>{displayed.action}</dd>
                </div>
              </dl>
              <FailurePolicyCatalog
                category={displayed.category}
                catalog={catalog}
                summaryLabel="匹配条件"
                clientId={clientId}
                rules={rules}
                revision={revision}
              />
              {hasParameters ? (
                <>
                  <h4>当前客户端共享参数</h4>
                  {!allOpen && (
                    <>
                      {fields(displayed.parameters)}
                      {!!displayed.commonParameters.length && (
                        <details className="failure-common">
                          <summary>通用参数</summary>
                          {fields(displayed.commonParameters)}
                        </details>
                      )}
                    </>
                  )}
                </>
              ) : (
                <p>固定处理方式，无可调参数</p>
              )}
            </>
          ) : (
            <>
              <p>未匹配到规则</p>
              <button
                type="button"
                className="secondary"
                onClick={() => setAdding(true)}
              >
                加入故障规则
              </button>
            </>
          )}
        </section>
      )}
      {adding && (
        <FailureRuleDialog
          clientId={clientId}
          initial={{
            matchKind: (input.code ?? result?.parsed?.code)?.trim()
              ? "code"
              : "keyword",
            value: (
              input.code?.trim() ||
              result?.parsed?.code ||
              input.message?.trim() ||
              input.query?.trim() ||
              ""
            ).slice(0, 256),
          }}
          close={() => setAdding(false)}
          saved={() => setRuleChange((n) => n + 1)}
        />
      )}
    </div>
  );
}
