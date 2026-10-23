use super::*;

pub const FLASH: &str = "deepseek-flash";
pub const PRO: &str = "deepseek-v4-pro";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelMapping(pub BTreeMap<String, Option<String>>);
impl ModelMapping {
    pub fn resolve(&self, model: &str) -> Option<&str> {
        self.0.get(model).and_then(|value| value.as_deref())
    }
    pub fn allowed(&self) -> Vec<String> {
        self.0
            .keys()
            .filter(|id| self.resolve(id).is_some())
            .cloned()
            .collect()
    }
    pub fn validate(&mut self) -> Result<()> {
        if self.0.len() > 4096 {
            return Err(error("模型映射数量超限"));
        }
        for (id, model) in &mut self.0 {
            if !valid_id(id) {
                return Err(error("请输入完整官方模型 ID，不支持通配符"));
            }
            *model = model
                .take()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty());
            if model.as_ref().is_some_and(|v| !valid_id(v)) {
                return Err(error("请输入完整模型 ID，不支持通配符"));
            }
        }
        Ok(())
    }
    pub fn infer(models: &[String]) -> Self {
        let unique = |kind: &str| {
            let candidates: std::collections::BTreeSet<_> = models
                .iter()
                .filter(|id| {
                    let name = id.to_ascii_lowercase();
                    name.contains("deepseek")
                        && name
                            .split(|c: char| !c.is_ascii_alphanumeric())
                            .any(|part| part == kind)
                })
                .cloned()
                .collect();
            (candidates.len() == 1)
                .then(|| candidates.into_iter().next())
                .flatten()
        };
        Self(BTreeMap::from([
            (FLASH.into(), unique("flash")),
            (PRO.into(), unique("pro")),
        ]))
    }
}

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.trim() == id
        && id.len() <= 256
        && !id.chars().any(|c| c.is_control() || c == '*')
}

pub fn catalog() -> Vec<serde_json::Value> {
    let mut models = vec![
        serde_json::json!({"id": FLASH, "name": "DeepSeek-V4.1-Flash", "input": ["text", "image"]}),
        serde_json::json!({"id": PRO, "name": "DeepSeek-V4-Pro", "input": ["text"]}),
    ];
    for model in &mut models {
        normalize(model);
    }
    models
}

/// Retain explicit older IDs without guessing which new model replaces them.
pub fn for_store(store: &super::super::model::Store) -> Vec<serde_json::Value> {
    let mut saved = vec![];
    if let Some(snapshot) = &store.dsh {
        if let Ok(Some(route)) = sequence(&snapshot.patch.after).and_then(|s| owned_route(&s)) {
            if let Some(entries) = route.as_mapping().and_then(|m| m.get_sequence("models")) {
                for entry in entries.values() {
                    if let Some(id) = string(entry.get("id")).filter(|id| valid_id(id)) {
                        let name = string(entry.get("name")).unwrap_or_else(|| id.clone());
                        let mut model = serde_json::json!({"id":id,"name":name});
                        for key in ["contextWindow", "maxTokens"] {
                            if let Some(value) =
                                entry.get(key).and_then(|v| v.to_i64()).filter(|v| *v > 0)
                            {
                                model[key] = value.into();
                            }
                        }
                        if let Some(inputs) = entry.as_mapping().and_then(|m| {
                            m.get_sequence("input")
                                .or_else(|| m.get_sequence("inputModalities"))
                        }) {
                            let values: Vec<_> = inputs
                                .values()
                                .filter_map(|v| string(Some(v)))
                                .filter(|v| matches!(v.as_str(), "text" | "image"))
                                .collect();
                            if !values.is_empty() {
                                model["input"] = serde_json::json!(values);
                            }
                        }
                        if let Some(efforts) = entry
                            .as_mapping()
                            .and_then(|m| m.get_mapping("reasoningEfforts"))
                        {
                            let mut values = serde_json::Map::new();
                            for key in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
                                if let Some(value) = efforts.get(key) {
                                    if value.as_scalar().is_some_and(|s| s.is_null()) {
                                        values.insert(key.into(), serde_json::Value::Null);
                                    } else if let Some(value) = string(Some(value)) {
                                        values.insert(key.into(), value.into());
                                    }
                                }
                            }
                            if !values.is_empty() {
                                model["reasoningEfforts"] = values.into();
                            }
                        }
                        saved.push(model);
                    }
                }
            }
        }
    }
    let mut models = if store.dsh_catalog.is_empty() {
        if saved.is_empty() {
            catalog()
        } else {
            saved.clone()
        }
    } else {
        store.dsh_catalog.clone()
    };
    for model in &mut models {
        if let Some(previous) = saved.iter().find(|m| m["id"] == model["id"]) {
            if let Some(efforts) = previous.get("reasoningEfforts") {
                model["reasoningEfforts"] = efforts.clone();
            }
        }
    }
    let saved_choices: Vec<_> = store
        .dsh
        .as_ref()
        .map(Snapshot::entries)
        .unwrap_or_default()
        .into_values()
        .filter_map(|p| p.options.model)
        .collect();
    for id in store
        .dsh_models
        .values()
        .flat_map(|m| m.0.keys())
        .chain(saved_choices.iter())
    {
        if !models.iter().any(|m| m["id"] == *id) {
            models.push(serde_json::json!({"id":id,"name":id}));
        }
    }
    for model in &mut models {
        normalize(model);
    }
    models
}

pub(super) fn require_chat(protocol: Protocol) -> Result<()> {
    if protocol != Protocol::ChatCompletions {
        return Err(error("DSH 仅支持 Chat Completions，请先修改供应商协议"));
    }
    Ok(())
}

/// Normalize old saved catalogs while keeping official capabilities authoritative.
pub(crate) fn normalize(model: &mut serde_json::Value) {
    let Some(fields) = model.as_object_mut() else {
        return;
    };
    if let Some(old) = fields.remove("inputModalities") {
        fields.entry("input").or_insert(old);
    }
    if !fields.contains_key("input") {
        let input = if fields.get("id").and_then(|v| v.as_str()) == Some(FLASH) {
            serde_json::json!(["text", "image"])
        } else {
            serde_json::json!(["text"])
        };
        fields.insert("input".into(), input);
    }
    if matches!(fields.get("id").and_then(|v| v.as_str()), Some(FLASH | PRO)) {
        // The managed provider has no built-in catalog to inherit reasoning from.
        fields.entry("reasoningEfforts").or_insert_with(
            || serde_json::json!({"off":"none", "low":"low", "high":"high", "max":"max"}),
        );
    }
}
