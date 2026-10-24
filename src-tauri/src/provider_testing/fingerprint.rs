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
            let index = result?;
            let detail = self.detail(id)?;
            let outputs: Vec<_> = detail
                .job
                .groups
                .iter()
                .filter(|group| group.status == "completed")
                .filter_map(|group| detail.output.as_ref()?.groups.get(&group.index).cloned())
                .collect();
            let report = modeltrace::analyze(&outputs, &context.bank)?;
            let first = detail
                .job
                .groups
                .first()
                .and_then(|group| group.report.as_ref());
            let downgraded_first = detail.job.automatic
                && index == 0
                && first.is_some_and(|report| {
                    downgrade::valid_report(report)
                        && downgrade::is_downgrade(
                            &context.request.model,
                            compat::string(&report["prediction"]),
                        )
                });
            let early = report["probability"].as_f64().is_some_and(|p| p >= 0.99);
            self.update(id, |job, _| {
                job.report = (!report.is_null()).then_some(report);
                if downgraded_first {
                    job.completion_reason = Some("firstGroupDowngrade".into());
                } else if early {
                    job.completion_reason = Some("confidence99".into());
                }
            })?;
            if downgraded_first || early {
                // Dropping in-flight futures cancels local waits; never replay them.
                drop(running);
                self.update(id, |job, _| {
                    for group in &mut job.groups {
                        if matches!(group.status.as_str(), "running" | "pending") {
                            group.uncertain = group.attempts > 0;
                            group.status = "skipped".into();
                            group.retryable = false;
                        }
                    }
                })?;
                break;
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
        let target = fingerprint_stream::Target {
            url: context.target.url("/v1/responses")?,
            headers: context.target.headers.clone(),
            proxy: None,
            model: context.target.model(&context.request.model),
            oauth: false,
            protocol: context.target.protocol.clone(),
            client_metadata: context.target.codex_metadata(),
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
                fingerprint_stream::execute(
                    &target,
                    prompt,
                    *expected,
                    &client,
                    Some(callback),
                    &mut diagnostics,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(fingerprint_stream::Failure::new("timeout", false)));
            let elapsed = started.elapsed().as_millis() as u64;
            match result {
                Ok((output, returned)) => {
                    let report = modeltrace::analyze(std::slice::from_ref(&output), &context.bank)?;
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
                        group.error = (!valid).then(|| "有效整数不足 80 个，无法判定指纹".into());
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
        let report = job
            .groups
            .first()
            .and_then(|g| g.report.as_ref())
            .filter(|r| {
                downgrade::valid_report(r)
                    && downgrade::is_downgrade(
                        &job.requested_model,
                        compat::string(&r["prediction"]),
                    )
            })
            .or_else(|| {
                job.report.as_ref().filter(|r| {
                    downgrade::valid_report(r)
                        && downgrade::is_downgrade(
                            &job.requested_model,
                            compat::string(&r["prediction"]),
                        )
                })
            });
        let Some(report) = report else {
            return;
        };
        let mark = Degradation {
            provider_id: job.provider_id.clone(),
            job_id: id.into(),
            detected_at: self.now(),
            requested_model: job.requested_model.clone(),
            predicted_model: compat::string(&report["prediction"]).into(),
            probability: report["probability"].as_f64().unwrap_or(0.0),
            queue_removed: false,
            error: None,
        };
        let mut data = lock(&self.0.data);
        if data
            .contexts
            .get(id)
            .and_then(|c| c.detection_revision.as_deref())
            != Some(config_revision(&data.saved, &job.provider_id).as_str())
            || data.cancel.get(id).is_some_and(|cancel| *cancel.borrow())
        {
            return;
        }
        let mut next = data.saved.clone();
        next.degraded.insert(job.provider_id.clone(), mark);
        if let Err(error) = self.save(&mut data, next) {
            data.error = Some(error.message);
            return;
        }
        let result = self
            .gateway(ClientId::Codex)
            .dequeue_degraded(&job.provider_id, &job.provider_version);
        let mut next = data.saved.clone();
        if let Some(mark) = next.degraded.get_mut(&job.provider_id) {
            mark.queue_removed = result.is_ok();
            mark.error = result.err().map(|e| e.message);
        }
        if let Err(error) = self.save(&mut data, next) {
            data.error = Some(error.message);
        }
    }
}
fn error_message(code: &str) -> &'static str {
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
