//! Model policy is independent of transport versions, health and capacity.
/// Give an established conversation two short, cancellable retries on its
/// owner before ordinary failover. These still consume the normal retry budget.
pub fn transient_delay(
    retries: &mut u8,
    retry_after: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    if *retries >= 2 {
        return None;
    }
    *retries += 1;
    Some(std::time::Duration::from_secs(u64::from(*retries)).max(retry_after.unwrap_or_default()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Requirement {
    Resource,
    Model(String),
    Unknown,
}
impl Requirement {
    pub fn model(model: Option<&str>) -> Self {
        model
            .filter(|m| !m.is_empty())
            .map(|m| Self::Model(m.into()))
            .unwrap_or(Self::Unknown)
    }
    pub fn allows(&self, allowed: Option<&[String]>) -> bool {
        match (self, allowed) {
            (Self::Resource, _) | (_, None) => true,
            (Self::Model(model), Some(allowed)) => allowed.contains(model),
            (Self::Unknown, Some(_)) => false,
        }
    }
    pub fn code(&self) -> &'static str {
        if matches!(self, Self::Unknown) {
            "MODEL_UNDETERMINED"
        } else {
            "MODEL_NOT_ALLOWED"
        }
    }
    pub fn message(&self) -> &'static str {
        if matches!(self, Self::Unknown) {
            "无法识别请求模型，且没有不限模型的供应商"
        } else {
            "当前候选供应商不允许此模型，请检查模型白名单和上下文归属"
        }
    }
}
pub fn resource(method: &hyper::Method, path: &str) -> bool {
    let path = path
        .strip_prefix("/v1/")
        .map(|p| format!("/{p}"))
        .unwrap_or_else(|| path.to_owned());
    ["/models", "/files", "/uploads"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
        || (*method == hyper::Method::GET && path.starts_with("/responses/"))
}
