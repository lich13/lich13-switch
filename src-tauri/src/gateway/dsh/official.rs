//! Account discovery preserves DSH login; managed routes synchronize transactionally.
use super::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Serialize)]
pub struct CatalogView {
    pub models: Vec<Value>,
    pub error: Option<String>,
}

fn account_token(home: &Path) -> Result<String> {
    let raw = text(&home.join(".credentials.yaml"))?;
    let record = credentials(raw.as_deref())?
        .get_mapping("records")
        .and_then(|r| r.get_mapping("deepseek-account-platform/default"))
        .filter(|r| string(r.get("kind")).as_deref() == Some("grant"))
        .and_then(|r| r.get_mapping("payload"))
        .filter(|r| r.get("version").and_then(|v| v.to_i64()) == Some(1))
        .filter(|r| string(r.get("issuer")).as_deref() == Some("https://platform.deepseek.com"));
    record
        .and_then(|r| string(r.get("token")))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| error("请先在 DSH 登录官方账号，再刷新模型"))
}

fn installed_version(home: &Path) -> Option<semver::Version> {
    let mut roots = vec![home.to_owned(), home.join("runtime")];
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            for name in ["dsh", "dsh.cmd", "dsh.exe"] {
                if let Ok(binary) = std::fs::canonicalize(path.join(name)) {
                    roots.extend(binary.ancestors().take(9).map(Path::to_owned));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    roots.push(PathBuf::from("/Applications/DeepSeek Harness.app/Contents"));
    for root in roots {
        #[cfg(target_os = "macos")]
        if let Ok(info) = plist::Value::from_file(root.join("Info.plist")) {
            if let Some(version) = info
                .as_dictionary()
                .and_then(|d| d.get("CFBundleShortVersionString"))
                .and_then(|v| v.as_string())
                .and_then(|v| semver::Version::parse(v).ok())
            {
                return Some(version);
            }
        }
        for path in [
            root.join("package.json"),
            root.join("node_modules/@deepseek-ai/dsh/package.json"),
        ] {
            if let Ok(Some(raw)) = text(&path) {
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    if value["name"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("@deepseek-ai/dsh"))
                    {
                        if let Some(version) = value["version"]
                            .as_str()
                            .and_then(|s| semver::Version::parse(s).ok())
                        {
                            return Some(version);
                        }
                    }
                }
            }
        }
    }
    None
}

fn parse_catalog(body: &Value, version: Option<&semver::Version>) -> Result<Vec<Value>> {
    let rows = body["data"]
        .as_array()
        .filter(|v| v.len() <= 4096)
        .ok_or_else(|| error("官方模型目录格式无效，请稍后重试"))?;
    let mut seen = BTreeSet::new();
    let mut models = vec![];
    for row in rows {
        let Some(id) = row["id"].as_str().filter(|id| models::valid_id(id)) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        if row
            .get("client_requirements")
            .is_some_and(|v| !v.is_object())
            || row
                .pointer("/client_requirements/deepseek-harness")
                .is_some_and(|v| !v.is_object())
        {
            continue;
        }
        let requirement = row.pointer("/client_requirements/deepseek-harness/min_version");
        let supported = match requirement {
            Some(minimum) => minimum
                .as_str()
                .and_then(|v| semver::Version::parse(v).ok())
                .zip(version)
                .is_some_and(|(minimum, current)| current >= &minimum),
            None => matches!(id, models::FLASH | models::PRO),
        };
        let capabilities = row.pointer("/api_capabilities/anthropic_messages");
        if !supported || !capabilities.is_some_and(Value::is_object) {
            continue;
        }
        if row
            .get("output_modalities")
            .is_some_and(|v| !v.as_array().is_some_and(|v| v.iter().any(|v| v == "text")))
        {
            continue;
        }
        let inputs = row
            .get("input_modalities")
            .cloned()
            .unwrap_or_else(|| json!(["text"]));
        let Some(inputs) = inputs.as_array() else {
            continue;
        };
        let inputs: Vec<_> = inputs
            .iter()
            .filter(|v| matches!(v.as_str(), Some("text" | "image")))
            .cloned()
            .collect();
        if inputs.is_empty() {
            continue;
        }
        let mut model = json!({"id":id, "name":row["name"].as_str().filter(|n| !n.is_empty() && n.len() <= 256 && !n.chars().any(char::is_control)).unwrap_or(id), "input":inputs});
        let mut valid = true;
        for (from, to) in [
            ("context_window", "contextWindow"),
            ("max_output_tokens", "maxTokens"),
        ] {
            if let Some(value) = row.get(from) {
                if value
                    .as_u64()
                    .is_some_and(|n| n > 0 && n <= 9_007_199_254_740_991)
                {
                    model[to] = value.clone();
                } else {
                    valid = false;
                }
            }
        }
        if valid {
            models::normalize(&mut model);
            models.push(model);
        }
    }
    if models.is_empty() {
        return Err(error(
            "官方目录没有当前 DSH 版本支持的模型，请检查 DSH 更新后重试",
        ));
    }
    Ok(models)
}

async fn fetch_catalog(home: &Path, token: &str) -> Result<Vec<Value>> {
    request_catalog(
        "https://api.deepseek.com/models",
        installed_version(home),
        token,
    )
    .await
}
async fn request_catalog(
    url: &str,
    version: Option<semver::Version>,
    token: &str,
) -> Result<Vec<Value>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|_| error("无法查询官方模型目录"))?;
    let mut response = client
        .get(url)
        .header("x-dsh-auth-token", token)
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            format!(
                "deepseek-harness/{}",
                version
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown".into())
            ),
        )
        .send()
        .await
        .map_err(|_| error("官方模型目录查询失败，请检查网络后重试"))?;
    if !response.status().is_success() {
        return Err(error(
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                "DSH 官方登录已失效，请重新登录后刷新模型"
            } else {
                "官方模型目录暂时不可用，请稍后重试"
            },
        ));
    }
    let mut bytes = vec![];
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("官方模型目录读取失败，请重试"))?
    {
        if bytes.len() + chunk.len() > LIMIT as usize {
            return Err(error("官方模型目录超过大小限制"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice(&bytes).map_err(|_| error("官方模型目录格式无效，请重试"))?;
    parse_catalog(&body, version.as_ref())
}

impl Gateway {
    pub async fn dsh_official_models(&self, home: &Path, refresh: bool) -> Result<CatalogView> {
        if self.0.client != ClientId::Dsh {
            return Err(error("此操作仅适用于 DSH"));
        }
        let mut failure = None;
        if refresh {
            let token = account_token(home);
            let result = match &token {
                Ok(token) => fetch_catalog(home, token).await,
                Err(e) => Err(e.clone()),
            };
            let result = match result {
                Ok(models) => {
                    self.save_official_catalog(home, token.as_deref().unwrap(), models)
                        .await
                }
                Err(e) => Err(e),
            };
            failure = result.err().map(|e| e.message);
        }
        Ok(CatalogView {
            models: models::for_store(&self.0.inner.lock().unwrap().store),
            error: failure,
        })
    }
    async fn save_official_catalog(
        &self,
        home: &Path,
        token: &str,
        models: Vec<Value>,
    ) -> Result<()> {
        let _lifecycle = self.0.lifecycle.lock().await;
        let _mutation = self.0.registry.mutation.lock().unwrap();
        self.recover_dsh_state()?;
        if account_token(home).as_deref().ok() != Some(token) {
            return Err(error("DSH 官方登录已变化，请重新刷新模型"));
        }
        let mut state = self.0.inner.lock().unwrap();
        let mut next = state.store.clone();
        next.dsh_catalog = models;
        state.revision = commit_mapping(&self.0.data, home, &mut next, &state.revision)?;
        self.replace_store(&mut state, next);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str) -> Value {
        json!({"id":id,"name":id,"context_window":1048576,"max_output_tokens":131072,"input_modalities":["text","image"],"api_capabilities":{"anthropic_messages":{}}})
    }
    #[test]
    fn catalog_accepts_future_ids_only_with_compatible_requirements() {
        let mut future = entry("deepseek-future");
        future["client_requirements"] = json!({"deepseek-harness":{"min_version":"0.2.1"}});
        let body = json!({"data":[entry(models::FLASH), future, entry("unannounced")]});
        let version = semver::Version::parse("0.2.1").unwrap();
        let models = parse_catalog(&body, Some(&version)).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[1]["id"], "deepseek-future");
        assert_eq!(models[1]["contextWindow"], 1048576);
        assert_eq!(
            parse_catalog(&body, Some(&semver::Version::parse("0.2.1-rc.2").unwrap()))
                .unwrap()
                .len(),
            1
        );
        assert!(parse_catalog(&json!({"data":[]}), None).is_err());
        assert!(parse_catalog(
            &json!({"data":[{"id":models::FLASH,"client_requirements":false}]}),
            None
        )
        .is_err());
    }
    #[tokio::test]
    async fn successful_catalog_persists_without_invalidating_mapping_drafts_or_changing_profiles()
    {
        let (home, _parent, gateway) = tests_v02516::fixture();
        let path = home.path().join(".credentials.yaml");
        let raw = "version: 1\nrefs: {FIXTURE_KEY: fixture-provider-secret}\nrecords:\n  deepseek-account-platform/default:\n    kind: grant\n    payload: {version: 1, issuer: 'https://platform.deepseek.com', token: fixture-account-token}\n";
        std::fs::write(&path, raw).unwrap();
        let profile = std::fs::read(home.path().join("profiles/desktop").join(PATCH)).unwrap();
        let revision = gateway.view().revision;
        let token = account_token(home.path()).unwrap();
        let catalog = parse_catalog(&json!({"data":[entry(models::FLASH)]}), None).unwrap();
        gateway
            .save_official_catalog(home.path(), &token, catalog.clone())
            .await
            .unwrap();
        assert_eq!(gateway.view().revision, revision);
        let (store, _) =
            super::super::super::model::Store::load(&gateway.0.data.join("gateway.json")).unwrap();
        assert_eq!(store.dsh_catalog, catalog);
        assert_eq!(
            gateway
                .dsh_official_models(home.path(), false)
                .await
                .unwrap()
                .models,
            catalog
        );
        assert_eq!(
            std::fs::read(home.path().join("profiles/desktop").join(PATCH)).unwrap(),
            profile
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert!(gateway
            .save_official_catalog(home.path(), "expired-fixture-token", vec![])
            .await
            .is_err());
        assert_eq!(
            gateway
                .dsh_official_models(home.path(), false)
                .await
                .unwrap()
                .models,
            catalog
        );
    }
    #[tokio::test]
    async fn failed_refresh_retains_saved_catalog_and_does_not_change_credentials() {
        let (home, _parent, gateway) = tests_v02516::fixture();
        gateway
            .0
            .inner
            .lock()
            .unwrap()
            .editable_mut()
            .store
            .dsh_catalog = vec![json!({"id":"deepseek-future","name":"Future"})];
        let before = std::fs::read(home.path().join(".credentials.yaml")).unwrap();
        let result = gateway
            .dsh_official_models(home.path(), true)
            .await
            .unwrap();
        assert!(result.error.is_some());
        assert_eq!(result.models[0]["id"], "deepseek-future");
        assert_eq!(
            before,
            std::fs::read(home.path().join(".credentials.yaml")).unwrap()
        );
    }
    #[tokio::test]
    async fn listing_uses_account_header_and_refuses_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for redirect in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/models", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let n = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                assert!(request.contains("x-dsh-auth-token: fixture-account-token"));
                assert!(request.starts_with("GET /models "));
                let body = json!({"data":[entry(models::FLASH)]}).to_string();
                let response = if redirect {
                    "HTTP/1.1 302 Found\r\nLocation: https://example.invalid/models\r\nContent-Length: 0\r\n\r\n".into()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let result = request_catalog(&url, None, "fixture-account-token").await;
            assert_eq!(result.is_err(), redirect);
            server.await.unwrap();
        }
    }
}
