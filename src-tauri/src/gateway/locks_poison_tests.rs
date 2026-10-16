use super::{cache, published};
use std::{
    collections::HashMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
};

#[test]
fn poisoned_disposable_cache_is_reset_and_usable_again() {
    let values = Mutex::new(HashMap::from([("fixture-stale".to_owned(), 1usize)]));
    let injected = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = values.lock().unwrap();
        guard.insert("fixture-partial".into(), 2);
        panic!("inject cache poison");
    }));
    assert!(injected.is_err());
    assert!(values.is_poisoned());

    {
        let mut recovered = cache(&values);
        assert!(
            recovered.is_empty(),
            "poisoned cache must be rebuilt from Default"
        );
        recovered.insert("fixture-rebuilt".into(), 3);
    }

    assert!(!values.is_poisoned());
    assert_eq!(values.lock().unwrap().get("fixture-rebuilt"), Some(&3));
}

#[test]
fn poisoned_published_handle_keeps_the_complete_replacement() {
    let values = Mutex::new(Arc::new(vec!["fixture-before".to_owned()]));
    let replacement = Arc::new(vec!["fixture-complete".to_owned()]);
    let expected = Arc::clone(&replacement);
    let injected = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = values.lock().unwrap();
        *guard = Arc::clone(&replacement);
        panic!("inject published-handle poison after complete replacement");
    }));
    assert!(injected.is_err());
    assert!(values.is_poisoned());

    let recovered = published(&values).clone();
    assert!(Arc::ptr_eq(&recovered, &expected));
    assert_eq!(recovered.as_slice(), ["fixture-complete"]);
}
