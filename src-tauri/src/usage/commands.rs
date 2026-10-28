use super::{model::*, pricing, store, State};
use crate::{storage::Result, Runtime};
use std::sync::Arc;
use tauri::Emitter;
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
pub async fn get_usage_logs(r: R<'_>, filter: Filter) -> Result<store::Page> {
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.read(|db| db.logs(&filter)))
        .await
        .map_err(|_| failure("日志查询中断"))?
}
#[tauri::command]
pub async fn get_usage_interruptions(
    r: R<'_>,
    filter: Filter,
) -> Result<super::interruptions::Summary> {
    let service = r.usage.clone();
    tokio::task::spawn_blocking(move || service.read(|store| store.interruptions(&filter)))
        .await
        .map_err(|_| failure("中断统计已停止"))?
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
pub fn get_models_dev(r: R<'_>) -> Result<crate::models_dev::View> {
    Ok(r.usage.prices()?.models_dev.view())
}
#[tauri::command]
pub async fn update_models_dev(r: R<'_>, app: tauri::AppHandle) -> Result<crate::models_dev::View> {
    let view = r.usage.prices()?.models_dev.refresh(true).await?;
    let _ = app.emit("pricing-state", ());
    Ok(view)
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
    tokio::task::spawn_blocking(move || service.query(|db| db.backfill(service.prices()?)))
        .await
        .map_err(|_| failure("价格补算中断"))??;
    let _ = app.emit("usage-state", ());
    let _ = app.emit("pricing-state", ());
    Ok(result)
}
#[tauri::command]
pub async fn update_pricing(r: R<'_>, app: tauri::AppHandle) -> Result<pricing::View> {
    let result = r.usage.update_prices(true).await;
    let _ = app.emit("pricing-state", ());
    result
}
#[tauri::command]
pub async fn reload_pricing(r: R<'_>, app: tauri::AppHandle) -> Result<pricing::View> {
    let result = r.usage.prices()?.reload()?;
    let service = r.usage.clone();
    tokio::task::spawn_blocking(move || service.query(|db| db.backfill(service.prices()?)))
        .await
        .map_err(|_| failure("价格补算中断"))??;
    let _ = app.emit("usage-state", ());
    let _ = app.emit("pricing-state", ());
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
            let _ = handle.emit("usage-state", ());
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
                    let before = prices.view();
                    let _ = prices.update(false).await;
                    let after = prices.view();
                    if before.version != after.version {
                        usage.schedule_price_backfill();
                    }
                    if before.checked_at != after.checked_at {
                        let _ = price_handle.emit("pricing-state", ());
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
                let maintenance = r.usage.clone();
                let _ = tokio::task::spawn_blocking(move || maintenance.maintain()).await;
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
            }
            drop(r);
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
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
    let mut roots = Vec::new();
    for client in &request.clients {
        roots.push((
            client.clone(),
            if client == "claude" {
                r.home(crate::gateway::ClientId::Claude)?
            } else {
                crate::antigravity::home()?
            },
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let Some(file) = app
            .dialog()
            .file()
            .set_title("导出账单并清理明细")
            .set_file_name("usage-bill.csv")
            .add_filter("CSV", &["csv"])
            .blocking_save_file()
        else {
            return Ok(None);
        };
        let path = file
            .into_path()
            .map_err(|_| failure("请选择本地账单保存路径"))?;
        service.export_bill(&request, &roots, &path).map(Some)
    })
    .await
    .map_err(|_| failure("账单导出中断，明细清理结果请检查用量页"))?
}
