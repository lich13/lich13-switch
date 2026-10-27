import { useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf, type ClientId } from "./types";
import type {
  FailureCategory,
  FailureFocus,
  FailureInput,
  FailureMatch,
  FailureParameter,
  FailureRule,
  FailureOrigin,
} from "./failure-policy-types";
import FailurePolicyCatalog from "./FailurePolicyCatalog";
import FailureRuleDialog from "./FailureRuleDialog";

const sources = {
  builtin_code: "内置错误码",
  builtin_type: "内置错误类型",
  builtin_message: "内置关键词",
  builtin_condition: "判断条件",
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
  catalog,
  onSelection,
}: {
  clientId: ClientId;
  rules: FailureRule[];
  revision: string;
  focus?: FailureFocus | null;
  consumed?: () => void;
  catalog: FailureCategory[];
  onSelection?: (origin: FailureOrigin, keys: FailureParameter[]) => void;
}) {
  const [input, setInput] = useState<FailureInput>({});
  const [selected, setSelected] = useState<string | null>(null);
  const [recorded, setRecorded] = useState<string | null>(null);
  const [result, setResult] = useState<FailureMatch | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [adding, setAdding] = useState(false);
  const [ruleChange, setRuleChange] = useState(0);
  useEffect(() => {
    if (!focus) return;
    setInput(focus.input);
    setSelected(null);
    setRecorded(focus.recordedCategory ?? null);
    consumed?.();
  }, [focus, consumed]);
  useEffect(() => {
    let active = true;
    setResult(null);
    setError("");
    setLoading(false);
    if (
      !Object.values(input).some((v) => (typeof v === "string" ? v.trim() : v))
    )
      return;
    setLoading(true);
    const timer = setTimeout(() => {
      void command("match_failure_policy", { clientId, input })
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
  }, [clientId, input, revision, ruleChange]);
  const displayed =
    result?.candidates?.find((c) => c.category === selected) ?? result;
  const origin = result?.origin ?? "upstream";
  const key = result?.policyId ?? displayed?.category;
  const category = catalog.find(
    (c) => c.category === key && (c.origin ?? "upstream") === origin,
  );
  const label = (id: string) =>
    catalog.find((c) => c.category === id)?.label ?? id;
  useEffect(() => {
    if (result && (displayed?.category || result.policyId))
      onSelection?.(result.origin ?? "upstream", [
        ...(displayed?.parameters ?? []),
        ...(displayed?.commonParameters ?? []),
      ]);
  }, [result, displayed, onSelection]);
  const clear = () => {
    setInput({});
    setSelected(null);
    setRecorded(null);
    setResult(null);
    setError("");
  };
  const shown = input.eventId && result?.parsed ? result.parsed : input;
  return (
    <div className="failure-search">
      <div className="failure-search-box">
        <label className="failure-search-message">
          搜索错误
          <textarea
            rows={2}
            value={
              input.query ??
              input.rawResponse ??
              [shown.status, shown.code, shown.message]
                .filter((v) => v != null && v !== "")
                .join(" ")
            }
            placeholder="粘贴状态码、错误码或报错信息"
            onChange={(e) => {
              setInput({ query: e.target.value });
              setRecorded(null);
              setResult(null);
              setSelected(null);
            }}
          />
        </label>
        <button
          type="button"
          className="text-button"
          onClick={clear}
          disabled={!Object.values(input).some(Boolean)}
        >
          清空
        </button>
      </div>
      {error && (
        <div role="alert" className="form-error">
          {error}
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
          <div className="failure-result-heading">
            <strong>
              {selected
                ? `候选策略：${label(selected)}`
                : `当前判定：${result.label ?? (result.category ? label(result.category) : result.policyId ? label(result.policyId) : "信息不足，尚未匹配")}`}
            </strong>
            {result.origin && (
              <span className="failure-origin">
                {origin === "gateway" ? "网关内部" : "上游报错"}
              </span>
            )}
          </div>
          {selected && (
            <div className="failure-recorded">
              当前判定：
              {result.label ??
                (result.category
                  ? label(result.category)
                  : "信息不足，尚未匹配")}
            </div>
          )}
          {!!result.candidates?.length && (
            <details
              className="failure-common"
              open={!result.parsed?.code && !result.parsed?.message}
            >
              <summary>可能原因（{result.candidates.length}）</summary>
              <div className="failure-candidates">
                {result.candidates.map((c) => (
                  <button
                    type="button"
                    className="secondary"
                    key={c.category}
                    aria-pressed={selected === c.category}
                    onClick={() => setSelected(c.category)}
                  >
                    {c.label} · {c.action}
                  </button>
                ))}
              </div>
            </details>
          )}
          {displayed?.category || result.policyId ? (
            <>
              <dl className="failure-match-evidence">
                <div>
                  <dt>命中依据</dt>
                  <dd>
                    {result.source ? sources[result.source] : "—"}
                    {result.evidence && (
                      <>
                        {" "}
                        <code>{result.evidence}</code>
                      </>
                    )}
                  </dd>
                </div>
                <div>
                  <dt>处理方式</dt>
                  <dd>{displayed?.action ?? "—"}</dd>
                </div>
              </dl>
              {category && (
                <FailurePolicyCatalog
                  category={category.category}
                  catalog={catalog}
                  summaryLabel="匹配条件"
                  clientId={clientId}
                  rules={rules}
                  revision={revision}
                />
              )}
            </>
          ) : (
            <>
              <p>未匹配到规则</p>
              {result.origin !== "gateway" && (
                <button
                  type="button"
                  className="secondary"
                  onClick={() => setAdding(true)}
                >
                  加入故障规则
                </button>
              )}
            </>
          )}
        </section>
      )}
      {adding && (
        <FailureRuleDialog
          clientId={clientId}
          catalog={catalog}
          initial={{
            matchKind:
              (input.code ?? result?.parsed?.code) ? "code" : "keyword",
            value: (
              input.code ||
              result?.parsed?.code ||
              input.message ||
              result?.parsed?.message ||
              input.query ||
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
