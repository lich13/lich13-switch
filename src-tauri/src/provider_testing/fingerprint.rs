use super::*;
use futures_util::{stream::FuturesUnordered, StreamExt};
use std::time::Duration;

impl Service {
    pub(super) async fn run_fingerprint(&self, id: &str, context: &Context) -> Result<()> {
        let pending: Vec<_> = self
            .detail(id)?
            .job
            .groups
            .iter()
            .filter(|group| group.status == "pending")
            .map(|group| group.index)
            .collect();
        let concurrency = context.request.concurrency;
        let permits = Arc::new(Semaphore::new(concurrency));
        let mut running = FuturesUnordered::new();
        for index in pending {
            let permits = permits.clone();
            running.push(async move {
                let _permit = permits
                    .acquire_owned()
                    .await
                    .map_err(|_| failure("测试已停止"))?;
                self.fingerprint_group(id, context, index).await?;
                Ok::<_, AppError>(index)
            });
        }
        while let Some(result) = running.next().await {
            result?;
            let detail = self.detail(id)?;
            let outputs: Vec<_> = detail
                .job
                .groups
                .iter()
                .filter(|group| group.status == "completed")
                .filter_map(|group| {
                    Some((
                        group.expected,
                        detail.output.as_ref()?.groups.get(&group.index)?.clone(),
                    ))
                })
                .collect();
            let mut report = score(context, outputs).await?;
            if !report.is_null() {
                let complete = detail
                    .job
                    .groups
                    .iter()
                    .filter(|g| g.status == "completed")
                    .count()
                    == context.sample_count;
                report["disposition"] = if complete {
                    downgrade::verdict(&detail.job.forwarded_model, &report)
                } else {
                    "unknown"
                }
                .into();
                report["ambiguous"] = json!(
                    report["disposition"] == "unknown"
                        && compat::string(&report["prediction_name"]).contains('/')
                );
                report["planned_outputs"] = context.sample_count.into();
                report["initial"] =
                    json!(context.engine == Engine::LmDetector && context.sample_count == 1);
            }
            self.update(id, |job, _| {
                job.report = (!report.is_null()).then_some(report);
            })?;
        }
        if context.request.tokenizer && self.detail(id)?.job.tokenizer_result.is_none() {
            if let Err(error) = self.run_tokenizer(id, context).await {
                self.update(id, |job, _| {
                    job.tokenizer_result = Some(json!({"status":"failed","error":error.message}))
                })?;
            }
        }
        let detail = self.detail(id)?;
        if detail
            .job
            .report
            .as_ref()
            .is_some_and(downgrade::valid_report)
        {
            Ok(())
        } else {
            Err(failure("没有足够的有效指纹样本，请查看逐组诊断"))
        }
    }
    async fn fingerprint_group(&self, id: &str, context: &Context, index: usize) -> Result<()> {
        let (expected, prompt) = &context.challenges[index];
        let target = target(context)?;
        let body = if context.engine == Engine::LmDetector {
            target
                .lm_payload(prompt)
                .map_err(|_| failure("指纹请求构造失败"))?
        } else {
            target.payload(prompt)
        };
        let client = target
            .client()
            .map_err(|_| failure("模型测试连接初始化失败"))?;
        for attempt in 0..3 {
            self.gateway(context.target.client_id)
                .verify_test_version(&context.target.provider_id, &context.target.version)?;
            self.gateway(context.target.client_id)
                .test_endpoint(&context.target.provider_id, Some(&context.target.base_url))?;
            self.update(id, |job, output| {
                output.group_errors.remove(&index);
                let group = &mut job.groups[index];
                group.status = "running".into();
                group.attempts += 1;
                group.error = None;
            })?;
            let started = Instant::now();
            let mut diagnostics = json!({});
            let service = self.clone();
            let job_id = id.to_owned();
            let callback: fingerprint_stream::Progress = Arc::new(move |progress| {
                let service = service.clone();
                let id = job_id.clone();
                Box::pin(async move {
                    service.progress(&id, |job, output| {
                        if job.first_byte_ms.is_none() {
                            job.first_byte_ms = progress["ttft_ms"].as_u64();
                        }
                        if let Some(text) = progress["text"].as_str() {
                            output.groups.insert(index, text.into());
                        }
                        if let Some(model) = progress["returned_model"].as_str() {
                            job.groups[index].returned_model = Some(model.into());
                        }
                        if let Some(diag) = progress.get("diagnostics") {
                            job.groups[index].diagnostics = diag.clone();
                        }
                    });
                    Ok(())
                })
            });
            let result = tokio::time::timeout(
                Duration::from_secs(180),
                fingerprint_stream::execute_body(
                    &target,
                    &body,
                    if context.engine == Engine::ModelTrace {
                        *expected
                    } else {
                        0
                    },
                    &client,
                    Some(callback),
                    &mut diagnostics,
                    false,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(fingerprint_stream::Failure::new("timeout", false)));
            let elapsed = started.elapsed().as_millis() as u64;
            match result {
                Ok((output, returned)) => {
                    let report = score(context, vec![(*expected, output.clone())]).await?;
                    let valid = downgrade::valid_report(&report);
                    self.update(id, |job, result| {
                        let group = &mut job.groups[index];
                        group.status = if valid { "completed" } else { "failed" }.into();
                        group.elapsed_ms = elapsed;
                        group.returned_model = returned.clone();
                        group.diagnostics = diagnostics;
                        group.report = (!report.is_null()).then_some(report);
                        group.retryable = !valid;
                        group.uncertain = false;
                        group.error = (!valid).then(|| {
                            if context.engine == Engine::ModelTrace {
                                "有效整数不足 80 个，无法判定指纹"
                            } else {
                                "样本未通过 LM Detector 校验，请查看评分诊断"
                            }
                            .into()
                        });
                        if returned.is_some() {
                            job.returned_model = returned;
                        }
                        result.groups.insert(index, output);
                    })?;
                    return Ok(());
                }
                Err(error) => {
                    self.update(id, |job, output| {
                        if let Some(detail) = error.detail.clone() {
                            output
                                .group_errors
                                .insert(index, errors::redact(detail, &context.target));
                        }
                        let group = &mut job.groups[index];
                        group.status = "failed".into();
                        group.elapsed_ms = elapsed;
                        group.diagnostics = diagnostics;
                        group.retryable = error.retryable && !error.uncertain;
                        group.uncertain = error.uncertain;
                        group.error = Some(error_message(error.code).into());
                    })?;
                    if error.code == "client_restricted" {
                        return Err(failure(errors::CLIENT_RESTRICTED));
                    }
                    if !error.retryable || error.uncertain || attempt == 2 {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_secs(if attempt == 0 { 1 } else { 3 })).await;
                }
            }
        }
        Ok(())
    }
    pub(super) fn apply_degradation(&self, id: &str) {
        if !self.automatic_current(id) {
            return;
        }
        let Ok(detail) = self.detail(id) else {
            return;
        };
        let job = detail.job;
        if job
            .groups
            .iter()
            .filter(|g| g.status == "completed")
            .count()
            != job.sample_count
        {
            return;
        }
        let Some(report) = job.report.as_ref().filter(|r| {
            r["used_outputs"].as_u64() == Some(job.sample_count as u64)
                && downgrade::verdict(&job.forwarded_model, r) == "downgrade"
        }) else {
            return;
        };
        let mark = Degradation {
            provider_id: job.provider_id.clone(),
            job_id: id.into(),
            detected_at: self.now(),
            requested_model: job.forwarded_model.clone(),
            predicted_model: compat::string(&report["prediction"]).into(),
            probability: report["probability"].as_f64(),
            engine: job.engine,
            queue_removed: false,
            error: None,
        };
        let mut data = lock(&self.0.data);
        if data
            .contexts
            .get(id)
            .and_then(|c| c.detection_revision.as_deref())
            != Some(config_revision(&data.saved, job.client_id, &job.provider_id).as_str())
            || data.cancel.get(id).is_some_and(|cancel| *cancel.borrow())
        {
            return;
        }
        let mut next = data.saved.clone();
        next.degraded
            .insert(scope_key(job.client_id, &job.provider_id), mark);
        if let Err(error) = self.save(&mut data, next) {
            data.error = Some(error.message);
            return;
        }
        let result = self
            .gateway(job.client_id)
            .dequeue_degraded(&job.provider_id, &job.provider_version);
        let mut next = data.saved.clone();
        if let Some(mark) = next
            .degraded
            .get_mut(&scope_key(job.client_id, &job.provider_id))
        {
            mark.queue_removed = result.is_ok();
            mark.error = result.err().map(|e| e.message);
        }
        if let Err(error) = self.save(&mut data, next) {
            data.error = Some(error.message);
        }
    }
}
pub(super) fn error_message(code: &str) -> &'static str {
    match code {
        "client_restricted" => errors::CLIENT_RESTRICTED,
        "auth_or_quota" => "认证失败或额度不足",
        "rate_limited" => "上游限流",
        "invalid_model_or_request" => "模型或请求参数被上游拒绝",
        "timeout" => "等待超时，结果未知，不自动重放",
        "network_error" => "连接中断，结果未知，不自动重放",
        "incomplete_stream" => "响应未完整结束，结果未知",
        "invalid_stream" | "protocol_mismatch" => "上游返回协议不匹配或数据无效",
        "output_limit" => "上游输出超出测试限制",
        _ => "上游模型测试失败",
    }
}

pub(super) fn target(context: &Context) -> Result<fingerprint_stream::Target> {
    let path = match context.target.protocol.as_str() {
        "responses" => "/v1/responses",
        "messages" => "/v1/messages",
        "chat_completions" => "/v1/chat/completions",
        _ => return Err(failure("检测协议不受支持")),
    };
    Ok(fingerprint_stream::Target {
        url: context.target.url(path)?,
        headers: context.target.headers.clone(),
        proxy: None,
        model: context.target.model(&context.request.model),
        oauth: false,
        protocol: context.target.protocol.clone(),
        client_metadata: context.target.codex_metadata(),
    })
}
async fn score(context: &Context, outputs: Vec<(usize, String)>) -> Result<Value> {
    let lm = context.lm.clone();
    let bank = context.bank.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(resource) = lm {
            resource.analyze(
                &outputs
                    .into_iter()
                    .map(|(count, text)| lmfpd::Output {
                        text,
                        expected_count: count as u64,
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            modeltrace::analyze(
                &outputs
                    .into_iter()
                    .map(|(_, text)| text)
                    .collect::<Vec<_>>(),
                &bank,
            )
        }
    })
    .await
    .map_err(|_| failure("指纹评分失败"))?
}
