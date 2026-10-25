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
        {
            let mut data = lock(&self.0.data);
            if data.lm_updating {
                return Err(failure("指纹库正在更新"));
            }
            data.lm_updating = true;
            data.lm_error = None;
        }
        self.changed();
        let result = download().await;
        let result = match result {
            Ok(raw) => {
                let service = self.clone();
                tokio::task::spawn_blocking(move || service.install_lm_bundle(raw))
                    .await
                    .map_err(|_| failure("指纹资源加载失败"))
                    .and_then(|r| r)
            }
            Err(e) => Err(e),
        };
        let status = match result {
            Ok(resource) => {
                let status = resource.metadata.clone();
                *lock(&self.0.lm) = Some(resource);
                lock(&self.0.data).lm_status = status.clone();
                Ok(status)
            }
            Err(error) => {
                lock(&self.0.data).lm_error = Some(error.message.clone());
                Err(error)
            }
        };
        lock(&self.0.data).lm_updating = false;
        self.changed();
        status
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
async fn download() -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("lich13-switch fingerprint-bank")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .map_err(|_| failure("无法创建指纹更新连接"))?;
    let commit: Value = serde_json::from_slice(
        &read(
            &client,
            "https://api.github.com/repos/Ikaleio/lm-detector/commits/main",
        )
        .await?,
    )
    .map_err(|_| failure("指纹库提交无效"))?;
    let revision = compat::string(&commit["sha"]);
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("指纹库提交无效"));
    }
    let tree: Value = serde_json::from_slice(
        &read(
            &client,
            &format!(
                "https://api.github.com/repos/Ikaleio/lm-detector/git/trees/{revision}?recursive=1"
            ),
        )
        .await?,
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
        let raw = read(
            &client,
            &format!("https://raw.githubusercontent.com/Ikaleio/lm-detector/{revision}/{path}"),
        )
        .await?;
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
    gzip.finish().map_err(|_| failure("指纹资源无法编码"))
}

#[cfg(test)]
mod tests {
    use super::*;
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
