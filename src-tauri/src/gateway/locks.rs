//! Recovery is limited to replace-only handles and disposable caches. Routing
//! authority must return an error on poison, never accept a partially edited state.
use std::sync::{Mutex, MutexGuard};

/// The caller must only publish fully constructed values under this lock.
pub(super) fn published<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A cache can be rebuilt without changing persisted ownership or credentials.
pub(super) fn cache<T: Default>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(value) => value,
        Err(error) => {
            let mut value = error.into_inner();
            *value = T::default();
            mutex.clear_poison();
            value
        }
    }
}

#[cfg(test)]
#[path = "locks_poison_tests.rs"]
mod poison_tests;
