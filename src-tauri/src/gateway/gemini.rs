//! Gemini wire protocol. Requests and responses remain native and unmodified.
use hyper::Uri;

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
pub fn target(base: &str, incoming: &Uri) -> Result<Uri, Box<dyn std::error::Error + Send + Sync>> {
    if !accepts(incoming.path()) {
        return Err("不支持的 Gemini 接口".into());
    }
    let mut url = url::Url::parse(base)?;
    let base_path = url.path().trim_end_matches('/');
    let version = ["/v1beta", "/v1alpha", "/v1"]
        .into_iter()
        .find(|v| incoming.path().starts_with(&format!("{v}/")))
        .unwrap_or("/v1beta");
    let prefix = ["/v1beta", "/v1alpha", "/v1"]
        .into_iter()
        .find_map(|v| base_path.strip_suffix(v))
        .unwrap_or(base_path);
    url.set_path(&format!("{prefix}{version}{}", path(incoming.path())));
    // A downstream key must never survive upstream authentication.
    let query: Vec<_> = url::form_urlencoded::parse(incoming.query().unwrap_or("").as_bytes())
        .filter(|(k, _)| k != "key" && k != "api_key")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Ok(url.as_str().parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_paths_keep_prefix_and_strip_downstream_credentials() {
        let uri = "/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse&key=fixture-local"
            .parse()
            .unwrap();
        assert_eq!(target("https://example.invalid/api/v1beta", &uri).unwrap().to_string(),
            "https://example.invalid/api/v1beta/models/gemini-fixture:streamGenerateContent?alt=sse");
        assert_eq!(model(uri.path()).as_deref(), Some("gemini-fixture"));
        assert!(!accepts("/v1beta/models/x:delete"));
        assert!(!valid_model("../escape"));
    }
}
