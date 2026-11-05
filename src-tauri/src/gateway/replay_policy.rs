//! Transport failures are replayable only before any response data arrives.
//! Explicit, fully parsed first-event rejections have a separate eligibility path.
pub(super) fn transport_retry(response_data_started: bool, eligible: bool) -> bool {
    eligible && !response_data_started
}
