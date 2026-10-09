use super::{
    model::{Meter, Operation, Record, Settings, Tokens, Totals},
    pricing::Config,
    Service,
};
use serde_json::{json, Value};

#[test]
fn meter_reads_json_response_models_and_does_not_clear_them_on_sparse_events() {
    let mut meter = Meter::default();
    let response: Value = json!({
        "id": "fixture-json-response",
        "model": "fixture-json-model",
        "usage": {"input_tokens": 4, "output_tokens": 2}
    });
    meter.observe(&response, 10);
    assert_eq!(meter.model.as_deref(), Some("fixture-json-model"));

    let event: Value = json!({
        "type": "response.output_text.delta",
        "delta": "fixture"
    });
    meter.observe(&event, 20);
    assert_eq!(meter.model.as_deref(), Some("fixture-json-model"));
    assert_eq!(meter.tokens.input, Some(4));

    let final_event: Value = json!({
        "type": "response.completed",
        "response": {"model": "fixture-final-model"}
    });
    meter.observe(&final_event, 30);
    assert_eq!(meter.model.as_deref(), Some("fixture-final-model"));
}

fn fixed_prices(service: &Service) {
    let pricing = service.prices().unwrap();
    let view = pricing.view();
    let mut config: Config = view.config;
    config.auto_update = false;
    config.fixed.insert(
        "fixture-request-model".into(),
        json!({
            "input_cost_per_token": "0.1",
            "output_cost_per_token": "0.2"
        }),
    );
    config.fixed.insert(
        "fixture-response-model".into(),
        json!({
            "input_cost_per_token": "1",
            "output_cost_per_token": "2"
        }),
    );
    pricing.configure(config, &view.revision).unwrap();
}

fn priced_attempt(service: &Service, selection: &str) -> (String, String, String, String) {
    let mut settings: Settings = service.settings();
    settings.pricing_model = selection.into();
    settings.multiplier = "2".into();
    service.configure(settings).unwrap();

    let trace = service.begin("codex", Some("fixture-request-model"));
    let mut attempt = trace.attempt("fixture-provider", false, "http");
    let meter = Meter {
        model: Some("fixture-response-model".into()),
        tokens: Tokens {
            input: Some(2),
            output: Some(3),
            ..Tokens::default()
        },
        ..Meter::default()
    };
    attempt.update(&meter, Some(200), Some("success"));

    let response_model = attempt.attempt.response_model.clone().unwrap();
    let pricing_model = attempt.attempt.pricing_model.clone().unwrap();
    let price = attempt.attempt.price.as_ref().unwrap();
    (
        response_model,
        pricing_model,
        price.model.clone(),
        price.cost.clone(),
    )
}

#[test]
fn request_and_response_pricing_settings_choose_the_configured_model_and_multiplier() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    fixed_prices(&service);

    assert_eq!(
        priced_attempt(&service, "request"),
        (
            "fixture-response-model".into(),
            "fixture-request-model".into(),
            "fixture-request-model".into(),
            "1.6".into(),
        )
    );
    assert_eq!(
        priced_attempt(&service, "response"),
        (
            "fixture-response-model".into(),
            "fixture-response-model".into(),
            "fixture-response-model".into(),
            "16".into(),
        )
    );
}

#[test]
fn sparse_final_meter_preserves_response_model_and_prices_cumulative_usage() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    fixed_prices(&service);
    let mut settings = service.settings();
    settings.pricing_model = "response".into();
    settings.multiplier = "2".into();
    service.configure(settings).unwrap();

    let trace = service.begin("codex", Some("fixture-request-model"));
    let mut attempt = trace.attempt("fixture-provider", true, "http");
    let first = Meter {
        model: Some("fixture-response-model".into()),
        tokens: Tokens {
            input: Some(2),
            output: Some(1),
            ..Tokens::default()
        },
        ..Meter::default()
    };
    attempt.update(&first, Some(200), None);
    assert_eq!(attempt.attempt.price.as_ref().unwrap().cost, "8");

    let final_meter = Meter {
        tokens: Tokens {
            output: Some(3),
            ..Tokens::default()
        },
        ..Meter::default()
    };
    attempt.update(&final_meter, None, Some("success"));

    assert_eq!(
        attempt.attempt.response_model.as_deref(),
        Some("fixture-response-model")
    );
    assert_eq!(
        attempt.attempt.pricing_model.as_deref(),
        Some("fixture-response-model")
    );
    assert_eq!(attempt.attempt.tokens.input, Some(2));
    assert_eq!(attempt.attempt.tokens.output, Some(3));
    assert_eq!(attempt.attempt.status, Some(200));
    assert_eq!(attempt.attempt.price.as_ref().unwrap().cost, "16");
}

#[test]
fn missing_response_price_or_usage_remains_unpriced_with_known_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    fixed_prices(&service);
    let mut settings = service.settings();
    settings.pricing_model = "response".into();
    service.configure(settings).unwrap();

    for (model, tokens) in [
        (
            "fixture-unpriced-response-model",
            Tokens {
                input: Some(2),
                output: Some(3),
                ..Tokens::default()
            },
        ),
        ("fixture-response-model", Tokens::default()),
    ] {
        let trace = service.begin("codex", Some("fixture-request-model"));
        let mut attempt = trace.attempt("fixture-provider", false, "http");
        let meter = Meter {
            model: Some(model.into()),
            tokens: tokens.clone(),
            ..Meter::default()
        };
        attempt.update(&meter, Some(200), Some("success"));

        assert_eq!(attempt.attempt.response_model.as_deref(), Some(model));
        assert_eq!(attempt.attempt.pricing_model.as_deref(), Some(model));
        assert_eq!(attempt.attempt.tokens, tokens);
        assert!(attempt.attempt.price.is_none());

        let row = Record {
            source: "proxy".into(),
            completed: true,
            attempts: vec![attempt.attempt.clone()],
            ..Record::default()
        };
        assert!(row.cost().is_none());
        let mut totals = Totals::default();
        totals.add_record(&row);
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.unpriced, 1);
        assert_eq!(totals.tokens, tokens);
    }
}

#[test]
fn successful_search_is_priced_once_per_request_without_token_usage() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    fixed_prices(&service);
    let mut settings = service.settings();
    settings.multiplier = "2.5".into();
    service.configure(settings).unwrap();

    for path in ["/v1/alpha/search", "/alpha/search", "/v1/alpha/search/"] {
        let trace = service.begin_operation("codex", None, Operation::for_path(path));
        let mut attempt = trace.attempt("fixture-provider", false, "http");
        attempt.update(&Meter::default(), Some(200), None);
        assert!(attempt.attempt.price.is_none());

        attempt.update(&Meter::default(), None, Some("success"));
        attempt.update(&Meter::default(), None, None);

        let price = attempt.attempt.price.as_ref().unwrap();
        assert_eq!(price.model, "web_search");
        assert_eq!(price.multiplier, "2.5");
        assert_eq!(price.cost, "0.025");
        assert_eq!(
            price.rates.get("cost_per_request").map(String::as_str),
            Some("0.01")
        );
        assert_eq!(price.basis.as_ref().unwrap()["unit"], "request");
        assert_eq!(price.basis.as_ref().unwrap()["quantity"], 1);
        assert_eq!(attempt.attempt.grouping_model(), Some("web_search"));
        assert_eq!(attempt.attempt.tokens.total(), None);

        let row = Record {
            source: "proxy".into(),
            completed: true,
            attempts: vec![attempt.attempt.clone()],
            ..Record::default()
        };
        assert_eq!(row.cost().unwrap().to_string(), "0.025");
        let mut totals = Totals::default();
        totals.add_record(&row);
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.attempts, 1);
        assert_eq!(totals.unpriced, 0);
        assert_eq!(totals.cost, "0.025");
        assert_eq!(totals.tokens.total(), None);
    }
}

#[test]
fn unsuccessful_search_and_model_requests_without_usage_do_not_get_search_fees() {
    let dir = tempfile::tempdir().unwrap();
    let service = Service::new(dir.path());
    fixed_prices(&service);

    for (status, outcome) in [
        (Some(400), "error"),
        (Some(429), "error"),
        (Some(500), "error"),
        (Some(200), "failed"),
        (Some(200), "cancelled"),
        (None, "error"),
    ] {
        let trace = service.begin_operation("codex", None, Operation::WebSearch);
        let mut attempt = trace.attempt("fixture-provider", false, "http");
        attempt.update(&Meter::default(), status, Some(outcome));
        assert!(attempt.attempt.price.is_none(), "{status:?} {outcome}");
        assert_eq!(attempt.attempt.tokens.total(), None);
    }

    let trace = service.begin_operation(
        "codex",
        Some("fixture-request-model"),
        Operation::for_path("/v1/responses"),
    );
    let mut attempt = trace.attempt("fixture-provider", false, "http");
    attempt.update(&Meter::default(), Some(200), Some("success"));
    assert!(attempt.attempt.price.is_none());
    assert_eq!(
        attempt.attempt.grouping_model(),
        Some("fixture-request-model")
    );
}
