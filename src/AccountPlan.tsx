import type { Account } from "./types";

export default function AccountPlan({ account }: { account: Account }) {
  if (account.kind !== "chatgpt") return null;
  const plan = account.subscription;
  return (
    <span
      className="account-plan"
      data-plan={plan?.planType?.toLowerCase() || undefined}
      title={plan?.stale ? "凭据需刷新，套餐信息可能已变化" : undefined}
    >
      {plan?.label || "未识别"}
    </span>
  );
}
