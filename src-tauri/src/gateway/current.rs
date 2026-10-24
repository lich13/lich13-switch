//! Read effective selection without applying takeover-specific restrictions.
use super::{grok_config, grok_layers, model::Store, takeover::Pair, ClientId};
use serde::Serialize;
use std::path::Path;
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Kind {
    Gateway,
    Provider { id: String, by: Match },
    Official,
    Mixed,
    Unmatched { host: String },
    Unknown { reason: String },
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Match {
    UrlToken,
    UrlOnly,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Current {
    #[serde(flatten)]
    pub state: Kind,
    pub model: Option<String>,
    pub model_source: Option<String>,
    pub warning: Option<String>,
    pub provider_name: Option<String>,
    pub address: Option<String>,
    pub address_source: Option<String>,
    pub credential_source: Option<String>,
    pub protocol: Option<super::provider_protocol::Protocol>,
}
impl Current {
    pub fn new(state: Kind) -> Self {
        Self {
            state,
            model: None,
            model_source: None,
            warning: None,
            provider_name: None,
            address: None,
            address_source: None,
            credential_source: None,
            protocol: None,
        }
    }
    pub fn unknown(reason: &str) -> Self {
        Self::new(Kind::Unknown {
            reason: reason.into(),
        })
    }
    pub fn legacy(&self) -> (&'static str, Option<String>) {
        match &self.state {
            Kind::Gateway => ("gateway", None),
            Kind::Provider { id, .. } => ("provider", Some(id.clone())),
            Kind::Official | Kind::Unmatched { .. } | Kind::Mixed => ("unsaved", None),
            Kind::Unknown { .. } => ("unknown", None),
        }
    }
}
pub fn normalize_base(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/v1" } else { &path });
    Some(url.to_string())
}
pub(super) fn pair(client: ClientId, pair: &Pair, store: &Store, ports: &[u16]) -> Current {
    let gateway = pair.is_gateway(client, &store.local_token, ports)
        || (client == ClientId::Dsh
            && pair.base_url.as_ref().is_some_and(|base| {
                url::Url::parse(base).is_ok_and(|u| u.path().trim_end_matches('/').is_empty())
            })
            && pair.is_gateway(ClientId::Claude, &store.local_token, ports));
    if gateway {
        return Current::new(Kind::Gateway);
    }
    let Some(base) = pair.base_url.as_deref().and_then(normalize_base) else {
        return Current::unknown("未找到可识别的连接地址");
    };
    let candidates: Vec<_> = store
        .providers
        .iter()
        .filter(|p| normalize_base(&p.base_url).as_deref() == Some(&base))
        .collect();
    let matched: Vec<_> = candidates
        .iter()
        .filter(|p| {
            pair.token.as_ref().is_some_and(|token| {
                super::listener::token_matches(token.as_bytes(), p.token.as_bytes())
            })
        })
        .collect();
    if matched.len() == 1 {
        return Current::new(Kind::Provider {
            id: matched[0].id.clone(),
            by: Match::UrlToken,
        });
    }
    if pair.token.is_none() && candidates.len() == 1 {
        return Current::new(Kind::Provider {
            id: candidates[0].id.clone(),
            by: Match::UrlOnly,
        });
    }
    if matched.len() > 1 || pair.token.is_none() && candidates.len() > 1 {
        return Current::unknown("地址对应多家供应商，无法确定凭据归属");
    }
    let host = url::Url::parse(&base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    Current::new(Kind::Unmatched { host })
}
pub fn grok(
    home: &Path,
    store: &Store,
    ports: &[u16],
    resolve: &impl Fn(&str) -> Option<String>,
) -> Current {
    let effective = match grok_layers::read(home, resolve) {
        Ok(value) => value,
        Err(e) => return Current::unknown(&e.message),
    };
    let mut result = if let Some(error) = &effective.selection_error {
        Current::unknown(error)
    } else {
        let resolve = |name: &str| effective.resolve(name, resolve);
        match grok_config::profile_with_env(&effective.text, &resolve) {
            Err(e) => Current::unknown(&e.message),
            Ok(profile) => {
                let mut current = if profile.selected.is_empty() {
                    Current::unknown("尚未选择默认模型")
                } else if profile.pair.base_url.is_none()
                    || profile.pair.base_url.as_ref().is_some_and(|base| {
                        url::Url::parse(base).is_ok_and(|u| u.host_str() == Some("api.x.ai"))
                    })
                {
                    Current::new(Kind::Official)
                } else {
                    pair(ClientId::Grok, &profile.pair, store, ports)
                };
                current.model = crate::events::safe_model(&profile.model);
                current.protocol = profile
                    .protocol_known
                    .then_some(profile.connection.protocol);
                current.provider_name = profile
                    .table
                    .get("name")
                    .and_then(toml_edit::Item::as_str)
                    .and_then(crate::events::safe_message);
                current.address_source = profile.address_source.map(str::to_owned);
                current.address =
                    super::grok_import::safe_address(profile.pair.base_url.as_deref());
                current.credential_source = Some(
                    if profile
                        .table
                        .get("api_key")
                        .and_then(toml_edit::Item::as_str)
                        .is_some_and(|v| !v.is_empty())
                    {
                        "模型 api_key".into()
                    } else if profile
                        .credential_names
                        .iter()
                        .any(|name| effective.variables.contains_key(name))
                    {
                        "endpoint.env".into()
                    } else if profile.pair.token.is_some() {
                        "进程环境".into()
                    } else {
                        "未读取到".into()
                    },
                );
                current
            }
        }
    };
    result.model_source = effective.model_source;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_model_env_and_requirements_are_distinct_from_takeover() {
        let root = tempfile::tempdir().unwrap();
        let text = "[models]\ndefault='relay'\n[model.relay]\nbase_url='https://Relay.Example.invalid/'\nenv_key='FIXTURE_KEY'\n[model.lich13-switch]\nbase_url='http://127.0.0.1:43721/v1'\napi_key='fixture-local'\n";
        std::fs::write(root.path().join("config.toml"), text).unwrap();
        let mut store = Store {
            local_token: "fixture-local".into(),
            ..Default::default()
        };
        store
            .edit(
                super::super::Edit::SaveProvider {
                    search_only: None,
                    quota_access_token: None,
                    id: None,
                    base_url: "https://relay.example.invalid/v1".into(),
                    token: "fixture-key".into(),
                    name: None,
                },
                false,
            )
            .unwrap();
        assert!(matches!(
            grok(root.path(), &store, &[43721], &|_| None).state,
            Kind::Provider {
                by: Match::UrlOnly,
                ..
            }
        ));
        assert!(matches!(
            grok(root.path(), &store, &[43721], &|k| (k == "FIXTURE_KEY")
                .then(|| "fixture-key".into()))
            .state,
            Kind::Provider {
                by: Match::UrlToken,
                ..
            }
        ));
        assert!(matches!(
            grok(root.path(), &store, &[43721], &|k| (k == "FIXTURE_KEY")
                .then(|| "wrong".into()))
            .state,
            Kind::Unmatched { .. }
        ));
        let selected = grok(root.path(), &store, &[43721], &|k| {
            (k == "GROK_DEFAULT_MODEL").then(|| "lich13-switch".into())
        });
        assert_eq!(selected.state, Kind::Gateway);
        assert_eq!(
            selected.model_source.as_deref(),
            Some("环境 GROK_DEFAULT_MODEL")
        );
        let serialized = serde_json::to_string(&selected).unwrap();
        assert!(
            !serialized.contains("fixture-local")
                && !serialized.contains(root.path().to_str().unwrap())
        );
        std::fs::write(
            root.path().join("requirements.toml"),
            "[models]\ndisabled_models=['relay']\n",
        )
        .unwrap();
        assert_eq!(
            grok(root.path(), &store, &[43721], &|_| None).state,
            Kind::Unknown {
                reason: "受 requirements 限制".into()
            }
        );
    }
    #[test]
    fn addresses_preserve_custom_paths() {
        assert_eq!(
            normalize_base("https://Example.invalid"),
            normalize_base("https://example.invalid:443/v1/")
        );
        assert_ne!(
            normalize_base("https://example.invalid/custom"),
            normalize_base("https://example.invalid/v1")
        );
    }
}
