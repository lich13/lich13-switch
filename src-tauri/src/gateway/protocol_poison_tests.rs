use super::Protocol;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

#[test]
fn delivery_cancel_and_protocol_drop_recover_poisoned_activity_handle() {
    let protocol = Protocol::new(true);
    let delivery = protocol.delivery();
    let activity = Arc::new(super::super::wire_progress::Activity::default());
    let expected = Arc::clone(&activity);
    let injected = catch_unwind(AssertUnwindSafe(|| {
        let mut current = delivery.activity.lock().unwrap();
        *current = Some(activity);
        panic!("inject activity poison after publishing a complete handle");
    }));
    assert!(injected.is_err());
    assert!(delivery.activity.is_poisoned());

    let published = super::super::locks::published(&delivery.activity)
        .as_ref()
        .cloned()
        .unwrap();
    assert!(Arc::ptr_eq(&published, &expected));

    let cancel_result = catch_unwind(AssertUnwindSafe(|| delivery.dropped()));
    assert!(
        cancel_result.is_ok(),
        "delivery cleanup must not panic on poison"
    );
    assert!(delivery.is_settled());

    let drop_result = catch_unwind(AssertUnwindSafe(|| drop(protocol)));
    assert!(
        drop_result.is_ok(),
        "Protocol::drop must not panic on poison"
    );
    assert!(delivery.activity.is_poisoned());
}
