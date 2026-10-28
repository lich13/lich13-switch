//! Gemini wire protocol. Requests and responses remain native and unmodified.

pub fn path(path: &str) -> &str {
    for version in ["/v1beta", "/v1alpha", "/v1"] {
        if let Some(rest) = path.strip_prefix(version).filter(|s| s.starts_with('/')) {
            return rest;
        }
    }
    path
}
pub fn model(pathname: &str) -> Option<String> {
    let (name, operation) = path(pathname).strip_prefix("/models/")?.rsplit_once(':')?;
    if !matches!(
        operation,
        "generateContent" | "streamGenerateContent" | "countTokens"
    ) {
        return None;
    }
    valid_model(name).then(|| name.to_owned())
}
pub fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 256
        && model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
pub fn accepts(pathname: &str) -> bool {
    path(pathname) == "/models"
        || model(pathname).is_some()
        || path(pathname)
            .strip_prefix("/models/")
            .is_some_and(valid_model)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_protocol_identification_remains_readable() {
        assert_eq!(
            model("/v1beta/models/gemini-fixture:streamGenerateContent").as_deref(),
            Some("gemini-fixture")
        );
        assert!(!accepts("/v1beta/models/x:delete"));
        assert!(!valid_model("../escape"));
    }
}
