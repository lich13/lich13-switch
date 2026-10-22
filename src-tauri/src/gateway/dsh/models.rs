use super::*;

pub const FLASH: &str = "deepseek-flash";
pub const PRO: &str = "deepseek-v4-pro";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelMapping {
    pub flash: Option<String>,
    pub pro: Option<String>,
}
impl ModelMapping {
    pub fn resolve(&self, model: &str) -> Option<&str> {
        match model {
            FLASH => self.flash.as_deref(),
            PRO => self.pro.as_deref(),
            _ => None,
        }
    }
    pub fn allowed(&self) -> Vec<String> {
        [FLASH, PRO]
            .into_iter()
            .filter(|id| self.resolve(id).is_some())
            .map(str::to_owned)
            .collect()
    }
    pub fn validate(&mut self) -> Result<()> {
        for model in [&mut self.flash, &mut self.pro] {
            *model = model
                .take()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty());
            if model
                .as_ref()
                .is_some_and(|v| v.len() > 256 || v.chars().any(|c| c.is_control() || c == '*'))
            {
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
        Self {
            flash: unique("flash"),
            pro: unique("pro"),
        }
    }
}

pub fn catalog() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"id": FLASH, "name": "DeepSeek-V4.1-Flash"}),
        serde_json::json!({"id": PRO, "name": "DeepSeek-V4-Pro"}),
    ]
}

pub(super) fn require_chat(protocol: Protocol) -> Result<()> {
    if protocol != Protocol::ChatCompletions {
        return Err(error("DSH 仅支持 Chat Completions，请先修改供应商协议"));
    }
    Ok(())
}

pub(super) fn default_model(model: Option<&str>) -> &'static str {
    if model.is_some_and(|m| m == PRO || ModelMapping::infer(&[m.to_owned()]).pro.is_some()) {
        PRO
    } else {
        FLASH
    }
}
