export type FailureRule = {
  id: string;
  matchKind: "code" | "keyword";
  value: string;
  category: string;
};
/** Upstream error classes a custom rule can map to; values match Rust `ErrorBasis`. */
export const failureCategories: { category: string; label: string }[] = [
  { category: "rpm", label: "请求频率 RPM" },
  { category: "rps", label: "请求频率 RPS" },
  { category: "tpm", label: "Token 速率" },
  { category: "input_tokens", label: "输入 Token 速率" },
  { category: "output_tokens", label: "输出 Token 速率" },
  { category: "concurrency", label: "并发与连接数" },
  { category: "acceleration", label: "突发与增长过快" },
  { category: "spending_rate", label: "短时消费速率" },
  { category: "capacity", label: "容量不足" },
  { category: "unknown_rate_limit", label: "未分类限流" },
  { category: "usage_limit", label: "待核验用量限制" },
  { category: "quota_exhausted", label: "周期额度耗尽" },
  { category: "billing", label: "余额与消费硬上限" },
  { category: "authentication", label: "认证失败" },
  { category: "permission", label: "权限与访问限制" },
  { category: "model_unavailable", label: "模型不可用" },
  { category: "service_error", label: "服务异常" },
  { category: "invalid_request", label: "请求或协议错误" },
  { category: "request_too_large", label: "单次请求超限" },
  { category: "safety_rejection", label: "安全拒绝" },
];
export type ErrorResponseHeader = {
  name: string;
  value: string;
  base64: boolean;
};
export type ErrorResponse = {
  headers: ErrorResponseHeader[];
  event: string | null;
  body: string;
  base64: boolean;
  incomplete: string | null;
};
