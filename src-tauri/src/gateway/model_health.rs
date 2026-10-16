//! A model's upstream capacity must never close unrelated models on the same key.
use super::circuit::{Circuit, CircuitState, Health};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

const MAX_MODELS: usize = 1024;

#[derive(Clone, Default)]
pub(super) struct Models(Arc<Mutex<HashMap<String, Circuit>>>);

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelHealth {
    pub model: String,
    pub health: Health,
}

#[cfg(test)]
#[path = "model_probe_tests.rs"]
mod v0232_tests;

impl Models {
    pub fn circuit(&self, model: &str) -> Circuit {
        let mut models = self.0.lock().unwrap();
        if let Some(circuit) = models.get(model) {
            return circuit.clone();
        }
        if models.len() >= MAX_MODELS {
            models.retain(|_, c| !c.discardable());
        }
        if models.len() >= MAX_MODELS {
            // Retain every in-flight or cooling model. An untracked model still
            // uses the request's bounded/cancellable retry policy, never the
            // provider breaker as a fallback for a full metadata cache.
            return Circuit::default();
        }
        models.entry(model.into()).or_default().clone()
    }
    pub fn states(&self) -> Vec<ModelHealth> {
        let mut result: Vec<_> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(model, c)| {
                let health = c.health();
                (!health.available
                    || health.state != CircuitState::Closed
                    || health.failures != 0
                    || health.cause.is_some())
                .then(|| ModelHealth {
                    model: crate::events::safe_model(model).unwrap_or_else(|| "未知模型".into()),
                    health,
                })
            })
            .collect();
        result.sort_by(|a, b| a.model.cmp(&b.model));
        result
    }
    pub fn circuits(&self) -> Vec<Circuit> {
        self.0.lock().unwrap().values().cloned().collect()
    }
}
