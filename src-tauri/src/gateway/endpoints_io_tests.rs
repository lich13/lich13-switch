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

fn assert_same_snapshot(
    actual: BTreeMap<String, (String, u64)>,
    expected: &BTreeMap<String, (String, u64)>,
    message: &str,
) {
    assert!(actual == *expected, "{message}");
}

#[test]
fn refresh_failures_preserve_memory_and_retry_keeps_the_endpoint() {
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
        let result = service.select(&provider, &urls, Some(thread), None, false);
        drop(injected);
        let error = result.unwrap_err();

        assert_eq!(error.code, code::ENDPOINT_AFFINITY);
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

        let selected = service
            .select(&provider, &urls, Some(thread), None, false)
            .unwrap();
        assert_eq!(selected, primary);
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
