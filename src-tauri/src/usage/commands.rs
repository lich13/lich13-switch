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
pub async fn get_usage_heatmap(r: R<'_>, filter: Filter) -> Result<Vec<store::Point>> {
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.read(|db| db.heatmap(&filter)))
        .await
        .map_err(|_| failure("热力图查询中断"))?
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
        .is_some_and(|s| !matches!(s, "codex" | "claude"))
    {
        return Err(failure("未知的会话来源"));
    }
    let roots = vec![
        ("codex".into(), r.home(crate::gateway::ClientId::Codex)?),
        ("claude".into(), r.home(crate::gateway::ClientId::Claude)?),
    ];
    let s = r.usage.clone();
    tokio::task::spawn_blocking(move || s.sync(&roots, rebuild.as_deref()))
        .await
        .map_err(|_| failure("同步中断"))?
}
#[tauri::command]
pub fn get_pricing(r: R<'_>) -> Result<pricing::View> {
    Ok(r.usage.prices()?.view())
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
    tokio::task::spawn_blocking(move || {
        service.query(|db| db.backfill(service.prices()?, &service.settings().multiplier))
    })
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
    tokio::task::spawn_blocking(move || {
        service.query(|db| db.backfill(service.prices()?, &service.settings().multiplier))
    })
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
    let r = Arc::downgrade(r);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let Some(r) = r.upgrade() else {
                break;
            };
            if r.smoke.is_none() {
                let maintenance = r.usage.clone();
                let _ = tokio::task::spawn_blocking(move || maintenance.maintain()).await;
                if r.usage.settings().auto_sync {
                    let roots = [
                        r.home(crate::gateway::ClientId::Codex)
                            .map(|p| ("codex".into(), p)),
                        r.home(crate::gateway::ClientId::Claude)
                            .map(|p| ("claude".into(), p)),
                    ]
                    .into_iter()
                    .collect::<Result<Vec<_>>>();
                    if let Ok(roots) = roots {
                        let s = r.usage.clone();
                        let _ = tokio::task::spawn_blocking(move || s.sync(&roots, None)).await;
                    }
                }
                let _ = r.usage.update_prices(false).await;
                let _ = handle.emit("pricing-state", ());
            }
            drop(r);
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}
