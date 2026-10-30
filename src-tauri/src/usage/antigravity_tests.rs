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
fn request_timings_override_step_clocks_and_small_generation_is_not_a_speed() {
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
        let mut memory = Totals::default();
        for (index, generation) in [Some(0), Some(50), Some(51), None].into_iter().enumerate() {
            let mut v = entry(&format!("timing-{index}"), 0);
            v["chatModel"]["timeToFirstToken"] = json!("1.999s");
            if let Some(ms) = generation {
                v["chatModel"]["streamingDuration"] = json!(ms as f64 / 1000.0);
            } else {
                v["chatModel"]
                    .as_object_mut()
                    .unwrap()
                    .remove("streamingDuration");
            }
            let r = parsed(&v);
            let a = &r.attempts[0];
            assert_eq!(a.first_token_ms, Some(1999));
            assert_eq!(
                a.measured_duration_ms(),
                Some(generation.map_or(2000, |ms| 1999 + ms))
            );
            assert_eq!(a.measured_generation_ms(), generation.filter(|ms| *ms > 50));
            memory.add_record(&r);
            store.write_batch(&[r], None).unwrap();
        }
        if native {
            while store.compact_storage_batch().unwrap() {}
        }
        let total = store.dashboard(&Filter::default()).unwrap().totals;
        assert_eq!(total.generation_ms, 51);
        assert_eq!(total.generation_ms, memory.generation_ms);
        assert_eq!(total.duration_ms, memory.duration_ms);
        assert_eq!(total.first_token_sum_ms, 1999 * 4);
    }
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
fn history_reprojects_small_generation_even_when_total_was_already_correct() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open_v11_fixture(dir.path()).unwrap();
    let mut v = entry("old-small-generation", 0);
    v["chatModel"]["streamingDuration"] = json!("0.05s");
    let r = parsed(&v);
    store.write_batch(std::slice::from_ref(&r), None).unwrap();
    store
        .connection
        .execute(
            "UPDATE usage_metrics SET generation_ms=50,measured_outputs=20 WHERE owner=?1",
            [&r.id],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT OR REPLACE INTO metadata VALUES('antigravity_fields_v02610','done')",
            [],
        )
        .unwrap();
    while store.repair_metadata_batch().unwrap() {}
    let total = store.dashboard(&Filter::default()).unwrap().totals;
    assert_eq!(total.generation_ms, 0);
    assert_eq!(
        store.detail(&r.id).unwrap().attempts[0].generation_ms,
        Some(50)
    );
    let before = store.connection.total_changes();
    assert!(!store.repair_metadata_batch().unwrap());
    assert_eq!(store.connection.total_changes(), before);
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
    entries: Arc<Mutex<Vec<Value>>>,
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
    let entries = Arc::new(Mutex::new(vec![entry("first", 0), entry("second", 1)]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (history, version, metadata) = (calls.clone(), changed.clone(), entries.clone());
    let server = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let (history, version, metadata) = (history.clone(), version.clone(), metadata.clone());
            tokio::spawn(async move {
                let service = service_fn(move |r: hyper::Request<hyper::body::Incoming>| {
                    let (history, version, metadata) =
                        (history.clone(), version.clone(), metadata.clone());
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
                                    let mut entries = metadata.lock().unwrap().clone();
                                    if body["cascadeId"] != "z-good" {
                                        for (i, entry) in entries.iter_mut().enumerate() {
                                            let response = json!(format!(
                                                "{}-response-{i}",
                                                body["cascadeId"].as_str().unwrap()
                                            ));
                                            entry["chatModel"]["usage"]["responseId"] =
                                                response.clone();
                                            entry["chatModel"]["retryInfos"][0]["usage"]
                                                ["responseId"] = response;
                                            entry["chatModel"]["customMetadata"]["request_id"] =
                                                json!(format!(
                                                    "{}-{i}",
                                                    body["cascadeId"].as_str().unwrap()
                                                ));
                                        }
                                    }
                                    json!({"generatorMetadata":&entries[offset.min(entries.len())..(offset + 1).min(entries.len())]})
                                }
                                "GetCascadeTrajectorySteps" => {
                                    let offset = body["stepOffset"].as_u64().unwrap_or(0) as usize;
                                    if body["trajectoryVerbosity"] == 2 {
                                        // One page per step also exercises the unknown-total fallback.
                                        json!({"steps":&[step(),step()][offset.min(2)..(offset + 1).min(2)]})
                                    } else {
                                        json!({"steps":[step()],"detail":"x".repeat(LIMIT + 1)})
                                    }
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
        api: Api::from_single(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            origin,
            "fixture".into(),
        ),
        calls,
        changed,
        entries,
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
            dir.path(),
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
            dir.path(),
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
            dir.path(),
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
            dir.path(),
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
            dir.path(),
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
            dir.path(),
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

#[tokio::test]
async fn local_history_discovers_unlisted_sessions_and_fallback_steps_cover_transcript() {
    let f = fixture(false).await;
    let dir = tempfile::tempdir().unwrap();
    f.entries.lock().unwrap()[1]
        .as_object_mut()
        .unwrap()
        .remove("stepIndices");
    for id in ["z-good", "local-only"] {
        let path = dir.path().join(format!(
            "antigravity/brain/{id}/.system_generated/logs/transcript.jsonl"
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            "{\"step_index\":0,\"input_tokens\":100}\n{\"step_index\":1,\"input_tokens\":100}\n",
        )
        .unwrap();
    }
    let mut store = Store::open(&dir.path().join("db")).unwrap();
    let pricing = Pricing::new(&dir.path().join("db")).unwrap();
    for _ in 0..2 {
        let mut report = Report::default();
        f.api
            .collect(
                &mut store,
                dir.path(),
                &pricing,
                &Settings::default(),
                &mut report,
                false,
            )
            .await
            .unwrap();
        scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!((report.total_files, report.files, report.errors), (2, 2, 0));
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            4
        );
        for id in ["z-good", "local-only"] {
            let cursor: Cursor = serde_json::from_str(
                &store
                    .cursor(&safe_id(&format!("antigravity:rpc:{id}")))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(cursor.covered, BTreeSet::from([0, 1]));
        }
    }
    assert!(f
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "GetCascadeTrajectorySteps")
        .all(|(_, b)| b["trajectoryVerbosity"] == 2));
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, b)| {
                m == "GetCascadeTrajectoryGeneratorMetadata" && b["cascadeId"] == "local-only"
            })
            .count(),
        3
    );
    let local_transcript = dir
        .path()
        .join("antigravity/brain/local-only/.system_generated/logs/transcript.jsonl");
    let mut existing = std::fs::read_to_string(&local_transcript).unwrap();
    existing.push_str("{\"step_index\":2,\"input_tokens\":50}\n");
    std::fs::write(&local_transcript, existing).unwrap();
    let mut report = Report::default();
    f.api
        .collect(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, b)| {
                m == "GetCascadeTrajectoryGeneratorMetadata" && b["cascadeId"] == "local-only"
            })
            .count(),
        5
    );
}

#[tokio::test]
async fn missing_identity_keeps_gap_but_saves_later_requests_and_repairs_after_restart() {
    let f = fixture(false).await;
    let original = f.entries.lock().unwrap()[0].clone();
    {
        let mut entries = f.entries.lock().unwrap();
        entries[0].as_object_mut().unwrap().remove("executionId");
        entries[0]["chatModel"]
            .as_object_mut()
            .unwrap()
            .remove("customMetadata");
    }
    let dir = tempfile::tempdir().unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut report = Report::default();
    f.api
        .collect(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .await
        .unwrap();
    assert_eq!(report.errors, 1);
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        1
    );
    let cursor: Cursor = serde_json::from_str(
        &store
            .cursor(&safe_id("antigravity:rpc:z-good"))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(cursor.count, 0);
    assert!(cursor.modified.is_empty());
    assert_eq!(cursor.covered, BTreeSet::from([1]));
    drop(store);
    f.entries.lock().unwrap()[0] = original;
    let mut store = Store::open(dir.path()).unwrap();
    for _ in 0..2 {
        let mut report = Report::default();
        f.api
            .collect(
                &mut store,
                dir.path(),
                &pricing,
                &Settings::default(),
                &mut report,
                false,
            )
            .await
            .unwrap();
        assert_eq!(report.errors, 0);
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            2
        );
    }
}

#[test]
fn file_gap_does_not_hide_later_usage_or_advance_past_unresolved_history() {
    let dir = tempfile::tempdir().unwrap();
    let brain = dir.path().join("antigravity/brain");
    let path = brain
        .join("fixture")
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let gap = "{\"step_index\":0,\"input_tokens\":7}\n";
    let later = "{\"request_id\":\"later\",\"model\":\"fixture-model\",\"created_at\":\"2026-01-01T00:00:00Z\",\"input_tokens\":3}\n";
    std::fs::write(&path, format!("{gap}{later}")).unwrap();
    let mut store = Store::open(&dir.path().join("db")).unwrap();
    let pricing = Pricing::new(&dir.path().join("db")).unwrap();
    let key = safe_id(&format!("antigravity:file:{}", path.to_string_lossy()));
    for _ in 0..2 {
        let mut report = Report::default();
        scan_files(
            &mut store,
            dir.path(),
            &pricing,
            &Settings::default(),
            &mut report,
            false,
        )
        .unwrap();
        assert_eq!(report.errors, 1);
        assert_eq!(
            store.dashboard(&Filter::default()).unwrap().totals.requests,
            1
        );
        let cursor: FileCursor =
            serde_json::from_str(&store.cursor(&key).unwrap().unwrap()).unwrap();
        assert_eq!(cursor.offset, 0);
    }
    // A verified API association resolves the gap without rewriting the file.
    let rpc = serde_json::to_string(&Cursor {
        covered: BTreeSet::from([0]),
        ..Cursor::default()
    })
    .unwrap();
    store
        .write_batch(&[], Some((&safe_id("antigravity:rpc:fixture"), &rpc)))
        .unwrap();
    let mut report = Report::default();
    scan_files(
        &mut store,
        dir.path(),
        &pricing,
        &Settings::default(),
        &mut report,
        false,
    )
    .unwrap();
    assert_eq!(report.errors, 0);
    let cursor: FileCursor = serde_json::from_str(&store.cursor(&key).unwrap().unwrap()).unwrap();
    assert_eq!(cursor.offset, (gap.len() + later.len()) as u64);
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        format!("{gap}{later}")
    );
}

#[test]
fn known_placeholders_and_enums_resolve_pricing_and_effort_while_unknown_stays_unpriced() {
    let dir = tempfile::tempdir().unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let settings = Settings::default();
    let steps = Steps::from([step()].as_slice());

    for (raw, expected_model, expected_display, expected_effort, expects_priced) in [
        (
            "MODEL_PLACEHOLDER_M37",
            Some("gemini-3.1-pro"),
            Some("Gemini 3.1 Pro"),
            Some("high"),
            true,
        ),
        (
            "MODEL_PLACEHOLDER_M36",
            Some("gemini-3.1-pro"),
            Some("Gemini 3.1 Pro"),
            Some("low"),
            true,
        ),
        (
            "MODEL_PLACEHOLDER_M18",
            Some("gemini-3-flash"),
            Some("Gemini 3 Flash"),
            None,
            true,
        ),
        (
            "MODEL_PLACEHOLDER_M26",
            Some("claude-opus-4-6"),
            Some("Claude Opus 4.6"),
            Some("high"),
            true,
        ),
        (
            "MODEL_PLACEHOLDER_M35",
            Some("claude-sonnet-4-6"),
            Some("Claude Sonnet 4.6"),
            None,
            true,
        ),
        (
            "MODEL_CLAUDE_4_5_SONNET_THINKING",
            Some("claude-sonnet-4-5"),
            Some("Claude Sonnet 4.5"),
            Some("high"),
            true,
        ),
        (
            "MODEL_CHAT_20706",
            Some("gemini-2.5-flash"),
            Some("Gemini 2.5 Flash"),
            None,
            true,
        ),
        (
            "MODEL_OPENAI_GPT_OSS_120B_MEDIUM",
            Some("gpt-oss-120b"),
            Some("GPT-OSS 120B"),
            Some("medium"),
            false,
        ),
        ("MODEL_PLACEHOLDER_M999", None, None, None, false),
    ] {
        let item = json!({
            "executionId": "exec",
            "stepIndices": [0],
            "chatModel": {
                "model": raw,
                "customMetadata": {"request_id": format!("req-{raw}")},
                "usage": {
                    "responseId": format!("resp-{raw}"),
                    "model": raw,
                    "inputTokens": "1000",
                    "cacheReadTokens": "500",
                    "outputTokens": "200",
                    "thinkingOutputTokens": "80"
                },
                "timeToFirstToken": "0.2s",
                "streamingDuration": "1.0s"
            }
        });
        let mut rec = record("cascade-one", &item, &steps, 0).unwrap().unwrap();
        price(&mut rec, &pricing, &settings);
        let a = &rec.attempts[0];
        assert_eq!(a.requested_model.as_deref(), expected_model, "{raw}");
        assert_eq!(a.response_model.as_deref(), expected_model, "{raw}");
        assert_eq!(a.model_display_name.as_deref(), expected_display, "{raw}");
        assert_eq!(a.reasoning_effort.as_deref(), expected_effort, "{raw}");
        assert_eq!(a.price.is_some(), expects_priced, "{raw}");
    }

    // Historical attempt with only display name or raw enum recovers canonical model via normalize_attempt.
    let mut legacy = Attempt {
        model_display_name: Some("Claude Opus 4.6 (Thinking)".into()),
        duration_ms: 1000,
        ..Attempt::default()
    };
    assert!(normalize_attempt(&mut legacy));
    assert_eq!(
        legacy.model_display_name.as_deref(),
        Some("Claude Opus 4.6")
    );
    assert_eq!(legacy.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(legacy.response_model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(legacy.pricing_model.as_deref(), Some("claude-opus-4-6"));
}

#[tokio::test]
async fn multi_window_endpoints_merge_fresher_trajectories_and_fallback_for_brain_only_sessions() {
    let pages = json!([
        {"type":"page","url":"https://127.0.0.1:41001/index.html"},
        {"type":"page","url":"https://127.0.0.1:41001/other"},
        {"type":"other","url":"https://127.0.0.1:41002/worker"},
        {"type":"page","url":"https://127.0.0.1:41002/index.html"}
    ]);
    assert_eq!(
        parse_page_origins(&pages),
        vec![
            "https://127.0.0.1:41001".to_owned(),
            "https://127.0.0.1:41002".to_owned(),
        ]
    );

    async fn spawn_window(
        summaries: Value,
        owned_sessions: BTreeMap<String, Vec<Value>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let summaries = summaries.clone();
                let owned_sessions = owned_sessions.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |r: hyper::Request<hyper::body::Incoming>| {
                        let summaries = summaries.clone();
                        let owned_sessions = owned_sessions.clone();
                        async move {
                            let method = r.uri().path().rsplit('/').next().unwrap().to_owned();
                            let body: Value = serde_json::from_slice(
                                &r.into_body().collect().await.unwrap().to_bytes(),
                            )
                            .unwrap();
                            let cid = body["cascadeId"].as_str().unwrap_or("");
                            let value = match method.as_str() {
                                "GetAllCascadeTrajectories" => {
                                    json!({"trajectorySummaries": summaries})
                                }
                                "GetCascadeTrajectoryGeneratorMetadata" => {
                                    let offset =
                                        body["generatorMetadataOffset"].as_u64().unwrap_or(0)
                                            as usize;
                                    if let Some(items) = owned_sessions.get(cid) {
                                        let end = items.len();
                                        json!({"generatorMetadata": &items[offset.min(end)..end]})
                                    } else {
                                        json!({})
                                    }
                                }
                                "GetCascadeTrajectorySteps" => {
                                    json!({"steps": [step(), step()]})
                                }
                                _ => json!({}),
                            };
                            Ok::<_, std::convert::Infallible>(Response::new(Full::new(
                                Bytes::from(serde_json::to_vec(&value).unwrap()),
                            )))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        (origin, handle)
    }

    let (origin_a, server_a) = spawn_window(
        json!({
            "shared": {"lastModifiedTime":"2026-01-01T00:00:10Z","stepCount":1},
            "only-a": {"lastModifiedTime":"2026-01-01T00:00:20Z","stepCount":1}
        }),
        BTreeMap::from([
            ("shared".into(), vec![entry("shared-stale", 0)]),
            ("only-a".into(), vec![entry("only-a-req", 0)]),
        ]),
    )
    .await;
    let (origin_b, server_b) = spawn_window(
        json!({
            "shared": {"lastModifiedTime":"2026-01-01T00:00:50Z","stepCount":2}
        }),
        BTreeMap::from([
            (
                "shared".into(),
                vec![entry("shared-fresh-1", 0), entry("shared-fresh-2", 1)],
            ),
            ("brain-b".into(), vec![entry("brain-b-req", 0)]),
        ]),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let brain_file = dir
        .path()
        .join("antigravity/brain/brain-b/transcript.jsonl");
    std::fs::create_dir_all(brain_file.parent().unwrap()).unwrap();
    std::fs::write(&brain_file, "{}\n").unwrap();

    let api = Api::from_endpoints(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        vec![(origin_a, "tok-a".into()), (origin_b, "tok-b".into())],
    );
    let mut store = Store::open(dir.path()).unwrap();
    let pricing = Pricing::new(dir.path()).unwrap();
    let mut report = Report::default();
    api.collect(
        &mut store,
        dir.path(),
        &pricing,
        &Settings::default(),
        &mut report,
        false,
    )
    .await
    .unwrap();
    server_a.abort();
    server_b.abort();

    assert_eq!(report.errors, 0);
    // 2 from fresher window_b "shared" + 1 from window_a "only-a" + 1 from fallback window_b "brain-b"
    assert_eq!(
        store.dashboard(&Filter::default()).unwrap().totals.requests,
        4
    );
    assert!(store
        .detail(&safe_id("antigravity:request:shared-fresh-2"))
        .is_ok());
    assert!(store
        .detail(&safe_id("antigravity:request:shared-stale"))
        .is_err());
    assert!(store
        .detail(&safe_id("antigravity:request:brain-b-req"))
        .is_ok());
}

#[tokio::test]
async fn antigravity_stats_aggregate_token_mix_savings_dual_tracks_sessions_and_live_quotas() {
    let quota_payload = json!({
        "email": "private-user@example.invalid",
        "userQuotaSummary": {
            "quotas": [
                {
                    "label": "Gemini Pro",
                    "remainingPercentage": 72.5,
                    "resetTime": "2026-10-30T15:00:00Z"
                },
                {
                    "displayName": "Claude / GPT",
                    "usedFraction": 0.4,
                    "resetTime": "2026-10-30T16:00:00Z"
                },
                {
                    "label": "leak@example.invalid",
                    "remainingPercentage": 50.0
                },
                {
                    "label": "~/workspace/secret",
                    "remainingPercentage": 50.0
                }
            ]
        }
    });
    let parsed_quotas = parse_quota_summary(&quota_payload);
    assert_eq!(parsed_quotas.len(), 2);
    assert_eq!(parsed_quotas[0].label, "Gemini Pro");
    assert_eq!(parsed_quotas[0].remaining_percent, Some(72.5));
    assert_eq!(parsed_quotas[0].used_percent, Some(27.5));
    assert!(parsed_quotas[0].reset_at.is_some());
    assert_eq!(parsed_quotas[1].label, "Claude / GPT");
    assert_eq!(parsed_quotas[1].used_percent, Some(40.0));
    assert_eq!(parsed_quotas[1].remaining_percent, Some(60.0));

    // Offline / closed endpoint degrades silently without error.
    let closed_api = Api::from_single(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        "http://127.0.0.1:9".into(),
        "fixture".into(),
    );
    assert_eq!(
        probe_live_quotas_with_api(&closed_api).await,
        (false, vec![])
    );

    let now_ms = 1_760_000_000_000i64;
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
        let pricing = Pricing::new(dir.path()).unwrap();
        let settings = Settings::default();

        let make_ag = |req_id: &str,
                       cascade: &str,
                       model: &str,
                       at: i64,
                       inp: u64,
                       cr: u64,
                       out: u64,
                       think: u64| {
            let mut r = Record {
                id: safe_id(&format!("antigravity:request:{req_id}")),
                client: "antigravity".into(),
                source: "antigravity".into(),
                started_at: at - 2_000,
                finished_at: Some(at),
                session_id: Some(safe_id(cascade)),
                completed: true,
                attempts: vec![Attempt {
                    id: safe_id(&format!("antigravity:request:{req_id}:final")),
                    requested_model: actual_model(Some(model)),
                    response_model: actual_model(Some(model)),
                    started_at: at - 2_000,
                    duration_ms: 2_000,
                    first_token_ms: Some(300),
                    generation_ms: Some(1_700),
                    stream: true,
                    transport: "local_api".into(),
                    outcome: "success".into(),
                    usage_status: "reported".into(),
                    tokens: Tokens {
                        input: Some(inp),
                        cache_read: Some(cr),
                        cache_write: Some(0),
                        output: Some(out),
                        reasoning: Some(think),
                        ..Tokens::default()
                    },
                    ..Attempt::default()
                }],
                ..Record::default()
            };
            normalize_record(&mut r);
            price(&mut r, &pricing, &settings);
            r
        };

        // 1) Gemini 3.1 Pro within 5h window
        let r1 = make_ag(
            "g-recent",
            "session-alpha",
            "MODEL_PLACEHOLDER_M37",
            now_ms - 3_600_000,
            10_000,
            40_000,
            2_000,
            1_200,
        );
        // 2) Gemini 3 Flash outside 5h but inside 7d
        let r2 = make_ag(
            "g-older",
            "session-alpha",
            "MODEL_PLACEHOLDER_M18",
            now_ms - 24 * 3_600_000,
            5_000,
            5_000,
            1_000,
            200,
        );
        // 3) Claude Opus 4.6 within 5h window
        let r3 = make_ag(
            "c-recent",
            "session-beta",
            "MODEL_PLACEHOLDER_M26",
            now_ms - 1_800_000,
            8_000,
            32_000,
            3_000,
            1_800,
        );
        // 4) Non-Antigravity Codex record (must be excluded from Antigravity stats)
        let mut other = r1.clone();
        other.id = safe_id("codex:other");
        other.client = "codex".into();
        other.source = "proxy".into();

        store
            .write_batch(&[r1.clone(), r2, r3.clone(), other], None)
            .unwrap();
        if native {
            while store.compact_storage_batch().unwrap() {}
        }

        let stats = store
            .antigravity_stats(
                &Filter {
                    end: Some(now_ms),
                    ..Filter::default()
                },
                &pricing,
                "1",
                now_ms,
                (true, parsed_quotas.clone()),
            )
            .unwrap();

        assert_eq!(stats.requests, 3);
        assert_eq!(stats.sessions, 2);
        assert_eq!(stats.token_mix.input, 23_000);
        assert_eq!(stats.token_mix.cache_read, 77_000);
        assert_eq!(stats.token_mix.output, 6_000);
        assert_eq!(stats.token_mix.reasoning, 3_200);
        assert_eq!(stats.token_mix.visible_output, 2_800);
        assert_eq!(stats.token_mix.thinking_share_percent, Some(53.3));
        assert_eq!(stats.token_mix.cache_hit_percent, Some(77.0));
        assert!(decimal(&stats.token_mix.cache_savings_usd).unwrap() > rust_decimal::Decimal::ZERO);

        assert_eq!(stats.tracks[0].id, "gemini");
        assert_eq!(stats.tracks[0].burst_5h.requests, 1);
        assert_eq!(stats.tracks[0].weekly_7d.requests, 2);
        assert_eq!(
            stats.tracks[0]
                .live_quota
                .as_ref()
                .and_then(|q| q.remaining_percent),
            Some(72.5)
        );
        assert_eq!(stats.tracks[1].id, "claude_gpt");
        assert_eq!(stats.tracks[1].burst_5h.requests, 1);
        assert_eq!(stats.tracks[1].weekly_7d.requests, 1);
        assert_eq!(
            stats.tracks[1]
                .live_quota
                .as_ref()
                .and_then(|q| q.remaining_percent),
            Some(60.0)
        );

        assert_eq!(stats.families.len(), 2);
        assert_eq!(stats.top_sessions.len(), 2);
        assert_eq!(stats.top_sessions[0].short_hash.len(), 8);
        assert!(!stats.top_sessions[0].session_id.contains("session-"));

        // Verify session_id filter isolates logs cleanly in both v11 and v12.
        let alpha_logs = store
            .logs(&Filter {
                client: Some("antigravity".into()),
                session_id: Some(safe_id("session-alpha")),
                ..Filter::default()
            })
            .unwrap();
        assert_eq!(alpha_logs.total, 2);
    }
}
