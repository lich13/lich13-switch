import {
  routingReason,
  routingInput,
  routingRestriction,
  routingIdentity,
  routingBoundary,
  routingOutcome,
} from "./routing-labels";
import { Drawer } from "./UsageDrawer";
import { preciseDate, promptTokens, missingReason } from "./usage-format";
import {
  interruption,
  interruptionLabel,
  logSpeed,
  unpricedReason,
} from "./usage-log-columns";
import {
  clientName,
  compact,
  money,
  numeric,
  type Attempt,
  type Tokens,
  type UsageRecord,
} from "./usage-types";

const outcomes: Record<string, string> = {
  success: "已完成",
  limited: "正常结束",
  rejected: "请求被拒绝",
  model_unavailable: "模型不支持",
  cancelled: "已取消",
  failure: "请求失败",
  unknown: "结果未确认",
  completed: "会话完成",
  reported: "会话已报告",
};
const failoverLabels: Record<string, string> = {
  initial: "初次分配",
  proactive_handoff: "主动压缩交接",
  failure_fallback: "失败后重新选商",
  cursor_pinned: "响应游标固定",
  unknown_owner: "归属未确认",
  rate_limit: "临时限流",
  account_rejected: "账号明确拒绝",
  model_unavailable: "模型不可用",
  upstream_service: "上游服务错误",
  transport: "连接或传输错误",
  protocol: "协议错误",
  model: "模型限制",
  conversation_policy: "线程接管策略",
  health: "健康限制",
  capacity: "并发容量",
  rpm: "RPM 限制",
  forwarded: "已开始交付响应",
  no_eligible_provider: "没有合格替代",
  attempts_exhausted: "重试次数耗尽",
  permanent_rejection: "明确拒绝",
  replay_unsafe: "请求可能已发送，未自动重放",
  cancelled: "转发已取消",
  stopped: "网关停止",
  admission_rejected: "准入受限",
  capacity_retry: "容量重试",
  retry_after: "上游重试限制",
  transient_retry: "短暂错误重试",
  admission: "等待准入",
};
export default function Detail({
  record: r,
  name,
  onClose,
}: {
  record: UsageRecord;
  name: (id: string | null) => string;
  onClose: () => void;
}) {
  const last = r.attempts.at(-1)!;
  const a: Attempt =
    r.source === "grok" && r.attempts.length > 1
      ? {
          ...last,
          tokens: promptTokens(r.attempts),
          requestedModel: null,
          responseModel: null,
          durationMs: r.finishedAt == null ? 0 : r.finishedAt - r.startedAt,
          repeatCount: r.attempts.reduce(
            (sum, a) => sum + (a.repeatCount ?? 1),
            0,
          ),
        }
      : last;
  const total = r.attempts.reduce(
    (sum, a) => sum + Number(a.price?.cost ?? 0),
    0,
  );
  return (
    <Drawer
      title={r.source === "grok" ? "Grok 轮次详情" : "请求详情"}
      onClose={onClose}
    >
      <div className="usage-detail-status">
        <b>
          {a.status == null
            ? r.source === "proxy"
              ? "—"
              : "会话"
            : `HTTP ${a.status}`}
        </b>
        <span>{outcomes[a.outcome] ?? "结果未确认"}</span>
        <span>{clientName(r.client)}</span>
        {r.detailArchived && <span>细节已归档</span>}
      </div>
      <AttemptDetails
        a={a}
        name={name}
        source={r.source}
        metadata={[
          ["客户端", clientName(r.client)],
          [
            "来源",
            r.source === "proxy" ? "网关" : clientName(r.source) + " 会话",
          ],
          [r.source === "grok" ? "轮次 ID" : "请求 ID", r.id],
          ...(r.source === "grok"
            ? ([
                [
                  "模型调用",
                  numeric(
                    r.attempts.reduce(
                      (sum, a) => sum + (a.repeatCount ?? 1),
                      0,
                    ),
                  ),
                ],
              ] as [string, React.ReactNode][])
            : []),
          ...(r.failoverEvidence
            ? ([
                [
                  "故障转移阶段",
                  failoverLabels[r.failoverEvidence.stage] ?? "未记录",
                ],
                [
                  "失败分类",
                  r.failoverEvidence.failure
                    ? (failoverLabels[r.failoverEvidence.failure] ??
                      "故障原因未确认")
                    : "未记录",
                ],
                [
                  "候选观察",
                  `${r.failoverEvidence.eligibleCandidates} 个可用候选`,
                ],
                ...Object.entries(r.failoverEvidence.exclusions).map(
                  ([reason, count]) => [
                    `排除 · ${failoverLabels[reason] ?? reason}`,
                    `${count} 个`,
                  ],
                ),
                ...(r.failoverEvidence.wait
                  ? [
                      [
                        "等待依据",
                        `${failoverLabels[r.failoverEvidence.wait.kind] ?? "未记录"} · ${r.failoverEvidence.wait.seconds} 秒`,
                      ],
                    ]
                  : []),
                [
                  "转发退出",
                  r.failoverEvidence.exit
                    ? (failoverLabels[r.failoverEvidence.exit] ?? "未记录")
                    : "未记录",
                ],
              ] as [string, React.ReactNode][])
            : []),
          [
            "列表时间依据",
            r.source !== "proxy"
              ? "会话事件时间"
              : a.transfer?.terminalAt != null
                ? "协议终态"
                : a.transfer?.endedAt
                  ? "本地结束"
                  : "历史开始时间（缺少结束记录）",
          ],
          [
            "数据归属",
            r.deduplication === "ambiguous"
              ? "关联未确认，未计入合计"
              : r.duplicateOf
                ? "已合并"
                : r.deduplication === "response_id"
                  ? "响应 ID 合并"
                  : r.deduplication === "strict_match"
                    ? "严格匹配合并"
                    : "独立记录",
          ],
        ]}
      />
      {r.attempts.length > 1 && (
        <>
          <h3>{r.source === "grok" ? "模型用量" : "此前尝试"}</h3>
          <dl className="usage-detail-grid">
            <dt>{r.source === "grok" ? "估算费用" : "全部尝试费用"}</dt>
            <dd>
              {r.attempts.some((a) => !a.price || a.compactedUnpriced)
                ? "含未定价项 · "
                : ""}
              {r.source === "grok" ? "—" : money(String(total))}
            </dd>
            <dt>{r.source === "grok" ? "模型调用" : "实际尝试"}</dt>
            <dd>
              {numeric(
                r.attempts.reduce((sum, a) => sum + (a.repeatCount ?? 1), 0),
              )}
            </dd>
          </dl>
          {(r.source === "grok" ? r.attempts : r.attempts.slice(0, -1)).map(
            (attempt, i) => (
              <details key={attempt.id}>
                <summary>
                  {i + 1} · {name(attempt.provider)} · {attempt.status ?? "—"}
                  {(attempt.repeatCount ?? 1) > 1
                    ? ` · 合并 ${compact(attempt.repeatCount)} 次`
                    : ""}
                </summary>
                <AttemptDetails a={attempt} name={name} source={r.source} />
              </details>
            ),
          )}
        </>
      )}
    </Drawer>
  );
}
function rateLabel(key: string) {
  const labels: Record<string, string> = {
    input_cost_per_token: "输入",
    output_cost_per_token: "输出",
    cache_read_input_token_cost: "缓存读取",
    cache_creation_input_token_cost: "缓存写入",
    cache_creation_input_token_cost_above_1hr: "缓存写入（1 小时）",
    input_cost_per_image_token: "图片输入",
    output_cost_per_image_token: "图片输出",
    input_cost_per_audio_token: "音频输入",
    output_cost_per_audio_token: "音频输出",
  };
  const tiers: Record<string, string> = {
    batches: "批处理",
    flex: "弹性",
    priority: "优先",
    ultrafast: "极速",
    fast: "fast",
  };
  const tier = key.match(/_(batches|flex|priority|ultrafast|fast)$/)?.[1];
  const base = tier ? key.slice(0, -tier.length - 1) : key;
  const threshold = base.match(/_above_(100|200|272)k_tokens$/)?.[1];
  const name = threshold
    ? base.replace(/_above_(100|200|272)k_tokens$/, "")
    : base;
  return [
    labels[name] ?? "其他",
    threshold ? `上下文 > ${threshold}K` : null,
    tier ? tiers[tier] : null,
  ]
    .filter(Boolean)
    .join(" · ");
}
function TokenField({
  a,
  field,
  value,
  source,
}: {
  a: Attempt;
  field: string;
  value: number | null;
  source: string;
}) {
  const reason =
    value == null
      ? missingReason(a, field, source)
      : a.usageSources?.[field]
        ? "会话补齐"
        : source === "proxy"
          ? "上游已报告"
          : "会话已报告";
  return (
    <span
      tabIndex={0}
      title={reason}
      aria-label={`${numeric(value)} · ${reason}`}
    >
      {numeric(value)}
    </span>
  );
}
function DetailFields({ items }: { items: [string, React.ReactNode][] }) {
  return (
    <dl className="usage-detail-grid">
      {items.map(([label, value]) => (
        <div key={label} className="usage-detail-pair">
          <dt>{label}</dt>
          <dd>{value ?? "—"}</dd>
        </div>
      ))}
    </dl>
  );
}
function AttemptDetails({
  a,
  name,
  source,
  metadata = [],
}: {
  a: Attempt;
  name: (id: string | null) => string;
  source: string;
  metadata?: [string, React.ReactNode][];
}) {
  const tps = source === "proxy" ? logSpeed(a) : null;
  const first =
    source === "proxy" && a.stream && (a.operation ?? "model") === "model"
      ? a.firstTokenMs
      : null;
  return (
    <>
      <h3>基本信息</h3>
      <DetailFields
        items={[
          ["供应商", name(a.provider)],
          ["开始时间", preciseDate(a.startedAt)],
          [
            "请求类型",
            a.operation === "web_search"
              ? "网络搜索"
              : a.operation === "compaction" || a.compactionKind
                ? "压缩"
                : "生成",
          ],
          [
            "用量采集",
            (
              {
                reported: "上游已报告",
                partial: "部分字段未报告",
                upstream_unreported: "上游未报告",
                ended_early: "提前结束",
                parse_incomplete: "解析不完整",
                session_supplemented: "会话补齐",
              } as Record<string, string>
            )[a.usageStatus ?? ""] ?? "历史记录",
          ],
          ...(source === "proxy"
            ? [["结束原因", endReason(a)] as [string, string]]
            : []),
          ...(a.end
            ? [
                [
                  "结束阶段",
                  a.end.terminalReceived
                    ? "已收到协议终态"
                    : a.end.responseStarted
                      ? "读取上游响应"
                      : "等待上游响应",
                ] as [string, string],
              ]
            : []),
          ...(a.routingDecision
            ? [
                ["线程调度", routingReason(a.routingDecision)] as [
                  string,
                  string,
                ],
              ]
            : []),
          ...(a.routingEvidence
            ? ([
                ["输入证据", routingInput(a.routingEvidence.input)],
                ...(a.routingEvidence.identity
                  ? [["线程身份", routingIdentity(a.routingEvidence.identity)]]
                  : []),
                ...(a.routingEvidence.boundary
                  ? [["边界状态", routingBoundary(a.routingEvidence.boundary)]]
                  : []),
                ...(a.routingEvidence.hasToolDeclarations
                  ? [["初始化声明", "包含工具声明"]]
                  : []),
                ...(a.routingEvidence.restriction
                  ? [
                      [
                        "准入限制",
                        routingRestriction(a.routingEvidence.restriction),
                      ],
                    ]
                  : []),
              ] as [string, string][])
            : []),
          ...(a.routingOutcome
            ? [
                ["最终路由结果", routingOutcome(a.routingOutcome)] as [
                  string,
                  string,
                ],
              ]
            : []),
          ...(a.transfer
            ? ([
                [
                  "请求进入",
                  a.transfer.receivedAt == null
                    ? "—"
                    : preciseDate(a.transfer.receivedAt),
                ],
                ["上游发送", preciseDate(a.transfer.sentAt)],
                [
                  "收到首字节",
                  a.transfer.firstByteAt == null
                    ? "—"
                    : preciseDate(a.transfer.firstByteAt),
                ],
                [
                  "协议终态",
                  a.transfer.terminalAt == null
                    ? "—"
                    : preciseDate(a.transfer.terminalAt),
                ],
                ["本地结束", preciseDate(a.transfer.endedAt)],
                ...(a.transfer.terminalEvent
                  ? [["终态事件", a.transfer.terminalEvent]]
                  : []),
                ...(a.transfer.terminalStatus
                  ? [["终态状态", a.transfer.terminalStatus]]
                  : []),
                ...(a.transfer.incompleteReason
                  ? [
                      [
                        "未完成原因",
                        a.transfer.incompleteReason === "unrecognized"
                          ? "未识别"
                          : a.transfer.incompleteReason,
                      ],
                    ]
                  : []),
                ...(a.transfer.upstreamClientRequestHash
                  ? [
                      [
                        "上游关联摘要",
                        a.transfer.upstreamClientRequestHash.slice(0, 12),
                      ],
                    ]
                  : []),
                ["上游读取", `${numeric(a.transfer.upstreamBytes)} B`],
                [
                  a.transport === "websocket"
                    ? "交给 WebSocket 层"
                    : "交给 HTTP 层",
                  `${numeric(a.transfer.downstreamBytes)} B`,
                ],
                ...(a.transfer.connection
                  ? [
                      [
                        "连接序号",
                        `${a.transfer.connection.id.slice(0, 12)} · 请求 ${a.transfer.connection.requestSequence}`,
                      ],
                      [
                        "连接累计 socket 写出",
                        `${numeric(a.transfer.connection.socketWrittenBytes)} B（含 HTTP 头，未确认客户端接收）`,
                      ],
                      [
                        "连接事件顺序",
                        a.transfer.connection.events
                          .map(
                            (e) =>
                              `${e.sequence}. ${e.kind} · ${preciseDate(e.at)}`,
                          )
                          .join("\n") || "—",
                      ],
                    ]
                  : []),
                ["终态已交出", a.transfer.terminalHandedOff ? "是" : "未确认"],
                [
                  "上游结束",
                  (
                    {
                      EOF: "正常 EOF",
                      DISCONNECTED: "连接中断",
                      TIMEOUT: "无进展超时",
                      CONNECT_FAILED: "连接失败",
                    } as Record<string, string>
                  )[a.transfer.upstreamEnd ?? ""] ?? "—",
                ],
                [
                  "WS 关闭码",
                  a.transfer.wsCloseCode == null
                    ? "—"
                    : String(a.transfer.wsCloseCode),
                ],
                ["下游错误", a.transfer.downstreamError ?? "—"],
                ["超时阶段", a.transfer.timeoutPhase ?? "—"],
                [
                  "超时阈值",
                  a.transfer.timeoutSeconds == null
                    ? "—"
                    : `${a.transfer.timeoutSeconds} 秒`,
                ],
                [
                  "距最后进展",
                  a.transfer.lastProgressMs == null
                    ? "—"
                    : `${a.transfer.lastProgressMs} ms`,
                ],
              ] as [string, string][])
            : []),
          ["请求模型", a.requestedModel],
          ...(a.upstreamModel
            ? [["上游请求模型", a.upstreamModel] as [string, string]]
            : []),
          ["收到的响应模型", a.responseModel],
          ["中断状态", interruptionLabel(a) || null],
          [
            "请求速度",
            a.requestedSpeed === "fast" && !a.responseSpeed
              ? "fast（仅观察到请求）"
              : a.requestedSpeed,
          ],
          ["响应速度", a.responseSpeed],
          ["服务层级", a.serviceTier],
          ...metadata,
        ]}
      />
      <h3>Token</h3>
      {interruption(a) === "partial" && (
        <p className="form-error">
          上游可能在断开后继续生成并计费，本地不估算。
        </p>
      )}
      <DetailFields
        items={[
          [
            "原始输入",
            a.rawInputTokens == null ? "—" : numeric(a.rawInputTokens),
          ],
          [
            "输入口径",
            a.inputBasis === "exclusive"
              ? "不含缓存"
              : a.inputBasis === "inclusive"
                ? "含缓存"
                : "未知",
          ],
          [
            "推理（输出子集）",
            <TokenField
              a={a}
              field="reasoning"
              value={a.tokens.reasoning ?? null}
              source={source}
            />,
          ],
          [
            "缓存写入 · 5m",
            a.tokens.cacheWrite5m == null
              ? "—"
              : numeric(a.tokens.cacheWrite5m),
          ],
          [
            "缓存写入 · 1h",
            a.tokens.cacheWrite1h == null
              ? "—"
              : numeric(a.tokens.cacheWrite1h),
          ],
          [
            "解析依据",
            a.parseReasons
              ?.map(
                (reason) =>
                  (
                    ({
                      cache_write_total_below_ttl_details:
                        "缓存写总数小于 TTL 明细合计",
                      prompt_cache_total_mismatch:
                        "缓存命中与未命中之和不等于原始输入",
                      reasoning_exceeds_output: "推理 token 超过输出 token",
                    }) as Record<string, string>
                  )[reason] ?? reason,
              )
              .join("；"),
          ],
          [
            "输入",
            <TokenField
              a={a}
              field="input"
              value={a.tokens.input}
              source={source}
            />,
          ],
          [
            "输出",
            <TokenField
              a={a}
              field="output"
              value={a.tokens.output}
              source={source}
            />,
          ],
          [
            "缓存读取",
            <TokenField
              a={a}
              field="cache_read"
              value={a.tokens.cacheRead}
              source={source}
            />,
          ],
          [
            "缓存写入",
            <TokenField
              a={a}
              field="cache_write"
              value={a.tokens.cacheWrite}
              source={source}
            />,
          ],
          ...(a.usageSources && Object.keys(a.usageSources).length
            ? [
                [
                  "补齐字段",
                  Object.keys(a.usageSources)
                    .map(
                      (key) =>
                        (
                          ({
                            inclusive_input: "完整输入",
                            input: "输入",
                            output: "输出",
                            reasoning: "推理",
                            cache_read: "缓存读取",
                            cache_write: "缓存写入",
                            cache_write_5m: "短时缓存写入",
                            cache_write_1h: "长时缓存写入",
                            image_input: "图片输入",
                            image_output: "图片输出",
                            audio_input: "音频输入",
                            audio_output: "音频输出",
                          }) as Record<string, string>
                        )[key] ?? key,
                    )
                    .join("、"),
                ] as [string, string],
              ]
            : []),
        ]}
      />
      <h3>费用</h3>
      <DetailFields
        items={[
          ["估算费用", money(a.price?.cost)],
          ["倍率", a.price?.multiplier],
          ["计价模型", a.pricingModel],
          [
            "计价依据",
            a.pricingBasis === "provider_mapping"
              ? "供应商精确映射（估算）"
              : a.pricingBasis === "compacted"
                ? "重试消耗汇总（估算）"
                : a.pricingBasis === "request"
                  ? "请求模型（估算）"
                  : a.operation === "web_search"
                    ? "搜索按次"
                    : "收到的响应模型（估算）",
          ],
          ["价格版本", a.price?.version],
          ["价格来源", a.price?.source],
          [
            "价格匹配",
            a.price?.basis?.models_dev_match
              ? `${a.price.basis.models_dev_match.providers.join("、")} / ${a.price.basis.models_dev_match.model} · ${a.price.basis.models_dev_match.matchedBy}`
              : a.price?.basis?.price_match
                ? `${a.price.basis.price_match.model} · ${a.price.basis.price_match.matchedBy}`
                : null,
          ],
          [
            "上下文阶梯",
            a.price?.basis?.models_dev_context_threshold == null
              ? null
              : `>${a.price.basis.models_dev_context_threshold} token`,
          ],
          ["未定价原因", a.price ? null : unpricedReason(a)],
          ...(a.mappingRevision
            ? [["映射版本", a.mappingRevision] as [string, string]]
            : []),
        ]}
      />
      {a.price && (
        <details>
          <summary>
            {a.operation === "web_search" ? "按次计费" : "单价 / 百万 Token"}
          </summary>
          {a.operation === "web_search" && (
            <p>
              {a.price.basis?.quantity ?? 1} 次 × $0.01 × {a.price.multiplier}
            </p>
          )}
          <DetailFields
            items={Object.entries(a.price.rates).map(([key, value]) => [
              key === "cost_per_request" ? "每次搜索" : rateLabel(key),
              `$${(Number(value) * (key === "cost_per_request" ? 1 : 1e6)).toLocaleString("zh-CN", { maximumFractionDigits: 8 })}`,
            ])}
          />
        </details>
      )}
      <h3>性能</h3>
      <DetailFields
        items={[
          [
            "总耗时",
            a.durationMs != null ? (
              `${(a.durationMs / 1000).toFixed(3)}s`
            ) : (
              <span tabIndex={0} title="历史缺失">
                —
              </span>
            ),
          ],
          [
            "首字",
            first != null ? (
              `${(first / 1000).toFixed(3)}s`
            ) : (
              <span
                tabIndex={0}
                title={missingReason(a, "first_token", source)}
              >
                —
              </span>
            ),
          ],
          ["输出 TPS", tps == null ? "—" : `${tps.toFixed(1)} tok/s`],
          ["传输", a.transport],
          ["流式", a.stream ? "是" : "否"],
        ]}
      />
    </>
  );
}

function endReason(a: Attempt): string {
  if (!a.end) return "历史结束原因未记录";
  if (a.end.terminalReceived && a.transfer?.terminalHandedOff === false) {
    return "上游已结束，下游未完成交付";
  }
  if (a.end.cause === "client_disconnected") {
    return a.end.terminalReceived && ["success", "limited"].includes(a.outcome)
      ? "上游已完成，客户端随后断开"
      : a.end.terminalReceived
        ? "上游已结束，客户端随后断开"
        : "客户端断开，完成状态未知";
  }
  return (
    (
      {
        completed: "上游已完成",
        application_error: "上游返回应用错误",
        upstream_incomplete: "上游未完成，原因未确认",
        upstream_rejected: "上游拒绝请求",
        client_cancelled: "生成已取消",
        downstream_write_timeout: "写入下游超时，完成状态未知",
        downstream_transport_error: "下游传输异常，完成状态未知",
        gateway_stopped: "网关停止",
        upstream_disconnected: "上游提前断开",
        connect_failed: "上游连接失败",
        first_byte_timeout: "等待上游响应超时",
        stream_timeout: "读取上游流超时",
        parse_incomplete: "响应解析不完整",
        unsettled: "结束原因未确认",
      } as Record<string, string>
    )[a.end.cause] ?? "结束原因未确认"
  );
}
