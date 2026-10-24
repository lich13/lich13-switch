use super::{compat::string, failure};
use crate::storage::Result;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::BTreeSet;

fn vector(value: &Value, n: usize, predicate: impl Fn(f64) -> bool) -> bool {
    value.as_array().is_some_and(|v| {
        v.len() == n
            && v.iter()
                .all(|v| v.as_f64().is_some_and(|v| v.is_finite() && predicate(v)))
    })
}
fn matrix(value: &Value, n: usize, dim: usize) -> bool {
    value
        .as_array()
        .is_some_and(|v| v.len() == n && v.iter().all(|row| vector(row, dim, |_| true)))
}
pub fn validate(value: &Value) -> Result<()> {
    let invalid = || failure("指纹库数据无效");
    let models = value["models"].as_array().ok_or_else(invalid)?;
    let robust = &value["robust"];
    let method = &value["method"];
    if value["schema"] != "robust-number-fingerprint-bank"
        || chrono::DateTime::parse_from_rfc3339(string(&value["built_at"])).is_err()
        || value["minimum_valid_numbers"] != 80
        || value["recommended_queries"] != 3
        || method["range"] != json!([1, 355])
        || method["alpha"] != 0.5
        || method["ordered_block_weight"] != 0.25
        || models.is_empty()
        || models.len() > 256
        || robust["robust_ready"] != true
    {
        return Err(invalid());
    }
    let mut ids = BTreeSet::new();
    for model in models {
        for field in ["id", "display_name", "family", "family_name"] {
            if model[field]
                .as_str()
                .is_none_or(|v| v.is_empty() || v.chars().count() > 256)
            {
                return Err(invalid());
            }
        }
        if !ids.insert(string(&model["id"]))
            || !vector(&model["counts"], 355, |v| {
                (0.0..=9007199254740991.0).contains(&v) && v.trunc() == v
            })
        {
            return Err(invalid());
        }
    }
    if robust["model_order"] != json!(models.iter().map(|m| &m["id"]).collect::<Vec<_>>()) {
        return Err(invalid());
    }
    for (key, dim) in [("hellinger", 355), ("ordered_blocks", 74)] {
        let feature = &robust[key];
        let basis = &feature["nuisance_basis"];
        if !vector(&feature["feature_mean"], dim, |_| true)
            || !vector(&feature["feature_scale"], dim, |v| v > 0.0)
            || basis
                .as_array()
                .is_none_or(|v| v.len() > dim || !matrix(basis, v.len(), dim))
            || !matrix(&feature["centroids"], models.len(), dim)
        {
            return Err(invalid());
        }
    }
    let ordered = &robust["ordered_blocks"];
    if ordered["weight"] != 0.25
        || ordered["environment_centroids"]
            .as_array()
            .is_none_or(|env| {
                env.is_empty()
                    || env.len() > 256
                    || !env.iter().all(|v| matrix(v, models.len(), 74))
            })
    {
        return Err(invalid());
    }
    for count in ["1", "2", "3"] {
        let c = &value["calibration"][count];
        if !c["beta"].as_f64().is_some_and(|v| v.is_finite() && v > 0.0)
            || !c["cv_accuracy"]
                .as_f64()
                .is_some_and(|v| v.is_finite() && (0.0..=1.0).contains(&v))
        {
            return Err(invalid());
        }
    }
    Ok(())
}
pub fn git_blob_digest(raw: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", raw.len()));
    hash.update(raw);
    format!("{:x}", hash.finalize())
}

fn cached(saved: &super::types::SavedBank) -> Option<(Value, Value)> {
    if let (Some(data), Some(version), Some(provenance)) =
        (&saved.data, &saved.version, &saved.provenance)
    {
        let manifest = super::modeltrace::manifest();
        let valid = validate(data).is_ok()
            && version["analyzer_version"] == manifest["analyzerVersion"]
            && ["core", "challenge"]
                .iter()
                .all(|key| provenance[format!("{key}_blob")] == manifest["files"][key]["gitBlob"])
            && provenance["stored_sha256"] == super::compat::sha256(data.to_string());
        if valid {
            return Some((data.clone(), version.clone()));
        }
    }
    None
}
pub fn snapshot(saved: &super::types::SavedBank) -> Result<(Value, Value)> {
    cached(saved)
        .map(Ok)
        .unwrap_or_else(super::modeltrace::bundled)
}
pub fn status(saved: &super::types::SavedBank, updating: bool) -> Value {
    let cached = cached(saved);
    let from_cache = cached.is_some();
    match cached.map(Ok).unwrap_or_else(super::modeltrace::bundled) {
        Ok((data, version)) => json!({
            "version": version,
            "source": if from_cache { "cache" } else { "builtin" },
            "models": data["models"].as_array().map(|models| models.iter().map(|m| json!({"id":m["id"],"name":m["display_name"]})).collect::<Vec<_>>()).unwrap_or_default(),
            "checkedAt": saved.checked_at, "updating": updating,
            "error": saved.error.clone().or_else(|| (saved.data.is_some() && !from_cache).then(|| "指纹缓存校验失败，已使用内置库".into())),
        }),
        Err(error) => json!({"error":error.message,"updating":updating,"models":[]}),
    }
}
async fn read(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| failure("指纹库更新连接失败，请稍后重试"))?;
    if !response.status().is_success() {
        return Err(failure("指纹库更新被上游拒绝或限流，请稍后重试"));
    }
    if response
        .content_length()
        .is_some_and(|n| n > 8 * 1024 * 1024)
    {
        return Err(failure("指纹库文件过大"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("指纹库下载中断"))?
    {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err(failure("指纹库文件过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub async fn update() -> Result<super::types::SavedBank> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("lich13-switch fingerprint-bank")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|_| failure("无法创建指纹库连接"))?;
    let manifest = super::modeltrace::manifest();
    let commit: Value = serde_json::from_slice(
        &read(
            &client,
            "https://api.github.com/repos/Hanmo123/ModelTrace/commits/hanmo",
        )
        .await?,
    )
    .map_err(|_| failure("上游指纹库版本无效"))?;
    let revision = string(&commit["sha"]);
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("上游指纹库版本无效"));
    }
    let tree: Value = serde_json::from_slice(
        &read(
            &client,
            &format!(
                "https://api.github.com/repos/Hanmo123/ModelTrace/git/trees/{revision}?recursive=1"
            ),
        )
        .await?,
    )
    .map_err(|_| failure("上游指纹库目录无效"))?;
    if tree["truncated"] == true {
        return Err(failure("上游指纹库目录不完整"));
    }
    let files = tree["tree"]
        .as_array()
        .ok_or_else(|| failure("上游指纹库目录无效"))?;
    let mut provenance = json!({});
    for name in ["core", "challenge", "bank"] {
        let file = files
            .iter()
            .find(|f| f["path"] == manifest["files"][name]["path"] && f["type"] == "blob")
            .ok_or_else(|| failure("上游指纹库缺少校验文件"))?;
        if name != "bank" && file["sha"] != manifest["files"][name]["gitBlob"] {
            return Err(failure("上游指纹算法已变化，请升级应用后更新指纹库"));
        }
        provenance[format!("{name}_blob")] = file["sha"].clone();
    }
    let raw = read(&client, &format!("https://raw.githubusercontent.com/Hanmo123/ModelTrace/{revision}/data/unified_bank.json")).await?;
    if provenance["bank_blob"] != git_blob_digest(&raw) {
        return Err(failure("指纹库内容校验失败，已保留当前版本"));
    }
    let data: Value = serde_json::from_slice(&raw).map_err(|_| failure("指纹库 JSON 无效"))?;
    validate(&data)?;
    provenance["stored_sha256"] = super::compat::sha256(data.to_string()).into();
    let version = json!({"revision":revision,"sha256":super::compat::sha256(&raw),"built_at":data["built_at"],"analyzer_version":manifest["analyzerVersion"]});
    Ok(super::types::SavedBank {
        data: Some(data),
        version: Some(version),
        checked_at: Some(crate::usage::model::now()),
        error: None,
        provenance: Some(provenance),
    })
}
