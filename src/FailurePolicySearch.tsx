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
  status: "状态码判断",
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
  const change = (key: "code" | "message", value: string) => {
    setInput((old) => ({ code: old.code, message: old.message, [key]: value }));
    setRecorded(null);
    setResult(null);
  };
  const category = catalog.find((c) => c.category === result?.category);
  const label = (key: string) =>
    catalog.find((c) => c.category === key)?.label ?? key;
  const hasParameters = !!(
    result?.parameters.length || result?.commonParameters.length
  );
  return (
    <div className="failure-search">
      <div className="failure-search-inputs">
        <label>
          错误码
          <input
            value={input.code ?? ""}
            onChange={(e) => change("code", e.target.value)}
            placeholder="例如 model_capacity_exceeded"
          />
        </label>
        <label>
          HTTP 状态（可选）
          <input
            inputMode="numeric"
            value={status}
            maxLength={3}
            onChange={(e) => {
              setStatus(e.target.value);
              setInput((old) => ({ code: old.code, message: old.message }));
              setRecorded(null);
              setResult(null);
            }}
            placeholder="例如 429"
          />
        </label>
        <label className="failure-search-message">
          报错信息
          <textarea
            rows={2}
            value={input.message ?? ""}
            onChange={(e) => change("message", e.target.value)}
            placeholder="粘贴报错信息"
          />
        </label>
      </div>
      <button
        type="button"
        className="text-button"
        onClick={() => {
          setInput({});
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
          {result.category ? (
            <>
              <h3>{category?.label ?? label(result.category)}</h3>
              <dl className="failure-match-evidence">
                <div>
                  <dt>命中依据</dt>
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
                  <dd>{result.action}</dd>
                </div>
              </dl>
              <FailurePolicyCatalog
                category={result.category}
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
                      {fields(result.parameters)}
                      {!!result.commonParameters.length && (
                        <details className="failure-common">
                          <summary>通用参数</summary>
                          {fields(result.commonParameters)}
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
            matchKind: input.code?.trim() ? "code" : "keyword",
            value: (input.code?.trim() || input.message?.trim() || "").slice(
              0,
              256,
            ),
          }}
          close={() => setAdding(false)}
          saved={() => setRuleChange((n) => n + 1)}
        />
      )}
    </div>
  );
}
