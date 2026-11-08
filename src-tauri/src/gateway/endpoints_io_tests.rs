use super::super::{
    model::{Edit, Provider},
    replay, Gateway,
};
use super::{digest, now, pin_key, Pin, Service};
use crate::errors::code;
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
    time::Duration,
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
        search_only: false,
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
    // A failed refresh preserves the known endpoint and waits before retrying.
    let key = pin_key(&provider, "thread", existing_thread);
    service
        .pins
        .lock()
        .unwrap()
        .entries
        .get_mut(&key)
        .unwrap()
        .updated = now().saturating_sub(60_001);
    let before_refresh = now();
    assert_eq!(
        service
            .select(&provider, &urls, Some(existing_thread), None, false)
            .unwrap(),
        primary
    );
    let attempted = service.pins.lock().unwrap().entries[&key].updated;
    assert!(attempted >= before_refresh);
    assert_eq!(
        service
            .select(&provider, &urls, Some(existing_thread), None, false)
            .unwrap(),
        primary
    );
    assert_eq!(
        service.pins.lock().unwrap().entries[&key].updated,
        attempted
    );
    assert!(path.is_dir());
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
                search_only: None,
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
