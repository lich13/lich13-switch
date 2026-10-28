use super::*;
use std::time::Duration;

const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const TEXT_LIMIT: usize = 256 * 1024;
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|_| failure("无法创建测试连接"))
}
async fn bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, TestError> {
    let status = response.status().as_u16();
    let mut raw = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = limit - raw.len();
                raw.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if chunk.len() > room {
                    let mut detail =
                        errors::body(Some(status), &String::from_utf8_lossy(&raw), true);
                    detail.message = "测试返回内容超出上限".into();
                    return Err(detail);
                }
            }
            Ok(None) => return Ok(raw),
            Err(error) => {
                let mut detail = errors::body(Some(status), &String::from_utf8_lossy(&raw), true);
                detail.message = format!("测试响应中断：{}", errors::network(error).message);
                return Err(detail);
            }
        }
    }
}
pub(super) fn text_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    ![
        "image",
        "video",
        "tts",
        "stt",
        "realtime",
        "embedding",
        "whisper",
        "moderation",
        "dall-e",
        "audio",
    ]
    .iter()
    .any(|word| model.contains(word))
}
pub fn default_model(client: ClientId) -> &'static str {
    match client {
        ClientId::Grok => "grok-4.1-fast",
        ClientId::Claude => "claude-sonnet-4-5",
        ClientId::Dsh => "deepseek-v4-pro",
        ClientId::Codex => downgrade::DEFAULT_TEST_MODEL,
    }
}
impl Service {
    pub(super) fn models_key(client: ClientId, provider: &str, endpoint: &str) -> String {
        format!(
            "{client:?}:{provider}:{}",
            storage::digest(endpoint.trim_end_matches('/').as_bytes())
        )
    }
    pub(super) fn cached_model(
        data: &Data,
        client: ClientId,
        provider: &str,
        endpoint: &str,
        model: &str,
    ) -> Result<()> {
        let models = data
            .saved
            .models
            .get(&Self::models_key(client, provider, endpoint))
            .ok_or_else(|| failure("尚未缓存该端点模型，请手动重新拉取"))?;
        if !models.models.iter().any(|m| m.id == model) {
            return Err(failure(
                "所选模型不在该端点缓存目录中，请重新选择或手动拉取",
            ));
        }
        Ok(())
    }
    pub(super) fn check_cached_model(&self, target: &Target, model: &str) -> Result<()> {
        Self::cached_model(
            &lock(&self.0.data),
            target.client_id,
            &target.provider_id,
            &target.base_url,
            model,
        )
    }
    pub(super) fn default_connection_model(&self, target: &Target) -> String {
        lock(&self.0.data)
            .saved
            .models
            .get(&Self::models_key(
                target.client_id,
                &target.provider_id,
                &target.base_url,
            ))
            .filter(|models| !models.default_model.is_empty())
            .map(|models| models.default_model.clone())
            .unwrap_or_else(|| default_model(target.client_id).into())
    }
    pub async fn models(
        &self,
        client_id: ClientId,
        provider: &str,
        expected: &str,
        endpoint: Option<&str>,
        force_refresh: bool,
    ) -> Result<Models> {
        let gateway = self.gateway(client_id);
        let endpoint = gateway.test_endpoint(provider, endpoint)?;
        let key = Self::models_key(client_id, provider, &endpoint);
        let cached = lock(&self.0.data)
            .saved
            .models
            .get(&key)
            .cloned()
            .unwrap_or_default();
        if !force_refresh {
            return Ok(cached);
        }
        let target = gateway.test_target_at(provider, Some(expected), Some(&endpoint))?;
        let mut error_detail = None;
        let result = async {
            let client = client()?;
            let mut url = url::Url::parse(&target.url("/v1/models")?)
                .map_err(|_| failure("模型目录地址无效"))?;
            let mut models = Vec::new();
            let mut cursors = std::collections::HashSet::new();
            loop {
                let response = client
                    .get(url.clone())
                    .headers(target.headers.clone())
                    .send()
                    .await
                    .map_err(|error| {
                        let detail = errors::redact(errors::network(error), &target);
                        let result = failure("上游模型目录连接失败");
                        error_detail = Some(detail);
                        result
                    })?;
                if !response.status().is_success() {
                    let detail = errors::redact(errors::response(response).await, &target);
                    let result = failure(&detail.message);
                    error_detail = Some(detail);
                    return Err(result);
                }
                let status = response.status().as_u16();
                let raw = bounded(response, 8 * 1024 * 1024).await.map_err(|detail| {
                    error_detail = Some(errors::redact(detail, &target));
                    failure("读取模型目录失败，详见完整错误信息")
                })?;
                let data: Value = serde_json::from_slice(&raw).map_err(|_| {
                    error_detail = Some(errors::redact(
                        errors::body(Some(status), &String::from_utf8_lossy(&raw), false),
                        &target,
                    ));
                    failure("上游模型目录格式无效")
                })?;
                if is_error(&data) {
                    error_detail = Some(errors::redact(
                        errors::body(Some(status), &data.to_string(), false),
                        &target,
                    ));
                    return Err(failure(
                        if errors::is_client_restricted(&data.to_string()) {
                            errors::CLIENT_RESTRICTED
                        } else {
                            "上游返回模型目录错误"
                        },
                    ));
                }
                let list = data["data"]
                    .as_array()
                    .or_else(|| data["models"].as_array())
                    .ok_or_else(|| {
                        error_detail = Some(errors::redact(
                            errors::body(Some(status), &data.to_string(), false),
                            &target,
                        ));
                        failure("上游未返回模型目录")
                    })?;
                for item in list {
                    let id = item["id"]
                        .as_str()
                        .or_else(|| item["slug"].as_str())
                        .or_else(|| item.as_str());
                    if let Some(id) = id.filter(|s| {
                        !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                    }) {
                        if models.len() >= 10_000 {
                            return Err(failure("上游模型目录超过 10000 项"));
                        }
                        models.push(Model {
                            id: id.into(),
                            name: item["display_name"]
                                .as_str()
                                .or_else(|| item["name"].as_str())
                                .filter(|s| s.len() <= 256)
                                .unwrap_or(id)
                                .into(),
                        });
                    }
                }
                if data["has_more"] != true {
                    break;
                }
                let cursor = data["last_id"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                    .ok_or_else(|| failure("模型目录分页游标无效"))?;
                if !cursors.insert(cursor.to_owned()) {
                    return Err(failure("模型目录分页重复"));
                }
                let pairs: Vec<_> = url
                    .query_pairs()
                    .filter(|(key, _)| key != "after_id")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                url.query_pairs_mut()
                    .clear()
                    .extend_pairs(pairs)
                    .append_pair("after_id", cursor);
            }
            models.sort_by(|a, b| a.id.cmp(&b.id));
            models.dedup_by(|a, b| a.id == b.id);
            models.retain(|model| text_model(&model.id));
            let default = default_model(client_id);
            let default_model = models
                .iter()
                .find(|m| m.id == default)
                .or_else(|| models.first())
                .map(|model| model.id.clone())
                .unwrap_or_default();
            Ok(Models {
                models,
                default_model,
                fetched_at: Some(self.now()),
                error: None,
                error_detail: None,
            })
        }
        .await;
        match result {
            Ok(models) => {
                gateway.verify_test_version(provider, &target.version)?;
                gateway.test_endpoint(provider, Some(&target.base_url))?;
                let mut data = lock(&self.0.data);
                let mut next = data.saved.clone();
                next.models.insert(key, models.clone());
                self.save(&mut data, next)?;
                Ok(models)
            }
            Err(error) => Ok(Models {
                error: Some(error.message),
                error_detail,
                ..cached
            }),
        }
    }
    pub(super) async fn run_connection(&self, id: &str, context: &Context) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(300), self.connection_inner(id, context))
            .await
            .unwrap_or_else(|_| Err(failure("连接测试超时；上游可能仍在处理，请手动确认后重试")))
    }
    async fn connection_inner(&self, id: &str, context: &Context) -> Result<()> {
        let target = &context.target;
        let model = target.model(&context.request.model);

        let (path, mut payload) = payload(&context.request, &model, &target.protocol);
        if let Some(metadata) = target.codex_metadata() {
            payload["client_metadata"] = metadata;
        }
        let started = Instant::now();
        let response = client()?
            .post(target.url(&path)?)
            .headers(target.headers.clone())
            .json(&payload)
            .send()
            .await
            .map_err(|error| self.connection_error(id, errors::network(error), target))?;
        self.progress(id, |job, output| {
            job.first_byte_ms = Some(started.elapsed().as_millis() as u64);
            output.diagnostics =
                json!({"httpStatus":response.status().as_u16(),"protocol":target.protocol});
        });
        if !response.status().is_success() {
            return Err(self.connection_error(id, errors::response(response).await, target));
        }
        let streaming = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|mime| mime.split(';').next() == Some("text/event-stream"));
        if streaming {
            self.stream(id, response, target).await?;
        } else {
            let status = response.status().as_u16();
            let raw = bounded(response, OUTPUT_LIMIT)
                .await
                .map_err(|detail| self.connection_error(id, detail, target))?;
            let value: Value = serde_json::from_slice(&raw).map_err(|_| {
                let mut detail = errors::body(Some(status), &String::from_utf8_lossy(&raw), false);
                detail.message = "供应商返回的测试结果不是有效 JSON".into();
                self.connection_error(id, detail, target)
            })?;
            self.reject_error(id, &value, status, target)?;
            self.result_value(id, &value);
        }
        if self
            .detail(id)?
            .output
            .is_none_or(|output| output.text.trim().is_empty())
        {
            return Err(failure("普通测试未返回文本内容"));
        }
        Ok(())
    }
    fn result_value(&self, id: &str, value: &Value) {
        let returned = value["model"]
            .as_str()
            .or_else(|| value["response"]["model"].as_str())
            .or_else(|| value["message"]["model"].as_str())
            .or_else(|| value["modelVersion"].as_str())
            .filter(|s| s.len() <= 256)
            .map(str::to_owned);
        self.progress(id, |job, output| {
            if returned.is_some() {
                job.returned_model = returned;
            }
            if let Some(parts) = value
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    if part["thought"] != true {
                        if let Some(text) = part["text"].as_str() {
                            append_text(&mut output.text, text);
                        }
                    }
                }
                return;
            }
            let delta = value["delta"]
                .as_str()
                .or_else(|| value["delta"]["text"].as_str())
                .or_else(|| value["choices"][0]["delta"]["content"].as_str());
            if let Some(delta) = delta {
                append_text(&mut output.text, delta);
            } else {
                let text = response_text(value);
                if !text.is_empty() {
                    output.text = text.chars().take(TEXT_LIMIT).collect();
                }
            }
        });
    }
    fn connection_error(&self, id: &str, detail: TestError, target: &Target) -> AppError {
        let restricted = errors::is_client_restricted(&detail.body);
        let detail = errors::redact(detail, target);
        // Only the summary is persisted with the job. The complete response stays in Output.
        let result = failure(if restricted {
            errors::CLIENT_RESTRICTED
        } else if detail.http_status.is_some() {
            "上游返回错误，详见完整错误信息"
        } else {
            "测试连接失败，详见完整错误信息"
        });
        self.progress(id, |_, output| output.error = Some(detail));
        result
    }
    fn reject_error(&self, id: &str, value: &Value, status: u16, target: &Target) -> Result<()> {
        if is_error(value) {
            return Err(self.connection_error(
                id,
                errors::body(Some(status), &value.to_string(), false),
                target,
            ));
        }
        Ok(())
    }
    async fn stream(
        &self,
        id: &str,
        mut response: reqwest::Response,
        target: &Target,
    ) -> Result<()> {
        let status = response.status().as_u16();
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        let mut events = 0usize;
        let mut terminal = false;
        let mut total = 0usize;
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            let mut detail = errors::network(error);
            detail.http_status = Some(status);
            self.connection_error(id, detail, target)
        })? {
            total += chunk.len();
            if total > OUTPUT_LIMIT {
                return Err(failure("流式测试输出超限"));
            }
            buffer.extend_from_slice(&chunk);
            if buffer.len() > 2 * 1024 * 1024 {
                return Err(failure("流式测试单帧超限"));
            }
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    let data = lines.join("\n");
                    lines.clear();
                    if data == "[DONE]" {
                        terminal = true;
                        continue;
                    }
                    if !data.is_empty() {
                        let value: Value = serde_json::from_str(&data).map_err(|_| {
                            self.connection_error(
                                id,
                                errors::body(Some(status), &data, false),
                                target,
                            )
                        })?;
                        self.reject_error(id, &value, status, target)?;
                        self.result_value(id, &value);
                        events += 1;
                        terminal |= matches!(
                            value["type"].as_str(),
                            Some("response.completed" | "response.done" | "message_stop")
                        ) || value["choices"][0]["finish_reason"].as_str().is_some()
                            || value["candidates"][0]["finishReason"].as_str().is_some();
                    }
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    lines.push(
                        std::str::from_utf8(data)
                            .map_err(|_| failure("流式测试编码无效"))?
                            .trim_start_matches(' ')
                            .to_owned(),
                    );
                }
            }
            if terminal {
                break;
            }
        }
        if events == 0 || !terminal {
            return Err(failure("流式测试缺少完成事件，结果不完整"));
        }
        Ok(())
    }
}
pub(super) fn payload(request: &Request, model: &str, protocol: &str) -> (String, Value) {
    let text = request
        .prompt
        .as_deref()
        .unwrap_or(DEFAULT_CONNECTION_PROMPT);
    match protocol {
        "gemini_native" => (
            format!("/v1beta/models/{model}:streamGenerateContent?alt=sse"),
            json!({"contents":[{"role":"user","parts":[{"text":text}]}],"generationConfig":{"maxOutputTokens":256}}),
        ),
        "messages" => (
            "/v1/messages".into(),
            json!({"model":model,"max_tokens":256,"messages":[{"role":"user","content":[{"type":"text","text":text}]}],"stream":true}),
        ),
        "chat_completions" => {
            let mut body = json!({"model":model,"messages":[{"role":"user","content":[{"type":"text","text":text}]}],"stream":true});
            body[if request.client_id == ClientId::Dsh {
                "max_tokens"
            } else {
                "max_completion_tokens"
            }] = json!(256);
            ("/v1/chat/completions".into(), body)
        }
        _ => (
            "/v1/responses".into(),
            json!({"model":model,"input":[{"role":"user","content":[{"type":"input_text","text":text}]}],"stream":true,"store":false,"max_output_tokens":256}),
        ),
    }
}
fn is_error(value: &Value) -> bool {
    if value
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|r| r != "BLOCK_REASON_UNSPECIFIED")
        || value
            .get("candidates")
            .and_then(Value::as_array)
            .is_some_and(|c| {
                c.iter().any(|c| {
                    c["finishReason"].as_str().is_some_and(|r| {
                        !matches!(r, "STOP" | "MAX_TOKENS" | "FINISH_REASON_UNSPECIFIED")
                    })
                })
            })
    {
        return true;
    }
    [value, &value["response"]].iter().any(|value| {
        value.get("error").is_some_and(|e| !e.is_null())
            || matches!(
                value["type"].as_str(),
                Some("error" | "response.failed" | "response.incomplete")
            )
            || matches!(value["status"].as_str(), Some("failed" | "incomplete"))
    })
}

fn append_text(target: &mut String, text: &str) {
    for c in text.chars() {
        if target.len() + c.len_utf8() > TEXT_LIMIT {
            break;
        }
        target.push(c);
    }
}
fn response_text(value: &Value) -> String {
    for candidate in [
        &value["output_text"],
        &value["text"],
        &value["choices"][0]["message"]["content"],
    ] {
        if let Some(s) = candidate.as_str() {
            return s.into();
        }
    }
    let value = value.get("response").unwrap_or(value);
    if let Some(text) = value["output_text"].as_str() {
        return text.into();
    }
    let mut text = String::new();
    if let Some(content) = value["content"].as_array() {
        for block in content {
            if let Some(s) = block["text"].as_str() {
                append_text(&mut text, s);
            }
        }
    }
    if let Some(output) = value["output"].as_array() {
        for item in output {
            if let Some(content) = item["content"].as_array() {
                for block in content {
                    if let Some(s) = block["text"].as_str() {
                        append_text(&mut text, s);
                    }
                }
            }
        }
    }
    text
}
