//! Client-local model choices. This file never contains connection credentials.
use super::{grok_config::Protocol, ClientId, Gateway};
use crate::{
    errors::code,
    models_dev,
    storage::{self, AppError, Result},
};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
};

const LIMIT: u64 = 256 * 1024;
const INVALID: &str = "模型列表文件不可用，请修复后重试";
fn error(message: &str) -> AppError {
    AppError::new(code::MODELS, message)
}
fn valid_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('/')
        && !id.contains("://")
        && id.as_bytes().get(1) != Some(&b':')
        && !lower.starts_with("users/")
        && !lower.starts_with("home/")
        && !["sk-", "eyj", "ghp_", "gho_", "github_pat_"]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-:/()[]".contains(c))
        && !id
            .as_bytes()
            .windows(32)
            .any(|w| w.iter().all(u8::is_ascii_hexdigit))
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manual {
    pub id: String,
    pub source: Option<String>,
    pub added_at: i64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct File {
    schema: u8,
    pub manual: Vec<Manual>,
    pub pins: BTreeMap<String, String>,
    pub backfilled: BTreeSet<String>,
}
impl Default for File {
    fn default() -> Self {
        Self {
            schema: 1,
            manual: vec![],
            pins: BTreeMap::new(),
            backfilled: BTreeSet::new(),
        }
    }
}
impl File {
    pub fn source(&self, id: &str) -> Option<&str> {
        self.pins.get(id).map(String::as_str).or_else(|| {
            self.manual
                .iter()
                .find(|m| m.id == id)
                .and_then(|m| m.source.as_deref())
        })
    }
    fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::new();
        if self.schema != 1
            || self.manual.len() > 200
            || self.pins.len() > 4096
            || self.manual.iter().any(|m| {
                !valid_id(&m.id)
                    || !ids.insert(&m.id)
                    || m.source.as_ref().is_some_and(|s| !valid_id(s))
            })
            || self
                .pins
                .iter()
                .any(|(id, source)| !valid_id(id) || !valid_id(source))
        {
            return Err(error("模型 ID、来源或数量无效"));
        }
        Ok(())
    }
}
pub struct Service {
    path: PathBuf,
    state: Mutex<(File, String, Option<String>)>,
}
impl Service {
    pub fn new(path: PathBuf) -> Self {
        let loaded = storage::read_bounded(&path, LIMIT).and_then(|raw| {
            let revision = storage::revision(raw.as_deref());
            let file: File = raw
                .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| error(INVALID)))
                .transpose()?
                .unwrap_or_default();
            file.validate()?;
            Ok((file, revision, None))
        });
        Self {
            path,
            state: Mutex::new(
                loaded.unwrap_or_else(|_| (File::default(), String::new(), Some(INVALID.into()))),
            ),
        }
    }
    pub fn snapshot(&self) -> (File, Option<String>) {
        let state = self.state.lock().unwrap();
        (state.0.clone(), state.2.clone())
    }
    fn update(&self, change: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.2.is_some() {
            return Err(error(INVALID));
        }
        let mut next = state.0.clone();
        change(&mut next)?;
        next.validate()?;
        let raw = serde_json::to_vec(&next).map_err(|_| error("无法保存模型列表"))?;
        storage::atomic_write_bounded(&self.path, &raw, Some(&state.1), LIMIT)?;
        *state = (next, storage::digest(&raw), None);
        Ok(())
    }
    pub fn forget(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() || self.snapshot().1.is_some() {
            return Ok(());
        }
        self.update(|file| {
            file.backfilled.retain(|id| !ids.contains(id));
            Ok(())
        })
    }
    pub fn mark_backfilled(&self, id: &str) -> Result<()> {
        self.update(|file| {
            file.backfilled.insert(id.into());
            Ok(())
        })
    }
}
#[derive(Clone, Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub providers: Vec<Source>,
    pub manual: bool,
    pub source: Option<String>,
    pub pinned: Option<String>,
    pub metadata: Option<models_dev::Metadata>,
    pub price: Option<crate::usage::pricing::Reference>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub count: usize,
    pub checked_at: Option<u64>,
    pub stale: bool,
    pub error: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub client_id: ClientId,
    pub passthrough: bool,
    pub hint: &'static str,
    pub models: Vec<Model>,
    pub providers: Vec<Provider>,
    pub models_dev: models_dev::View,
    pub error: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    Add,
    Remove,
    Pin,
    Unpin,
}

impl Gateway {
    pub fn report_model_error(&self, error: AppError) {
        self.0.inner.lock().unwrap().error = Some(error.message);
        self.changed();
    }
    pub(crate) fn union_models(&self, protocol: Option<Protocol>) -> Result<Vec<Model>> {
        let state = self.request_state()?;
        let (file, _) = self.0.model_list.snapshot();
        let mut models: Vec<Model> = vec![];
        for provider in state.store.providers.iter().filter(|p| p.queued) {
            if protocol.is_some_and(|protocol| {
                state
                    .store
                    .grok_connections
                    .get(&provider.id)
                    .is_none_or(|c| c.protocol != protocol)
            }) {
                continue;
            }
            let cached = self
                .0
                .catalog
                .cached(&provider.id, &Self::quota_version(&state.store, provider));
            let ids = provider
                .allowed_models
                .as_ref()
                .or_else(|| (!cached.stale).then_some(&cached.models));
            for id in ids.into_iter().flatten() {
                let index = if let Some(index) = models.iter().position(|m| &m.id == id) {
                    index
                } else {
                    models.push(Model {
                        id: id.clone(),
                        providers: vec![],
                        manual: false,
                        source: None,
                        pinned: None,
                        metadata: None,
                        price: None,
                    });
                    models.len() - 1
                };
                if !models[index].providers.iter().any(|p| p.id == provider.id) {
                    models[index].providers.push(Source {
                        id: provider.id.clone(),
                        name: provider.name.clone(),
                    });
                }
            }
        }
        for manual in &file.manual {
            if let Some(model) = models.iter_mut().find(|m| m.id == manual.id) {
                model.manual = true;
            } else {
                models.push(Model {
                    id: manual.id.clone(),
                    providers: vec![],
                    manual: true,
                    source: None,
                    pinned: None,
                    metadata: None,
                    price: None,
                });
            }
        }
        for model in &mut models {
            model.source = file.source(&model.id).map(str::to_owned);
            model.pinned = file.pins.get(&model.id).cloned();
        }
        Ok(models)
    }
    pub fn gateway_models(&self) -> Result<View> {
        let mut models = self.union_models(None)?;
        let usage = self.0.usage.lock().unwrap().clone();
        let prices = usage.as_ref().and_then(|u| u.prices().ok());
        if let Some(prices) = prices {
            for model in &mut models {
                model.metadata = prices
                    .models_dev
                    .metadata_from(&model.id, model.source.as_deref());
                model.price = prices.reference(&model.id, model.source.as_deref());
            }
        }
        let state = self.request_state()?;
        let providers = state
            .store
            .providers
            .iter()
            .filter(|p| p.queued)
            .map(|p| {
                let v = self
                    .0
                    .catalog
                    .cached(&p.id, &Self::quota_version(&state.store, p));
                Provider {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    count: p.allowed_models.as_ref().map_or(v.models.len(), Vec::len),
                    checked_at: v.checked_at,
                    stale: v.stale,
                    error: v.error,
                }
            })
            .collect();
        Ok(View {
            client_id: self.0.client,
            passthrough: self.0.client == ClientId::Codex,
            hint: match self.0.client {
                ClientId::Codex => "Codex 透传上游目录，本列表不影响 Codex",
                ClientId::Claude => {
                    "Claude Code 需设置 CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 才会读取本列表"
                }
                ClientId::Grok => "Grok 选择器使用配置中的模型表；网关按请求模型名转发",
                ClientId::Dsh => "修改后请同步模型列表，将变更写入 profile",
            },
            models,
            providers,
            models_dev: prices.map(|p| p.models_dev.view()).unwrap_or_default(),
            error: self.0.model_list.snapshot().1,
        })
    }
    pub fn edit_model_list(&self, op: Operation, id: &str, source: Option<&str>) -> Result<View> {
        if self.0.client == ClientId::Codex {
            return Err(error("Codex 模型列表只读"));
        }
        if !valid_id(id) {
            return Err(error("模型 ID 无效"));
        }
        if let Some(source) = source {
            let usage = self.0.usage.lock().unwrap().clone();
            if !usage
                .as_ref()
                .and_then(|u| u.prices().ok())
                .is_some_and(|p| p.models_dev.has_source(source))
            {
                return Err(error("模型来源不存在，请刷新索引后重试"));
            }
        }
        self.0.model_list.update(|file| {
            match op {
                Operation::Add => {
                    if !file.manual.iter().any(|m| m.id == id) {
                        file.manual.push(Manual {
                            id: id.into(),
                            source: source.map(str::to_owned),
                            added_at: crate::usage::model::now(),
                        });
                    }
                }
                Operation::Remove => {
                    file.manual.retain(|m| m.id != id);
                }
                Operation::Pin => {
                    file.pins.insert(
                        id.into(),
                        source.ok_or_else(|| error("请选择模型来源"))?.into(),
                    );
                }
                Operation::Unpin => {
                    file.pins.remove(id);
                }
            }
            Ok(())
        })?;
        self.changed();
        self.gateway_models()
    }
    async fn refresh_model_ids(&self, ids: Vec<String>) -> Result<()> {
        let results = stream::iter(ids.into_iter().map(|id| async move {
            let result = self.list_models(&id, true).await;
            // Removed providers must not be reintroduced by an in-flight query.
            if self
                .request_state()?
                .store
                .providers
                .iter()
                .any(|p| p.id == id)
            {
                self.0.model_list.mark_backfilled(&id)?;
                result?;
            }
            Ok::<_, AppError>(())
        }))
        .buffer_unordered(2)
        .collect::<Vec<_>>()
        .await;
        for result in results {
            result?;
        }
        let usage = self.0.usage.lock().unwrap().clone();
        if let Some(prices) = usage.as_ref().and_then(|u| u.prices().ok()) {
            prices.models_dev.refresh(false).await?;
        }
        self.changed();
        Ok(())
    }
    pub async fn refresh_gateway_models(&self, ids: Option<Vec<String>>) -> Result<View> {
        let selected = {
            let state = self.request_state()?;
            if ids.as_ref().is_some_and(|ids| {
                ids.iter()
                    .any(|id| !state.store.providers.iter().any(|p| &p.id == id))
            }) {
                return Err(error("供应商已变化，请重新读取"));
            }
            state
                .store
                .providers
                .iter()
                .filter(|p| p.queued && ids.as_ref().is_none_or(|ids| ids.contains(&p.id)))
                .map(|p| p.id.clone())
                .collect()
        };
        self.refresh_model_ids(selected).await?;
        self.gateway_models()
    }
    pub async fn backfill_models(&self) -> Result<()> {
        #[cfg(test)]
        if !self
            .0
            .discover_new_providers
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }
        let ids = {
            let state = self.request_state()?;
            let (file, error) = self.0.model_list.snapshot();
            if error.is_some() {
                return Ok(());
            }
            state
                .store
                .providers
                .iter()
                .filter(|p| {
                    p.queued
                        && !file.backfilled.contains(&p.id)
                        && self
                            .0
                            .catalog
                            .cached(&p.id, &Self::quota_version(&state.store, p))
                            .checked_at
                            .is_none()
                })
                .map(|p| p.id.clone())
                .collect()
        };
        self.refresh_model_ids(ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn backfill_records_success_and_failure_once_and_limits_batch_to_two() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let pair = Arc::new(tokio::sync::Barrier::new(2));
        let port = super::super::tests::server({
            let (calls, active, peak, pair) =
                (calls.clone(), active.clone(), peak.clone(), pair.clone());
            move |_| {
                let (calls, active, peak, pair) =
                    (calls.clone(), active.clone(), peak.clone(), pair.clone());
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    pair.wait().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    hyper::Response::builder()
                        .status(if n % 2 == 0 { 200 } else { 500 })
                        .body(super::super::replay::full(bytes::Bytes::from_static(
                            br#"{"data":[{"id":"fixture-model"}]}"#,
                        )))
                        .unwrap()
                }
            }
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let g = Gateway::new(dir.path().join("data")).unwrap();
        {
            let mut state = g.0.inner.lock().unwrap();
            for n in 0..4 {
                state
                    .store
                    .edit(
                        super::super::Edit::SaveProvider {
                            id: None,
                            name: Some(format!("Fixture {n}")),
                            base_url: format!("http://127.0.0.1:{port}/{n}/v1"),
                            token: "fixture-key".into(),
                        },
                        false,
                    )
                    .unwrap();
                state.store.providers.last_mut().unwrap().queued = true;
            }
        }
        g.0.discover_new_providers.store(true, Ordering::Relaxed);
        tokio::time::timeout(std::time::Duration::from_secs(5), g.backfill_models())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            Service::new(g.0.data.join("model-list.json"))
                .snapshot()
                .0
                .backfilled
                .len(),
            4
        );
        let view = g.gateway_models().unwrap();
        assert_eq!(
            view.providers.iter().filter(|p| p.error.is_some()).count(),
            2
        );
        assert!(view.providers.iter().all(|p| p.checked_at.is_some()));
        g.backfill_models().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
    #[test]
    fn choices_roundtrip_and_conflicts_preserve_disk() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("model-list.json");
        let service = Service::new(path.clone());
        service
            .update(|f| {
                f.manual.push(Manual {
                    id: "fixture-model".into(),
                    source: Some("maker/fixture-model".into()),
                    added_at: 1,
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(Service::new(path.clone()).snapshot().0.manual.len(), 1);
        std::fs::write(&path, "external").unwrap();
        assert_eq!(
            service.mark_backfilled("fixture").unwrap_err().code,
            code::CONFLICT
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
        assert!(Service::new(path).snapshot().1.is_some());
    }
    #[test]
    fn invalid_and_oversized_choices_never_replace_file() {
        let root = tempfile::tempdir().unwrap();
        let service = Service::new(root.path().join("model-list.json"));
        assert!(service
            .update(|f| {
                for n in 0..201 {
                    f.manual.push(Manual {
                        id: format!("model-{n}"),
                        source: None,
                        added_at: 0,
                    });
                }
                Ok(())
            })
            .is_err());
        assert!(!valid_id("bad\nmodel"));
        assert!(!valid_id("sk-fixture-credential"));
        assert!(valid_id("tokenfactory/fixture-model(high)"));
        assert!(!service.path.exists());
    }
    #[test]
    fn queued_union_preserves_sources_filters_protocol_and_ignores_stale_cache() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Gateway::new(dir.path().join("data")).unwrap();
        let g = primary
            .grok_companion(dir.path().join("data/grok"))
            .unwrap();
        {
            let mut state = g.0.inner.lock().unwrap();
            for (name, protocol) in [
                ("a", Protocol::Messages),
                ("b", Protocol::ChatCompletions),
                ("c", Protocol::Messages),
            ] {
                state
                    .store
                    .edit(
                        super::super::Edit::SaveGrokProvider {
                            id: None,
                            name: Some(name.into()),
                            base_url: format!("https://{name}.example.invalid/v1"),
                            token: format!("fixture-{name}"),
                            protocol,
                        },
                        false,
                    )
                    .unwrap();
                let p = state.store.providers.last_mut().unwrap();
                p.queued = true;
            }
            state.store.providers[0].allowed_models = Some(vec!["shared".into(), "first".into()]);
            state.store.providers[1].allowed_models = Some(vec!["shared".into(), "second".into()]);
            let p = &state.store.providers[2];
            g.0.catalog
                .commit(&super::super::catalog::View {
                    provider_id: p.id.clone(),
                    version: Gateway::quota_version(&state.store, p),
                    models: vec!["cached".into(), "shared".into()],
                    checked_at: Some(1),
                    stale: false,
                    error: None,
                    retry_at: None,
                    metadata: BTreeMap::new(),
                })
                .unwrap();
        }
        g.0.model_list
            .update(|f| {
                f.manual.push(Manual {
                    id: "manual".into(),
                    source: None,
                    added_at: 1,
                });
                f.manual.push(Manual {
                    id: "shared".into(),
                    source: None,
                    added_at: 1,
                });
                Ok(())
            })
            .unwrap();
        let models = g.union_models(Some(Protocol::Messages)).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["shared", "first", "cached", "manual"]
        );
        assert_eq!(
            models[0]
                .providers
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert!(models[0].manual);
        g.0.inner.lock().unwrap().store.providers[2].version = "fixture-changed".into();
        assert!(!g
            .model_union(Some(Protocol::Messages))
            .unwrap()
            .contains(&"cached".into()));
        assert_eq!(
            g.model_union(Some(Protocol::ChatCompletions)).unwrap(),
            ["shared", "second", "manual"]
        );
        // A damaged local list still permits the provider union and forwarding.
        let corrupt_path = dir.path().join("corrupt/model-list.json");
        storage::private_dir(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, "broken").unwrap();
        let corrupt = Service::new(corrupt_path);
        assert!(corrupt.snapshot().1.is_some());
        assert!(corrupt.snapshot().0.manual.is_empty());
    }
}
