use super::*;

pub(super) fn migrate(value: &mut Value, gateways: &[Gateway]) -> Result<bool> {
    if value["schema"] == 3 {
        return Ok(false);
    }
    migrate_v1(value, gateways)?;
    let prompts = value.get("connectionPrompts").cloned().unwrap_or(json!({}));
    let mut jobs: Vec<_> = value["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|j| j["kind"] == "connection")
        .collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j["createdAt"].as_i64().unwrap_or(0)));
    let prompt = jobs
        .iter()
        .find_map(|j| {
            let client: ClientId = serde_json::from_value(j["clientId"].clone()).ok()?;
            let key = format!("{client:?}:{}", j["providerId"].as_str()?);
            prompts[&key].as_str().filter(|p| !p.trim().is_empty())
        })
        .unwrap_or(DEFAULT_CONNECTION_PROMPT)
        .to_owned();
    let object = value
        .as_object_mut()
        .ok_or_else(|| failure("测试状态无法读取，已保留原文件"))?;
    for field in ["detection", "detectionTiming", "degraded"] {
        let old = object.remove(field).unwrap_or(json!({}));
        let old = old
            .as_object()
            .ok_or_else(|| failure("检测配置无法迁移，已保留原文件"))?;
        let scoped: serde_json::Map<String, Value> = old
            .iter()
            .map(|(id, v)| (scope_key(ClientId::Codex, id), v.clone()))
            .collect();
        object.insert(field.into(), Value::Object(scoped));
    }
    object.remove("connectionPrompts");
    object.insert("connectionPrompt".into(), prompt.into());
    object.insert("schema".into(), json!(3));
    Ok(true)
}

fn migrate_v1(value: &mut Value, gateways: &[Gateway]) -> Result<bool> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| failure("测试状态无法读取，已保留原文件"))?;
    let mut changed = object.remove("schedules").is_some();
    match object.get("schema").and_then(Value::as_u64).unwrap_or(1) {
        2 => return Ok(changed),
        1 => (),
        _ => return Err(failure("测试状态来自更高版本，请升级应用")),
    }
    let legacy = object.remove("detection").unwrap_or(json!({}));
    let base: Detection = serde_json::from_value(legacy.clone())
        .map_err(|_| failure("检测设置无法读取，已保留原文件"))?;
    let gateway = gateways
        .iter()
        .find(|g| g.client_id() == ClientId::Codex)
        .unwrap();
    let mut configs = std::collections::BTreeMap::new();
    let mut timing = std::collections::BTreeMap::new();
    for provider in gateway
        .test_candidates()?
        .into_iter()
        .filter(|p| !p.search_only)
    {
        let mut config = base.clone();
        let old = &legacy["providers"][&provider.id];
        config.enabled = base.enabled && old["enabled"].as_bool().unwrap_or(true);
        if let Some(model) = old["model"].as_str() {
            config.model = model.into();
        }
        config.endpoint = Some(gateway.test_endpoint(&provider.id, None)?);
        let jobs = object
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(
                object
                    .get("history")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .filter(|j| {
                j["automatic"] == true && j["providerId"] == provider.id && j["clientId"] == "codex"
            })
            .collect::<Vec<_>>();
        timing.insert(
            provider.id.clone(),
            DetectionTiming {
                anchor_at: jobs
                    .iter()
                    .filter(|j| j["startedAt"].is_number())
                    .filter_map(|j| j["completedAt"].as_i64())
                    .max(),
                last_started_at: jobs.iter().filter_map(|j| j["startedAt"].as_i64()).max(),
                paused: !provider.queued,
            },
        );
        configs.insert(provider.id, config);
    }
    object.insert(
        "detection".into(),
        serde_json::to_value(configs).map_err(|_| failure("检测设置迁移失败"))?,
    );
    object.insert(
        "detectionTiming".into(),
        serde_json::to_value(timing).map_err(|_| failure("检测计时迁移失败"))?,
    );
    object.insert("schema".into(), json!(2));
    changed = true;
    Ok(changed)
}
