use super::*;
use crate::Runtime;
use tauri::{Emitter, Manager};
type R<'a> = tauri::State<'a, Arc<Runtime>>;
#[tauri::command]
pub fn provider_test_state(r: R<'_>) -> State {
    r.provider_tests.state()
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
pub fn provider_test_batch(r: R<'_>, requests: Vec<Request>) -> Result<Value> {
    r.provider_tests.batch(requests)
}
#[tauri::command]
pub fn provider_test_stop_batch(r: R<'_>, id: String) -> Result<()> {
    r.provider_tests.cancel_batch(&id)
}
#[tauri::command]
pub fn provider_test_schedule(r: R<'_>, requests: Vec<Request>, at: i64) -> Result<Schedule> {
    r.provider_tests.schedule(requests, at)
}
#[tauri::command]
pub fn provider_test_cancel_schedule(r: R<'_>, id: String) -> Result<()> {
    r.provider_tests.cancel_schedule(&id)
}
#[tauri::command]
pub fn provider_test_next_cycle(
    r: R<'_>,
    client_id: ClientId,
    providers: Vec<String>,
) -> Result<i64> {
    r.provider_tests.next_cycle(client_id, &providers)
}
#[tauri::command]
pub fn provider_test_set_detection(
    r: R<'_>,
    detection: Detection,
    expected_revision: String,
) -> Result<()> {
    r.provider_tests
        .set_detection(detection, &expected_revision)
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
    fingerprint: bool,
) -> Result<Models> {
    r.provider_tests
        .models(client_id, &provider_id, &expected_revision, fingerprint)
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
