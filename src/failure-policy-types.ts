import type { ClientId, GatewaySettings } from "./types";

export type FailureInput = {
  query?: string | null;
  rawResponse?: string | null;
  event?: string | null;
  eventId?: string | null;
  headers?: ErrorResponseHeader[] | null;
  code?: string | null;
  message?: string | null;
  status?: number | null;
  upstreamType?: string | null;
  upstreamReason?: string | null;
  parameter?: string | null;
};
export type FailureParameter = Exclude<
  keyof GatewaySettings,
  "customFailureRules"
>;
export type FailureMatch = {
  parsed?: FailureInput;
  candidates?: FailureCandidate[];
  category: string | null;
  source:
    | "builtin_code"
    | "builtin_type"
    | "builtin_message"
    | "builtin_condition"
    | "custom_code"
    | "custom_keyword"
    | "status"
    | null;
  evidence: string;
  action: string | null;
  customRuleId: string | null;
  conflict: boolean;
  parameters: FailureParameter[];
  commonParameters: FailureParameter[];
};
export type FailureFocus = {
  clientId: ClientId;
  input: FailureInput;
  recordedCategory?: string | null;
};

export type FailureRule = {
  id: string;
  matchKind: "code" | "keyword";
  value: string;
  category: string;
};
export type FailureCategory = {
  category: string;
  label: string;
  group: string;
  action: string;
  codes: { value: string; category: string; platform: string }[];
  keywords: { allOf: string[][]; exact: boolean }[];
  protocols?: string[];
  excludedKeywords?: string[];
  conditions: string[];
  statuses: string[];
};

export type FailureCandidate = {
  category: string;
  label: string;
  action: string;
  parameters: FailureParameter[];
  commonParameters: FailureParameter[];
};
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
