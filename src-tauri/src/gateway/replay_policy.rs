//! Transport failures are replayable only before any response data arrives.
//! Explicit, fully parsed first-event rejections have a separate eligibility path.
pub(super) fn transport_retry(response_data_started: bool, eligible: bool) -> bool {
    eligible && !response_data_started
}

/// A first-byte timeout says nothing about whether generation already began.
/// Once a request was given to a transport, neither an incomplete upload nor
/// absent response headers proves that replay is safe. False is reserved for
/// a request that has not been dispatched at all.
pub(super) fn first_byte_timeout_retry(request_dispatched: bool) -> bool {
    !request_dispatched
}

#[cfg(test)]
mod tests {
    #[test]
    fn first_byte_timeout_requires_proof_that_dispatch_never_started() {
        assert!(super::first_byte_timeout_retry(false));
        assert!(!super::first_byte_timeout_retry(true));
    }
}
