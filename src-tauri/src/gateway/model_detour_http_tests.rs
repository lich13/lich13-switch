//! HTTP/SSE regressions for cross-model side requests preserving a thread owner.
//! All providers, request bodies, responses, and thread identifiers are fixtures.
use super::*;
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    Request, Response, StatusCode,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify, Semaphore};

const V0255_MAIN_INITIAL: &[u8] = br#" { "model":"fixture-main", "stream":true,
  "input":[{"role":"user","content":"fixture-main-initial"}], "unknown":9007199254740993 } "#;
const V0255_MAIN_DURING: &[u8] = br#" { "model":"fixture-main", "stream":true,
  "input":[{"role":"assistant","content":"fixture-main-initial"},{"role":"user","content":"fixture-main-during-side"}] } "#;
const V0255_MAIN_FINAL: &[u8] = br#" { "model":"fixture-main", "stream":true,
  "input":[{"role":"assistant","content":"fixture-main-during-side"},{"role":"user","content":"fixture-main-after-side"}] } "#;
const V0255_SIDE_ONE: &[u8] = br#" { "model":"fixture-side", "stream":true,
  "input":[{"role":"assistant","content":"fixture-prior"},{"role":"user","content":"fixture-side-one"}] } "#;
const V0255_SIDE_TWO: &[u8] = br#" { "model":"fixture-side", "stream":true,
  "input":[{"role":"assistant","content":"fixture-prior"},{"role":"user","content":"fixture-side-two"}] } "#;
const V0255_FAIL_RETRY_REQUEST: &[u8] = br#" { "model":"fixture-side", "stream":true,
  "input":[{"role":"assistant","content":"fixture-prior"},{"role":"user","content":"fixture-side-fail-then-retry"}] } "#;
const V0255_INTERNAL_RETRY_REQUEST: &[u8] = br#" { "model":"fixture-side", "stream":true,
  "input":[{"role":"assistant","content":"fixture-prior"},{"role":"user","content":"fixture-side-internal-retry"}] } "#;
const V0255_CANCEL_REQUEST: &[u8] = br#" { "model":"fixture-side", "stream":true,
  "input":[{"role":"assistant","content":"fixture-prior"},{"role":"user","content":"fixture-side-cancel"}] } "#;

const V0255_MAIN_RESPONSE: &[u8] = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-main-response\"}}\r\n\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-main-response\",\"status\":\"completed\"}}\r\n\r\n";
const V0255_SIDE_ONE_RESPONSE: &[u8] = b": fixture-side-one-heartbeat\r\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-side-one-response\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-side-one-response\",\"status\":\"completed\"}}\n\n";
const V0255_SIDE_TWO_RESPONSE: &[u8] = b": fixture-side-two-heartbeat\r\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-side-two-response\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-side-two-response\",\"status\":\"completed\"}}\n\n";
const V0255_SIDE_RESPONSE: &[u8] = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fixture-side-response\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture-side-response\",\"status\":\"completed\"}}\n\n";
const V0255_FAILURE_RESPONSE: &[u8] =
    br#"{"error":{"code":"fixture_failure","message":"fixture upstream rejection"}}"#;
const V0255_INTERNAL_RETRY_FAILURE_RESPONSE: &[u8] =
    br#"{"error":{"code":"fixture_transient","message":"fixture upstream unavailable"}}"#;
const V0255_CANCEL_PREFIX: &[u8] =
    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture\",\"response\":{\"id\":\"fixture-cancel-response\"}}\n\n";

async fn v0255_body(request: Request<Incoming>) -> Vec<u8> {
    request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn v0255_request(gateway: &Gateway, thread: &str, body: Vec<u8>) -> Response<Incoming> {
    tokio::time::timeout(
        Duration::from_secs(5),
        super::v022_request(gateway, thread, body),
    )
    .await
    .expect("fixture HTTP request timed out")
}

async fn v0255_response_body(response: Response<Incoming>) -> Bytes {
    tokio::time::timeout(Duration::from_secs(5), super::v022_body(response))
        .await
        .expect("fixture HTTP response body timed out")
}

async fn v0255_setup_ports(ports: &[u16]) -> (tempfile::TempDir, Gateway, Vec<String>) {
    let urls = ports
        .iter()
        .map(|port| format!("http://127.0.0.1:{port}/v1"))
        .collect::<Vec<_>>();
    let (dir, gateway) = super::v022_fixture(urls).await;
    let ids = super::v022_ids(&gateway);
    assert_eq!(ids.len(), ports.len());
    for (index, id) in ids.iter().enumerate() {
        super::v022_update(
            &gateway,
            &dir,
            Edit::PolicyProvider {
                id: id.clone(),
                allowed_models: (index == 0).then(|| vec!["fixture-main".into()]),
                handoff_after_compaction: Some(false),
                take_new_threads: Some(index == 0),
            },
        );
    }
    super::v022_start(&gateway, &dir).await;
    (dir, gateway, ids)
}

async fn v0255_setup(main_port: u16, side_port: u16) -> (tempfile::TempDir, Gateway, Vec<String>) {
    v0255_setup_ports(&[main_port, side_port]).await
}

async fn v0255_wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture gateway state did not settle");
}

async fn v0255_wait_for_no_active_requests(gateway: &Gateway) {
    v0255_wait_until(|| {
        gateway
            .view()
            .providers
            .iter()
            .all(|provider| provider.active_requests == 0)
    })
    .await;
}

fn v0255_read_reservations(dir: &tempfile::TempDir) -> std::result::Result<Option<Value>, String> {
    let bytes = match std::fs::read(dir.path().join("thread-reservations.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read error kind={:?}", error.kind())),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        format!(
            "invalid JSON at line {} column {}",
            error.line(),
            error.column()
        )
    })
}

fn v0255_reservations(dir: &tempfile::TempDir) -> Option<Value> {
    v0255_read_reservations(dir).ok().flatten()
}

fn v0255_reservation_entry(snapshot: &Value, thread: &str, provider: &str) -> Option<Value> {
    snapshot
        .as_array()?
        .iter()
        .find(|entry| entry["thread"] == thread && entry["provider"] == provider)
        .cloned()
}

async fn v0255_wait_for_owner_reservation(
    dir: &tempfile::TempDir,
    gateway: &Gateway,
    thread: &str,
    provider: &str,
    fixture_providers: &[String],
    phase: &str,
) -> (Value, Value) {
    let mut last_snapshot = json!({"status": "not read"});
    let mut last_read = String::from("not read");
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match v0255_read_reservations(dir) {
                Ok(Some(snapshot)) => {
                    last_read = String::from("ok");
                    last_snapshot =
                        v0255_synthetic_reservation_snapshot(&snapshot, thread, fixture_providers);
                    if let Some(entry) = v0255_reservation_entry(&snapshot, thread, provider) {
                        if entry["busy"] == false {
                            break (snapshot, entry);
                        }
                    }
                }
                Ok(None) => last_read = String::from("not written yet"),
                Err(error) => last_read = error,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    match result {
        Ok(reservation) => reservation,
        Err(_) => {
            let (active, _) = gateway.0.admission.counts();
            let in_memory = fixture_providers
                .iter()
                .enumerate()
                .map(|(slot, id)| {
                    json!({
                        "provider_slot": slot,
                        "active": active.get(id).copied().unwrap_or(0),
                        "reserved": gateway.0.admission.reserved(id),
                    })
                })
                .collect::<Vec<_>>();
            panic!(
                "fixture owner reservation timed out at phase={phase}; synthetic_snapshot={last_snapshot}; snapshot_read={last_read}; in_memory={in_memory:?}; retention_error={}",
                gateway.0.admission.retention_error()
            );
        }
    }
}

fn v0255_synthetic_reservation_snapshot(
    snapshot: &Value,
    thread: &str,
    fixture_providers: &[String],
) -> Value {
    let Some(entries) = snapshot.as_array() else {
        return json!({"shape": "not an array"});
    };
    let fixture_entries = entries
        .iter()
        .filter_map(|entry| {
            if entry.get("thread").and_then(Value::as_str) != Some(thread) {
                return None;
            }
            let provider = entry.get("provider").and_then(Value::as_str);
            let provider_slot = fixture_providers
                .iter()
                .position(|id| Some(id.as_str()) == provider);
            Some(json!({
                "provider_slot": provider_slot,
                "busy": entry.get("busy").cloned().unwrap_or(Value::Null),
                "idle_since": entry.get("idle_since").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect::<Vec<_>>();
    json!({"fixture_entries": fixture_entries})
}

fn v0255_usage_rows(usage: &crate::usage::Service) -> Vec<crate::usage::model::Record> {
    usage
        .query(|store| Ok(store.logs(&crate::usage::model::Filter::default())?.rows))
        .unwrap()
}

async fn v0255_wait_for_usage(
    usage: &crate::usage::Service,
    count: usize,
) -> Vec<crate::usage::model::Record> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = v0255_usage_rows(usage);
            if rows.len() >= count {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture usage rows were not persisted")
}

async fn v0255_wait_for_detour_usage(
    usage: &crate::usage::Service,
    provider: &str,
    model: &str,
    count: usize,
) -> Vec<crate::usage::model::Record> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = v0255_usage_rows(usage);
            let detours = rows
                .iter()
                .filter(|row| {
                    row.final_attempt().is_some_and(|attempt| {
                        attempt.provider.as_deref() == Some(provider)
                            && attempt.requested_model.as_deref() == Some(model)
                    })
                })
                .count();
            if detours >= count {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture detour usage rows were not persisted")
}

struct V0255UpstreamBodyDropped(Arc<Notify>);

impl Drop for V0255UpstreamBodyDropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn v0255_open_stream(
    release: Arc<Notify>,
    cancelled: Arc<Notify>,
) -> super::super::replay::WireBody {
    let guard = V0255UpstreamBodyDropped(cancelled);
    let chunks = async_stream::try_stream! {
        let _guard = guard;
        yield Frame::data(Bytes::from_static(V0255_CANCEL_PREFIX));
        release.notified().await;
        loop {
            yield Frame::data(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"fixture tail\"}\n\n",
            ));
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    };
    StreamBody::new(chunks)
        .map_err(|error: std::io::Error| -> super::super::connector::BoxError { error.into() })
        .boxed_unsync()
}

#[tokio::test]
async fn v0255_http_parallel_side_models_preserve_owner_during_and_after_completion() {
    let main_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let main_seen = main_requests.clone();
    let main_port = super::v022_server(move |request| {
        let seen = main_seen.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body);
            super::v022_sse_response(Bytes::from_static(V0255_MAIN_RESPONSE))
        }
    })
    .await;

    let side_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let side_seen = side_requests.clone();
    let (arrived_tx, mut arrived_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let side_gate = Arc::new(Semaphore::new(0));
    let side_release = side_gate.clone();
    let side_port = super::v022_server(move |request| {
        let seen = side_seen.clone();
        let arrived = arrived_tx.clone();
        let release = side_release.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body.clone());
            if body.as_slice() == V0255_SIDE_ONE || body.as_slice() == V0255_SIDE_TWO {
                arrived.send(body.clone()).unwrap();
                let _permit = release.acquire_owned().await.unwrap();
                let response = if body.as_slice() == V0255_SIDE_ONE {
                    V0255_SIDE_ONE_RESPONSE
                } else {
                    V0255_SIDE_TWO_RESPONSE
                };
                super::v022_sse_response(Bytes::from_static(response))
            } else {
                // If a regression routes the interleaved main request to B,
                // answer it promptly so the assertion can show the wrong route.
                super::v022_sse_response(Bytes::from_static(V0255_MAIN_RESPONSE))
            }
        }
    })
    .await;

    let (dir, gateway, ids) = v0255_setup(main_port, side_port).await;
    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    let usage = crate::usage::Service::new(dir.path());
    gateway.set_usage(usage.clone());
    let thread = "fixture-v0255-http-main-thread";
    let thread_hash = crate::storage::digest(thread.as_bytes());

    let initial = v0255_request(&gateway, thread, V0255_MAIN_INITIAL.to_vec()).await;
    assert_eq!(initial.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(initial).await,
        Bytes::from_static(V0255_MAIN_RESPONSE),
        "initial main SSE bytes must pass through unchanged"
    );
    assert_eq!(
        *main_requests.lock().unwrap(),
        vec![V0255_MAIN_INITIAL.to_vec()]
    );
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    let (initial_reservations, _) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "parallel side requests: initial owner reservation",
    )
    .await;
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);

    let first_gateway = gateway.clone();
    let first_thread = thread.to_owned();
    let first_side = tokio::spawn(async move {
        v0255_request(&first_gateway, &first_thread, V0255_SIDE_ONE.to_vec()).await
    });
    let second_gateway = gateway.clone();
    let second_thread = thread.to_owned();
    let second_side = tokio::spawn(async move {
        v0255_request(&second_gateway, &second_thread, V0255_SIDE_TWO.to_vec()).await
    });

    let mut arrived = vec![
        tokio::time::timeout(Duration::from_secs(5), arrived_rx.recv())
            .await
            .expect("first side request did not reach B")
            .expect("side fixture channel closed"),
        tokio::time::timeout(Duration::from_secs(5), arrived_rx.recv())
            .await
            .expect("second side request did not reach B")
            .expect("side fixture channel closed"),
    ];
    arrived.sort();
    let mut expected_side_requests = vec![V0255_SIDE_ONE.to_vec(), V0255_SIDE_TWO.to_vec()];
    expected_side_requests.sort();
    assert_eq!(arrived, expected_side_requests);
    assert!(
        !first_side.is_finished(),
        "first side response must stay held"
    );
    assert!(
        !second_side.is_finished(),
        "second side response must stay held"
    );
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );

    let during = v0255_request(&gateway, thread, V0255_MAIN_DURING.to_vec()).await;
    assert_eq!(during.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(during).await,
        Bytes::from_static(V0255_MAIN_RESPONSE),
        "interleaved main SSE bytes must pass through unchanged"
    );
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone()),
        "the original main provider must retain ownership while both side responses are delayed"
    );

    side_gate.add_permits(2);
    let first = tokio::time::timeout(Duration::from_secs(5), first_side)
        .await
        .expect("first side request did not finish after release")
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), second_side)
        .await
        .expect("second side request did not finish after release")
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first_bytes = v0255_response_body(first).await;
    let second_bytes = v0255_response_body(second).await;
    assert_eq!(
        first_bytes,
        Bytes::from_static(V0255_SIDE_ONE_RESPONSE),
        "the first delayed side SSE response must preserve its exact wire bytes"
    );
    assert_eq!(
        second_bytes,
        Bytes::from_static(V0255_SIDE_TWO_RESPONSE),
        "the second delayed side SSE response must preserve its exact wire bytes"
    );
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );

    let final_main = v0255_request(&gateway, thread, V0255_MAIN_FINAL.to_vec()).await;
    assert_eq!(final_main.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(final_main).await,
        Bytes::from_static(V0255_MAIN_RESPONSE),
        "the next main SSE bytes must pass through unchanged"
    );
    v0255_wait_for_no_active_requests(&gateway).await;
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(
        gateway.0.admission.reserved(&ids[1]),
        0,
        "model detours must not add the side provider to the 300-second reservations"
    );
    let reservations = v0255_reservations(&dir).unwrap();
    assert_eq!(reservations.as_array().unwrap().len(), 1);
    assert!(v0255_reservation_entry(&reservations, &thread_hash, &ids[1]).is_none());

    assert_eq!(
        *main_requests.lock().unwrap(),
        vec![
            V0255_MAIN_INITIAL.to_vec(),
            V0255_MAIN_DURING.to_vec(),
            V0255_MAIN_FINAL.to_vec(),
        ],
        "all main-model request bytes must stay on A"
    );
    let mut observed_side = side_requests.lock().unwrap().clone();
    observed_side.sort();
    assert_eq!(observed_side, expected_side_requests);

    let usage_rows = v0255_wait_for_detour_usage(&usage, &ids[1], "fixture-side", 2).await;
    let detours = usage_rows
        .iter()
        .filter(|row| {
            row.final_attempt().is_some_and(|attempt| {
                attempt.provider.as_deref() == Some(ids[1].as_str())
                    && attempt.requested_model.as_deref() == Some("fixture-side")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(detours.len(), 2);
    for row in detours {
        let attempt = row.final_attempt().unwrap();
        assert_eq!(
            serde_json::to_value(attempt.routing_decision).unwrap(),
            json!("model_detour_owner_preserved")
        );
    }

    // The initial snapshot proves A got the reservation before either side call.
    assert_eq!(
        v0255_reservation_entry(&initial_reservations, &thread_hash, &ids[0]).unwrap()["busy"],
        false
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0255_http_failure_client_retry_and_cancel_leave_owner_reservation_unchanged() {
    let main_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let main_seen = main_requests.clone();
    let main_port = super::v022_server(move |request| {
        let seen = main_seen.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body);
            super::v022_sse_response(Bytes::from_static(V0255_MAIN_RESPONSE))
        }
    })
    .await;

    let side_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let side_seen = side_requests.clone();
    let retry_attempts = Arc::new(AtomicUsize::new(0));
    let retry_count = retry_attempts.clone();
    let cancel_release = Arc::new(Notify::new());
    let release_upstream = cancel_release.clone();
    let cancelled_upstream = Arc::new(Notify::new());
    let cancelled_signal = cancelled_upstream.clone();
    let side_port = super::v022_server(move |request| {
        let seen = side_seen.clone();
        let attempts = retry_count.clone();
        let release = release_upstream.clone();
        let cancelled = cancelled_signal.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body.clone());
            if body.as_slice() == V0255_FAIL_RETRY_REQUEST {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    super::v022_json_response(StatusCode::BAD_REQUEST, V0255_FAILURE_RESPONSE)
                } else {
                    super::v022_sse_response(Bytes::from_static(V0255_SIDE_RESPONSE))
                }
            } else if body.as_slice() == V0255_CANCEL_REQUEST {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(v0255_open_stream(release, cancelled))
                    .unwrap()
            } else {
                super::v022_json_response(
                    StatusCode::BAD_REQUEST,
                    Bytes::from_static(
                        br#"{"error":{"code":"fixture_unexpected","message":"unexpected fixture request"}}"#,
                    ),
                )
            }
        }
    })
    .await;

    let (dir, gateway, ids) = v0255_setup(main_port, side_port).await;
    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    let thread = "fixture-v0255-http-failure-thread";
    let thread_hash = crate::storage::digest(thread.as_bytes());
    let initial = v0255_request(&gateway, thread, V0255_MAIN_INITIAL.to_vec()).await;
    assert_eq!(initial.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(initial).await,
        Bytes::from_static(V0255_MAIN_RESPONSE)
    );
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    let (baseline, owner_reservation) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "failure retry cancellation: initial owner reservation",
    )
    .await;
    assert_eq!(owner_reservation["busy"], false);
    let baseline_idle_since = owner_reservation["idle_since"].clone();
    assert!(!baseline_idle_since.is_null());
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);

    let failed = v0255_request(&gateway, thread, V0255_FAIL_RETRY_REQUEST.to_vec()).await;
    assert_eq!(failed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        v0255_response_body(failed).await,
        Bytes::from_static(V0255_FAILURE_RESPONSE),
        "the failed side response bytes must pass through unchanged"
    );
    v0255_wait_for_no_active_requests(&gateway).await;
    assert_eq!(retry_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);
    let (after_failure, failure_owner) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "failure retry cancellation: after failed side request",
    )
    .await;
    assert_eq!(failure_owner["idle_since"], baseline_idle_since);
    assert!(v0255_reservation_entry(&after_failure, &thread_hash, &ids[1]).is_none());

    let retried = v0255_request(&gateway, thread, V0255_FAIL_RETRY_REQUEST.to_vec()).await;
    assert_eq!(retried.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(retried).await,
        Bytes::from_static(V0255_SIDE_RESPONSE),
        "a later retry must preserve the successful side SSE bytes"
    );
    v0255_wait_for_no_active_requests(&gateway).await;
    assert_eq!(retry_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);
    let (after_retry, retry_owner) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "failure retry cancellation: after retry",
    )
    .await;
    assert_eq!(retry_owner["idle_since"], baseline_idle_since);
    assert!(v0255_reservation_entry(&after_retry, &thread_hash, &ids[1]).is_none());

    let cancellation = v0255_request(&gateway, thread, V0255_CANCEL_REQUEST.to_vec()).await;
    assert_eq!(cancellation.status(), StatusCode::OK);
    let mut body = cancellation.into_body();
    let prefix = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while bytes.len() < V0255_CANCEL_PREFIX.len() {
            let frame = body
                .frame()
                .await
                .expect("cancel fixture stream ended")
                .unwrap();
            if let Some(data) = frame.data_ref() {
                bytes.extend_from_slice(data);
            }
        }
        bytes
    })
    .await
    .expect("cancel fixture stream did not produce its first SSE event");
    assert_eq!(prefix, V0255_CANCEL_PREFIX);
    let upstream_cancelled = cancelled_upstream.notified();
    drop(body);
    cancel_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), upstream_cancelled)
        .await
        .expect("HTTP cancellation did not drop the held upstream body");
    v0255_wait_for_no_active_requests(&gateway).await;
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);
    let (after_cancel, cancel_owner) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "failure retry cancellation: after cancellation",
    )
    .await;
    assert_eq!(cancel_owner["idle_since"], baseline_idle_since);
    assert!(v0255_reservation_entry(&after_cancel, &thread_hash, &ids[1]).is_none());

    assert_eq!(
        *main_requests.lock().unwrap(),
        vec![V0255_MAIN_INITIAL.to_vec()]
    );
    assert_eq!(
        *side_requests.lock().unwrap(),
        vec![
            V0255_FAIL_RETRY_REQUEST.to_vec(),
            V0255_FAIL_RETRY_REQUEST.to_vec(),
            V0255_CANCEL_REQUEST.to_vec(),
        ],
        "failure, retry, and cancellation must use the exact side request bytes"
    );
    assert_eq!(
        baseline.as_array().unwrap().len(),
        1,
        "only the original owner may have an idle reservation"
    );
    gateway.stop().await.unwrap();
}

#[tokio::test]
async fn v0255_http_internal_retry_keeps_one_request_and_preserves_detour_owner() {
    let main_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let main_seen = main_requests.clone();
    let main_port = super::v022_server(move |request| {
        let seen = main_seen.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body);
            super::v022_sse_response(Bytes::from_static(V0255_MAIN_RESPONSE))
        }
    })
    .await;

    let first_candidate_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let first_candidate_seen = first_candidate_requests.clone();
    let first_candidate_port = super::v022_server(move |request| {
        let seen = first_candidate_seen.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body);
            super::v022_json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                V0255_INTERNAL_RETRY_FAILURE_RESPONSE,
            )
        }
    })
    .await;

    let fallback_requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let fallback_seen = fallback_requests.clone();
    let fallback_port = super::v022_server(move |request| {
        let seen = fallback_seen.clone();
        async move {
            let body = v0255_body(request).await;
            seen.lock().unwrap().push(body);
            super::v022_sse_response(Bytes::from_static(V0255_SIDE_RESPONSE))
        }
    })
    .await;

    let (dir, gateway, ids) =
        v0255_setup_ports(&[main_port, first_candidate_port, fallback_port]).await;
    assert!(gateway.view().settings.max_retries > 0);
    assert_eq!(gateway.view().settings.thread_idle_release_seconds, 300);
    let usage = crate::usage::Service::new(dir.path());
    gateway.set_usage(usage.clone());
    let thread = "fixture-v0255-http-internal-retry-thread";
    let thread_hash = crate::storage::digest(thread.as_bytes());

    let initial = v0255_request(&gateway, thread, V0255_MAIN_INITIAL.to_vec()).await;
    assert_eq!(initial.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(initial).await,
        Bytes::from_static(V0255_MAIN_RESPONSE)
    );
    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    let (baseline, owner_reservation) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "internal retry: initial owner reservation",
    )
    .await;
    assert_eq!(owner_reservation["busy"], false);
    let baseline_idle_since = owner_reservation["idle_since"].clone();
    assert!(!baseline_idle_since.is_null());
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);
    assert_eq!(gateway.0.admission.reserved(&ids[2]), 0);

    let response = v0255_request(&gateway, thread, V0255_INTERNAL_RETRY_REQUEST.to_vec()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        v0255_response_body(response).await,
        Bytes::from_static(V0255_SIDE_RESPONSE),
        "the fallback provider response bytes must pass through unchanged"
    );
    v0255_wait_for_no_active_requests(&gateway).await;

    let rows = v0255_wait_for_usage(&usage, 2).await;
    let record = rows
        .iter()
        .find(|record| {
            record.attempts.len() == 2
                && record.attempts[0].provider.as_deref() == Some(ids[1].as_str())
                && record.attempts[1].provider.as_deref() == Some(ids[2].as_str())
        })
        .expect("the internal retry must stay in one usage record");
    assert!(record.completed);
    assert_ne!(record.attempts[0].id, record.attempts[1].id);
    assert_eq!(
        record.attempts[0].requested_model.as_deref(),
        Some("fixture-side")
    );
    assert_eq!(
        record.attempts[1].requested_model.as_deref(),
        Some("fixture-side")
    );
    assert_eq!(record.attempts[0].status, Some(503));
    assert_eq!(record.attempts[1].status, Some(200));
    for attempt in &record.attempts {
        assert_eq!(
            serde_json::to_value(attempt.routing_decision).unwrap(),
            json!("model_detour_owner_preserved")
        );
    }
    assert_eq!(
        *first_candidate_requests.lock().unwrap(),
        vec![V0255_INTERNAL_RETRY_REQUEST.to_vec()]
    );
    assert_eq!(
        *fallback_requests.lock().unwrap(),
        vec![V0255_INTERNAL_RETRY_REQUEST.to_vec()],
        "both providers must receive the same original HTTP request bytes"
    );
    assert_eq!(
        *main_requests.lock().unwrap(),
        vec![V0255_MAIN_INITIAL.to_vec()]
    );

    assert_eq!(
        gateway.0.compaction.owner(&thread_hash),
        Some(ids[0].clone())
    );
    assert_eq!(gateway.0.admission.reserved(&ids[0]), 1);
    assert_eq!(gateway.0.admission.reserved(&ids[1]), 0);
    assert_eq!(gateway.0.admission.reserved(&ids[2]), 0);
    let (after_retry, retry_owner) = v0255_wait_for_owner_reservation(
        &dir,
        &gateway,
        &thread_hash,
        &ids[0],
        &ids,
        "internal retry: after retry",
    )
    .await;
    assert_eq!(retry_owner["idle_since"], baseline_idle_since);
    assert!(v0255_reservation_entry(&after_retry, &thread_hash, &ids[1]).is_none());
    assert!(v0255_reservation_entry(&after_retry, &thread_hash, &ids[2]).is_none());
    assert_eq!(
        baseline.as_array().unwrap().len(),
        1,
        "only the original owner may have an idle reservation"
    );
    gateway.stop().await.unwrap();
}
