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
async fn bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(failure("测试返回内容超出上限"));
    }
    let mut raw = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("测试响应中断"))?
    {
        if raw.len() + chunk.len() > limit {
            return Err(failure("测试返回内容超出上限"));
        }
        raw.extend_from_slice(&chunk);
    }
    Ok(raw)
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
    pub(super) fn default_connection_model(&self, target: &Target) -> String {
        let key = format!(
            "{:?}:{}:{}",
            target.client_id, target.provider_id, target.version
        );
        lock(&self.0.data)
            .models
            .get(&key)
            .filter(|(at, models)| {
                at.elapsed() < Duration::from_secs(300) && !models.default_model.is_empty()
            })
            .map(|(_, models)| models.default_model.clone())
            .unwrap_or_else(|| default_model(target.client_id).into())
    }
    pub async fn models(
        &self,
        client_id: ClientId,
        provider: &str,
        expected: &str,
        force_refresh: bool,
    ) -> Result<Models> {
        let target = self
            .gateway(client_id)
            .test_target(provider, Some(expected))?;
        let key = format!("{client_id:?}:{}:{}", provider, target.version);
        let cached = lock(&self.0.data).models.get(&key).cloned();
        if let Some((at, models)) = &cached {
            if !force_refresh && at.elapsed() < Duration::from_secs(60) {
                return Ok(models.clone());
            }
        }
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
                    .map_err(|_| failure("上游模型目录连接失败"))?;
                if !response.status().is_success() {
                    return Err(failure(&format!(
                        "上游模型目录返回 HTTP {}",
                        response.status().as_u16()
                    )));
                }
                let data: Value =
                    serde_json::from_slice(&bounded(response, 8 * 1024 * 1024).await?)
                        .map_err(|_| failure("上游模型目录格式无效"))?;
                let list = data["data"]
                    .as_array()
                    .or_else(|| data["models"].as_array())
                    .ok_or_else(|| failure("上游未返回模型目录"))?;
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
            if models.is_empty() {
                return Err(failure("上游未返回可用的普通对话模型"));
            }
            let default = default_model(client_id);
            let default_model = models
                .iter()
                .find(|m| m.id == default)
                .or_else(|| models.iter().find(|m| text_model(&m.id)))
                .unwrap_or(&models[0])
                .id
                .clone();
            Ok(Models {
                models,
                default_model,
                stale: false,
                error: None,
            })
        }
        .await;
        match result {
            Ok(models) => {
                lock(&self.0.data)
                    .models
                    .insert(key, (Instant::now(), models.clone()));
                Ok(models)
            }
            Err(error) => match cached.filter(|(at, _)| at.elapsed() < Duration::from_secs(300)) {
                Some((_, mut models)) => {
                    models.stale = true;
                    models.error = Some(error.message);
                    Ok(models)
                }
                None => Err(error),
            },
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
        let (path, payload) = payload(&context.request, &model, &target.protocol);
        let started = Instant::now();
        let response = client()?
            .post(target.url(path)?)
            .headers(target.headers.clone())
            .json(&payload)
            .send()
            .await
            .map_err(|_| failure("供应商连接失败或超时，请检查地址与网络"))?;
        self.progress(id, |job, output| {
            job.first_byte_ms = Some(started.elapsed().as_millis() as u64);
            output.diagnostics =
                json!({"httpStatus":response.status().as_u16(),"protocol":target.protocol});
        });
        if !response.status().is_success() {
            return Err(failure(&format!(
                "供应商返回 HTTP {}，请检查该模型、协议及访问权限",
                response.status().as_u16()
            )));
        }
        let streaming = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|mime| mime.split(';').next() == Some("text/event-stream"));
        if streaming {
            self.stream(id, response).await?;
        } else {
            let value: Value = serde_json::from_slice(&bounded(response, OUTPUT_LIMIT).await?)
                .map_err(|_| failure("供应商返回的测试结果不是有效 JSON"))?;
            reject_error(&value)?;
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
            .filter(|s| s.len() <= 256)
            .map(str::to_owned);
        self.progress(id, |job, output| {
            if returned.is_some() {
                job.returned_model = returned;
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
    async fn stream(&self, id: &str, mut response: reqwest::Response) -> Result<()> {
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        let mut events = 0usize;
        let mut terminal = false;
        let mut total = 0usize;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failure("流式测试响应中断"))?
        {
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
                        let value: Value = serde_json::from_str(&data)
                            .map_err(|_| failure("流式测试数据格式无效"))?;
                        reject_error(&value)?;
                        self.result_value(id, &value);
                        events += 1;
                        terminal |= matches!(
                            value["type"].as_str(),
                            Some("response.completed" | "response.done" | "message_stop")
                        ) || value["choices"][0]["finish_reason"].as_str().is_some();
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
pub(super) fn payload(request: &Request, model: &str, protocol: &str) -> (&'static str, Value) {
    let text = "Reply with a short greeting.";
    match protocol {
        "messages" => (
            "/v1/messages",
            json!({"model":model,"max_tokens":256,"messages":[{"role":"user","content":[{"type":"text","text":text}]}],"stream":true}),
        ),
        "chat_completions" => {
            let mut body = json!({"model":model,"messages":[{"role":"user","content":[{"type":"text","text":text}]}],"stream":true});
            body[if request.client_id == ClientId::Dsh {
                "max_tokens"
            } else {
                "max_completion_tokens"
            }] = json!(256);
            ("/v1/chat/completions", body)
        }
        _ => (
            "/v1/responses",
            json!({"model":model,"input":[{"role":"user","content":[{"type":"input_text","text":text}]}],"stream":true,"store":false,"max_output_tokens":256}),
        ),
    }
}
fn reject_error(value: &Value) -> Result<()> {
    if value.get("error").is_some_and(|e| !e.is_null())
        || matches!(
            value["type"].as_str(),
            Some("error" | "response.failed" | "response.incomplete")
        )
        || matches!(value["status"].as_str(), Some("failed" | "incomplete"))
    {
        Err(failure(
            "上游返回测试错误或不完整结果，请检查模型与供应商能力",
        ))
    } else {
        Ok(())
    }
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
