import { useEffect, useState } from "react";
import { Pencil, Plus, Trash2 } from "lucide-react";
import { command } from "./bridge";
import { errorOf, type ClientId } from "./types";
import type { FailureCategory, FailureRule } from "./failure-policy-types";
import FailureRuleDialog from "./FailureRuleDialog";

export default function FailurePolicyCatalog({
  group,
  category,
  catalog: providedCatalog,
  summaryLabel,
  clientId,
  rules,
  revision,
}: {
  group?: string;
  category?: string;
  catalog?: FailureCategory[];
  summaryLabel?: string;
  clientId: ClientId;
  rules: FailureRule[];
  revision: string;
}) {
  const [catalog, setCatalog] = useState<FailureCategory[]>([]),
    [error, setError] = useState("");
  const [editing, setEditing] = useState<Partial<FailureRule> | null>(null),
    [deleting, setDeleting] = useState("");
  useEffect(() => {
    if (providedCatalog) return;
    let active = true;
    void command("get_failure_policy_catalog")
      .then((items) => {
        if (active) setCatalog(items ?? []);
      })
      .catch((e) => {
        if (active) setError(errorOf(e).message);
      });
    return () => {
      active = false;
    };
  }, [providedCatalog]);
  return (
    <div className="failure-catalog">
      {error && (
        <div role="alert" className="form-error">
          {error}
        </div>
      )}
      {(providedCatalog ?? catalog)
        .filter(
          (c) =>
            (!group || c.group === group) &&
            (!category || c.category === category),
        )
        .map((c) => (
          <details className="failure-category" key={c.category}>
            <summary>
              <span>{summaryLabel ?? c.label}</span>
              {!summaryLabel && (
                <span className="failure-action">{c.action}</span>
              )}
            </summary>
            <div className="failure-conditions">
              {c.protocols?.length ? (
                <dl>
                  <div>
                    <dt>协议</dt>
                    <dd>{c.protocols.join(" / ")}</dd>
                  </div>
                </dl>
              ) : null}
              {c.codes.length > 0 && (
                <dl>
                  {[...new Set(c.codes.map((v) => v.platform))].map(
                    (platform) => (
                      <div key={platform}>
                        <dt>{platform}</dt>
                        <dd className="failure-codes">
                          {c.codes
                            .filter((v) => v.platform === platform)
                            .map((code) => (
                              <code key={code.value}>{code.value}</code>
                            ))}
                        </dd>
                      </div>
                    ),
                  )}
                </dl>
              )}
              {c.keywords.length > 0 && (
                <dl>
                  <div>
                    <dt>消息关键词</dt>
                    <dd>
                      {c.keywords.map((condition, i) => (
                        <div className="failure-keywords" key={i}>
                          {condition.exact && <span>完整匹配：</span>}
                          {condition.allOf.map((words, j) => (
                            <span key={j}>
                              {j > 0 && <b> 且 </b>}
                              {words.map((word, k) => (
                                <span key={word}>
                                  {k > 0 && (
                                    <span className="keyword-or"> 或 </span>
                                  )}
                                  <code>{word}</code>
                                </span>
                              ))}
                            </span>
                          ))}
                        </div>
                      ))}
                    </dd>
                  </div>
                </dl>
              )}
              {c.conditions.length > 0 && (
                <dl>
                  <div>
                    <dt>判断条件</dt>
                    <dd>
                      {c.conditions.map((condition) => (
                        <div key={condition}>{condition}</div>
                      ))}
                    </dd>
                  </div>
                </dl>
              )}
              {!!c.keywords.length && !!c.excludedKeywords?.length && (
                <dl>
                  <div>
                    <dt>排除消息</dt>
                    <dd className="failure-codes">
                      {[...new Set(c.excludedKeywords)].map((word) => (
                        <code key={word}>{word}</code>
                      ))}
                    </dd>
                  </div>
                </dl>
              )}
              {c.statuses.length > 0 && (
                <dl>
                  <div>
                    <dt>状态码兜底</dt>
                    <dd>{c.statuses.join(" / ")}</dd>
                  </div>
                </dl>
              )}
              {rules
                .filter((r) => r.category === c.category)
                .map((rule) => (
                  <div className="custom-failure-rule" key={rule.id}>
                    <span>
                      {rule.matchKind === "code" ? "错误码" : "关键词"}
                    </span>
                    <code>{rule.value}</code>
                    <button
                      className="icon-button"
                      aria-label={`编辑 ${rule.value}`}
                      onClick={() => setEditing(rule)}
                    >
                      <Pencil size={14} />
                    </button>
                    <button
                      className="icon-button"
                      disabled={deleting !== ""}
                      aria-label={`删除 ${rule.value}`}
                      onClick={async () => {
                        setDeleting(rule.id);
                        setError("");
                        try {
                          await command("update_gateway", {
                            clientId,
                            expectedRevision: revision,
                            edit: { op: "deleteFailureRule", id: rule.id },
                          });
                        } catch (e) {
                          setError(errorOf(e).message);
                        } finally {
                          setDeleting("");
                        }
                      }}
                    >
                      <Trash2 size={14} />
                    </button>
                  </div>
                ))}
              <button
                type="button"
                className="text-button failure-add"
                onClick={() => setEditing({ category: c.category })}
              >
                <Plus size={14} />
                添加匹配条件
              </button>
            </div>
          </details>
        ))}
      {editing && (
        <FailureRuleDialog
          clientId={clientId}
          initial={editing}
          close={() => setEditing(null)}
        />
      )}
    </div>
  );
}
