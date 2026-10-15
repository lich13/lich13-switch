use crate::{errors::ErrorCode, events::Reason, storage::AppError};

#[test]
fn app_error_keeps_existing_wire_codes_as_strings() {
    for wire_code in [
        "MODEL_NOT_ALLOWED",
        "MODEL_UNDETERMINED",
        "NO_ELIGIBLE_PROVIDER",
        "PROVIDERS_COOLING_DOWN",
    ] {
        let value = serde_json::to_value(AppError::new(wire_code, "fixture failure")).unwrap();
        assert_eq!(value["code"], wire_code);
    }
}

#[test]
fn reason_codes_keep_their_existing_serialized_values() {
    for (reason, wire_code) in [
        (Reason::ModelUnavailable, "MODEL_UNAVAILABLE"),
        (Reason::Authentication, "AUTHENTICATION_FAILED"),
        (Reason::UpstreamService, "UPSTREAM_SERVICE_ERROR"),
        (Reason::Network, "NETWORK_ERROR"),
        (Reason::RateLimit, "RATE_LIMITED"),
        (Reason::Capacity, "CAPACITY_LIMITED"),
    ] {
        assert_eq!(
            serde_json::to_string(&reason.code()).unwrap(),
            format!("\"{wire_code}\"")
        );
    }
}

#[test]
fn unknown_historical_app_error_json_roundtrips_unchanged() {
    let original = serde_json::json!({
        "code": "LEGACY_NATIVE_FAILURE",
        "message": "fixture failure",
        "line": 12,
        "column": 4
    });

    let error: AppError = serde_json::from_value(original.clone()).unwrap();

    assert_eq!(error.code, "LEGACY_NATIVE_FAILURE");
    assert_eq!(serde_json::to_value(error).unwrap(), original);
}

#[test]
fn unknown_upstream_codes_stay_outside_the_local_enum() {
    let wire_code = "VENDOR_EXTENDED_FAILURE";
    assert!(serde_json::from_str::<ErrorCode>(&format!("\"{wire_code}\"")).is_err());

    let value = serde_json::json!({
        "code": wire_code,
        "message": "fixture upstream failure",
        "line": null,
        "column": null
    });
    let error: AppError = serde_json::from_value(value).unwrap();
    assert_eq!(error.code, wire_code);
}
