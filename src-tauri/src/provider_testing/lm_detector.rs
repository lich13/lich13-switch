//! Fixed LM Detector SDK and paired public data. Network access is explicit here.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::io::{Read, Write};

const BUNDLE: &[u8] = include_bytes!("../../resources/lm-detector/bundle.json.gz");
const MANIFEST: &str = include_str!("../../resources/lm-detector/manifest.json");
const LIMIT: u64 = 32 * 1024 * 1024;
const CACHE: &str = "lm-detector.json.gz";
#[derive(serde::Serialize, serde::Deserialize)]
struct Bundle {
    revision: String,
    bank: Value,
    detector: String,
    tokenizer: Value,
    algorithm: Value,
}
pub(super) struct Resource {
    pub detector: lmfpd::Detector,
    pub tokenizer: tokenizer::Bank,
    metadata: Value,
}
impl Resource {
    pub fn version(&self) -> Value {
        self.metadata["version"].clone()
    }
    pub fn analyze(&self, outputs: &[lmfpd::Output]) -> Result<Value> {
        if outputs.is_empty() {
            return Ok(Value::Null);
        }
        let report = self
            .detector
            .analyze(outputs, false)
            .map_err(|_| failure("LM Detector 评分失败"))?;
        let mut value = serde_json::to_value(report).map_err(|_| failure("指纹结果无法读取"))?;
        value["bank_version"] = self.version();
        Ok(value)
    }
}
fn unpack(raw: &[u8]) -> Result<Bundle> {
    if raw.len() as u64 > LIMIT {
        return Err(failure("指纹资源包过大"));
    }
    let mut bytes = Vec::new();
    GzDecoder::new(raw)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure("指纹资源包无法解压"))?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure("指纹资源包过大"));
    }
    let bundle: Bundle =
        serde_json::from_slice(&bytes).map_err(|_| failure("指纹资源包格式无效"))?;
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    if bundle.algorithm != manifest["algorithm"]
        || bundle.revision.len() != 40
        || !bundle.revision.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(failure("上游指纹算法已变化，请升级应用"));
    }
    Ok(bundle)
}
fn metadata(bundle: &Bundle, updated: bool) -> Value {
    json!({"version":{"revision":bundle.revision,"built_at":bundle.bank["built_at"]},"source":if updated {"cache"} else {"builtin"},"models":bundle.bank["models"].as_array().into_iter().flatten().map(|m|json!({"id":m["id"],"name":m["display_name"]})).collect::<Vec<_>>(),"updating":false,"error":null})
}
fn bytes(directory: &Path) -> Result<(Vec<u8>, bool)> {
    let saved = storage::read_bounded(&directory.join(CACHE), LIMIT)?;
    if let Some(saved) = saved {
        Ok((saved, true))
    } else {
        let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
        if compat::sha256(BUNDLE) != manifest["sha256"] {
            return Err(failure("内置指纹资源校验失败"));
        }
        Ok((BUNDLE.to_vec(), false))
    }
}
pub fn initial_status(directory: &Path) -> Value {
    match bytes(directory).and_then(|(raw, updated)| unpack(&raw).map(|b| metadata(&b, updated))) {
        Ok(value) => value,
        Err(error) => json!({"models":[],"error":error.message,"updating":false}),
    }
}
pub(super) fn status(data: &Data) -> Value {
    let mut value = data.lm_status.clone();
    let saved = &data.saved.lm_bank;
    value["checkedAt"] = json!(saved.checked_at);
    value["updatedAt"] = json!(saved.updated_at);
    value["updating"] = data.lm_updating.into();
    value["result"] = json!(saved.result);
    value["error"] = data
        .lm_error
        .as_ref()
        .or(saved.error.as_ref())
        .map(|error| json!(error))
        .unwrap_or_else(|| value["error"].clone());
    if data.lm_updating {
        value["error"] = Value::Null;
        value["result"] = Value::Null;
    }
    value
}
fn load(raw: &[u8], updated: bool) -> Result<Resource> {
    let bundle = unpack(raw)?;
    let tokenizer = tokenizer::Bank::from_value(bundle.tokenizer.clone())?;
    let detector = STANDARD
        .decode(&bundle.detector)
        .map_err(|_| failure("评分参数格式无效"))?;
    let dir = tempfile::tempdir().map_err(|_| failure("指纹资源无法读取"))?;
    std::fs::write(
        dir.path().join("unified_bank.json"),
        bundle.bank.to_string(),
    )
    .map_err(|_| failure("指纹资源无法读取"))?;
    std::fs::write(dir.path().join("shared_detector.bin"), detector)
        .map_err(|_| failure("评分参数无法读取"))?;
    let detector = lmfpd::Detector::load(&lmfpd::LoadOptions {
        sources: vec![dir.path().to_string_lossy().into_owned()],
        ..Default::default()
    })
    .map_err(|_| failure("指纹库与评分参数不匹配"))?;
    Ok(Resource {
        detector,
        tokenizer,
        metadata: metadata(&bundle, updated),
    })
}
impl Service {
    pub(super) fn lm_resource(&self) -> Result<Arc<Resource>> {
        let mut cached = lock(&self.0.lm);
        if let Some(resource) = cached.as_ref() {
            return Ok(resource.clone());
        }
        let (raw, updated) = bytes(self.0.path.parent().unwrap())?;
        let resource = Arc::new(load(&raw, updated)?);
        *cached = Some(resource.clone());
        Ok(resource)
    }
    fn install_lm_bundle(&self, raw: Vec<u8>) -> Result<Arc<Resource>> {
        let resource = Arc::new(load(&raw, true)?);
        storage::atomic_write_bounded(
            &self.0.path.parent().unwrap().join(CACHE),
            &raw,
            None,
            LIMIT,
        )?;
        Ok(resource)
    }
    pub async fn update_lm_bank(&self) -> Result<Value> {
        self.update_lm_bank_from(
            "https://api.github.com/repos/Ikaleio/lm-detector",
            "https://raw.githubusercontent.com/Ikaleio/lm-detector",
        )
        .await
    }
    async fn update_lm_bank_from(&self, api: &str, raw_source: &str) -> Result<Value> {
        {
            let mut data = lock(&self.0.data);
            if data.lm_updating {
                return Err(failure("指纹库正在更新"));
            }
            data.lm_updating = true;
            data.lm_error = None;
        }
        self.changed();
        let result = async {
            let service = self.clone();
            // Only a fully loaded resource can justify skipping a same-version download.
            let current = tokio::task::spawn_blocking(move || service.lm_resource().ok())
                .await
                .map_err(|_| failure("指纹资源加载失败"))?;
            let revision = current.as_ref().map(|resource| resource.version());
            let raw = download(
                revision.as_ref().and_then(|v| v["revision"].as_str()),
                api,
                raw_source,
            )
            .await?;
            match raw {
                Some(raw) => {
                    let service = self.clone();
                    tokio::task::spawn_blocking(move || service.install_lm_bundle(raw))
                        .await
                        .map_err(|_| failure("指纹资源加载失败"))
                        .and_then(|r| r)
                        .map(|resource| (resource, true))
                }
                None => current
                    .map(|resource| (resource, false))
                    .ok_or_else(|| failure("指纹资源不可用，请重试更新")),
            }
        }
        .await;
        // Keep lock ordering consistent with lm_resource; running jobs retain their Arc.
        if let Ok((resource, _)) = &result {
            *lock(&self.0.lm) = Some(resource.clone());
        }
        let mut data = lock(&self.0.data);
        data.lm_updating = false;
        let mut next = data.saved.clone();
        next.lm_bank.checked_at = Some(self.now());
        match result {
            Ok((resource, updated)) => {
                data.lm_status = resource.metadata.clone();
                next.lm_bank.error = None;
                next.lm_bank.result = Some(if updated { "updated" } else { "up-to-date" }.into());
                if updated {
                    next.lm_bank.updated_at = next.lm_bank.checked_at;
                }
            }
            Err(error) => {
                next.lm_bank.result = Some("failed".into());
                next.lm_bank.error = Some(error.message);
            }
        }
        let result = match self.save(&mut data, next) {
            Ok(()) => Ok(status(&data)),
            Err(_) => {
                let error = failure("检测库更新状态无法保存，请重试");
                data.lm_error = Some(error.message.clone());
                Err(error)
            }
        };
        drop(data);
        self.changed();
        result
    }
}
async fn read(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| failure("指纹库更新连接失败"))?;
    if !response.status().is_success() {
        return Err(failure("指纹库更新被上游拒绝或限流"));
    }
    let mut raw = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("指纹库下载中断"))?
    {
        if raw.len() + chunk.len() > LIMIT as usize {
            return Err(failure("指纹库文件过大"));
        }
        raw.extend_from_slice(&chunk);
    }
    Ok(raw)
}
async fn download(
    current_revision: Option<&str>,
    api: &str,
    raw_source: &str,
) -> Result<Option<Vec<u8>>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("lich13-switch fingerprint-bank")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|_| failure("无法创建指纹更新连接"))?;
    let commit: Value =
        serde_json::from_slice(&read(&client, &format!("{api}/commits/main")).await?)
            .map_err(|_| failure("指纹库提交无效"))?;
    let revision = compat::string(&commit["sha"]);
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("指纹库提交无效"));
    }
    if current_revision == Some(revision) {
        return Ok(None);
    }
    let tree: Value = serde_json::from_slice(
        &read(&client, &format!("{api}/git/trees/{revision}?recursive=1")).await?,
    )
    .map_err(|_| failure("指纹库目录无效"))?;
    if tree["truncated"] == true {
        return Err(failure("指纹库目录不完整"));
    }
    let entries = tree["tree"]
        .as_array()
        .ok_or_else(|| failure("指纹库目录无效"))?;
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    for (path, hash) in manifest["algorithm"].as_object().unwrap() {
        if !entries
            .iter()
            .any(|e| e["path"] == *path && e["sha"] == *hash)
        {
            return Err(failure("上游指纹算法已变化，请升级应用"));
        }
    }
    let mut files = HashMap::new();
    for path in [
        "data/unified_bank.json",
        "data/shared_detector.bin",
        "data/tokenizer_bank.json",
    ] {
        let expected = entries
            .iter()
            .find(|e| e["path"] == path && e["type"] == "blob")
            .ok_or_else(|| failure("指纹资源缺失"))?;
        let raw = read(&client, &format!("{raw_source}/{revision}/{path}")).await?;
        if bank::git_blob_digest(&raw) != expected["sha"] {
            return Err(failure("指纹资源内容校验失败"));
        }
        files.insert(path, raw);
    }
    let original: Value = serde_json::from_slice(&files["data/unified_bank.json"])
        .map_err(|_| failure("指纹库格式无效"))?;
    let models = original["models"]
        .as_array()
        .ok_or_else(|| failure("指纹模型目录缺失"))?;
    if models.is_empty() || models.len() > 256 {
        return Err(failure("指纹模型目录无效"));
    }
    let models: Vec<_> = models
        .iter()
        .map(|m| {
            [
                "id",
                "display_name",
                "family",
                "family_name",
                "response_count",
            ]
            .into_iter()
            .map(|k| (k.to_owned(), m[k].clone()))
            .collect::<serde_json::Map<_, _>>()
        })
        .collect();
    let bundle = Bundle {
        revision: revision.into(),
        bank: json!({"built_at":original["built_at"],"models":models}),
        detector: STANDARD.encode(&files["data/shared_detector.bin"]),
        tokenizer: serde_json::from_slice(&files["data/tokenizer_bank.json"])
            .map_err(|_| failure("分词器库格式无效"))?,
        algorithm: manifest["algorithm"].clone(),
    };
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&serde_json::to_vec(&bundle).map_err(|_| failure("指纹资源无法编码"))?)
        .map_err(|_| failure("指纹资源无法编码"))?;
    gzip.finish()
        .map(Some)
        .map_err(|_| failure("指纹资源无法编码"))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, response, server};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn update_status_survives_restart_and_same_revision_skips_resource_requests() {
        let bundle = unpack(BUNDLE).unwrap();
        let files = HashMap::from([
            (
                "data/unified_bank.json".to_owned(),
                serde_json::to_vec(&bundle.bank).unwrap(),
            ),
            (
                "data/shared_detector.bin".to_owned(),
                STANDARD.decode(bundle.detector).unwrap(),
            ),
            (
                "data/tokenizer_bank.json".to_owned(),
                serde_json::to_vec(&bundle.tokenizer).unwrap(),
            ),
        ]);
        let mut entries: Vec<_> = bundle
            .algorithm
            .as_object()
            .unwrap()
            .iter()
            .map(|(path, sha)| json!({"path":path,"sha":sha,"type":"blob"}))
            .collect();
        entries.extend(files.iter().map(
            |(path, bytes)| json!({"path":path,"sha":bank::git_blob_digest(bytes),"type":"blob"}),
        ));
        let tree = json!({"tree":entries,"truncated":false}).to_string();
        let mode = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = mode.clone();
        let hits = requests.clone();
        let pinned_revision = bundle.revision.clone();
        let port = server(move |request| {
            let path = request.uri().path().to_owned();
            lock(&hits).push(path.clone());
            let mode = state.load(Ordering::SeqCst);
            let reply = if mode == 2 {
                response(503, "text/plain", "fixture unavailable")
            } else if path == "/commits/main" {
                response(200, "application/json", json!({"sha":if mode == 0 { pinned_revision.clone() } else if mode == 3 { "2".repeat(40) } else { "1".repeat(40) }}).to_string())
            } else if path.starts_with("/git/trees/") {
                response(200, "application/json", tree.clone())
            } else if let Some((_, name)) = path.split_once("/data/") {
                if mode == 3 {
                    response(200, "application/octet-stream", "corrupt fixture")
                } else {
                    response(200, "application/octet-stream", files[&format!("data/{name}")].clone())
                }
            } else {
                response(404, "text/plain", "unknown fixture path")
            };
            async move { reply }
        }).await;
        let source = format!("http://127.0.0.1:{port}");
        let (dir, service) = fixture();
        let original = service.lm_resource().unwrap();
        let first = service.update_lm_bank_from(&source, &source).await.unwrap();
        assert_eq!(first["result"], "up-to-date");
        assert_eq!(first["checkedAt"], service.now());
        assert!(first["updatedAt"].is_null());
        assert_eq!(*lock(&requests), vec!["/commits/main"]);
        assert!(!dir.path().join(CACHE).exists());
        let restarted = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
        assert_eq!(status(&lock(&restarted.0.data)), first);

        mode.store(1, Ordering::SeqCst);
        service.0.clock.fetch_add(1000, Ordering::SeqCst);
        let updated = service.update_lm_bank_from(&source, &source).await.unwrap();
        assert_eq!(updated["result"], "updated");
        assert_eq!(updated["version"]["revision"], "1".repeat(40));
        assert_eq!(updated["updatedAt"], service.now());
        assert_eq!(original.version()["revision"], bundle.revision);
        let saved = std::fs::read(dir.path().join(CACHE)).unwrap();
        let restarted = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
        assert_eq!(status(&lock(&restarted.0.data)), updated);

        lock(&requests).clear();
        service.0.clock.fetch_add(1000, Ordering::SeqCst);
        let unchanged = service.update_lm_bank_from(&source, &source).await.unwrap();
        assert_eq!(unchanged["result"], "up-to-date");
        assert_eq!(unchanged["updatedAt"], updated["updatedAt"]);
        assert_eq!(*lock(&requests), vec!["/commits/main"]);
        for failure_mode in [2, 3] {
            mode.store(failure_mode, Ordering::SeqCst);
            service.0.clock.fetch_add(1000, Ordering::SeqCst);
            let failed = service.update_lm_bank_from(&source, &source).await.unwrap();
            assert_eq!(failed["result"], "failed");
            assert_eq!(failed["checkedAt"], service.now());
            assert_eq!(failed["updatedAt"], updated["updatedAt"]);
            assert!(failed["error"].is_string());
            assert_eq!(std::fs::read(dir.path().join(CACHE)).unwrap(), saved);
            let restarted = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
            assert_eq!(status(&lock(&restarted.0.data)), failed);
        }

        // An invalid local bundle must not turn a matching revision into false success.
        std::fs::write(dir.path().join(CACHE), b"corrupt fixture").unwrap();
        let repaired = Service::new(dir.path(), service.0.gateways.clone()).unwrap();
        mode.store(1, Ordering::SeqCst);
        lock(&requests).clear();
        let repair = repaired
            .update_lm_bank_from(&source, &source)
            .await
            .unwrap();
        assert_eq!(repair["result"], "updated");
        assert!(repair["error"].is_null());
        assert!(lock(&requests).len() > 1);
        assert_eq!(
            repaired.lm_resource().unwrap().version()["revision"],
            "1".repeat(40)
        );

        // State write conflicts remain errors; never claim that check metadata was saved.
        let mut external = std::fs::read(&repaired.0.path).unwrap();
        external.push(b' ');
        std::fs::write(&repaired.0.path, &external).unwrap();
        assert!(repaired
            .update_lm_bank_from(&source, &source)
            .await
            .is_err());
        assert_eq!(std::fs::read(&repaired.0.path).unwrap(), external);
        let failed = status(&lock(&repaired.0.data));
        assert_eq!(failed["updating"], false);
        assert_eq!(failed["checkedAt"], repair["checkedAt"]);
        assert_eq!(failed["error"], "检测库更新状态无法保存，请重试");
    }

    #[tokio::test]
    async fn simultaneous_updates_are_rejected_without_changing_check_time() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let entered = started.clone();
        let wait = release.clone();
        let revision = unpack(BUNDLE).unwrap().revision;
        let port = server(move |_| {
            let entered = entered.clone();
            let wait = wait.clone();
            let revision = revision.clone();
            async move {
                entered.notify_one();
                wait.notified().await;
                response(200, "application/json", json!({"sha":revision}).to_string())
            }
        })
        .await;
        let source = format!("http://127.0.0.1:{port}");
        let (_dir, service) = fixture();
        let running = service.clone();
        let url = source.clone();
        let task = tokio::spawn(async move { running.update_lm_bank_from(&url, &url).await });
        tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
            .await
            .unwrap();
        assert!(service.update_lm_bank_from(&source, &source).await.is_err());
        assert!(status(&lock(&service.0.data))["checkedAt"].is_null());
        assert_eq!(status(&lock(&service.0.data))["updating"], true);
        release.notify_one();
        assert_eq!(task.await.unwrap().unwrap()["result"], "up-to-date");
        assert_eq!(status(&lock(&service.0.data))["updating"], false);
    }
    #[test]
    fn pinned_sdk_matches_upstream_scoring_fixtures() {
        let resource = load(BUNDLE, false).unwrap();
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/lm-analyze.json")).unwrap();
        assert_eq!(resource.version()["built_at"], fixture["data"]["built_at"]);
        for case in fixture["cases"].as_array().unwrap() {
            let outputs: Vec<lmfpd::Output> =
                serde_json::from_value(case["outputs"].clone()).unwrap();
            let actual = serde_json::to_value(
                resource
                    .detector
                    .analyze(&outputs, case["strict"] == true)
                    .unwrap(),
            )
            .unwrap();
            let expected = &case["expected"];
            for key in [
                "decision",
                "prediction",
                "used_outputs",
                "probability_status",
            ] {
                assert_eq!(actual[key], expected[key], "{} / {key}", case["name"]);
            }
            match (
                actual["probability"].as_f64(),
                expected["probability"].as_f64(),
            ) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9),
                (None, None) => (),
                _ => panic!("probability mismatch"),
            }
            for (a, b) in actual["results"]
                .as_array()
                .unwrap()
                .iter()
                .zip(expected["results"].as_array().unwrap())
            {
                assert_eq!(a["model"], b["model"]);
                assert!(
                    (a["score"].as_f64().unwrap() - b["score"].as_f64().unwrap()).abs() < 1e-9,
                    "{} / {}: actual {}, expected {}",
                    case["name"],
                    a["model"],
                    a["score"],
                    b["score"]
                );
            }
        }
    }
    fn pack(bundle: &Bundle) -> Vec<u8> {
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&serde_json::to_vec(bundle).unwrap())
            .unwrap();
        gzip.finish().unwrap()
    }
    #[test]
    fn incompatible_or_corrupt_update_keeps_current_bundle() {
        let (dir, service) = super::super::tests::fixture();
        let current = service.lm_resource().unwrap();
        service.install_lm_bundle(BUNDLE.to_vec()).unwrap();
        let original = std::fs::read(dir.path().join(CACHE)).unwrap();
        let mut invalid = unpack(BUNDLE).unwrap();
        invalid.bank["built_at"] = "invalid".into();
        assert!(service.install_lm_bundle(pack(&invalid)).is_err());
        let mut incompatible = unpack(BUNDLE).unwrap();
        incompatible.algorithm = json!({});
        assert!(service.install_lm_bundle(pack(&incompatible)).is_err());
        let mut corrupt = BUNDLE.to_vec();
        corrupt[100] ^= 1;
        assert!(service.install_lm_bundle(corrupt).is_err());
        let mut mismatched = unpack(BUNDLE).unwrap();
        mismatched.tokenizer["classes"][0]["counts"] = json!([]);
        assert!(service.install_lm_bundle(pack(&mismatched)).is_err());
        assert_eq!(std::fs::read(dir.path().join(CACHE)).unwrap(), original);
        assert_eq!(
            current.version()["revision"],
            "567e11d65a48d4fbba2713cc4dfbe685cdd05a73"
        );
    }
}
