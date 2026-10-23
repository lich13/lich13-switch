use super::super::{
    model::{Edit, Provider},
    replay, Gateway,
};
use super::{digest, now, pin_key, Batch, Measurement, Pin, Service, MAX_PINS};
use crate::{
    errors::code,
    storage::{self, faults},
};
use hyper::{body::Incoming, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo},
};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn fixture_provider(base_url: &str) -> Provider {
    Provider {
        id: "fixture-provider".into(),
        name: "fixture-provider".into(),
        base_url: base_url.into(),
        token: "fixture-token".into(),
        queued: true,
        version: "fixture-version".into(),
        max_concurrency: 0,
        max_rpm: 0,
        allowed_models: None,
        handoff_after_compaction: false,
        take_new_threads: false,
    }
}

fn pin_snapshot(entries: &HashMap<String, Pin>) -> BTreeMap<String, (String, u64)> {
    entries
        .iter()
        .map(|(key, pin)| (key.clone(), (pin.endpoint.clone(), pin.updated)))
        .collect()
}

fn write_entries(path: &Path, entries: &HashMap<String, Pin>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(entries).unwrap()).unwrap();
}

fn read_snapshot(path: &Path) -> BTreeMap<String, (String, u64)> {
    let entries: HashMap<String, Pin> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    pin_snapshot(&entries)
}

fn set_measurements(
    service: &Service,
    provider: &Provider,
    urls: &[String],
    rows: Vec<Measurement>,
) {
    service.samples.lock().unwrap().insert(
        provider.id.clone(),
        Batch {
            config: Service::config(provider, urls),
            checked: Instant::now(),
            rows,
        },
    );
}

fn fixture_measurement(base_url: &str, latency_ms: u64) -> Measurement {
    Measurement {
        base_url: base_url.to_owned(),
        latency_ms: Some(latency_ms),
        status: Some(200),
        error: None,
        checked_at: now(),
    }
}

fn assert_same_snapshot(
    actual: BTreeMap<String, (String, u64)>,
    expected: &BTreeMap<String, (String, u64)>,
    message: &str,
) {
    assert!(actual == *expected, "{message}");
}

#[test]
fn failed_pin_write_for_another_thread_does_not_block_existing_selection() {
    let primary = "https://primary.example.invalid/v1";
    let provider = fixture_provider(primary);
    let urls = vec![primary.to_owned()];
    let existing_thread = "fixture-existing-thread";
    let failing_thread = "fixture-failing-thread";
    let existing_key = pin_key(&provider, "thread", existing_thread);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state/endpoint-owners.json");
    let entries = HashMap::from([(
        existing_key,
        Pin {
            endpoint: digest(primary),
            updated: now(),
        },
    )]);
    write_entries(&path, &entries);
    let service = Service::new(path.clone());

    assert_eq!(
        service
            .select(&provider, &urls, Some(existing_thread), None, false)
            .unwrap(),
        primary
    );

    // An obstruction at the persistence target keeps every later write failing.
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let first_error = service
        .select(&provider, &urls, Some(failing_thread), None, false)
        .unwrap_err();
    assert_eq!(first_error.code, code::ENDPOINT_AFFINITY);

    let selected = service
        .select(&provider, &urls, Some(existing_thread), None, false)
        .expect("another thread's failed pin write must not block an existing pin");
    assert_eq!(selected, primary);
}

#[test]
fn stale_pin_refresh_failure_still_selects_the_existing_endpoint() {
    let primary = "https://primary.example.invalid/v1";
    let fallback = "https://fallback.example.invalid/v1";
    let provider = fixture_provider(fallback);
    let urls = vec![primary.to_owned(), fallback.to_owned()];
    let thread = "fixture-thread";
    let key = pin_key(&provider, "thread", thread);

    for point in [faults::Point::Replace, faults::Point::DirectorySync] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("endpoint-owners.json");
        let entries = HashMap::from([(
            key.clone(),
            Pin {
                endpoint: digest(primary),
                updated: now().saturating_sub(60_001),
            },
        )]);
        write_entries(&path, &entries);
        let service = Service::new(path);

        let injected = faults::inject(point);
        let selected = service
            .select(&provider, &urls, Some(thread), None, false)
            .expect("refresh failure must preserve and select the existing endpoint");
        drop(injected);

        assert_eq!(selected, primary);
    }
}

#[test]
fn existing_pin_selection_returns_while_another_writer_is_held() {
    let primary = "https://primary.example.invalid/v1";
    let provider = fixture_provider(primary);
    let urls = vec![primary.to_owned()];
    let thread = "fixture-stale-thread";
    let key = pin_key(&provider, "thread", thread);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("endpoint-owners.json");
    write_entries(
        &path,
        &HashMap::from([(
            key,
            Pin {
                endpoint: digest(primary),
                updated: now().saturating_sub(60_001),
            },
        )]),
    );
    let service = Arc::new(Service::new(path));
    let writer = service.write.lock().unwrap();
    let (selected_tx, selected_rx) = std::sync::mpsc::channel();
    let worker_service = service.clone();
    let worker_provider = provider.clone();
    let worker_urls = urls.clone();
    let worker = std::thread::spawn(move || {
        let result =
            worker_service.select(&worker_provider, &worker_urls, Some(thread), None, false);
        selected_tx.send(result).unwrap();
    });

    let selected_while_locked = selected_rx.recv_timeout(Duration::from_secs(2));
    drop(writer);
    worker.join().unwrap();

    let selected = selected_while_locked
        .expect("existing pin selection blocked on another writer")
        .expect("existing pin must remain selectable while refresh is skipped");
    assert_eq!(selected, primary);
}

#[test]
fn failed_pending_keys_retry_independently_without_overwriting_each_other() {
    let primary = "https://primary.example.invalid/v1";
    let fallback = "https://fallback.example.invalid/v1";
    let provider = fixture_provider(primary);
    let urls = vec![primary.to_owned(), fallback.to_owned()];
    let first_thread = "fixture-first-thread";
    let second_thread = "fixture-second-thread";
    let first_key = pin_key(&provider, "thread", first_thread);
    let second_key = pin_key(&provider, "thread", second_thread);

    for point in [faults::Point::Replace, faults::Point::DirectorySync] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("endpoint-owners.json");
        let service = Service::new(path.clone());
        set_measurements(
            &service,
            &provider,
            &urls,
            vec![
                fixture_measurement(primary, 10),
                fixture_measurement(fallback, 40),
            ],
        );
        let injected = faults::inject(point);
        let first_error = service
            .select(&provider, &urls, Some(first_thread), None, true)
            .unwrap_err();
        drop(injected);
        assert_eq!(first_error.code, code::ENDPOINT_AFFINITY);

        set_measurements(
            &service,
            &provider,
            &urls,
            vec![
                fixture_measurement(primary, 40),
                fixture_measurement(fallback, 10),
            ],
        );
        let injected = faults::inject(point);
        let second_error = service
            .select(&provider, &urls, Some(second_thread), None, true)
            .unwrap_err();
        drop(injected);
        assert_eq!(second_error.code, code::ENDPOINT_AFFINITY);

        {
            let pins = service.pins.lock().unwrap();
            assert_eq!(pins.pending.len(), 2);
            assert_eq!(pins.pending[&first_key].endpoint, digest(primary));
            assert_eq!(pins.pending[&second_key].endpoint, digest(fallback));
        }

        // The current preference now favors the second key; the first retry
        // must keep the choice captured before its write failed.
        let first_retry = service
            .select(&provider, &urls, Some(first_thread), None, true)
            .unwrap();
        assert_eq!(first_retry, primary);
        {
            let pins = service.pins.lock().unwrap();
            assert_eq!(pins.entries[&first_key].endpoint, digest(primary));
            assert!(!pins.pending.contains_key(&first_key));
            assert!(pins.pending.contains_key(&second_key));
        }

        set_measurements(
            &service,
            &provider,
            &urls,
            vec![
                fixture_measurement(primary, 10),
                fixture_measurement(fallback, 40),
            ],
        );
        let second_retry = service
            .select(&provider, &urls, Some(second_thread), None, true)
            .unwrap();
        assert_eq!(second_retry, fallback);

        let entries = pin_snapshot(&service.pins.lock().unwrap().entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[&first_key].0, digest(primary));
        assert_eq!(entries[&second_key].0, digest(fallback));
        assert!(service.pins.lock().unwrap().pending.is_empty());
        assert_same_snapshot(
            read_snapshot(&path),
            &entries,
            "successful retries for separate keys disagreed with persisted pins",
        );
    }
}

#[tokio::test]
async fn refresh_failure_emits_an_ordinary_warning_without_notification() {
    let primary = "https://primary.example.invalid/v1";
    let mut provider = fixture_provider(primary);
    provider.id = "00000000-0000-4000-8000-000000000001".into();
    let urls = vec![primary.to_owned()];
    let thread = "fixture-thread";
    let key = pin_key(&provider, "thread", thread);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("endpoint-owners.json");
    write_entries(
        &path,
        &HashMap::from([(
            key,
            Pin {
                endpoint: digest(primary),
                updated: now().saturating_sub(60_001),
            },
        )]),
    );
    let service = Service::new(path);
    let diagnostics = crate::events::Service::new(temp.path());
    service.diagnostics(super::super::ClientId::Codex, diagnostics.clone());
    let mut changes = diagnostics.subscribe();

    let injected = faults::inject(faults::Point::Replace);
    let selected = service
        .select(&provider, &urls, Some(thread), None, false)
        .unwrap();
    drop(injected);
    assert_eq!(selected, primary);

    let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("refresh diagnostic was not emitted")
        .expect("diagnostic event stream closed");
    assert!(!change.notify);
    let record = change
        .record
        .expect("refresh diagnostic omitted its record");
    assert_eq!(record.client_id, Some(super::super::ClientId::Codex));
    assert_eq!(record.provider_id.as_deref(), Some(provider.id.as_str()));
    assert_eq!(record.reason, crate::events::Reason::ConfigConflict);
    assert_eq!(record.action, crate::events::Action::Stopped);
    assert_eq!(record.level, crate::events::Level::Warning);
    assert_eq!(
        record.error_code,
        Some(crate::events::ErrorCode::EndpointAffinity)
    );
    assert_eq!(
        record.details.local_code.as_deref(),
        Some(code::ENDPOINT_AFFINITY)
    );
    assert_eq!(record.details.phase, Some(crate::events::Phase::Admission));
    assert_eq!(record.details.counted_failure, Some(false));
    assert!(!record.reason.notifiable(record.action));

    let ordinary = diagnostics.query(crate::events::Filter {
        category: Some(crate::events::Category::OrdinaryError),
        ..Default::default()
    });
    assert_eq!(ordinary.total, 1);
    assert_eq!(
        ordinary.items[0].category,
        crate::events::Category::OrdinaryError
    );
}

#[test]
fn refresh_failures_keep_the_endpoint_and_retry_after_the_interval() {
    let primary = "https://primary.example.invalid/v1";
    let secondary = "https://secondary.example.invalid/v1";
    let provider = fixture_provider(primary);
    let urls = vec![primary.to_owned(), secondary.to_owned()];
    let thread = "fixture-thread";
    let key = pin_key(&provider, "thread", thread);

    for point in [faults::Point::Replace, faults::Point::DirectorySync] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state/endpoint-owners.json");
        let entries = HashMap::from([(
            key.clone(),
            Pin {
                endpoint: digest(primary),
                updated: now().saturating_sub(60_001),
            },
        )]);
        write_entries(&path, &entries);
        let service = Service::new(path.clone());
        let before = pin_snapshot(&service.pins.lock().unwrap().entries);
        assert_same_snapshot(
            read_snapshot(&path),
            &before,
            "fixture file differs from pins",
        );

        let injected = faults::inject(point);
        let selected = service
            .select(&provider, &urls, Some(thread), None, false)
            .unwrap();
        drop(injected);
        assert_eq!(selected, primary);
        assert_same_snapshot(
            pin_snapshot(&service.pins.lock().unwrap().entries),
            &before,
            "refresh failure changed the in-memory pin snapshot",
        );

        let disk_after_failure = read_snapshot(&path);
        assert_eq!(disk_after_failure.len(), before.len());
        assert_eq!(disk_after_failure[&key].0, before[&key].0);
        if point == faults::Point::Replace {
            assert_same_snapshot(
                disk_after_failure,
                &before,
                "replacement failure changed the persisted pin snapshot",
            );
        } else {
            assert!(disk_after_failure[&key].1 >= before[&key].1);
        }

        let selected_during_backoff = service
            .select(&provider, &urls, Some(thread), None, false)
            .unwrap();
        assert_eq!(selected_during_backoff, primary);
        assert_same_snapshot(
            pin_snapshot(&service.pins.lock().unwrap().entries),
            &before,
            "failed refresh retried before its 60-second interval elapsed",
        );

        service
            .pins
            .lock()
            .unwrap()
            .refresh_attempts
            .insert(key.clone(), Instant::now() - Duration::from_secs(61));
        let selected_after_interval = service
            .select(&provider, &urls, Some(thread), None, false)
            .unwrap();
        assert_eq!(selected_after_interval, primary);
        let after_retry = pin_snapshot(&service.pins.lock().unwrap().entries);
        assert_eq!(after_retry[&key].0, digest(primary));
        assert!(after_retry[&key].1 >= before[&key].1);
        assert_same_snapshot(
            read_snapshot(&path),
            &after_retry,
            "successful refresh retry disagrees with persisted pins",
        );
    }
}

#[test]
fn failed_new_pin_restores_entries_trimmed_for_capacity_and_retry_commits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state/endpoint-owners.json");
    let service = Service::new(path.clone());
    let provider = fixture_provider("https://primary.example.invalid/v1");
    let endpoint = digest(&provider.base_url);
    let base = now();
    let mut entries = HashMap::with_capacity(MAX_PINS);
    let mut oldest_key = String::new();
    for index in 0..MAX_PINS {
        let key = storage::digest(format!("fixture-existing-pin-{index}").as_bytes());
        if index + 1 == MAX_PINS {
            oldest_key = key.clone();
        }
        entries.insert(
            key,
            Pin {
                endpoint: endpoint.clone(),
                updated: base.saturating_sub(index as u64),
            },
        );
    }
    let thread = "fixture-new-thread";
    let new_key = pin_key(&provider, "thread", thread);
    assert!(!entries.contains_key(&new_key));
    *service.pins.lock().unwrap() = super::Pins {
        entries: entries.clone(),
        error: false,
        ..Default::default()
    };
    write_entries(&path, &entries);
    let before = pin_snapshot(&service.pins.lock().unwrap().entries);

    let injected = faults::inject(faults::Point::Replace);
    let result = service.select(
        &provider,
        std::slice::from_ref(&provider.base_url),
        Some(thread),
        None,
        false,
    );
    drop(injected);
    let error = result.unwrap_err();

    assert_eq!(error.code, code::ENDPOINT_AFFINITY);
    assert_same_snapshot(
        pin_snapshot(&service.pins.lock().unwrap().entries),
        &before,
        "new pin failure changed the in-memory pin snapshot after trimming",
    );
    assert_same_snapshot(
        read_snapshot(&path),
        &before,
        "new pin failure changed the persisted pin snapshot",
    );

    let selected = service
        .select(
            &provider,
            std::slice::from_ref(&provider.base_url),
            Some(thread),
            None,
            false,
        )
        .unwrap();
    assert_eq!(selected, provider.base_url);
    let after_retry = pin_snapshot(&service.pins.lock().unwrap().entries);
    assert_eq!(after_retry.len(), MAX_PINS);
    assert!(!after_retry.contains_key(&oldest_key));
    assert_eq!(after_retry[&new_key].0, endpoint);
    for (key, pin) in &before {
        if key != &oldest_key {
            assert_eq!(after_retry.get(key), Some(pin));
        }
    }
    assert_same_snapshot(
        read_snapshot(&path),
        &after_retry,
        "successful new pin retry disagrees with persisted pins",
    );
}

#[test]
fn successful_initial_pin_matches_the_persisted_entries() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("endpoint-owners.json");
    let service = Service::new(path.clone());
    let provider = fixture_provider("https://primary.example.invalid/v1");
    let thread = "fixture-thread";

    let selected = service
        .select(
            &provider,
            std::slice::from_ref(&provider.base_url),
            Some(thread),
            None,
            false,
        )
        .unwrap();

    assert_eq!(selected, provider.base_url);
    let entries = pin_snapshot(&service.pins.lock().unwrap().entries);
    let key = pin_key(&provider, "thread", thread);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[&key].0, digest(&provider.base_url));
    assert_eq!(read_snapshot(&path), entries);
}

#[test]
fn failed_new_pin_retry_keeps_the_original_choice_after_preference_changes() {
    let primary = "https://primary.example.invalid/v1";
    let secondary = "https://secondary.example.invalid/v1";
    let provider = fixture_provider(primary);
    let urls = vec![primary.to_owned(), secondary.to_owned()];
    let thread = "fixture-thread";
    let key = pin_key(&provider, "thread", thread);

    for point in [faults::Point::Replace, faults::Point::DirectorySync] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("endpoint-owners.json");
        let service = Service::new(path.clone());
        set_measurements(
            &service,
            &provider,
            &urls,
            vec![
                Measurement {
                    base_url: primary.to_owned(),
                    latency_ms: Some(40),
                    status: Some(200),
                    error: None,
                    checked_at: now(),
                },
                Measurement {
                    base_url: secondary.to_owned(),
                    latency_ms: Some(10),
                    status: Some(200),
                    error: None,
                    checked_at: now(),
                },
            ],
        );
        let original_choice = service.preferred(&provider, &urls);
        assert_eq!(original_choice, secondary);

        let injected = faults::inject(point);
        let result = service.select(&provider, &urls, Some(thread), None, true);
        drop(injected);
        let error = result.unwrap_err();
        assert_eq!(error.code, code::ENDPOINT_AFFINITY);
        assert!(!service.pins.lock().unwrap().entries.contains_key(&key));

        set_measurements(
            &service,
            &provider,
            &urls,
            vec![
                Measurement {
                    base_url: primary.to_owned(),
                    latency_ms: Some(5),
                    status: Some(200),
                    error: None,
                    checked_at: now(),
                },
                Measurement {
                    base_url: secondary.to_owned(),
                    latency_ms: Some(90),
                    status: Some(200),
                    error: None,
                    checked_at: now(),
                },
            ],
        );
        assert_eq!(service.preferred(&provider, &urls), primary);

        let retry = service
            .select(&provider, &urls, Some(thread), None, true)
            .unwrap();
        assert_eq!(retry, original_choice);
        let entries = pin_snapshot(&service.pins.lock().unwrap().entries);
        assert_eq!(entries[&key].0, digest(&original_choice));
        assert_same_snapshot(
            read_snapshot(&path),
            &entries,
            "retry after a preferred endpoint change disagrees with persisted pins",
        );
    }
}

async fn counter_upstream(hits: Arc<AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let hits = hits.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |_request: Request<Incoming>| {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::NO_CONTENT)
                                .body(replay::empty())
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    port
}

#[tokio::test]
async fn first_affinity_persist_failure_returns_before_sending_to_upstream() {
    let hits = Arc::new(AtomicUsize::new(0));
    let upstream_port = counter_upstream(hits.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let base_url = format!("http://127.0.0.1:{upstream_port}/v1");
    let config = format!(
        "# fixture\nmodel_provider='custom'\n[model_providers.custom]\nbase_url = {}\nexperimental_bearer_token = \"fixture-token\"\nwire_api='responses'\nsupports_websockets=false\n",
        serde_json::to_string(&base_url).unwrap()
    );
    fs::write(temp.path().join("config.toml"), config).unwrap();
    let gateway = Gateway::new(temp.path().to_path_buf()).unwrap();

    let port_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_port = port_listener.local_addr().unwrap().port();
    drop(port_listener);
    let mut settings = gateway.view().settings;
    settings.port = gateway_port;
    gateway
        .edit(
            Edit::Settings { settings },
            &gateway.view().revision,
            temp.path(),
        )
        .unwrap();
    gateway
        .edit(
            Edit::SaveProvider {
                quota_access_token: None,
                id: None,
                base_url: base_url.clone(),
                token: "fixture-token".into(),
                name: Some("fixture".into()),
            },
            &gateway.view().revision,
            temp.path(),
        )
        .unwrap();
    fs::write(temp.path().join("auth.json"), "fixture-auth").unwrap();
    gateway
        .start(&gateway.view().revision, temp.path())
        .await
        .unwrap();

    // The service was initialized while this path was absent. A directory at
    // the exact persistence target makes the first atomic replacement fail.
    fs::create_dir(temp.path().join("endpoint-owners.json")).unwrap();
    let (gateway_port, token) = {
        let inner = gateway.0.inner.lock().unwrap();
        (inner.store.settings.port, inner.store.local_token.clone())
    };
    let client: super::super::HttpClient = Client::builder(TokioExecutor::new()).build(
        super::super::connector::Connector::new(Duration::from_secs(2), 0),
    );
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{gateway_port}/v1/responses"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("thread-id", "fixture-thread")
        .body(replay::full(
            br#"{"model":"gpt-fixture","input":"fixture"}"#.as_slice(),
        ))
        .unwrap();
    let response = client.request(request).await;
    let status = match response {
        Ok(response) => response.status(),
        Err(error) => {
            let _ = gateway.stop().await;
            panic!("loopback gateway request failed: {error}");
        }
    };
    let upstream_hits = hits.load(Ordering::SeqCst);
    let owner_count = gateway.0.endpoints.pins.lock().unwrap().entries.len();
    gateway.stop().await.unwrap();

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream_hits, 0);
    assert_eq!(owner_count, 0);
}
