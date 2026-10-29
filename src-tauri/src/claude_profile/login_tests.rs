use super::*;

#[test]
fn windows_npm_supports_native_and_legacy_launchers_with_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fixture npm");
    let package = root.join("node_modules/@anthropic-ai/claude-code");
    std::fs::create_dir_all(package.join("bin")).unwrap();
    let shim = root.join("claude.cmd");
    std::fs::write(&shim, "fixture").unwrap();
    assert!(windows_command(&shim).is_err());
    let script = package.join("cli.js");
    std::fs::write(&script, "fixture").unwrap();
    let legacy = windows_command(&shim).unwrap();
    assert_eq!(legacy.as_std().get_program(), "node.exe");
    assert_eq!(
        legacy.as_std().get_args().collect::<Vec<_>>(),
        [script.as_os_str()]
    );
    let native = package.join("bin/claude.exe");
    std::fs::write(&native, "fixture").unwrap();
    let current = windows_command(&shim).unwrap();
    assert_eq!(current.as_std().get_program(), native);
    assert_eq!(current.as_std().get_args().count(), 0);
    assert_eq!(
        windows_command(&native).unwrap().as_std().get_program(),
        native
    );
}

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
