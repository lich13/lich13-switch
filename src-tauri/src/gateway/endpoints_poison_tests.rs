use super::{Batch, Measurement, Provider, Service};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Mutex,
    time::Instant,
};

fn provider() -> Provider {
    Provider {
        id: "fixture-provider".into(),
        name: "fixture-provider".into(),
        base_url: "https://primary.example.invalid/v1".into(),
        token: "fixture-upstream-token".into(),
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

fn urls(provider: &Provider) -> Vec<String> {
    vec![
        provider.base_url.clone(),
        "https://secondary.example.invalid/v1".into(),
    ]
}

fn measurement(base_url: &str, latency_ms: u64) -> Measurement {
    Measurement {
        base_url: base_url.into(),
        latency_ms: Some(latency_ms),
        status: Some(200),
        error: None,
        checked_at: 1,
    }
}

fn poison<T>(mutex: &Mutex<T>, edit: impl FnOnce(&mut T)) {
    let injected = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = mutex.lock().unwrap();
        edit(&mut guard);
        panic!("inject endpoint affinity poison");
    }));
    assert!(injected.is_err());
    assert!(mutex.is_poisoned());
}

#[test]
fn poisoned_endpoint_cache_is_cleared_then_accepts_a_rebuilt_batch() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path().join("endpoint-owners.json"));
    let provider = provider();
    let urls = urls(&provider);
    let config = Service::config(&provider, &urls);
    service.samples.lock().unwrap().insert(
        provider.id.clone(),
        Batch {
            config: config.clone(),
            checked: Instant::now(),
            rows: vec![measurement(&provider.base_url, 10)],
        },
    );

    poison(&service.samples, |_| {});
    assert!(service.measurements(&provider, &urls).is_empty());
    assert!(!service.samples.is_poisoned());

    service.samples.lock().unwrap().insert(
        provider.id.clone(),
        Batch {
            config,
            checked: Instant::now(),
            rows: vec![measurement(&urls[1], 4)],
        },
    );
    assert_eq!(service.measurements(&provider, &urls).len(), 1);
    assert_eq!(service.preferred(&provider, &urls), urls[1]);
}

#[test]
fn poisoned_private_pin_fails_closed_without_guessing_or_rewriting_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("endpoint-owners.json");
    let service = Service::new(path.clone());
    let provider = provider();
    let urls = urls(&provider);
    let thread = "fixture-thread";

    assert_eq!(
        service
            .select(&provider, &urls, Some(thread), None, false)
            .unwrap(),
        provider.base_url
    );
    let durable_before_poison = std::fs::read(&path).unwrap();
    let key = super::pin_key(&provider, "thread", thread);
    poison(&service.pins, |pins| {
        pins.entries.get_mut(&key).unwrap().endpoint =
            super::digest("https://fixture-partial.example.invalid/v1");
    });

    let error = service
        .select(&provider, &urls, Some(thread), None, false)
        .unwrap_err();
    assert_eq!(error.code, "ENDPOINT_AFFINITY");
    assert_eq!(std::fs::read(&path).unwrap(), durable_before_poison);
}

#[test]
fn poisoned_writer_preserves_confirmed_pin_reads_and_rejects_new_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("endpoint-owners.json");
    let service = Service::new(path.clone());
    let provider = provider();
    let urls = urls(&provider);
    let existing = "fixture-existing-thread";
    let new = "fixture-new-thread";
    assert_eq!(
        service
            .select(&provider, &urls, Some(existing), None, false)
            .unwrap(),
        provider.base_url
    );
    let durable_before_poison = std::fs::read(&path).unwrap();

    poison(&service.write, |_| {});
    assert_eq!(
        service
            .select(&provider, &urls, Some(existing), None, false)
            .unwrap(),
        provider.base_url,
        "a confirmed read must not wait on or depend on the writer mutex"
    );
    let error = service
        .select(&provider, &urls, Some(new), None, false)
        .unwrap_err();
    assert_eq!(error.code, "ENDPOINT_AFFINITY");
    assert_eq!(std::fs::read(&path).unwrap(), durable_before_poison);
}

#[test]
fn poisoned_response_cursor_fails_closed_without_rebinding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("endpoint-owners.json");
    let service = Service::new(path.clone());
    let provider = provider();
    let urls = urls(&provider);
    let response = "fixture-response-cursor";
    service.remember(&provider, response);
    let durable_before_poison = std::fs::read(&path).ok();
    let key = super::pin_key(&provider, "response", response);
    poison(&service.responses, |responses| {
        responses.get_mut(&key).unwrap().endpoint =
            super::digest("https://fixture-partial.example.invalid/v1");
    });

    let error = service
        .select(&provider, &urls, None, Some(response), false)
        .unwrap_err();
    assert_eq!(error.code, "ENDPOINT_AFFINITY");
    assert_eq!(std::fs::read(&path).ok(), durable_before_poison);
}
