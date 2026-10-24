use super::*;
use protocol::Protocol;
use serde_json::json;

fn observed(gateway: &Gateway, ua: &'static str, success: bool, websocket: bool, business: bool) {
    let mut protocol = Protocol::new(websocket);
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::USER_AGENT,
        hyper::header::HeaderValue::from_static(ua),
    );
    if business {
        protocol.business_request(gateway, "fixture-provider", &headers);
    }
    protocol.request_headers(&headers);
    if websocket {
        protocol.websocket_status(101);
    } else {
        protocol.response(200, false, "identity");
    }
    let payload = json!({"type": if success {"response.completed"} else {"response.failed"}, "response": {
        "id":"fixture-response", "status":if success {"completed"} else {"failed"}, "model":"fixture-model",
        "usage":{"input_tokens":2,"output_tokens":1}
    }});
    if websocket {
        protocol.value(&payload);
    } else {
        protocol.feed(payload.to_string().as_bytes());
    }
    protocol.delivery().handed(1, true);
    protocol.finish(Some(if websocket { 101 } else { 200 }), code::OK);
}

#[test]
fn v02523_business_user_agent_is_platform_scoped_success_only_and_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    for client in [
        ClientId::Codex,
        ClientId::Claude,
        ClientId::Grok,
        ClientId::Dsh,
    ] {
        let path = dir.path().join(format!("{client:?}"));
        let gateway = Gateway::new_client(path.clone(), client, None).unwrap();
        assert!(gateway.successful_user_agent().is_err());
        observed(&gateway, "fixture-failed", false, false, true);
        observed(&gateway, "fixture-probe", true, false, false);
        assert!(gateway.successful_user_agent().is_err());
        observed(
            &gateway,
            "fixture-http/1.0 exact  spaces",
            true,
            false,
            true,
        );
        assert_eq!(
            gateway.successful_user_agent().unwrap(),
            "fixture-http/1.0 exact  spaces"
        );
        observed(&gateway, "fixture-websocket/2.0", true, true, true);
        assert_eq!(
            gateway.successful_user_agent().unwrap(),
            "fixture-websocket/2.0"
        );
        observed(
            &gateway,
            "fixture-failure-must-not-replace",
            false,
            true,
            true,
        );
        let view = serde_json::to_string(&gateway.view()).unwrap();
        assert!(!view.contains("fixture-websocket"));
        drop(gateway);
        let gateway = Gateway::new_client(path, client, None).unwrap();
        assert_eq!(
            gateway.successful_user_agent().unwrap(),
            "fixture-websocket/2.0"
        );
    }
}

#[test]
fn v02523_search_only_is_excluded_from_model_union_exit_and_generation_routes() {
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(dir.path().join("data")).unwrap();
    {
        let mut state = gateway.0.inner.lock().unwrap();
        for (name, search_only) in [("search", true), ("ordinary", false)] {
            state
                .store
                .edit(
                    Edit::SaveProvider {
                        search_only: None,
                        id: None,
                        name: Some(name.into()),
                        base_url: format!("https://{name}.example.invalid/v1"),
                        token: "fixture-token".into(),
                        quota_access_token: None,
                    },
                    false,
                )
                .unwrap();
            let provider = state.store.providers.last_mut().unwrap();
            provider.queued = true;
            provider.search_only = search_only;
            provider.allowed_models = Some(vec![format!("fixture-{name}")]);
        }
        state.store.mode = "auto".into();
        assert_eq!(
            Gateway::exit_provider(&state.store, Some(&state.store.providers[0].id))
                .unwrap()
                .name,
            "ordinary"
        );
    }
    let routes = gateway.routing_ids(None);
    assert_eq!(routes.len(), 1);
    assert_eq!(gateway.model_union(None).unwrap(), vec!["fixture-ordinary"]);
    assert_eq!(gateway.gateway_models().unwrap().providers.len(), 1);
    let store = &gateway.0.inner.lock().unwrap().store;
    assert_eq!(routes[0], store.providers[1].id);
}
