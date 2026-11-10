use super::{model::*, pricing, store, State};
use crate::{storage::Result, Runtime};
use std::sync::Arc;
type R<'a> = tauri::State<'a, Arc<Runtime>>;
#[tauri::command]
pub fn get_usage_state(r: R<'_>) -> State {
    r.usage.state()
}
#[tauri::command]
pub async fn get_usage_dashboard(r: R<'_>, filter: Filter) -> Result<store::Dashboard> {
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.read(|db| db.dashboard(&filter)))
        .await
        .map_err(|_| failure("用量查询中断"))?
}
#[tauri::command]
pub async fn get_antigravity_usage_stats(
    r: R<'_>,
    filter: Filter,
) -> Result<super::antigravity::AntigravityUsageStats> {
    let s = r.usage.clone();
    let quota_error = s.refresh_antigravity_quota().await.err().map(|e| e.message);
    let now_ms = now();
    tokio::task::spawn_blocking(move || {
        let mut stats = s.read(|db| db.antigravity_stats(&filter, now_ms))?;
        stats.quota_error = quota_error;
        Ok(stats)
    })
    .await
    .map_err(|_| failure("Antigravity 统计查询中断"))?
}
#[tauri::command]
pub async fn get_claude_usage_stats(
    r: R<'_>,
    filter: Filter,
) -> Result<super::antigravity::AntigravityUsageStats> {
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.read(|db| db.claude_stats(&filter, now())))
        .await
        .map_err(|_| failure("Claude 统计查询中断"))?
}
#[tauri::command]
pub async fn get_claude_usage_quota(r: R<'_>, refresh: bool) -> Result<super::claude_quota::View> {
    Ok(r.usage
        .0
        .claude_quota
        .get(r.home(crate::gateway::ClientId::Claude)?, refresh)
        .await)
}
#[tauri::command]
pub async fn get_usage_logs(r: R<'_>, filter: Filter) -> Result<store::Page> {
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.read(|db| db.logs(&filter)))
        .await
        .map_err(|_| failure("日志查询中断"))?
}
#[tauri::command]
pub async fn get_usage_detail(r: R<'_>, id: String, source: Option<String>) -> Result<Record> {
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || {
        s.read(|db| {
            let mut record = db.detail(&id)?;
            if source.as_deref() == Some("proxy") {
                record.gateway_only();
            }
            Ok(record)
        })
    })
    .await
    .map_err(|_| failure("详情查询中断"))?
}
#[tauri::command]
pub fn set_usage_settings(r: R<'_>, settings: Settings) -> Result<State> {
    r.usage.configure(settings)
}
#[tauri::command]
pub async fn sync_usage(r: R<'_>, rebuild: Option<String>) -> Result<State> {
    if rebuild
        .as_deref()
        .is_some_and(|s| !matches!(s, "codex" | "claude" | "grok" | "antigravity"))
    {
        return Err(failure("未知的会话来源"));
    }
    let roots = vec![
        ("codex".into(), r.home(crate::gateway::ClientId::Codex)),
        ("claude".into(), r.home(crate::gateway::ClientId::Claude)),
        ("grok".into(), r.home(crate::gateway::ClientId::Grok)),
        ("antigravity".into(), crate::antigravity::home()),
    ];
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.sync_roots(roots, rebuild.as_deref()))
        .await
        .map_err(|_| failure("同步中断"))?
}
#[tauri::command]
pub fn get_pricing(r: R<'_>) -> Result<pricing::View> {
    Ok(r.usage.prices()?.view())
}
#[tauri::command]
pub async fn search_pricing(
    r: R<'_>,
    query: String,
    offset: usize,
    limit: usize,
) -> Result<pricing::SearchPage> {
    if query.len() > 256 || limit == 0 || limit > 100 || offset > 100_000 {
        return Err(failure("定价查询参数无效"));
    }
    let prices = r.usage.prices()?.frozen();
    tokio::task::spawn_blocking(move || prices.search(&query, offset, limit))
        .await
        .map_err(|_| failure("定价查询中断"))
}
#[tauri::command]
pub fn get_models_dev(r: R<'_>) -> Result<crate::models_dev::View> {
    Ok(r.usage.prices()?.models_dev.view())
}
#[tauri::command]
pub async fn update_models_dev(r: R<'_>, app: tauri::AppHandle) -> Result<crate::models_dev::View> {
    let before = r.usage.prices()?.models_dev.view().version;
    let view = r.usage.prices()?.models_dev.refresh(true).await?;
    if view.error.is_none() && view.version != before {
        let service = r.usage.clone();
        tokio::task::spawn_blocking(move || service.backfill_prices())
            .await
            .map_err(|_| failure("价格补算中断"))??;
    }
    crate::emit_if_main_visible(&app, crate::MainSignal::PricingState, ());
    Ok(view)
}
#[tauri::command]
pub async fn get_antigravity_vertex_models(
    r: R<'_>,
    app: tauri::AppHandle,
    refresh: bool,
) -> Result<crate::models_dev::VertexModels> {
    let catalog = &r.usage.prices()?.models_dev;
    let before = catalog.view().version;
    let result = catalog.vertex_models(refresh).await?;
    if result.error.is_none() && catalog.view().version != before {
        let service = r.usage.clone();
        tokio::task::spawn_blocking(move || service.backfill_prices())
            .await
            .map_err(|_| failure("价格补算中断"))??;
        crate::emit_if_main_visible(&app, crate::MainSignal::PricingState, ());
    }
    Ok(result)
}
#[tauri::command]
pub fn compare_model_prices(
    r: R<'_>,
    model: String,
    context: u64,
) -> Result<pricing::PriceComparison> {
    if model_id(Some(&model)).is_none() || context > 1_000_000_000_000 {
        return Err(failure("价格核对参数无效"));
    }
    Ok(r.usage.prices()?.compare(&model, context))
}
#[tauri::command]
pub async fn configure_pricing(
    r: R<'_>,
    config: pricing::Config,
    expected_revision: String,
    app: tauri::AppHandle,
) -> Result<pricing::View> {
    let result = r.usage.prices()?.configure(config, &expected_revision)?;
    let service = r.usage.clone();
    tokio::task::spawn_blocking(move || service.backfill_prices())
        .await
        .map_err(|_| failure("价格补算中断"))??;
    crate::emit_if_main_visible(&app, crate::MainSignal::UsageState, ());
    crate::emit_if_main_visible(&app, crate::MainSignal::PricingState, ());
    Ok(result)
}
#[tauri::command]
pub async fn update_pricing(r: R<'_>, app: tauri::AppHandle) -> Result<pricing::View> {
    let result = r.usage.update_prices(true).await;
    crate::emit_if_main_visible(&app, crate::MainSignal::PricingState, ());
    result
}
#[tauri::command]
pub async fn reload_pricing(r: R<'_>, app: tauri::AppHandle) -> Result<pricing::View> {
    let result = r.usage.prices()?.reload()?;
    let service = r.usage.clone();
    tokio::task::spawn_blocking(move || service.backfill_prices())
        .await
        .map_err(|_| failure("价格补算中断"))??;
    crate::emit_if_main_visible(&app, crate::MainSignal::UsageState, ());
    crate::emit_if_main_visible(&app, crate::MainSignal::PricingState, ());
    Ok(result)
}
#[tauri::command]
pub fn open_pricing_directory(r: R<'_>) -> Result<()> {
    let p = r
        .usage
        .prices()?
        .path()
        .parent()
        .ok_or_else(|| failure("定价目录无效"))?;
    open::that(p).map_err(|_| failure("无法打开定价目录"))
}

pub fn connect(app: &tauri::AppHandle, r: &Arc<Runtime>) {
    let mut events = r.usage.subscribe();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) =
            events.recv().await
        {
            crate::coalesce_gateway_events(&mut events);
            crate::emit_if_main_visible(&handle, crate::MainSignal::UsageState, ());
        }
    });
    // Price checks must not wait behind a session scan or historical compaction.
    let price_runtime = Arc::downgrade(r);
    let price_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut scheduled = tokio::time::interval(std::time::Duration::from_secs(600));
        scheduled.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        scheduled.tick().await;
        loop {
            let Some(runtime) = price_runtime.upgrade() else {
                break;
            };
            let usage = runtime.usage.clone();
            if runtime.smoke.is_none() {
                if let Ok(prices) = usage.prices() {
                    let before = prices.sync_stamp();
                    let _ = usage.update_prices(false).await;
                    let after = prices.sync_stamp();
                    if before.1 != after.1 {
                        crate::emit_if_main_visible(
                            &price_handle,
                            crate::MainSignal::PricingState,
                            (),
                        );
                    }
                }
            }
            drop(runtime);
            // Coalesce hints from unpriced requests, including high-volume streams.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            tokio::select! {
                _ = scheduled.tick() => {},
                _ = usage.wait_price_hint() => {},
            }
        }
    });
    let r = Arc::downgrade(r);
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(r) = r.upgrade() else {
                break;
            };
            if r.smoke.is_none() {
                if r.usage.settings().auto_sync {
                    let roots = vec![
                        ("codex".into(), r.home(crate::gateway::ClientId::Codex)),
                        ("claude".into(), r.home(crate::gateway::ClientId::Claude)),
                        ("grok".into(), r.home(crate::gateway::ClientId::Grok)),
                        ("antigravity".into(), crate::antigravity::home()),
                    ];
                    let s = r.usage.clone();
                    let _ = tokio::task::spawn_blocking(move || s.sync_roots(roots, None)).await;
                }
                let maintenance = r.usage.clone();
                let _ = tokio::task::spawn_blocking(move || maintenance.maintain()).await;
            }
            drop(r);
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

fn format_bill_file_name(host: Option<&str>, platform: &str) -> String {
    let cleaned = host
        .map(str::trim)
        .map(|h| h.strip_suffix(".local").unwrap_or(h))
        .map(|h| {
            h.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches(['-', '.'])
                .to_string()
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into());
    format!("{cleaned}+{platform}.csv")
}

fn default_bill_file_name() -> String {
    format_bill_file_name(
        sysinfo::System::host_name().as_deref(),
        std::env::consts::OS,
    )
}

#[tauri::command]
pub async fn export_usage_bill(
    r: R<'_>,
    app: tauri::AppHandle,
    request: super::billing::Request,
) -> Result<Option<super::billing::Exported>> {
    use tauri_plugin_dialog::DialogExt;
    request.validate()?;
    let service = r.usage.clone();
    let default_name = default_bill_file_name();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_title(if request.clean {
                "导出账单并清理明细"
            } else {
                "导出账单"
            })
            .set_file_name(&default_name)
            .add_filter("CSV", &["csv"])
            .blocking_save_file()
        else {
            return Ok(None);
        };
        let path = file
            .into_path()
            .map_err(|_| failure("请选择本地账单保存路径"))?;
        service.export_bill(&request, &path).map(Some)
    })
    .await
    .map_err(|_| failure("账单导出中断，明细清理结果请检查用量页"))?
}

#[cfg(test)]
mod tests {
    use super::format_bill_file_name;

    #[test]
    fn formats_default_bill_file_name_with_hostname_and_platform() {
        assert_eq!(
            format_bill_file_name(Some("MacBook-Pro.local"), "macos"),
            "MacBook-Pro+macos.csv"
        );
        assert_eq!(
            format_bill_file_name(Some("  my host/1  "), "windows"),
            "my-host-1+windows.csv"
        );
        assert_eq!(format_bill_file_name(None, "linux"), "localhost+linux.csv");
    }
}
