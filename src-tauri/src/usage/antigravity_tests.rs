use super::super::store::Store;
use super::*;
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, service::service_fn, Response};
use std::sync::{Arc, Mutex};

fn step() -> Value {
    json!({"metadata":{"executionId":"exec","startedAt":"2026-01-01T00:00:00Z","finishedGeneratingAt":"2026-01-01T00:00:02Z","completedAt":"2026-01-01T00:00:30Z"}})
}
fn entry(id: &str, index: usize) -> Value {
    let usage = json!({"responseId":format!("response-{id}"),"model":"MODEL_PLACEHOLDER_FIXTURE","apiProvider":"API_PROVIDER_FIXTURE","inputTokens":"100","cacheReadTokens":"40","outputTokens":"20","thinkingOutputTokens":"12","responseOutputTokens":"8"});
    json!({"executionId":"exec","stepIndices":[index],"chatModel":{"model":"MODEL_PLACEHOLDER_FIXTURE","responseModel":"fixture-response","modelDisplayName":"Fixture Model (High)","customMetadata":{"request_id":id},"usage":usage,"retryInfos":[{"usage":usage}],"timeToFirstToken":"0.25s","streamingDuration":"1.75s"}})
}
fn parsed(v: &Value) -> Record {
    record("fixture", v, &Steps::from([step()].as_slice()), 0)
        .unwrap()
        .unwrap()
}

#[test]
fn placeholder_mirror_uses_real_model_and_final_timings_once() {
    let r = parsed(&entry("one", 0));
    let a = &r.attempts[0];
    assert_eq!(r.attempts.len(), 1);
    assert_eq!(a.requested_model, None);
    assert_eq!(a.response_model.as_deref(), Some("fixture-response"));
    assert_eq!(a.model_display_name.as_deref(), Some("Fixture Model"));
    assert_eq!(a.api_provider.as_deref(), Some("API_PROVIDER_FIXTURE"));
    assert_eq!(a.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(
        (a.first_token_ms, a.generation_ms, a.measured_duration_ms()),
        (Some(250), Some(1750), Some(2000))
    );
    assert_eq!(r.tokens().total(), Some(160));
    assert_eq!(r.known_success(), Some(true));
    assert_eq!(r.finished_at, Some(r.started_at + 2000));
    assert_eq!(a.tokens.cache_write, None);
}

#[test]
fn zero_step_clock_uses_request_timings_and_unknown_suffixes_are_preserved() {
    let mut s = step();
    s["metadata"]["finishedGeneratingAt"] = s["metadata"]["startedAt"].clone();
    let steps = Steps::from([s].as_slice());
    let mut v = entry("one", 0);
    v["chatModel"]["responseModel"] = json!("gemini-4-argon-f");
    let r = record("fixture", &v, &steps, 0).unwrap().unwrap();
    assert_eq!(r.attempts[0].measured_duration_ms(), Some(2000));
    assert_eq!(r.finished_at, Some(r.started_at + 2000));
    assert_eq!(
        r.attempts[0].response_model.as_deref(),
        Some("gemini-4-argon-f")
    );
    v["chatModel"]["modelDisplayName"] = json!("Fixture (Preview) (High)");
    assert_eq!(
        parsed(&v).attempts[0].model_display_name.as_deref(),
        Some("Fixture (Preview)")
    );
    v["chatModel"]["modelDisplayName"] = json!("Fixture (Preview)");
    let r = parsed(&v);
    assert_eq!(
        r.attempts[0].model_display_name.as_deref(),
        Some("Fixture (Preview)")
    );
    assert_eq!(r.attempts[0].reasoning_effort, None);
    for key in ["timeToFirstToken", "streamingDuration"] {
        v["chatModel"].as_object_mut().unwrap().remove(key);
    }
    let r = record("fixture", &v, &steps, 0).unwrap().unwrap();
    assert_eq!(r.attempts[0].measured_duration_ms(), None);
    assert!(r.completed);
}

#[test]
fn historical_repair_preserves_prices_and_identity_in_both_storage_formats() {
    for native in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = if native {
            Store::open(dir.path())
        } else {
            Store::open_v11_fixture(dir.path())
        }
        .unwrap();
        if native {
            while store.compact_storage_batch().unwrap() {}
        }
        let mut old = parsed(&entry("one", 0));
        old.attempts[0].model_display_name = Some("Fixture Model (High)".into());
        old.attempts[0].reasoning_effort = None;
        old.attempts[0].duration_ms = 0;
        old.finished_at = Some(old.started_at);
        let price = super::super::pricing::Quote { model:"fixture-response".into(), data:Some(json!({"input_cost_per_token":1,"output_cost_per_token":2,"cache_read_input_token_cost":0})), version:"historical".into(), source:"fixed".into(), multiplier:"1".into() }.calculate(&old.attempts[0].tokens,None).unwrap();
        old.attempts[0].price = Some(price.clone());
        store.write_batch(&[old.clone()], None).unwrap();
        while store.repair_metadata_batch().unwrap() {}
        let repaired = store.detail(&old.id).unwrap();
        assert_eq!(repaired.attempts[0].id, old.attempts[0].id);
        assert_eq!(
            repaired.attempts[0].reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(repaired.attempts[0].measured_duration_ms(), Some(2000));
        assert_eq!(repaired.finished_at, Some(old.started_at + 2000));
        assert_eq!(
            serde_json::to_value(&repaired.attempts[0].price).unwrap(),
            serde_json::to_value(Some(&price)).unwrap()
        );
        let before = store.connection.total_changes();
        assert!(!store.repair_metadata_batch().unwrap());
        assert_eq!(store.connection.total_changes(), before);
        assert_eq!(
            store.logs(&super::super::Filter::default()).unwrap().total,
            1
        );
    }
}

#[test]
fn retries_keep_individual_models_clocks_and_top_level_error() {
    let mut v = entry("one", 0);
    v["chatModel"]["retryInfos"].as_array_mut().unwrap().insert(0,json!({"traceId":"retry-one","error":"fixture unavailable","startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:00:01Z","usage":{"model":"fixture-earlier","responseId":"early","inputTokens":"2","outputTokens":"0"}}));
    let r = parsed(&v);
    assert_eq!(r.attempts.len(), 2);
    assert_eq!(
        r.attempts[0].response_model.as_deref(),
        Some("fixture-earlier")
    );
    assert_eq!(r.attempts[0].model_display_name, None);
    assert_eq!(r.attempts[0].measured_duration_ms(), Some(1000));
    assert_eq!(r.attempts[0].first_token_ms, None);
    assert_eq!(
        r.attempts[1].availability["attempt_start"],
        "upstream_unreported"
    );
    v["chatModel"]["error"] = json!("fixture rejected");
    let r = parsed(&v);
    assert_eq!(r.attempts.len(), 2);
    assert_eq!(r.known_success(), Some(false));
    assert_eq!(r.final_attempt().unwrap().outcome, "failed");
    v["chatModel"].as_object_mut().unwrap().remove("usage");
    assert_eq!(parsed(&v).known_success(), Some(false));
    v["chatModel"]["error"] = json!({"code":429,"message":"fixture"});
    assert_eq!(
        parsed(&v).final_attempt().unwrap().error_code.as_deref(),
        Some("429")
    );
}

#[test]
fn final_mirror_preserves_retry_fields_and_running_requests_have_an_identity() {
    let mut v = entry("one", 0);
    v["chatModel"]["retryInfos"][0]["usage"]["model"] = json!("fixture-earlier");
    v["chatModel"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("inputTokens");
    let r = parsed(&v);
    assert_eq!(r.attempts.len(), 1);
    assert_eq!(r.attempts[0].tokens.input, Some(100));
    assert_eq!(
        r.attempts[0].response_model.as_deref(),
        Some("fixture-earlier")
    );
    v["chatModel"] =
        json!({"customMetadata":{"request_id":"one"},"responseModel":"fixture-response"});
    let steps = Steps::from([json!({"metadata":{"startedAt":"2026-01-01T00:00:00Z"}})].as_slice());
    let pending = record("fixture", &v, &steps, 0).unwrap().unwrap();
    assert_eq!(pending.id, r.id);
    assert_eq!(pending.final_attempt().unwrap().outcome, "in_progress");
    assert_eq!(pending.final_attempt().unwrap().tokens.output, None);
}

#[test]
fn reliable_clocks_and_explicit_generation_remain_distinct_in_summary() {
    let mut v = entry("one", 0);
    v["chatModel"]
        .as_object_mut()
        .unwrap()
        .remove("timeToFirstToken");
    let mut r = parsed(&v);
    assert_eq!(r.attempts[0].measured_duration_ms(), Some(2000));
    assert_eq!(r.attempts[0].measured_generation_ms(), Some(1750));
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    price(
        &mut r,
        &Pricing::new(dir.path()).unwrap(),
        &Settings::default(),
    );
    store.write_batch(&[r.clone()], None).unwrap();
    let total = store.dashboard(&Filter::default()).unwrap().totals;
    let mut memory = Totals::default();
    memory.add_record(&r);
    assert_eq!(
        (
            total.duration_ms,
            total.generation_ms,
            total.measured_outputs,
            total.first_token_samples,
            total.sessions
        ),
        (2000, 1750, 20, 0, 1)
    );
    assert_eq!(total.generation_ms, memory.generation_ms);
}

#[test]
fn tool_completion_and_partial_timing_do_not_invent_success_or_duration() {
    let mut v = entry("one", 0);
    v["chatModel"]
        .as_object_mut()
        .unwrap()
        .remove("streamingDuration");
    let mut tool = step();
    tool["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("finishedGeneratingAt");
    let r = record("fixture", &v, &Steps::from([tool].as_slice()), 0)
        .unwrap()
        .unwrap();
    assert_eq!(r.finished_at, None);
    assert_eq!(r.known_success(), None);
    assert_eq!(r.attempts[0].outcome, "in_progress");
    assert_eq!(r.attempts[0].measured_duration_ms(), None);
    assert_eq!(r.attempts[0].first_token_ms, Some(250));
    assert_eq!(duration(&json!("0s")), Some(0));
    assert_eq!(duration(&json!(0.25)), Some(250));
    for invalid in [json!("NaNs"), json!("-1s"), json!("infs"), json!("1e30s")] {
        assert_eq!(duration(&invalid), None);
    }
    assert_eq!(label(Some("person@example.invalid")), None);
}

#[test]
fn fields_roundtrip_metrics_and_price_snapshot_survive_history_repair() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let view = pricing.view();
    let mut config = view.config;
    config.auto_update = false;
    config.fixed.insert("fixture-response".into(),json!({"input_cost_per_token":"0.1","output_cost_per_token":"0.2","cache_read_input_token_cost":"0.01"}));
    pricing.configure(config, &view.revision).unwrap();
    let mut r = parsed(&entry("one", 0));
    price(&mut r, &pricing, &Settings::default());
    store.write_batch(&[r.clone()], None).unwrap();
    let saved = store.detail(&r.id).unwrap();
    assert_eq!(saved.attempts[0].generation_ms, Some(1750));
    assert_eq!(
        saved.attempts[0].model_display_name,
        r.attempts[0].model_display_name
    );
    assert_eq!(saved.attempts[0].api_provider, r.attempts[0].api_provider);
    let before = store.dashboard(&Filter::default()).unwrap().totals;
    assert_eq!(
        (
            before.success,
            before.status_known,
            before.first_token_samples,
            before.first_token_sum_ms
        ),
        (1, 1, 1, 250)
    );
    assert_eq!(before.cost, "14.4");
    r.attempts[0].price.as_mut().unwrap().cost = "999".into();
    store.write_batch(&[r.clone()], None).unwrap();
    assert_eq!(
        store.detail(&r.id).unwrap().attempts[0]
            .price
            .as_ref()
            .unwrap()
            .cost,
        "14.4"
    );
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        1
    );
    store.archive_batch(&[r.id.clone()]).unwrap();
    store.write_batch(&[r], None).unwrap();
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        1
    );
    assert_eq!(store.logs(&Filter::default()).unwrap().total, 0);
}

struct Fixture {
    api: Api,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    changed: Arc<Mutex<String>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(broken: bool) -> Fixture {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let changed = Arc::new(Mutex::new("first".to_string()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (history, version) = (calls.clone(), changed.clone());
    let server = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let (history, version) = (history.clone(), version.clone());
            tokio::spawn(async move {
                let service = service_fn(move |r: hyper::Request<hyper::body::Incoming>| {
                    let (history, version) = (history.clone(), version.clone());
                    async move {
                        let method = r.uri().path().rsplit('/').next().unwrap().to_owned();
                        let body: Value = serde_json::from_slice(
                            &r.into_body().collect().await.unwrap().to_bytes(),
                        )
                        .unwrap();
                        history.lock().unwrap().push((method.clone(), body.clone()));
                        let value = if body["cascadeId"] == "a-broken" {
                            json!({"unexpected":"fixture"})
                        } else {
                            match method.as_str() {
                                "GetAllCascadeTrajectories" => {
                                    let mut summaries = json!({"z-good":{"lastModifiedTime":*version.lock().unwrap(),"stepCount":2}});
                                    if broken {
                                        summaries["a-broken"] =
                                            json!({"lastModifiedTime":"first","stepCount":1});
                                    }
                                    json!({"trajectorySummaries":summaries})
                                }
                                "GetCascadeTrajectoryGeneratorMetadata" => {
                                    let offset =
                                        body["generatorMetadataOffset"].as_u64().unwrap_or(0)
                                            as usize;
                                    let entries = [entry("first", 0), entry("second", 1)];
                                    json!({"generatorMetadata":&entries[offset.min(2)..]})
                                }
                                "GetCascadeTrajectorySteps" => {
                                    let offset = body["stepOffset"].as_u64().unwrap_or(0) as usize;
                                    json!({"steps":&[step(),step()][offset.min(2)..]})
                                }
                                _ => panic!("unexpected read method"),
                            }
                        };
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(
                            serde_json::to_vec(&value).unwrap(),
                        ))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    Fixture {
        api: Api {
            origin,
            token: "fixture".into(),
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        },
        calls,
        changed,
        server,
    }
}

#[tokio::test]
async fn manual_repair_bypasses_cursor_and_incremental_sync_reads_only_needed_steps() {
    let f = fixture(false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let settings = Settings::default();
    f.api
        .collect(
            &mut store,
            &pricing,
            &settings,
            &mut Report::default(),
            false,
        )
        .await
        .unwrap();
    let id = parsed(&entry("first", 0)).id;
    let mut old = store.detail(&id).unwrap();
    old.attempts[0].model_display_name = None;
    store.write_batch(&[old], None).unwrap();
    f.api
        .collect(
            &mut store,
            &pricing,
            &settings,
            &mut Report::default(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        store.detail(&id).unwrap().attempts[0].model_display_name,
        None
    );
    f.api
        .collect(
            &mut store,
            &pricing,
            &settings,
            &mut Report::default(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        store.detail(&id).unwrap().attempts[0]
            .model_display_name
            .as_deref(),
        Some("Fixture Model")
    );
    f.calls.lock().unwrap().clear();
    *f.changed.lock().unwrap() = "next".into();
    let sequence: i64 = store
        .connection
        .query_row(
            "SELECT value FROM metadata WHERE key='record_sequence'",
            [],
            |r| r.get::<_, String>(0).map(|s| s.parse().unwrap()),
        )
        .unwrap();
    f.api
        .collect(
            &mut store,
            &pricing,
            &settings,
            &mut Report::default(),
            false,
        )
        .await
        .unwrap();
    let next: i64 = store
        .connection
        .query_row(
            "SELECT value FROM metadata WHERE key='record_sequence'",
            [],
            |r| r.get::<_, String>(0).map(|s| s.parse().unwrap()),
        )
        .unwrap();
    assert_eq!(sequence, next);
    let calls = f.calls.lock().unwrap();
    assert!(!calls.iter().any(|(m, _)| m == "GetCascadeTrajectory"));
    assert!(calls
        .iter()
        .any(|(m, b)| m == "GetCascadeTrajectorySteps" && b["stepOffset"] == 1));
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        2
    );
}

#[tokio::test]
async fn failed_session_does_not_block_other_sessions_or_advance_its_cursor() {
    let f = fixture(true).await;
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let mut report = Report::default();
    f.api
        .collect(
            &mut store,
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .await
        .unwrap();
    assert_eq!((report.errors, report.files, report.imported), (1, 1, 2));
    assert_eq!(
        store.cursor(&safe_id("antigravity:rpc:a-broken")).unwrap(),
        None
    );
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        2
    );
    struct Complete<'a>(&'a mut Store);
    impl Repository for Complete<'_> {
        fn requires_complete_history(&self) -> bool {
            true
        }
        fn cursor(&self, id: &str) -> Result<Option<String>> {
            self.0.cursor(id)
        }
        fn generation(&self, source: &str) -> Result<u64> {
            self.0.source_generation(source)
        }
        fn write_batch(&mut self, rows: &[Record], cursor: Option<(&str, &str)>) -> Result<()> {
            self.0.write_batch(rows, cursor)
        }
        fn begin_rebuild(&mut self, source: &str) -> Result<()> {
            self.0.begin_rebuild(source)
        }
        fn stage_rebuild(
            &mut self,
            source: &str,
            rows: &[Record],
            cursor: Option<(&str, &str)>,
        ) -> Result<()> {
            self.0.stage_rebuild(source, rows, cursor)
        }
        fn finish_rebuild(&mut self, source: &str) -> Result<()> {
            self.0.finish_rebuild(source)
        }
    }
    assert!(f
        .api
        .collect(
            &mut Complete(&mut store),
            &pricing,
            &Settings::default(),
            &mut Report::default(),
            false
        )
        .await
        .is_err());
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        2
    );
}
