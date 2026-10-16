use super::*;
use std::{
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
};

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn poisoned_owner_is_unavailable_and_lease_drop_does_not_persist_or_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conversation-owners.json");
    let registry = Registry::new(path.clone());
    let session = storage::digest(b"fixture-session");
    let boundary = storage::digest(b"fixture-compaction-boundary");

    let owner = registry
        .prepare(session.clone(), None, false, &ids(&["fixture-owner"]))
        .unwrap();
    owner.complete("fixture-owner", Some(&boundary));
    registry.flush();
    drop(owner);
    assert_eq!(registry.owner(&session).as_deref(), Some("fixture-owner"));

    let lease = registry
        .prepare(
            session.clone(),
            Some(&boundary),
            true,
            &ids(&["fixture-candidate", "fixture-owner"]),
        )
        .unwrap();
    assert!(lease.handoff);
    registry.flush();
    let durable_before_poison = fs::read(&path).unwrap();

    let injected = catch_unwind(AssertUnwindSafe(|| {
        let mut state = registry.state.lock().unwrap();
        state.entries.get_mut(&session).unwrap().provider = "fixture-partial-owner".into();
        panic!("inject partially edited ownership ledger poison");
    }));
    assert!(injected.is_err());
    assert!(registry.state.is_poisoned());

    assert_eq!(
        registry.owner(&session),
        None,
        "poison must not guess an owner"
    );
    assert_eq!(registry.error().as_deref(), Some(POISONED));
    assert!(registry
        .prepare(
            session.clone(),
            Some(&boundary),
            true,
            &ids(&[
                "fixture-candidate",
                "fixture-owner",
                "fixture-partial-owner"
            ]),
        )
        .is_none());
    assert_eq!(lease.admission_policy(), (Intake::Unavailable, None));
    assert!(!lease.owns("fixture-owner"));
    assert!(!lease.owns("fixture-partial-owner"));

    let drop_result = catch_unwind(AssertUnwindSafe(|| drop(lease)));
    assert!(
        drop_result.is_ok(),
        "poisoned lease destruction must not panic"
    );
    assert_eq!(fs::read(&path).unwrap(), durable_before_poison);
}
