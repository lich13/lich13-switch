use super::*;
use crate::Runtime;
use tauri::Manager;
type R<'a> = tauri::State<'a, Arc<Runtime>>;
#[tauri::command]
pub fn provider_test_state(r: R<'_>, client_id: ClientId, provider_id: String) -> Result<State> {
    r.provider_tests.state(client_id, &provider_id)
}
#[tauri::command]
pub async fn provider_test_start(r: R<'_>, request: Request) -> Result<Job> {
    let service = r.provider_tests.clone();
    tokio::task::spawn_blocking(move || service.start(request))
        .await
        .map_err(|_| failure("测试资源加载失败"))?
}
#[tauri::command]
pub fn provider_test_save_prompt(r: R<'_>, prompt: String) -> Result<()> {
    r.provider_tests.set_prompt(prompt)
}
#[tauri::command]
pub fn provider_test_set_platform(
    r: R<'_>,
    client_id: ClientId,
    platform: PlatformSettings,
) -> Result<()> {
    r.provider_tests.set_platform(client_id, platform)
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
    client_id: ClientId,
    provider_id: String,
    detection: Detection,
    expected_revision: String,
) -> Result<()> {
    r.provider_tests
        .set_detection(client_id, &provider_id, detection, &expected_revision)
}
#[tauri::command]
pub fn provider_test_clear_mark(
    r: R<'_>,
    client_id: ClientId,
    provider_id: String,
    expected_revision: String,
) -> Result<()> {
    r.provider_tests
        .clear_mark(client_id, &provider_id, &expected_revision)
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
pub async fn provider_test_update_bank(r: R<'_>, engine: Engine) -> Result<Value> {
    match engine {
        Engine::ModelTrace => r.provider_tests.update_bank().await,
        Engine::LmDetector => r.provider_tests.update_lm_bank().await,
    }
}
pub fn connect(app: &tauri::AppHandle) {
    let service = app.state::<Arc<Runtime>>().provider_tests.clone();
    let mut events = service.subscribe();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) = events.recv().await {
            crate::emit_if_main_visible(&handle, "provider-tests", ());
        }
    });
    service.run_scheduler();
}
