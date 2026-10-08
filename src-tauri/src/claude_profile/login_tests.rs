use super::*;

#[test]
fn accepts_only_official_claude_subscription_auth_methods() {
    for method in ["claude.ai", "oauth_token"] {
        let payload = format!(r#"{{"loggedIn":true,"authMethod":"{method}"}}"#);
        assert!(parse_status(payload.as_bytes()).unwrap());
    }

    for method in ["api_key", "api_key_helper", "third_party", "none"] {
        let payload = format!(r#"{{"loggedIn":true,"authMethod":"{method}"}}"#);
        assert!(!parse_status(payload.as_bytes()).unwrap());
    }
}

#[test]
fn logged_out_status_does_not_require_an_auth_method() {
    assert!(!parse_status(br#"{"loggedIn":false}"#).unwrap());
}

#[test]
fn rejects_invalid_json_and_invalid_status_shapes() {
    let cases: &[&[u8]] = &[
        b"not-json",
        b"[]",
        b"{}",
        br#"{"loggedIn":"true"}"#,
        br#"{"loggedIn":true}"#,
        br#"{"loggedIn":true,"authMethod":7}"#,
    ];

    for payload in cases {
        assert!(parse_status(payload).is_err());
    }
}

#[test]
fn status_errors_do_not_echo_sensitive_input() {
    let secret = "private-auth-output-sentinel";
    let malformed = format!(r#"{{"loggedIn":true,"authMethod":"{secret}" "#);
    let error = parse_status(malformed.as_bytes()).expect_err("malformed status must fail");

    assert!(!error.to_string().contains(secret));
}
