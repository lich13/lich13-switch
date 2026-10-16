//! v0.23 account exchange tests using isolated homes and synthetic JWTs.
use super::exchange::{Preview, Selection};
use super::*;
use crate::exchange_template::{payload, payload_mut};
use std::fs;

const CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const EXPIRES: i64 = 1_900_000_000;
const REFRESH: &str = "2026-10-13T00:00:00Z";
const AUTH_BYTES: &[u8] = b" {\n  \"fixture_only\": [true, null, 7]\n}\n";

fn setup() -> (tempfile::TempDir, Core) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("codex-home");
    fs::create_dir(&home).unwrap();
    let mut core = Core::new(temp.path().join("vault"), home.clone()).unwrap();
    core.store.preferences.claude_home = temp.path().join("claude-home").to_string_lossy().into();
    fs::write(home.join("auth.json"), AUTH_BYTES).unwrap();
    fs::write(home.join("config.toml"), "# fixture\nmodel = 'fixture'\n").unwrap();
    (temp, core)
}

fn jwt(value: Value) -> String {
    format!(
        "e30.{}.synthetic-signature",
        URL_SAFE_NO_PAD.encode(value.to_string())
    )
}

fn native(subject: &str, generation: i64) -> Value {
    let user = format!("fixture-user-{subject}");
    let workspace = format!("fixture-workspace-{subject}");
    native_for(subject, generation, &user, &workspace)
}

fn native_for(subject: &str, generation: i64, user: &str, workspace: &str) -> Value {
    let auth_claim = json!({
        "chatgpt_account_id": workspace,
        "chatgpt_user_id": user,
        "organization_id": format!("fixture-org-{subject}"),
        "chatgpt_plan_type": "plus"
    });
    json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": jwt(json!({"sub": subject, "email": "synthetic@example.invalid", "iat": 1780000000_i64 + generation, AUTH_CLAIM: auth_claim})),
            "access_token": jwt(json!({"sub": subject, "client_id": CLIENT, "iat": 1780000000_i64 + generation, "exp": EXPIRES + generation, AUTH_CLAIM: auth_claim})),
            "refresh_token": format!("fixture-refresh-{subject}-{generation}-punctuation_+/="),
            "account_id": workspace,
            "unknown_token": {"keep": [false, 0, "fixture"]}
        },
        "last_refresh": REFRESH,
        "future_native": {"keep": [null, true, {"fixture": 9}]}
    })
}

fn sub2_account(subject: &str, generation: i64) -> Value {
    let user = format!("fixture-user-{subject}");
    let workspace = format!("fixture-workspace-{subject}");
    sub2_account_for(subject, generation, &user, &workspace)
}

fn sub2_account_for(subject: &str, generation: i64, user: &str, workspace: &str) -> Value {
    let auth = native_for(subject, generation, user, workspace);
    json!({
        "name": format!("Fixture {subject}"), "platform": "openai", "type": "oauth",
        "credentials": {
            "id_token": auth["tokens"]["id_token"], "access_token": auth["tokens"]["access_token"],
            "refresh_token": auth["tokens"]["refresh_token"], "chatgpt_account_id": workspace,
            "chatgpt_user_id": user, "organization_id": format!("fixture-org-{subject}"),
            "client_id": CLIENT, "expires_at": EXPIRES + generation,
            "email": "synthetic@example.invalid", "plan_type": "plus",
            "unknown_credential": {"owner": subject, "keep": [null, false, 0]},
            "model_mapping": {"fixture-model": "fixture-upstream"}
        },
        "last_refresh": REFRESH, "concurrency": 4, "priority": 7, "rate_multiplier": 1.25,
        "auto_pause_on_expired": false, "proxy_key": "fixture-proxy",
        "extra": {"privacy_mode": "private", "credit_balance": 123, "owner": subject, "cache": ["fixture"]},
        "unknown_account": {"owner": subject, "keep": true}
    })
}

fn sub2(accounts: Vec<Value>, wrapped: bool) -> Value {
    let value = json!({
        "type": "sub2api-data", "version": 1, "exported_at": REFRESH,
        "accounts": accounts,
        "proxies": [{"proxy_key": "fixture-proxy", "name": "Fixture proxy", "protocol": "http", "host": "proxy.example.invalid", "port": 8080, "unknown_proxy": {"keep": true}}],
        "unknown_payload": [null, false, {"fixture": 3}]
    });
    if wrapped {
        json!({"data": value, "skip_default_group": true, "unknown_wrapper": {"keep": [1, 2]}})
    } else {
        value
    }
}

fn cpa(subject: &str, generation: i64) -> Value {
    let user = format!("fixture-user-{subject}");
    let workspace = format!("fixture-workspace-{subject}");
    cpa_for(subject, generation, &user, &workspace)
}

fn cpa_for(subject: &str, generation: i64, user: &str, workspace: &str) -> Value {
    let auth = native_for(subject, generation, user, workspace);
    json!({
        "type": "codex", "name": format!("Fixture {subject}"),
        "id_token": auth["tokens"]["id_token"], "access_token": auth["tokens"]["access_token"],
        "refresh_token": auth["tokens"]["refresh_token"], "account_id": workspace,
        "email": "synthetic@example.invalid", "plan_type": "plus",
        "expired": chrono::DateTime::from_timestamp(EXPIRES + generation, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "last_refresh": REFRESH, "proxy_url": "http://proxy.example.invalid:8080", "disabled": true,
        "unknown_root": {"owner": subject, "keep": [null, false, 0]}
    })
}

fn stabilize_update_source_metadata(format: &str, document: &mut Value) {
    match format {
        "sub2api" => {
            let account = &mut payload_mut(document)["accounts"][0];
            account["name"] = json!("Fixture stable source name");
            account["credentials"]["unknown_credential"]["owner"] =
                json!("fixture-stable-source-owner");
            account["extra"]["owner"] = json!("fixture-stable-source-owner");
            account["unknown_account"]["owner"] = json!("fixture-stable-source-owner");
        }
        "cliproxyapi" => {
            document["name"] = json!("Fixture stable source name");
            document["unknown_root"]["owner"] = json!("fixture-stable-source-owner");
        }
        _ => {}
    }
}

fn write(path: &Path, value: &Value) -> PathBuf {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    path.to_path_buf()
}

fn import(core: &mut Core, path: PathBuf) -> Vec<String> {
    let preview = core.exchange_preview(&[path]).unwrap();
    assert!(preview.rows.iter().all(|r| r.error.is_none()));
    let selection = preview
        .rows
        .iter()
        .map(|r| Selection {
            id: r.id,
            update: false,
        })
        .collect::<Vec<_>>();
    core.exchange_commit(&preview.token, &selection, &preview.revision)
        .unwrap();
    core.store.profiles.iter().map(|p| p.id.clone()).collect()
}

fn selected(ids: &[usize], update: bool) -> Vec<Selection> {
    ids.iter().map(|id| Selection { id: *id, update }).collect()
}

fn assert_auth_unchanged(core: &Core) {
    assert_eq!(fs::read(core.home().join("auth.json")).unwrap(), AUTH_BYTES);
    assert_eq!(
        fs::read(core.home().join("config.toml")).unwrap(),
        b"# fixture\nmodel = 'fixture'\n"
    );
}

fn export_cpa(core: &Core, ids: &[String], path: &Path) -> Vec<Value> {
    fs::create_dir(path).unwrap();
    core.exchange_export(ids, &core.exchange_revision(), "cliproxyapi", path)
        .unwrap();
    let mut paths = fs::read_dir(path)
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .iter()
        .map(|p| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
        .collect()
}

fn assert_preview_has_no_credentials(preview: &Preview, auth: &Value) {
    let raw = serde_json::to_string(preview).unwrap();
    for field in ["id_token", "access_token", "refresh_token"] {
        assert!(!raw.contains(auth["tokens"][field].as_str().unwrap()));
    }
    assert!(!raw.contains("unknown_credential"));
    assert!(!raw.contains("unknown_root"));
}

#[test]
fn v023_account_three_formats_preview_then_single_transaction_preserves_token_bytes() {
    let (temp, mut core) = setup();
    let values = [
        native("native", 0),
        sub2(vec![sub2_account("sub2", 0)], true),
        cpa("cpa", 0),
    ];
    let paths = values
        .iter()
        .enumerate()
        .map(|(i, value)| write(&temp.path().join(format!("input-{i}.json")), value))
        .collect::<Vec<_>>();
    let before_revision = core.exchange_revision();
    let preview = core.exchange_preview(&paths).unwrap();
    assert_eq!(preview.revision, before_revision);
    assert_eq!(
        preview
            .rows
            .iter()
            .map(|r| r.format.as_str())
            .collect::<Vec<_>>(),
        vec!["codex", "sub2api", "cliproxyapi"]
    );
    assert!(preview.rows.iter().all(|r| r.error.is_none()
        && !r.duplicate
        && r.identity_hint.as_ref().is_some_and(|h| h.len() == 12)));
    for subject in ["native", "sub2", "cpa"] {
        assert_preview_has_no_credentials(&preview, &native(subject, 0));
    }
    assert!(core.store.profiles.is_empty());
    assert!(!core.data_dir.join("accounts.json").exists());
    assert_auth_unchanged(&core);
    assert_eq!(
        core.exchange_commit(
            &preview.token,
            &selected(&[0, 1, 2], false),
            &preview.revision
        )
        .unwrap(),
        3
    );
    assert_eq!(core.store.profiles.len(), 3);
    for (profile, subject) in core.store.profiles.iter().zip(["native", "sub2", "cpa"]) {
        let stored: Value = serde_json::from_str(&profile.auth).unwrap();
        let original = native(subject, 0);
        for field in ["id_token", "access_token", "refresh_token"] {
            assert_eq!(stored["tokens"][field], original["tokens"][field]);
        }
        assert_eq!(
            stored["tokens"]["account_id"],
            original["tokens"]["account_id"]
        );
    }
    assert_eq!(
        serde_json::from_str::<Value>(&core.store.profiles[0].auth).unwrap(),
        values[0]
    );
    let reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
    assert_eq!(
        serde_json::to_value(&reopened.store.profiles).unwrap(),
        serde_json::to_value(&core.store.profiles).unwrap()
    );
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_same_account_same_format_round_trip_preserves_all_metadata_and_wrapper() {
    for wrapped in [false, true] {
        let (temp, mut core) = setup();
        let original = sub2(vec![sub2_account("roundtrip", 0)], wrapped);
        let ids = import(&mut core, write(&temp.path().join("sub2.json"), &original));
        assert!(core.exchange_template_ready(&ids));
        let output = temp.path().join("export.json");
        core.exchange_export(&ids, &core.exchange_revision(), "sub2api", &output)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
            original
        );
        assert_auth_unchanged(&core);
    }
    let (temp, mut core) = setup();
    let original = cpa("roundtrip", 0);
    let ids = import(&mut core, write(&temp.path().join("cpa.json"), &original));
    assert_eq!(
        export_cpa(&core, &ids, &temp.path().join("cpa-output")),
        vec![original]
    );
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_rejects_identity_client_and_incomplete_token_mismatches_in_preview() {
    let (temp, mut core) = setup();
    let mut cases = Vec::new();
    let mut wrong_subject = native("subject", 0);
    wrong_subject["tokens"]["access_token"] = native("other", 0)["tokens"]["access_token"].clone();
    cases.push((wrong_subject, "身份不一致"));
    let mut wrong_workspace = native("workspace", 0);
    wrong_workspace["tokens"]["account_id"] = json!("fixture-other-workspace");
    cases.push((wrong_workspace, "工作区不一致"));
    let mut wrong_client = native("client", 0);
    let mut access = claims(wrong_client["tokens"]["access_token"].as_str().unwrap()).unwrap();
    access["client_id"] = json!("fixture-other-client");
    wrong_client["tokens"]["access_token"] = json!(jwt(access));
    cases.push((wrong_client, "client_id"));
    let mut malformed = native("malformed", 0);
    malformed["tokens"]["id_token"] = json!("fixture-malformed-token");
    cases.push((malformed, "id_token"));
    let mut missing = native("missing", 0);
    missing["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("refresh_token");
    cases.push((missing, "Token"));
    let mut siwc = cpa("siwc", 0);
    siwc["use_siwc"] = json!(true);
    cases.push((siwc, "兼容"));
    for (index, (document, message)) in cases.into_iter().enumerate() {
        let preview = core
            .exchange_preview(&[write(
                &temp.path().join(format!("reject-{index}.json")),
                &document,
            )])
            .unwrap();
        assert!(
            preview.rows[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains(message)),
            "expected diagnostic category {message}"
        );
        assert!(core
            .exchange_commit(&preview.token, &selected(&[0], false), &preview.revision)
            .is_err());
        assert!(core.store.profiles.is_empty());
    }
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_batch_duplicates_and_invalid_selection_do_not_partially_commit() {
    let (temp, mut core) = setup();
    let document = sub2(
        vec![
            sub2_account("first", 0),
            sub2_account("second", 0),
            sub2_account("first", 0),
        ],
        false,
    );
    let preview = core
        .exchange_preview(&[write(&temp.path().join("batch.json"), &document)])
        .unwrap();
    assert!(preview.rows[2]
        .error
        .as_deref()
        .is_some_and(|e| e.contains("重复账号")));
    for ids in [&[0, 2][..], &[0, 0][..], &[0, 99][..], &[][..]] {
        assert!(core
            .exchange_commit(&preview.token, &selected(ids, false), &preview.revision)
            .is_err());
        assert!(core.store.profiles.is_empty());
        assert!(!core.data_dir.join("accounts.json").exists());
    }
    assert_eq!(
        core.exchange_commit(&preview.token, &selected(&[1], false), &preview.revision)
            .unwrap(),
        1
    );
    assert_eq!(core.store.profiles[0].name, "Fixture second");
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_duplicates_require_explicit_update_and_keep_existing_id_and_name() {
    let (temp, mut core) = setup();
    let input = temp.path().join("account.json");
    let ids = import(&mut core, write(&input, &cpa("duplicate", 0)));
    core.rename(&ids[0], "Locally chosen name").unwrap();
    let before = fs::read(core.data_dir.join("accounts.json")).unwrap();
    let preview = core
        .exchange_preview(&[write(&input, &cpa("duplicate", 1))])
        .unwrap();
    assert!(preview.rows[0].duplicate);
    assert!(preview.rows[0].error.is_none());
    assert!(core
        .exchange_commit(&preview.token, &selected(&[0], false), &preview.revision)
        .unwrap_err()
        .message
        .contains("默认跳过"));
    assert_eq!(
        fs::read(core.data_dir.join("accounts.json")).unwrap(),
        before
    );
    assert_eq!(
        core.exchange_commit(&preview.token, &selected(&[0], true), &preview.revision)
            .unwrap(),
        1
    );
    assert_eq!(core.store.profiles.len(), 1);
    assert_eq!(core.store.profiles[0].id, ids[0]);
    assert_eq!(core.store.profiles[0].name, "Locally chosen name");
    let auth: Value = serde_json::from_str(&core.store.profiles[0].auth).unwrap();
    assert_eq!(
        auth["tokens"]["refresh_token"],
        native("duplicate", 1)["tokens"]["refresh_token"]
    );
    let old = core
        .exchange_preview(&[write(&input, &cpa("duplicate", 0))])
        .unwrap();
    assert!(old.rows[0]
        .error
        .as_deref()
        .is_some_and(|e| e.contains("早于账号库")));
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_subject_rotation_in_all_three_formats_requires_explicit_update() {
    let user = "fixture-rotating-user";
    let workspace = "fixture-rotating-workspace";
    for format in ["codex", "sub2api", "cliproxyapi"] {
        let (temp, mut core) = setup();
        let (mut original, mut incoming) = match format {
            "codex" => (
                native_for("fixture-subject-old", 0, user, workspace),
                native_for("fixture-subject-new", 1, user, workspace),
            ),
            "sub2api" => (
                sub2(
                    vec![sub2_account_for("fixture-subject-old", 0, user, workspace)],
                    true,
                ),
                sub2(
                    vec![sub2_account_for("fixture-subject-new", 1, user, workspace)],
                    true,
                ),
            ),
            "cliproxyapi" => (
                cpa_for("fixture-subject-old", 0, user, workspace),
                cpa_for("fixture-subject-new", 1, user, workspace),
            ),
            _ => unreachable!(),
        };
        stabilize_update_source_metadata(format, &mut original);
        stabilize_update_source_metadata(format, &mut incoming);
        let ids = import(
            &mut core,
            write(&temp.path().join("original.json"), &original),
        );
        let id = ids[0].clone();
        core.rename(&id, "Locally chosen name").unwrap();
        let original_identity = core.store.profiles[0].identity.clone();
        let preview = core
            .exchange_preview(&[write(&temp.path().join("rotated.json"), &incoming)])
            .unwrap();

        assert_eq!(preview.rows.len(), 1, "{format}");
        assert_eq!(preview.rows[0].format, format);
        assert!(preview.rows[0].duplicate, "{format}");
        assert!(preview.rows[0].error.is_none(), "{format}");
        let before = fs::read(core.data_dir.join("accounts.json")).unwrap();
        let skipped = core
            .exchange_commit(&preview.token, &selected(&[0], false), &preview.revision)
            .unwrap_err();
        assert!(
            skipped.message.contains("默认跳过"),
            "{format}: {}",
            skipped.message
        );
        assert_eq!(
            fs::read(core.data_dir.join("accounts.json")).unwrap(),
            before
        );

        assert_eq!(
            core.exchange_commit(&preview.token, &selected(&[0], true), &preview.revision)
                .unwrap(),
            1,
            "{format}"
        );
        assert_eq!(core.store.profiles.len(), 1, "{format}");
        assert_eq!(core.store.profiles[0].id, id, "{format}");
        assert_eq!(
            core.store.profiles[0].name, "Locally chosen name",
            "{format}"
        );
        assert_eq!(
            core.store.profiles[0].identity, original_identity,
            "{format}"
        );
        let saved: Value = serde_json::from_str(&core.store.profiles[0].auth).unwrap();
        assert_eq!(
            claims(saved["tokens"]["id_token"].as_str().unwrap()).unwrap()["sub"],
            "fixture-subject-new",
            "{format}"
        );

        match format {
            "sub2api" => {
                let output = temp.path().join("updated-sub2.json");
                core.exchange_export(&[id], &core.exchange_revision(), "sub2api", &output)
                    .unwrap();
                let mut expected = incoming.clone();
                payload_mut(&mut expected)["accounts"][0]["name"] = json!("Locally chosen name");
                assert_eq!(
                    serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
                    expected
                );
            }
            "cliproxyapi" => assert_eq!(
                export_cpa(&core, &[id], &temp.path().join("updated-cpa")),
                vec![incoming.clone()]
            ),
            "codex" => {}
            _ => unreachable!(),
        }
        assert_auth_unchanged(&core);
    }
}

#[test]
fn v023_account_can_skip_duplicate_and_commit_only_new_account() {
    let (temp, mut core) = setup();
    import(
        &mut core,
        write(&temp.path().join("existing.json"), &native("existing", 0)),
    );
    let original = core.store.profiles[0].auth.clone();
    let preview = core
        .exchange_preview(&[write(
            &temp.path().join("batch.json"),
            &sub2(
                vec![sub2_account("existing", 1), sub2_account("new", 0)],
                false,
            ),
        )])
        .unwrap();
    assert!(preview.rows[0].duplicate);
    core.exchange_commit(&preview.token, &selected(&[1], false), &preview.revision)
        .unwrap();
    assert_eq!(core.store.profiles.len(), 2);
    assert_eq!(core.store.profiles[0].auth, original);
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_import_keeps_valid_active_auth_bytes_and_does_not_revert_newer_vault() {
    let (temp, mut core) = setup();
    let active = format!(
        " \n{}\n",
        serde_json::to_string_pretty(&native("active", 0)).unwrap()
    );
    fs::write(core.home().join("auth.json"), active.as_bytes()).unwrap();
    let input = write(&temp.path().join("newer.json"), &cpa("active", 1));
    let preview = core.exchange_preview(&[input]).unwrap();
    assert_eq!(
        fs::read(core.home().join("auth.json")).unwrap(),
        active.as_bytes()
    );
    core.exchange_commit(&preview.token, &selected(&[0], false), &preview.revision)
        .unwrap();
    let imported = core.store.profiles[0].auth.clone();
    assert_eq!(
        fs::read(core.home().join("auth.json")).unwrap(),
        active.as_bytes()
    );
    let mut reopened = Core::new(core.data_dir.clone(), core.home()).unwrap();
    assert_eq!(reopened.state().unwrap().accounts.len(), 1);
    assert_eq!(reopened.store.profiles[0].auth, imported);
    assert_eq!(
        fs::read(core.home().join("auth.json")).unwrap(),
        active.as_bytes()
    );
}

#[test]
fn v023_account_conflict_cancel_and_replaced_preview_never_change_vault_or_auth() {
    let (temp, mut core) = setup();
    let ids = import(
        &mut core,
        write(&temp.path().join("existing.json"), &native("existing", 0)),
    );
    let input = write(&temp.path().join("new.json"), &cpa("new", 0));
    let preview = core.exchange_preview(std::slice::from_ref(&input)).unwrap();
    core.rename(&ids[0], "Concurrent rename").unwrap();
    let after_rename = fs::read(core.data_dir.join("accounts.json")).unwrap();
    assert_eq!(
        core.exchange_commit(&preview.token, &selected(&[0], false), &preview.revision)
            .unwrap_err()
            .code,
        "CONFLICT"
    );
    assert_eq!(
        fs::read(core.data_dir.join("accounts.json")).unwrap(),
        after_rename
    );
    let first = core.exchange_preview(std::slice::from_ref(&input)).unwrap();
    let second = core.exchange_preview(std::slice::from_ref(&input)).unwrap();
    assert!(core
        .exchange_commit(&first.token, &selected(&[0], false), &first.revision)
        .is_err());
    core.exchange_cancel(&first.token);
    core.exchange_cancel(&second.token);
    assert!(core
        .exchange_commit(&second.token, &selected(&[0], false), &second.revision)
        .is_err());
    assert_eq!(core.store.profiles.len(), 1);
    assert_eq!(
        fs::read(core.data_dir.join("accounts.json")).unwrap(),
        after_rename
    );
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_persist_failure_rolls_back_entire_import_and_allows_retry() {
    let (temp, mut core) = setup();
    let preview = core
        .exchange_preview(&[write(
            &temp.path().join("batch.json"),
            &sub2(
                vec![sub2_account("first", 0), sub2_account("second", 0)],
                false,
            ),
        )])
        .unwrap();
    let vault = core.data_dir.join("accounts.json");
    fs::create_dir(&vault).unwrap();
    assert!(core
        .exchange_commit(&preview.token, &selected(&[0, 1], false), &preview.revision)
        .is_err());
    assert!(core.store.profiles.is_empty());
    assert_eq!(core.exchange_revision(), preview.revision);
    assert!(vault.is_dir());
    assert_eq!(fs::read_dir(&core.data_dir).unwrap().count(), 1);
    fs::remove_dir(&vault).unwrap();
    assert_eq!(
        core.exchange_commit(&preview.token, &selected(&[0, 1], false), &preview.revision)
            .unwrap(),
        2
    );
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_commit_detects_external_vault_write_without_overwriting_it() {
    let (temp, mut core) = setup();
    import(
        &mut core,
        write(&temp.path().join("existing.json"), &native("existing", 0)),
    );
    let preview = core
        .exchange_preview(&[write(&temp.path().join("new.json"), &cpa("new", 0))])
        .unwrap();
    let before_memory = serde_json::to_value(&core.store).unwrap();
    let mut external = core.store.clone();
    external.profiles[0].name = "External rename".into();
    let bytes = serde_json::to_vec_pretty(&external).unwrap();
    let path = core.data_dir.join("accounts.json");
    fs::write(&path, &bytes).unwrap();
    assert_eq!(
        core.exchange_commit(&preview.token, &selected(&[0], false), &preview.revision)
            .unwrap_err()
            .code,
        "CONFLICT"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(serde_json::to_value(&core.store).unwrap(), before_memory);
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_expiry_seconds_and_rfc3339_export_to_expected_format() {
    for (index, expiry) in [
        json!(EXPIRES),
        json!(EXPIRES.to_string()),
        json!(chrono::DateTime::from_timestamp(EXPIRES, 0)
            .unwrap()
            .to_rfc3339()),
    ]
    .into_iter()
    .enumerate()
    {
        let (temp, mut core) = setup();
        let mut document = cpa(&format!("expiry-{index}"), 0);
        let mut access = claims(document["access_token"].as_str().unwrap()).unwrap();
        access.as_object_mut().unwrap().remove("exp");
        document["access_token"] = json!(jwt(access));
        document["expired"] = expiry;
        let ids = import(&mut core, write(&temp.path().join("input.json"), &document));
        let template = write(
            &temp.path().join("template.json"),
            &sub2(vec![sub2_account("prototype", 0)], false),
        );
        assert!(core.exchange_prepare_template(&template).unwrap().ready);
        let cpa_export = export_cpa(&core, &ids, &temp.path().join("cpa"));
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(cpa_export[0]["expired"].as_str().unwrap())
                .unwrap()
                .timestamp(),
            EXPIRES
        );
        assert_eq!(cpa_export[0]["last_refresh"], REFRESH);
        let path = temp.path().join("sub2.json");
        core.exchange_export(&ids, &core.exchange_revision(), "sub2api", &path)
            .unwrap();
        let exported: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            payload(&exported)["accounts"][0]["credentials"]["expires_at"],
            EXPIRES
        );
        assert_auth_unchanged(&core);
    }
}

#[test]
fn v023_account_native_numeric_refresh_exports_as_rfc3339_without_changing_source() {
    let seconds = chrono::DateTime::parse_from_rfc3339(REFRESH)
        .unwrap()
        .timestamp();
    for last_refresh in [
        json!(seconds),
        json!(seconds * 1000 + 123),
        json!("2026-10-13T08:00:00+08:00"),
    ] {
        let (temp, mut core) = setup();
        let mut document = native("refresh", 0);
        document["last_refresh"] = last_refresh.clone();
        let ids = import(
            &mut core,
            write(&temp.path().join("native.json"), &document),
        );
        let before = core.store.profiles[0].auth.clone();
        let exported = export_cpa(&core, &ids, &temp.path().join("cpa"));
        let converted =
            chrono::DateTime::parse_from_rfc3339(exported[0]["last_refresh"].as_str().unwrap())
                .unwrap();
        let expected_millis = if last_refresh.as_i64() == Some(seconds * 1000 + 123) {
            seconds * 1000 + 123
        } else {
            seconds * 1000
        };
        assert_eq!(converted.timestamp_millis(), expected_millis);
        assert_eq!(core.store.profiles[0].auth, before);
        assert_auth_unchanged(&core);
    }
}

#[test]
fn v023_account_metadata_stays_with_its_owner_and_new_accounts_use_sanitized_template() {
    let (temp, mut core) = setup();
    let original = sub2(vec![sub2_account("owner", 0)], true);
    let source = write(&temp.path().join("owner.json"), &original);
    let owner_ids = import(&mut core, source.clone());
    assert!(core.exchange_prepare_template(&source).unwrap().ready);
    let all_ids = import(
        &mut core,
        write(&temp.path().join("new.json"), &native("new", 0)),
    );
    let output = temp.path().join("export.json");
    core.exchange_export(&all_ids, &core.exchange_revision(), "sub2api", &output)
        .unwrap();
    let exported: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert!(exported.get("unknown_payload").is_none());
    assert!(exported.get("unknown_wrapper").is_none());
    assert!(exported.get("data").is_none());
    assert_eq!(
        payload(&exported)["accounts"][0],
        payload(&original)["accounts"][0]
    );
    let added = &payload(&exported)["accounts"][1];
    assert_eq!(
        added["credentials"]["chatgpt_account_id"],
        "fixture-workspace-new"
    );
    assert!(added["credentials"].get("unknown_credential").is_none());
    assert!(added.get("unknown_account").is_none());
    assert!(added["extra"].get("owner").is_none());
    assert!(added["extra"].get("cache").is_none());
    assert!(added["extra"].get("credit_balance").is_none());
    assert_eq!(added["extra"]["privacy_mode"], "private");
    assert_eq!(
        added["credentials"]["model_mapping"],
        payload(&original)["accounts"][0]["credentials"]["model_mapping"]
    );
    let cross = export_cpa(&core, &owner_ids, &temp.path().join("cpa"));
    assert!(cross[0].get("extra").is_none());
    assert!(cross[0].get("unknown_account").is_none());
    assert!(cross[0].get("unknown_root").is_none());
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_mixed_sub2_sources_keep_private_roots_only_for_their_own_round_trip() {
    let (temp, mut core) = setup();
    let mut first = sub2(vec![sub2_account("first-root-owner", 0)], true);
    first["unknown_wrapper"] = json!({"owner": "fixture-private-first-wrapper"});
    payload_mut(&mut first)["unknown_payload"] = json!({"owner": "fixture-private-first-payload"});
    let first_ids = import(
        &mut core,
        write(&temp.path().join("first-root.json"), &first),
    );
    let first_id = first_ids[0].clone();

    let mut second = sub2(vec![sub2_account("second-root-owner", 0)], true);
    second["unknown_wrapper"] = json!({"owner": "fixture-private-second-wrapper"});
    payload_mut(&mut second)["unknown_payload"] =
        json!({"owner": "fixture-private-second-payload"});
    let all_ids = import(
        &mut core,
        write(&temp.path().join("second-root.json"), &second),
    );
    let second_id = all_ids.iter().find(|id| **id != first_id).unwrap().clone();

    // No export prototype is prepared or selected for these two native sources.
    let mixed_path = temp.path().join("mixed-roots.json");
    core.exchange_export(&all_ids, &core.exchange_revision(), "sub2api", &mixed_path)
        .unwrap();
    let mixed: Value = serde_json::from_slice(&fs::read(mixed_path).unwrap()).unwrap();
    assert!(mixed.get("data").is_none());
    assert!(mixed.get("unknown_wrapper").is_none());
    assert!(payload(&mixed).get("unknown_payload").is_none());
    assert_eq!(
        payload(&mixed)["accounts"],
        json!([
            payload(&first)["accounts"][0],
            payload(&second)["accounts"][0],
        ])
    );
    let mixed_text = mixed.to_string();
    for marker in [
        "fixture-private-first-wrapper",
        "fixture-private-first-payload",
        "fixture-private-second-wrapper",
        "fixture-private-second-payload",
    ] {
        assert!(!mixed_text.contains(marker));
    }

    // A mixed export must not rewrite either account's preserved source envelope.
    for (id, original, name) in [
        (first_id, first, "first-round-trip.json"),
        (second_id, second, "second-round-trip.json"),
    ] {
        let path = temp.path().join(name);
        core.exchange_export(&[id], &core.exchange_revision(), "sub2api", &path)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
            original
        );
    }
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_cpa_and_sub2_metadata_for_one_identity_remain_independent() {
    let (temp, mut core) = setup();
    let user = "fixture-shared-user";
    let workspace = "fixture-shared-workspace";
    let original_cpa = cpa_for("shared-old-subject", 0, user, workspace);
    let ids = import(
        &mut core,
        write(&temp.path().join("cpa.json"), &original_cpa),
    );
    let original_sub2 = sub2(
        vec![sub2_account_for("shared-new-subject", 1, user, workspace)],
        false,
    );
    let preview = core
        .exchange_preview(&[write(&temp.path().join("sub2.json"), &original_sub2)])
        .unwrap();
    assert!(preview.rows[0].duplicate);
    assert!(preview.rows[0].error.is_none());
    core.exchange_commit(&preview.token, &selected(&[0], true), &preview.revision)
        .unwrap();
    assert_eq!(core.store.profiles.len(), 1);
    assert_eq!(core.store.profiles[0].id, ids[0]);
    let exported_cpa = export_cpa(&core, &ids, &temp.path().join("cpa-output"));
    assert_eq!(exported_cpa.len(), 1);
    for field in ["name", "proxy_url", "disabled", "unknown_root"] {
        assert_eq!(exported_cpa[0][field], original_cpa[field], "{field}");
    }
    assert_eq!(
        exported_cpa[0]["id_token"],
        original_sub2["accounts"][0]["credentials"]["id_token"]
    );
    assert_eq!(
        exported_cpa[0]["access_token"],
        original_sub2["accounts"][0]["credentials"]["access_token"]
    );
    assert_eq!(
        exported_cpa[0]["refresh_token"],
        original_sub2["accounts"][0]["credentials"]["refresh_token"]
    );
    assert_eq!(
        claims(exported_cpa[0]["id_token"].as_str().unwrap()).unwrap()["sub"],
        "shared-new-subject"
    );
    let output = temp.path().join("sub2-output.json");
    core.exchange_export(&ids, &core.exchange_revision(), "sub2api", &output)
        .unwrap();
    let mut expected_sub2 = original_sub2.clone();
    payload_mut(&mut expected_sub2)["accounts"][0]["name"] = original_cpa["name"].clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
        expected_sub2
    );
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_export_checks_revision_and_stages_batch_before_publishing() {
    let (temp, mut core) = setup();
    let ids = import(
        &mut core,
        write(&temp.path().join("valid.json"), &native("valid", 0)),
    );
    let preview = core.exchange_export_preview();
    assert!(preview.rows[0].error.is_none());
    core.rename(&ids[0], "Changed name").unwrap();
    let destination = temp.path().join("export");
    fs::create_dir(&destination).unwrap();
    assert_eq!(
        core.exchange_export(&ids, &preview.revision, "cliproxyapi", &destination)
            .unwrap_err()
            .code,
        "CONFLICT"
    );
    let invalid = core.add_api_key("Fixture API", "fixture-api-key").unwrap();
    let selection = vec![ids[0].clone(), invalid];
    let preview = core.exchange_export_preview();
    assert!(preview.rows[1].error.is_some());
    assert!(core
        .exchange_export(&selection, &preview.revision, "cliproxyapi", &destination)
        .is_err());
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_proxy_conflict_rejects_export_without_creating_output() {
    let (temp, mut core) = setup();
    import(
        &mut core,
        write(
            &temp.path().join("one.json"),
            &sub2(vec![sub2_account("one", 0)], false),
        ),
    );
    let mut second = sub2(vec![sub2_account("two", 0)], false);
    payload_mut(&mut second)["proxies"][0]["host"] = json!("other-proxy.example.invalid");
    let ids = import(&mut core, write(&temp.path().join("two.json"), &second));
    let output = temp.path().join("output.json");
    let error = core
        .exchange_export(&ids, &core.exchange_revision(), "sub2api", &output)
        .unwrap_err();
    assert!(error.message.contains("代理定义冲突"));
    assert!(!output.exists());
    assert_auth_unchanged(&core);
}

#[test]
fn v023_account_emits_rust_outputs_for_real_upstream_parsers() {
    let (temp, mut core) = setup();
    let mut fixtures = Vec::new();
    for wrapped in [false, true] {
        let subject = if wrapped {
            "interop-wrapped"
        } else {
            "interop-bare"
        };
        let source = write(
            &temp.path().join(format!("input-{subject}.json")),
            &sub2(vec![sub2_account(subject, 0)], wrapped),
        );
        import(&mut core, source);
        let id = core.store.profiles.last().unwrap().id.clone();
        let filename = format!(
            "rust-sub2-{}.json",
            if wrapped { "wrapper" } else { "payload" }
        );
        let path = temp.path().join(&filename);
        core.exchange_export(&[id], &core.exchange_revision(), "sub2api", &path)
            .unwrap();
        fixtures.push((filename, fs::read(&path).unwrap()));
    }
    let native_path = write(
        &temp.path().join("native.json"),
        &native("interop-native", 0),
    );
    import(&mut core, native_path);
    let native_id = core.store.profiles.last().unwrap().id.clone();
    let exported = export_cpa(&core, &[native_id], &temp.path().join("cpa"));
    let canonical = &exported[0];
    for field in ["id_token", "access_token", "refresh_token"] {
        assert_eq!(
            canonical[field],
            native("interop-native", 0)["tokens"][field]
        );
    }
    fixtures.push((
        "rust-cpa.json".into(),
        serde_json::to_vec_pretty(canonical).unwrap(),
    ));
    assert_auth_unchanged(&core);
    if let Some(directory) = std::env::var_os("LICH13_INTEROP_FIXTURES") {
        let directory = PathBuf::from(directory);
        assert!(directory.is_absolute());
        fs::create_dir_all(&directory).unwrap();
        for (filename, bytes) in fixtures {
            fs::write(directory.join(filename), bytes).unwrap();
        }
        write(
            &directory.join("sub2api-cases.json"),
            &json!([
                {"name": "rust-oauth-payload", "file": "rust-sub2-payload.json", "mode": "payload", "stage": "accepted", "error_contains": "", "count": 1},
                {"name": "rust-oauth-wrapper", "file": "rust-sub2-wrapper.json", "mode": "request", "stage": "accepted", "error_contains": "", "count": 1}
            ]),
        );
        write(
            &directory.join("cpa-cases.json"),
            &json!([
                {"name": "rust-oauth", "file": "rust-cpa.json", "typed_ok": true, "accepted": true, "provider": "codex", "refresh_unix": chrono::DateTime::parse_from_rfc3339(REFRESH).unwrap().timestamp(), "has_id_token": true}
            ]),
        );
    }
}
