use super::{
    model::Tokens,
    pricing::{Pricing, View},
};
use crate::storage;
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    future::{poll_fn, Future},
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    task::Poll,
    thread::{self, JoinHandle},
    time::Duration,
};

const ETAG: &str = "\"fixture-v1\"";
const NEXT_ETAG: &str = "\"fixture-v2\"";

struct Reply {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            headers: vec![],
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn write(&self, stream: &mut TcpStream) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            302 => "Found",
            304 => "Not Modified",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        };
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            reason,
            self.body.len()
        )?;
        for (name, value) in &self.headers {
            write!(stream, "{name}: {value}\r\n")?;
        }
        stream.write_all(b"\r\n")?;
        stream.write_all(&self.body)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    path: String,
    etag: Option<String>,
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
        let read = stream.read(&mut chunk)?;
        if read == 0 || bytes.len() + read > 8192 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid fixture request",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid fixture headers"))?;
    let mut lines = text.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split_whitespace();
    if start.next() != Some("GET") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected fixture GET",
        ));
    }
    let path = start.next().unwrap_or_default().to_owned();
    let etag = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
        .map(|(_, value)| value.trim().to_owned());
    Ok(Request { path, etag })
}

struct SourceFixture {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
    errors: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    release: Option<mpsc::Sender<()>>,
    server: Option<JoinHandle<()>>,
}

impl SourceFixture {
    fn new(replies: Vec<Reply>) -> Self {
        Self::start(replies, false)
    }

    fn start(replies: Vec<Reply>, pause_first_response: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let (release, mut gate) = if pause_first_response {
            let (sender, receiver) = mpsc::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let server_requests = requests.clone();
        let server_replies = replies.clone();
        let server_errors = errors.clone();
        let server_stopped = stopped.clone();
        let server = thread::spawn(move || {
            while !server_stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) => {
                        server_errors.lock().unwrap().push(error.to_string());
                        break;
                    }
                };
                if server_stopped.load(Ordering::SeqCst) {
                    break;
                }
                let result = (|| {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                    server_requests
                        .lock()
                        .unwrap()
                        .push(read_request(&mut stream)?);
                    if let Some(receiver) = gate.take() {
                        let _ = receiver.recv();
                    }
                    let reply = server_replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| Reply::new(500, b"unexpected fixture request".to_vec()));
                    reply.write(&mut stream)
                })();
                if let Err(error) = result {
                    server_errors.lock().unwrap().push(error.to_string());
                }
            }
        });
        Self {
            address,
            requests,
            replies,
            errors,
            stopped,
            release,
            server: Some(server),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn release_first_response(&mut self) {
        if let Some(sender) = self.release.take() {
            let _ = sender.send(());
        }
    }

    fn assert_requests(&self, expected: &[(&str, Option<&str>)]) {
        let expected: Vec<_> = expected
            .iter()
            .map(|(path, etag)| Request {
                path: (*path).into(),
                etag: etag.map(str::to_owned),
            })
            .collect();
        assert_eq!(*self.requests.lock().unwrap(), expected);
        assert!(
            self.replies.lock().unwrap().is_empty(),
            "unused fixture responses"
        );
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "fixture errors: {errors:?}");
    }
}

impl Drop for SourceFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.release_first_response();
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

fn rates(input: &str) -> Value {
    json!({"input_cost_per_token": input, "output_cost_per_token": "0.000004"})
}

fn catalog(input: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"fixture-model": rates(input)})).unwrap()
}

fn hash_reply(body: &[u8]) -> Reply {
    Reply::new(200, storage::digest(body).into_bytes())
}

fn download_replies(body: &[u8], etag: &str) -> Vec<Reply> {
    vec![
        hash_reply(body),
        Reply::new(200, body.to_vec()).header("ETag", etag),
    ]
}

async fn refresh(prices: &Pricing, source: &SourceFixture, force: bool) -> storage::Result<View> {
    prices
        .update_from(
            force,
            &source.url("/prices.json"),
            &source.url("/prices.sha256"),
        )
        .await
}

fn assert_same_prices(before: &View, after: &View) {
    assert_eq!(after.models, before.models);
    assert_eq!(after.version, before.version);
    assert_eq!(after.updated_at, before.updated_at);
    assert!(!after.syncing);
}

fn age_cache(dir: &Path) {
    let path = dir.join("price-cache.json");
    let mut cache: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cache["checkedAt"] = json!(1);
    cache["updatedAt"] = json!(1);
    fs::write(path, serde_json::to_vec(&cache).unwrap()).unwrap();
}

#[tokio::test]
async fn http_200_updates_existing_model_and_adds_new_model() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let original = catalog("0.000001");
    let next = serde_json::to_vec(&json!({
        "fixture-model": rates("0.000002"),
        "fixture-new-model": rates("0.000003")
    }))
    .unwrap();
    let mut replies = download_replies(&original, ETAG);
    replies.extend(download_replies(&next, NEXT_ETAG));
    let source = SourceFixture::new(replies);
    let first = refresh(&prices, &source, true).await.unwrap();
    let updated = refresh(&prices, &source, true).await.unwrap();
    assert_eq!(first.models["fixture-model"], rates("0.000001"));
    assert_eq!(updated.models.len(), 2);
    assert_eq!(updated.models["fixture-model"], rates("0.000002"));
    assert_eq!(updated.models["fixture-new-model"], rates("0.000003"));
    assert_eq!(updated.version, storage::digest(&next));
    assert_ne!(updated.version, first.version);
    assert!(updated.checked_at.is_some());
    assert!(updated.updated_at.is_some());
    assert!(updated.error.is_none());
    assert!(!updated.syncing);
    assert_eq!(
        prices.quote(Some("fixture-new-model"), "1").data,
        Some(rates("0.000003"))
    );
    source.assert_requests(&[
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
        ("/prices.json", Some(ETAG)),
    ]);
}

#[tokio::test]
async fn etag_304_advances_check_time_without_changing_update_time() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let body = catalog("0.000001");
    let mut replies = download_replies(&body, ETAG);
    replies.extend([hash_reply(&body), Reply::new(304, vec![])]);
    let source = SourceFixture::new(replies);
    refresh(&prices, &source, true).await.unwrap();
    drop(prices);
    age_cache(dir.path());
    let prices = Pricing::new(dir.path()).unwrap();
    let before = prices.view();
    let checked = refresh(&prices, &source, true).await.unwrap();
    assert_same_prices(&before, &checked);
    assert_eq!(checked.updated_at, Some(1));
    assert!(checked.checked_at.unwrap() > 1);
    assert_ne!(checked.checked_at, checked.updated_at);
    assert!(checked.error.is_none());
    let persisted: Value =
        serde_json::from_slice(&fs::read(dir.path().join("price-cache.json")).unwrap()).unwrap();
    assert_eq!(persisted["updatedAt"], json!(1));
    assert!(persisted["checkedAt"].as_i64().unwrap() > 1);
    source.assert_requests(&[
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
        ("/prices.json", Some(ETAG)),
    ]);
}

#[tokio::test]
async fn changed_hash_after_304_retries_without_conditional_header() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let original = catalog("0.000001");
    let next = catalog("0.000002");
    let mut replies = download_replies(&original, ETAG);
    replies.extend([
        hash_reply(&next),
        Reply::new(304, vec![]),
        Reply::new(200, next.clone()).header("ETag", NEXT_ETAG),
    ]);
    let source = SourceFixture::new(replies);
    refresh(&prices, &source, true).await.unwrap();
    let updated = refresh(&prices, &source, true).await.unwrap();
    assert_eq!(updated.version, storage::digest(&next));
    assert_eq!(updated.models["fixture-model"], rates("0.000002"));
    assert!(updated.error.is_none());
    source.assert_requests(&[
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
        ("/prices.json", Some(ETAG)),
        ("/prices.json", None),
    ]);
}

async fn assert_rejected_update_keeps_cache(rejected: Vec<Reply>, expected_error: &str) {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let original = catalog("0.000001");
    let rejected_count = rejected.len();
    let mut replies = download_replies(&original, ETAG);
    replies.extend(rejected);
    let source = SourceFixture::new(replies);
    refresh(&prices, &source, true).await.unwrap();
    drop(prices);
    age_cache(dir.path());
    let prices = Pricing::new(dir.path()).unwrap();
    let before = prices.view();
    let saved = fs::read(dir.path().join("price-cache.json")).unwrap();
    assert!(
        refresh(&prices, &source, true).await.is_err(),
        "{expected_error}"
    );
    let failed = prices.view();
    assert_same_prices(&before, &failed);
    assert_eq!(failed.error.as_deref(), Some(expected_error));
    assert!(failed.checked_at.unwrap() > before.checked_at.unwrap());
    assert_eq!(
        fs::read(dir.path().join("price-cache.json")).unwrap(),
        saved
    );
    let restarted = Pricing::new(dir.path()).unwrap().view();
    assert_same_prices(&before, &restarted);
    assert!(restarted.error.is_none());
    let mut expected = vec![
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
    ];
    if rejected_count == 2 {
        expected.push(("/prices.json", Some(ETAG)));
    }
    source.assert_requests(&expected);
}

#[tokio::test]
async fn sha_mismatch_keeps_last_valid_cache() {
    assert_rejected_update_keeps_cache(
        vec![
            hash_reply(b"different fixture bytes"),
            Reply::new(200, catalog("0.000002")),
        ],
        "价格哈希不匹配，已保留上次有效数据",
    )
    .await;
}

#[tokio::test]
async fn invalid_json_with_matching_sha_keeps_last_valid_cache() {
    let invalid = b"{invalid fixture json".to_vec();
    assert_rejected_update_keeps_cache(
        vec![hash_reply(&invalid), Reply::new(200, invalid)],
        "价格数据无效",
    )
    .await;
}

#[tokio::test]
async fn invalid_catalog_with_matching_sha_keeps_last_valid_cache() {
    let invalid = catalog("-0.000001");
    assert_rejected_update_keeps_cache(
        vec![hash_reply(&invalid), Reply::new(200, invalid)],
        "价格必须为非负数",
    )
    .await;
}

#[tokio::test]
async fn redirect_is_not_followed_and_keeps_last_valid_cache() {
    let body = catalog("0.000002");
    assert_rejected_update_keeps_cache(
        vec![
            hash_reply(&body),
            Reply::new(302, vec![]).header("Location", "/redirected-prices.json"),
        ],
        "价格源暂不可用，正在使用缓存",
    )
    .await;
}

#[tokio::test]
async fn unavailable_source_keeps_last_valid_cache() {
    assert_rejected_update_keeps_cache(
        vec![
            hash_reply(&catalog("0.000002")),
            Reply::new(503, b"fixture unavailable".to_vec()),
        ],
        "价格源暂不可用，正在使用缓存",
    )
    .await;
}

#[tokio::test]
async fn invalid_sha_response_does_not_fetch_catalog_or_replace_cache() {
    assert_rejected_update_keeps_cache(
        vec![Reply::new(200, b"invalid-fixture-sha".to_vec())],
        "价格校验码无效",
    )
    .await;
}

#[tokio::test]
async fn fixed_price_survives_remote_refresh_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let mut replies = download_replies(&catalog("0.000001"), ETAG);
    replies.extend(download_replies(&catalog("0.000009"), NEXT_ETAG));
    let source = SourceFixture::new(replies);
    let view = refresh(&prices, &source, true).await.unwrap();
    let mut config = view.config;
    config
        .fixed
        .insert("fixture-model".into(), rates("0.000003"));
    prices.configure(config, &view.revision).unwrap();
    let fixed = prices.quote(Some("fixture-model"), "2");
    let updated = refresh(&prices, &source, true).await.unwrap();
    assert_eq!(updated.models["fixture-model"], rates("0.000003"));
    assert_eq!(updated.version, storage::digest(&catalog("0.000009")));
    for current in [prices, Pricing::new(dir.path()).unwrap()] {
        let quote = current.quote(Some("fixture-model"), "2");
        assert_eq!(quote.source, "fixed");
        assert_eq!(quote.version, fixed.version);
        assert_eq!(quote.data, fixed.data);
        assert_eq!(quote.multiplier, "2");
        assert_eq!(
            current.view().config.fixed["fixture-model"],
            rates("0.000003")
        );
    }
    source.assert_requests(&[
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
        ("/prices.json", Some(ETAG)),
    ]);
}

#[tokio::test]
async fn overlapping_force_updates_share_one_download() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let body = catalog("0.000002");
    let mut source = SourceFixture::start(download_replies(&body, ETAG), true);
    let source_url = source.url("/prices.json");
    let hash_url = source.url("/prices.sha256");
    let mut first = Box::pin(prices.update_from(true, &source_url, &hash_url));
    let mut second = Box::pin(prices.update_from(true, &source_url, &hash_url));
    assert!(poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx)))
        .await
        .is_pending());
    assert!(prices.view().syncing);
    assert!(poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx)))
        .await
        .is_pending());
    source.release_first_response();
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    assert_same_prices(&first, &second);
    assert_eq!(first.version, storage::digest(&body));
    assert_eq!(second.checked_at, first.checked_at);
    assert!(first.error.is_none());
    assert!(second.error.is_none());
    source.assert_requests(&[("/prices.sha256", None), ("/prices.json", None)]);
}

#[tokio::test]
async fn restart_loads_verified_catalog_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let body = catalog("0.000002");
    let source = SourceFixture::new(download_replies(&body, ETAG));
    let before = refresh(&prices, &source, true).await.unwrap();
    drop(prices);
    let restarted = Pricing::new(dir.path()).unwrap();
    let view = restarted.view();
    assert_same_prices(&before, &view);
    assert!(view.checked_at.is_some());
    assert!(view.error.is_none());
    assert_eq!(
        restarted.quote(Some("fixture-model"), "1").data,
        Some(rates("0.000002"))
    );
    source.assert_requests(&[("/prices.sha256", None), ("/prices.json", None)]);
}

#[tokio::test]
async fn restart_rejects_tampered_or_malformed_cache() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let seed = prices.view();
    let source = SourceFixture::new(download_replies(&catalog("0.000002"), ETAG));
    refresh(&prices, &source, true).await.unwrap();
    drop(prices);
    let path = dir.path().join("price-cache.json");
    let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut tampered = saved.clone();
    tampered["catalog"]["fixture-model"] = rates("0.000099");
    let mut missing_checksum = saved;
    missing_checksum
        .as_object_mut()
        .unwrap()
        .remove("catalogHash");
    for corrupted in [
        serde_json::to_vec(&tampered).unwrap(),
        serde_json::to_vec(&missing_checksum).unwrap(),
        b"{broken fixture cache".to_vec(),
    ] {
        fs::write(&path, corrupted).unwrap();
        let restarted = Pricing::new(dir.path()).unwrap();
        let view = restarted.view();
        assert_same_prices(&seed, &view);
        assert_eq!(
            view.error.as_deref(),
            Some("价格缓存校验失败，正在使用初始价格库")
        );
        assert!(restarted.quote(Some("fixture-model"), "1").data.is_none());
    }
    source.assert_requests(&[("/prices.sha256", None), ("/prices.json", None)]);
}

#[tokio::test]
async fn aliases_are_explicit_and_missing_prices_are_not_estimated() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let body = serde_json::to_vec(&json!({
        "fixture-model": rates("0.000001"),
        "fixture-model-v2": rates("0.000002"),
        "fixture-missing-output": {"input_cost_per_token": "0.000003"}
    }))
    .unwrap();
    let source = SourceFixture::new(download_replies(&body, ETAG));
    let view = refresh(&prices, &source, true).await.unwrap();
    let mut config = view.config;
    config
        .aliases
        .insert("fixture-alias".into(), "fixture-model".into());
    config
        .aliases
        .insert("fixture-model".into(), "fixture-model-v2".into());
    config
        .aliases
        .insert("fixture-broken-alias".into(), "fixture-absent".into());
    prices.configure(config, &view.revision).unwrap();
    let alias = prices.quote(Some("fixture-alias"), "1");
    assert_eq!(alias.model, "fixture-model");
    assert_eq!(alias.data, Some(rates("0.000001")));
    assert_eq!(alias.source, "sub2api");
    let exact = prices.quote(Some("fixture-model"), "1");
    assert_eq!(exact.model, "fixture-model");
    assert_eq!(exact.data, Some(rates("0.000001")));
    let tokens = Tokens {
        input: Some(10),
        output: Some(5),
        ..Tokens::default()
    };
    for missing in [
        "fixture-model-v3",
        "fixture-model-latest",
        "fixture-broken-alias",
    ] {
        let quote = prices.quote(Some(missing), "1");
        assert!(quote.data.is_none(), "{missing}");
        assert!(quote.calculate(&tokens, None).is_none(), "{missing}");
    }
    assert!(prices
        .quote(Some("fixture-missing-output"), "1")
        .calculate(&tokens, None)
        .is_none());
    assert!(exact.calculate(&tokens, Some("priority")).is_none());
    assert!(exact
        .calculate(
            &Tokens {
                cache_read: Some(1),
                ..tokens
            },
            None
        )
        .is_none());
    let view = prices.view();
    let mut config = view.config;
    config.selected = Some(BTreeSet::from(["fixture-model-v2".into()]));
    prices.configure(config, &view.revision).unwrap();
    assert!(prices.quote(Some("fixture-alias"), "1").data.is_none());
    source.assert_requests(&[("/prices.sha256", None), ("/prices.json", None)]);
}

#[tokio::test]
async fn auto_update_setting_and_check_interval_control_network_requests() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let initial = prices.view();
    let mut config = initial.config;
    config.auto_update = false;
    prices.configure(config, &initial.revision).unwrap();
    let mut replies = download_replies(&catalog("0.000001"), ETAG);
    replies.extend(download_replies(&catalog("0.000002"), NEXT_ETAG));
    let source = SourceFixture::new(replies);
    let skipped = refresh(&prices, &source, false).await.unwrap();
    assert!(skipped.checked_at.is_none());
    assert!(source.requests.lock().unwrap().is_empty());
    let forced = refresh(&prices, &source, true).await.unwrap();
    assert_eq!(forced.models["fixture-model"], rates("0.000001"));
    let mut config = forced.config.clone();
    config.auto_update = true;
    prices.configure(config, &forced.revision).unwrap();
    let throttled = refresh(&prices, &source, false).await.unwrap();
    assert_same_prices(&forced, &throttled);
    assert_eq!(throttled.checked_at, forced.checked_at);
    assert_eq!(source.requests.lock().unwrap().len(), 2);
    drop(prices);
    age_cache(dir.path());
    let prices = Pricing::new(dir.path()).unwrap();
    let refreshed = refresh(&prices, &source, false).await.unwrap();
    assert_eq!(refreshed.models["fixture-model"], rates("0.000002"));
    assert!(refreshed.checked_at.unwrap() > 1);
    source.assert_requests(&[
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
        ("/prices.json", Some(ETAG)),
    ]);
}

#[tokio::test]
async fn frozen_quotes_survive_remote_updates_and_manual_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let prices = Pricing::new(dir.path()).unwrap();
    let original = catalog("0.000001");
    let next = catalog("0.000002");
    let mut replies = download_replies(&original, ETAG);
    replies.extend(download_replies(&next, NEXT_ETAG));
    let source = SourceFixture::new(replies);
    refresh(&prices, &source, true).await.unwrap();
    let original_snapshot = prices.frozen();

    let updated = refresh(&prices, &source, true).await.unwrap();
    let updated_snapshot = prices.frozen();
    assert_eq!(
        original_snapshot.quote(Some("fixture-model"), "2").data,
        Some(rates("0.000001"))
    );
    assert_eq!(
        updated_snapshot.quote(Some("fixture-model"), "2").data,
        Some(rates("0.000002"))
    );

    let fixed = rates("0.000003");
    let mut config = updated.config;
    config.fixed.insert("fixture-model".into(), fixed.clone());
    prices.configure(config, &updated.revision).unwrap();
    let configured_snapshot = prices.frozen();
    let tokens = Tokens {
        input: Some(10),
        output: Some(5),
        ..Tokens::default()
    };
    let original_version = storage::digest(&original);
    let updated_version = storage::digest(&next);
    let fixed_version = storage::digest(fixed.to_string().as_bytes());
    for (snapshot, input, version, origin, cost) in [
        (
            &original_snapshot,
            "0.000001",
            &original_version,
            "sub2api",
            "0.00006",
        ),
        (
            &updated_snapshot,
            "0.000002",
            &updated_version,
            "sub2api",
            "0.00008",
        ),
        (
            &configured_snapshot,
            "0.000003",
            &fixed_version,
            "fixed",
            "0.0001",
        ),
        (&prices, "0.000003", &fixed_version, "fixed", "0.0001"),
    ] {
        let quote = snapshot.quote(Some("fixture-model"), "2");
        assert_eq!(quote.model, "fixture-model");
        assert_eq!(quote.data, Some(rates(input)));
        assert_eq!(&quote.version, version);
        assert_eq!(quote.source, origin);
        assert_eq!(quote.multiplier, "2");
        let calculated = quote.calculate(&tokens, None).unwrap();
        assert_eq!(calculated.cost, cost);
        assert_eq!(&calculated.version, version);
        assert_eq!(calculated.source, origin);
    }
    source.assert_requests(&[
        ("/prices.sha256", None),
        ("/prices.json", None),
        ("/prices.sha256", None),
        ("/prices.json", Some(ETAG)),
    ]);
}
