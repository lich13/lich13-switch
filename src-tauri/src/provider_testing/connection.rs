use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use reqwest::multipart::{Form, Part};
use std::time::Duration;

const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const TEXT_LIMIT: usize = 256 * 1024;
pub(super) fn input_bytes(media: &MediaInput) -> Result<Vec<u8>> {
    if media.data_url.len() > 28 * 1024 * 1024
        || !matches!(
            media.mime.as_str(),
            "image/png"
                | "image/jpeg"
                | "image/webp"
                | "image/gif"
                | "audio/wav"
                | "audio/x-wav"
                | "audio/mpeg"
                | "audio/mp4"
                | "audio/webm"
                | "audio/ogg"
                | "audio/flac"
        )
    {
        return Err(failure("素材类型不支持或超过 20 MiB"));
    }
    let encoded = media
        .data_url
        .strip_prefix(&format!("data:{};base64,", media.mime))
        .ok_or_else(|| failure("素材格式无效"))?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| failure("素材编码无效"))?;
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
        return Err(failure("素材为空或超过 20 MiB"));
    }
    Ok(bytes)
}
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
fn text_model(model: &str) -> bool {
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
pub fn default_model(client: ClientId, mode: Mode) -> &'static str {
    match (client, mode) {
        (ClientId::Grok, Mode::Image) => "grok-imagine-image-quality",
        (ClientId::Grok, Mode::Video) => "grok-imagine-video",
        (ClientId::Grok, Mode::Tts) => "grok-voice-tts",
        (ClientId::Grok, Mode::Stt) => "grok-stt",
        (ClientId::Grok, Mode::Realtime) => "grok-voice",
        (ClientId::Grok, _) => "grok-4.1-fast",
        (ClientId::Claude, _) => "claude-sonnet-4-5",
        (ClientId::Dsh, _) => "deepseek-v4-pro",
        (_, Mode::Image) => "gpt-image-1",
        _ => downgrade::DEFAULT_TEST_MODEL,
    }
}
impl Service {
    pub async fn models(
        &self,
        client_id: ClientId,
        provider: &str,
        expected: &str,
        fingerprint: bool,
    ) -> Result<Models> {
        let target = self
            .gateway(client_id)
            .test_target(provider, Some(expected))?;
        let key = format!("{client_id:?}:{}:{}", provider, target.version);
        let cached = lock(&self.0.data).models.get(&key).cloned();
        if let Some((at, models)) = &cached {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(filter_models(models.clone(), fingerprint));
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
            if models.is_empty() {
                return Err(failure("上游模型目录为空"));
            }
            let default = default_model(client_id, Mode::Text);
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
                Ok(filter_models(models, fingerprint))
            }
            Err(error) => match cached.filter(|(at, _)| at.elapsed() < Duration::from_secs(300)) {
                Some((_, mut models)) => {
                    models.stale = true;
                    models.error = Some(error.message);
                    Ok(filter_models(models, fingerprint))
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
        let request = &context.request;
        let target = &context.target;
        if request.mode == Mode::Realtime {
            return self.realtime(id, context).await;
        }
        let model = target.model(&request.model);
        let client = client()?;
        let (path, payload) = payload(request, &model, &target.protocol)?;
        let mut builder = client
            .post(target.url(&path)?)
            .headers(target.headers.clone());
        if request.mode == Mode::Stt {
            let (bytes, mime) = match &request.media {
                Some(media) => (input_bytes(media)?, media.mime.as_str()),
                None => (silent_wav(), "audio/wav"),
            };
            let extension = match mime {
                "audio/mpeg" => "mp3",
                "audio/mp4" => "m4a",
                "audio/webm" => "webm",
                "audio/ogg" => "ogg",
                "audio/flac" => "flac",
                _ => "wav",
            };
            let part = Part::bytes(bytes)
                .file_name(format!("probe.{extension}"))
                .mime_str(mime)
                .map_err(|_| failure("音频格式无效"))?;
            builder = builder.multipart(
                Form::new()
                    .part("file", part)
                    .text("model", model.clone())
                    .text("language", "en"),
            );
        } else if request.mode == Mode::Image
            && request.client_id == ClientId::Codex
            && request.media.is_some()
        {
            let media = request.media.as_ref().unwrap();
            let part = Part::bytes(input_bytes(media)?)
                .file_name("input.png")
                .mime_str(&media.mime)
                .map_err(|_| failure("图像格式无效"))?;
            builder = builder.multipart(
                Form::new()
                    .part("image", part)
                    .text("model", model.clone())
                    .text("prompt", prompt(request))
                    .text("n", "1"),
            );
        } else {
            builder = builder.json(&payload);
        }
        let started = Instant::now();
        let mut response = builder
            .send()
            .await
            .map_err(|_| failure("供应商连接失败或超时，请检查地址与网络"))?;
        self.progress(id, |job, output| {
            job.first_byte_ms = Some(started.elapsed().as_millis() as u64);
            output.diagnostics =
                json!({"httpStatus":response.status().as_u16(),"protocol":target.protocol});
        });
        if request.mode == Mode::Tts && matches!(response.status().as_u16(), 400 | 422) {
            // Only definite validation rejection permits the reference payload fallback.
            let _ = bounded(response, TEXT_LIMIT).await?;
            response = client
                .post(target.url(&path)?)
                .headers(target.headers.clone())
                .json(&json!({"text":prompt(request),"language":"en"}))
                .send()
                .await
                .map_err(|_| failure("语音测试连接失败"))?;
            if matches!(response.status().as_u16(), 400 | 422) {
                let _ = bounded(response, TEXT_LIMIT).await?;
                response = client
                    .post(target.url(&path)?)
                    .headers(target.headers.clone())
                    .json(&json!({"text":prompt(request),"language":"English","voice_id":"Ara"}))
                    .send()
                    .await
                    .map_err(|_| failure("语音测试连接失败"))?;
            }
        }
        if !response.status().is_success() {
            return Err(failure(&format!(
                "供应商返回 HTTP {}，请检查该模型、协议及访问权限",
                response.status().as_u16()
            )));
        }
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .to_owned();
        if mime.starts_with("audio/") || mime.starts_with("image/") || mime.starts_with("video/") {
            let bytes = bounded(response, OUTPUT_LIMIT).await?;
            return self.media_bytes(id, &mime, &bytes);
        }
        let data = if mime == "text/event-stream" {
            self.stream(id, response, request.mode == Mode::Image)
                .await?
        } else {
            serde_json::from_slice(&bounded(response, OUTPUT_LIMIT).await?)
                .map_err(|_| failure("供应商返回的测试结果不是有效 JSON"))?
        };
        reject_error(&data)?;
        if request.mode == Mode::Video {
            return self.video(id, &client, target, data).await;
        }
        self.result_value(id, &data);
        self.media_value(id, &client, target, &data).await?;
        if request.mode == Mode::Compact
            && self
                .detail(id)?
                .output
                .is_none_or(|o| o.diagnostics["compaction"] != true)
        {
            return Err(failure(
                "上游没有返回 compaction 项，原生 Compact 测试未通过",
            ));
        }
        if request.mode == Mode::Search
            && request.client_id == ClientId::Grok
            && self
                .detail(id)?
                .output
                .is_none_or(|o| o.diagnostics["webSearchObserved"] != true)
        {
            return Err(failure("未观察到网络搜索工具调用或来源，搜索测试未通过"));
        }
        if request.mode == Mode::Image && self.detail(id)?.output.is_none_or(|o| o.media.is_empty())
        {
            return Err(failure("图像接口未返回可展示的图像"));
        }
        if request.mode == Mode::Tts && self.detail(id)?.output.is_none_or(|o| o.media.is_empty()) {
            return Err(failure("语音接口未返回可播放的音频"));
        }
        if request.mode == Mode::Text && self.detail(id)?.output.is_none_or(|o| o.text.is_empty()) {
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
            let response = value.get("response").unwrap_or(value);
            for item in response["output"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(value.get("item"))
            {
                if item["type"] == "compaction" {
                    output.diagnostics["compaction"] = json!(true);
                }
                if item["type"] == "web_search_call" {
                    output.diagnostics["webSearchObserved"] = json!(true);
                }
            }
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
                } else if value.get("results").is_some() || value.get("output").is_some() {
                    output.text = serde_json::to_string_pretty(value)
                        .unwrap_or_default()
                        .chars()
                        .take(TEXT_LIMIT)
                        .collect();
                }
            }
        });
    }
    async fn stream(
        &self,
        id: &str,
        mut response: reqwest::Response,
        image: bool,
    ) -> Result<Value> {
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        let mut last = Value::Null;
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
            if buffer.len() > if image { OUTPUT_LIMIT } else { 2 * 1024 * 1024 } {
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
                            Some(
                                "response.completed"
                                    | "response.done"
                                    | "message_stop"
                                    | "image_generation.completed"
                                    | "image_edit.completed"
                            )
                        ) || value["choices"][0]["finish_reason"].as_str().is_some();
                        last = value;
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
        Ok(last)
    }
    fn media_bytes(&self, id: &str, mime: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty()
            || bytes.len() > OUTPUT_LIMIT
            || !mime
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/-+._".contains(&c))
        {
            return Err(failure("媒体响应为空、格式无效或过大"));
        }
        let kind = mime.split('/').next().unwrap_or("");
        if !["image", "audio", "video"].contains(&kind) {
            return Err(failure("媒体类型不支持"));
        }
        self.progress(id, |_, output| {
            output.media.push(Media {
                kind: kind.into(),
                mime: mime.into(),
                data_url: format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
            })
        });
        Ok(())
    }
    async fn media_value(
        &self,
        id: &str,
        client: &reqwest::Client,
        target: &Target,
        data: &Value,
    ) -> Result<()> {
        let mut media = data["data"].as_array().cloned().unwrap_or_default();
        if data.get("b64_json").is_some() {
            media.push(data.clone());
        }
        let response = data.get("response").unwrap_or(data);
        if let Some(output) = response["output"].as_array() {
            for item in output
                .iter()
                .filter(|item| item["type"] == "image_generation_call")
            {
                if let Some(result) = item["result"].as_str() {
                    media.push(json!({"b64_json":result}));
                }
            }
        }
        if data.get("video").is_some() {
            media.push(data["video"].clone());
        }
        if data.get("audio").is_some() {
            media.push(data["audio"].clone());
        }
        for item in media.into_iter().take(4) {
            let mime = item["mime_type"]
                .as_str()
                .or_else(|| item["content_type"].as_str())
                .unwrap_or(if data.get("video").is_some() {
                    "video/mp4"
                } else if data.get("audio").is_some() {
                    "audio/mpeg"
                } else {
                    "image/png"
                });
            if let Some(raw) = item["b64_json"].as_str().or_else(|| item["data"].as_str()) {
                if raw.len() > OUTPUT_LIMIT * 4 / 3 + 4 {
                    return Err(failure("媒体响应过大"));
                }
                self.media_bytes(
                    id,
                    mime,
                    &STANDARD.decode(raw).map_err(|_| failure("媒体编码无效"))?,
                )?;
            } else if let Some(url) = item["url"].as_str().or_else(|| item.as_str()) {
                if let Some(data) = url.strip_prefix("data:") {
                    let (mime, raw) = data
                        .split_once(";base64,")
                        .ok_or_else(|| failure("媒体数据地址无效"))?;
                    if raw.len() > OUTPUT_LIMIT * 4 / 3 + 4 {
                        return Err(failure("媒体响应过大"));
                    }
                    self.media_bytes(
                        id,
                        mime,
                        &STANDARD.decode(raw).map_err(|_| failure("媒体编码无效"))?,
                    )?;
                    continue;
                }
                let url = url::Url::parse(url).map_err(|_| failure("媒体下载地址无效"))?;
                if !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                {
                    return Err(failure("媒体下载地址无效"));
                }
                let base = url::Url::parse(&target.url("/v1/models")?)
                    .map_err(|_| failure("媒体接口无效"))?;
                let mut request = client.get(url.clone());
                if url.origin() == base.origin() {
                    request = request.headers(target.headers.clone());
                } else {
                    request = request.header("user-agent", target.headers["user-agent"].clone());
                }
                let response = request.send().await.map_err(|_| failure("媒体下载失败"))?;
                if !response.status().is_success() {
                    return Err(failure("媒体下载被拒绝"));
                }
                self.media_bytes(id, mime, &bounded(response, OUTPUT_LIMIT).await?)?;
            }
        }
        Ok(())
    }
    async fn video(
        &self,
        id: &str,
        client: &reqwest::Client,
        target: &Target,
        mut data: Value,
    ) -> Result<()> {
        let request_id = data["request_id"]
            .as_str()
            .or_else(|| data["id"].as_str())
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .map(str::to_owned);
        for _ in 0..90 {
            reject_error(&data)?;
            if data.get("video").is_none() {
                if let Some(url) = ["url", "video_url", "download_url"]
                    .iter()
                    .find_map(|key| data[*key].as_str())
                {
                    data["video"] = json!({"url":url});
                }
            }
            if data.get("video").is_some() || data["data"].as_array().is_some_and(|a| !a.is_empty())
            {
                self.media_value(id, client, target, &data).await?;
                return Ok(());
            }
            if matches!(
                data["status"].as_str(),
                Some("failed" | "error" | "expired" | "canceled" | "cancelled")
            ) {
                return Err(failure("视频生成失败"));
            }
            let request_id = request_id
                .as_deref()
                .ok_or_else(|| failure("视频接口未返回任务编号或视频"))?;
            let mut url = url::Url::parse(&target.url("/v1/videos")?)
                .map_err(|_| failure("视频接口地址无效"))?;
            url.path_segments_mut()
                .map_err(|_| failure("视频接口地址无效"))?
                .push(request_id);
            if matches!(
                data["status"].as_str(),
                Some("completed" | "done" | "succeeded" | "success")
            ) {
                url.path_segments_mut().unwrap().push("content");
                let response = client
                    .get(url)
                    .headers(target.headers.clone())
                    .send()
                    .await
                    .map_err(|_| failure("视频下载失败"))?;
                if !response.status().is_success() {
                    return Err(failure("视频下载被拒绝"));
                }
                return self.media_bytes(id, "video/mp4", &bounded(response, OUTPUT_LIMIT).await?);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let response = client
                .get(url)
                .headers(target.headers.clone())
                .send()
                .await
                .map_err(|_| failure("视频状态查询失败"))?;
            if !response.status().is_success() {
                return Err(failure("视频状态查询被拒绝"));
            }
            data = serde_json::from_slice(&bounded(response, TEXT_LIMIT).await?)
                .map_err(|_| failure("视频状态格式无效"))?;
            self.progress(id, |_, output| {
                output.text = format!("视频状态：{}", compat::clean(&data["status"], 40))
            });
        }
        Err(failure("视频生成等待超时"))
    }
    async fn realtime(&self, id: &str, context: &Context) -> Result<()> {
        let target = &context.target;
        let mut url = url::Url::parse(&target.url("/v1/realtime")?)
            .map_err(|_| failure("Realtime 地址无效"))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| failure("Realtime 地址无效"))?;
        url.query_pairs_mut()
            .append_pair("model", &target.model(&context.request.model));
        let mut request = hyper::Request::builder();
        for (name, value) in &target.headers {
            request = request.header(name, value);
        }
        let started = Instant::now();
        let mut socket = tokio::time::timeout(
            Duration::from_secs(30),
            yawc::WebSocket::connect(url)
                .with_request(request)
                .with_options(yawc::Options::default().with_limits(TEXT_LIMIT, TEXT_LIMIT)),
        )
        .await
        .map_err(|_| failure("Realtime 握手超时"))?
        .map_err(|_| failure("Realtime 握手失败"))?;
        self.progress(id, |job, output| {
            job.first_byte_ms = Some(started.elapsed().as_millis() as u64);
            output.text = "Realtime WebSocket 已连接".into();
        });
        if let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(3), socket.next()).await {
            if frame.opcode() == yawc::OpCode::Text {
                let value: Value = serde_json::from_slice(frame.payload())
                    .map_err(|_| failure("Realtime 初始事件格式无效"))?;
                reject_error(&value)?;
                self.progress(id, |_, output| {
                    output.text = format!(
                        "Realtime WebSocket 已连接\n{}",
                        serde_json::to_string_pretty(&value).unwrap_or_default()
                    )
                });
            }
        }
        Ok(())
    }
}
fn filter_models(mut models: Models, fingerprint: bool) -> Models {
    if fingerprint {
        models.models.retain(|model| text_model(&model.id));
        if !models.models.iter().any(|m| m.id == models.default_model) {
            models.default_model = models
                .models
                .first()
                .map(|m| m.id.clone())
                .unwrap_or_default();
        }
    }
    models
}
fn prompt(request: &Request) -> String {
    if !request.input.trim().is_empty() {
        request.input.clone()
    } else {
        match request.mode {
            Mode::Image | Mode::Video => "A small red balloon floating above a green field".into(),
            Mode::Search => "OpenAI API documentation".into(),
            Mode::Compact => "Respond with OK.".into(),
            Mode::Tts => "Hello, this is a connection test.".into(),
            _ => "Reply with a short greeting.".into(),
        }
    }
}
pub(super) fn payload(request: &Request, model: &str, protocol: &str) -> Result<(String, Value)> {
    let text = prompt(request);
    let image = request
        .media
        .as_ref()
        .filter(|m| m.mime.starts_with("image/"));
    if request.media.is_some() && image.is_none() && request.mode != Mode::Stt {
        return Err(failure("此测试只接受图像素材"));
    }
    let (path, value) = match request.mode {
        Mode::Compact => (
            "/v1/responses",
            json!({"model":model,"instructions":"You are a helpful coding assistant.","input":[{"type":"message","role":"user","content":text},{"type":"compaction_trigger"}],"stream":true,"store":false}),
        ),
        Mode::Image => {
            let mut value = json!({"model":model,"prompt":text,"n":1,"response_format":"b64_json"});
            if let Some(image) = image {
                value["image"] = json!({"url":image.data_url,"type":"image_url"});
            }
            (
                if image.is_some() {
                    "/v1/images/edits"
                } else {
                    "/v1/images/generations"
                },
                value,
            )
        }
        Mode::Video => {
            let mut value = json!({"model":model,"prompt":text,"duration":6,"aspect_ratio":"16:9","resolution":"480p"});
            if let Some(image) = image {
                value["image"] = json!({"url":image.data_url,"type":"image_url"});
            }
            ("/v1/videos/generations", value)
        }
        Mode::Search if request.client_id == ClientId::Codex => (
            "/v1/alpha/search",
            json!({"model":model,"query":text,"max_results":5}),
        ),
        Mode::Search => (
            "/v1/responses",
            json!({"model":model,"input":format!("Search the web for this query and return up to five results with URL, title and snippet as JSON. Use actual search sources. Query: {text}"),"tools":[{"type":"web_search"}],"include":["web_search_call.action.sources"],"store":false,"stream":false}),
        ),
        Mode::Tts => (
            "/v1/tts",
            json!({"text":text,"language":"en","voice_id":"Ara"}),
        ),
        Mode::Stt => ("/v1/stt", Value::Null),
        Mode::Realtime => ("/v1/realtime", Value::Null),
        Mode::Text if protocol == "messages" => {
            let mut content = vec![json!({"type":"text","text":text})];
            if let Some(image) = image {
                content.push(json!({"type":"image","source":{"type":"base64","media_type":image.mime,"data":image.data_url.split_once(',').map(|(_,s)|s).unwrap_or("")}}));
            }
            (
                "/v1/messages",
                json!({"model":model,"max_tokens":256,"messages":[{"role":"user","content":content}],"stream":true}),
            )
        }
        Mode::Text if protocol == "chat_completions" => {
            let mut content = vec![json!({"type":"text","text":text})];
            if let Some(image) = image {
                content.push(json!({"type":"image_url","image_url":{"url":image.data_url}}));
            }
            let mut body =
                json!({"model":model,"messages":[{"role":"user","content":content}],"stream":true});
            body[if request.client_id == ClientId::Dsh {
                "max_tokens"
            } else {
                "max_completion_tokens"
            }] = json!(256);
            ("/v1/chat/completions", body)
        }
        Mode::Text => {
            let mut content = vec![json!({"type":"input_text","text":text})];
            if let Some(image) = image {
                content.push(json!({"type":"input_image","image_url":image.data_url}));
            }
            (
                "/v1/responses",
                json!({"model":model,"input":[{"role":"user","content":content}],"stream":true,"store":false,"max_output_tokens":256}),
            )
        }
    };
    Ok((path.into(), value))
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
fn silent_wav() -> Vec<u8> {
    let data_len = 16000u32;
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((data_len + 36).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(16000u32.to_le_bytes());
    bytes.extend(32000u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(data_len.to_le_bytes());
    bytes.resize(44 + data_len as usize, 0);
    bytes
}
