//! Antigravity's API runtime is private; no native account configuration is taken over.
use super::*;
use serde_json::{json, Value};

pub(super) fn current(running: bool) -> current::Current {
    let mut value = current::Current::new(if running {
        current::Kind::Gateway
    } else {
        current::Kind::Official
    });
    value.protocol = Some(provider_protocol::Protocol::GeminiNative);
    value
}
pub fn guard_config(home: &Path, text: &str) -> Result<()> {
    let previous =
        storage::read_optional(&ClientId::Agy.config(home))?.unwrap_or_else(|| b"{}".to_vec());
    let old: Value = serde_json::from_slice(&previous)
        .map_err(|_| AppError::new(code::CONFIG, "Antigravity 配置无效，已保留原文件"))?;
    let next: Value = serde_json::from_str(text)
        .map_err(|_| AppError::new(code::CONFIG, "Antigravity 配置无效"))?;
    for key in ["modelProvider", "customModelsConfig"] {
        if old.get(key) != next.get(key) {
            return Err(AppError::new(
                code::GATEWAY_ACTIVE,
                "请先停用网关，再修改 Antigravity 连接配置",
            ));
        }
    }
    Ok(())
}
impl Gateway {
    pub(super) async fn start_agy(&self, home: &Path, expected: &str) -> Result<View> {
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        let token = {
            let state = self.request_state()?;
            if self.view_revision(&state)? != expected {
                return Err(AppError::new(code::CONFLICT, "网关设置已变化，请重试"));
            }
            if state.running {
                drop(state);
                return self.try_view();
            }
            if let Some(error) = &state.upgrade_error {
                return Err(error.clone());
            }
            Self::exit_provider(&state.store, None)?;
            state.store.local_token.clone()
        };
        let listener = self.0.registry.listener.prepare().await?;
        self.0.registry.update_ports()?;
        self.0.registry.listener.check_revision()?;
        let token = self.0.registry.unique_local_token(ClientId::Agy, &token);
        {
            let mut state = self.request_state()?;
            if self.view_revision_with_listener(&state, &listener.previous_revision) != expected {
                return Err(AppError::new(
                    code::CONFLICT,
                    "绑定端口期间设置已变化，请重试",
                ));
            }
            let mut next = state.store.clone();
            for provider in &next.providers {
                for url in Self::endpoint_urls(&next, provider) {
                    model::base_url(&url, listener.port)?;
                }
            }
            next.local_token = token;
            next.settings.port = listener.port;
            next.resume = Some(model::Resume {
                home: home.to_owned(),
                desired: true,
                pair_hash: String::new(),
            });
            self.0.registry.listener.activate_with(
                ClientId::Agy,
                listener.socket,
                self.0.registry.clone(),
                || {
                    state.revision =
                        next.persist(&self.0.data.join("gateway.json"), &state.revision)?;
                    self.replace_store(&mut state, next);
                    state.home = Some(home.to_owned());
                    state.running = true;
                    state.last_successful = None;
                    state.shutdown = Some(watch::channel(false).0);
                    state.error = listener.notice;
                    self.0.admission.configure(&state.store.providers, true);
                    self.0
                        .admission
                        .configure_retention(state.store.settings.thread_idle_release_seconds);
                    Ok(())
                },
            )?;
        }
        self.0.registry.changed();
        self.try_view()
    }
    pub(super) async fn stop_agy(&self, preserve: bool) -> Result<View> {
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        {
            let mut state = self.request_state()?;
            let mut next = state.store.clone();
            if let Some(resume) = next.resume.as_mut() {
                resume.desired = preserve;
            }
            state.revision = next.persist(&self.0.data.join("gateway.json"), &state.revision)?;
            self.replace_store(&mut state, next);
            if let Some(shutdown) = state.shutdown.take() {
                let _ = shutdown.send(true);
            }
            state.running = false;
            state.error = None;
            self.0.admission.configure(&state.store.providers, false);
        }
        self.0.registry.listener.deactivate(ClientId::Agy).await?;
        self.0.clients.lock().unwrap().clear();
        self.0.registry.changed();
        self.try_view()
    }
    pub fn prepare_agy(&self, home: &Path, model: &str, expected: &str) -> Result<()> {
        if !gemini::valid_model(model) {
            return Err(AppError::new(code::CONFIG, "模型 ID 无效"));
        }
        let state = self.request_state()?;
        if self.view_revision(&state)? != expected {
            return Err(AppError::new(code::CONFLICT, "网关设置已变化，请重试"));
        }
        if !state.running {
            return Err(AppError::new(
                code::GATEWAY_ACTIVE,
                "请先启用 Antigravity 网关",
            ));
        }
        if !state.store.providers.iter().any(|p| {
            (state.store.mode == "auto" && p.queued || state.store.selected.as_ref() == Some(&p.id))
                && routing::Requirement::Model(model.into()).allows(p.allowed_models.as_deref())
        }) {
            return Err(AppError::new(
                code::MODEL_NOT_ALLOWED,
                "当前供应商不允许此模型",
            ));
        }
        let port = self.0.registry.listener.request_port()?;
        let file = ClientId::Agy.config(home);
        let raw = storage::read_optional(&file)?;
        let revision = storage::revision(raw.as_deref());
        let text = std::str::from_utf8(raw.as_deref().unwrap_or(b"{}"))
            .map_err(|_| AppError::new(code::CONFIG, "Antigravity 配置不是 UTF-8"))?;
        crate::configuration::validate(ClientId::Agy, text)?;
        let mut config: Value = serde_json::from_str(text)
            .map_err(|_| AppError::new(code::CONFIG, "Antigravity 配置无效"))?;
        config["modelProvider"] = json!("gemini");
        config["model"] = json!(model);
        if config.get("customModelsConfig").is_none() {
            config["customModelsConfig"] = json!({});
        }
        if config["customModelsConfig"].get("customModels").is_none() {
            config["customModelsConfig"]["customModels"] = json!({});
        }
        let key = format!("lich13-switch/{model}");
        config["customModelsConfig"]["customModels"][&key] = json!({
            "label": model,
            "modelName": format!("gemini-api://http://127.0.0.1:{port}/models/{model}"),
            "toolFormatterType": "TOOL_FORMATTER_TYPE_NONE"
        });
        storage::private_dir(&home.join("antigravity-cli"))?;
        storage::atomic_write(
            &file,
            &serde_json::to_vec_pretty(&config)
                .map_err(|_| AppError::new(code::CONFIG, "无法保存 Antigravity 配置"))?,
            Some(&revision),
        )?;
        Ok(())
    }
}

pub fn load_launch_store(data: &Path) -> Result<(bool, String, String)> {
    let (store, _) = model::Store::load(&data.join("gateway.json"))?;
    Ok((
        store.resume.as_ref().is_some_and(|r| r.desired),
        store.local_token,
        ClientId::Agy.address(store.settings.port),
    ))
}
impl Gateway {
    pub fn agy_model(&self) -> Result<Option<String>> {
        let state = self.request_state()?;
        Ok(state
            .store
            .selected
            .as_ref()
            .and_then(|id| state.store.agy_models.get(id))
            .cloned())
    }
}
