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
