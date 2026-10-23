import type { CurrentConnection as Connection, Provider } from "./types";

export function connectionLabel(
  current: Connection | null | undefined,
  providers: Provider[],
) {
  if (!current) return "尚未读取";
  const name =
    providers.find((p) => p.id === current.id)?.name ??
    current.providerName ??
    current.host;
  switch (current.kind) {
    case "gateway":
      return "本地网关";
    case "official":
      return "官方";
    case "mixed":
      return "各 profile 配置不同";
    case "provider":
      return `${name ?? "供应商"}${current.by === "urlOnly" ? " · 按地址匹配" : ""}`;
    case "unmatched":
      return `${name ?? "当前连接"} · 已识别、未导入`;
    case "unknown":
      return current.reason ?? "无法判断当前连接";
  }
}

export default function CurrentConnection({
  current,
  providers,
}: {
  current: Connection | null | undefined;
  providers: Provider[];
}) {
  return (
    <div className="current-connection">
      <span>{connectionLabel(current, providers)}</span>
      {current?.model && (
        <span title={current.modelSource ?? undefined}>{current.model}</span>
      )}
      {current?.address && (
        <span title={current.address}>{current.address}</span>
      )}
      {(current?.protocol ||
        current?.addressSource ||
        current?.credentialSource) && (
        <small>
          {[
            current.protocol,
            current.addressSource &&
              `地址来源：${({ model: "模型配置", modelApi: "模型 API 地址", environment: "环境变量", global: "全局端点", globalApi: "全局 API 端点" } as Record<string, string>)[current.addressSource] ?? current.addressSource}`,
            current.credentialSource && `凭据：${current.credentialSource}`,
          ]
            .filter(Boolean)
            .join(" · ")}
        </small>
      )}
    </div>
  );
}
