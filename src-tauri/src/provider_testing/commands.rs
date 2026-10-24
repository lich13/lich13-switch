use super::*;
use crate::Runtime;
use tauri::{Emitter, Manager};
type R<'a> = tauri::State<'a, Arc<Runtime>>;
#[tauri::command]
pub fn provider_test_state(r: R<'_>, client_id: ClientId, provider_id: String) -> Result<State> {
    r.provider_tests.state(client_id, &provider_id)
}
#[tauri::command]
pub fn provider_test_start(r: R<'_>, request: Request) -> Result<Job> {
    r.provider_tests.start(request)
}
#[tauri::command]
pub fn provider_test_query(r: R<'_>, id: String) -> Result<Detail> {
    r.provider_tests.detail(&id)
}
#[tauri::command]
pub fn provider_test_cancel(r: R<'_>, id: String) -> Result<()> {
    r.provider_tests.cancel(&id)
}
#[tauri::command]
pub fn provider_test_retry(r: R<'_>, id: String) -> Result<Job> {
    r.provider_tests.retry(&id)
}
#[tauri::command]
pub fn provider_test_set_detection(
    r: R<'_>,
    provider_id: String,
    detection: Detection,
    expected_revision: String,
) -> Result<()> {
    r.provider_tests
        .set_detection(&provider_id, detection, &expected_revision)
}
#[tauri::command]
pub fn provider_test_clear_mark(
    r: R<'_>,
    provider_id: String,
    expected_revision: String,
) -> Result<()> {
    r.provider_tests
        .clear_mark(&provider_id, &expected_revision)
}
#[tauri::command]
pub async fn provider_test_models(
    r: R<'_>,
    client_id: ClientId,
    provider_id: String,
    expected_revision: String,
    endpoint: Option<String>,
    refresh: Option<bool>,
) -> Result<Models> {
    r.provider_tests
        .models(
            client_id,
            &provider_id,
            &expected_revision,
            endpoint.as_deref(),
            refresh.unwrap_or(false),
        )
        .await
}
#[tauri::command]
pub async fn provider_test_update_bank(r: R<'_>) -> Result<Value> {
    r.provider_tests.update_bank().await
}
pub fn connect(app: &tauri::AppHandle) {
    let service = app.state::<Arc<Runtime>>().provider_tests.clone();
    let mut events = service.subscribe();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = events.recv().await {
            let _ = handle.emit("provider-tests", ());
        }
    });
    service.run_scheduler();
}
