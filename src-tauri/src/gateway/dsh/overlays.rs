//! Only existing home overrides participate in the shared profile transaction.
use super::*;

fn configs(raw: &str, settings: bool) -> Result<(YamlFile, BTreeMap<String, Mapping>)> {
    let file = YamlFile::from_str(raw).map_err(|_| error("DSH 全局覆盖 YAML 无效"))?;
    let doc = yaml(raw)?;
    let mut configs = BTreeMap::new();
    if settings {
        let root = file
            .document()
            .and_then(|d| d.as_mapping())
            .ok_or_else(|| error("DSH settings 必须是映射"))?;
        for id in [PI, DEFAULT] {
            if root.contains_key(id) {
                configs.insert(
                    id.into(),
                    root.get_mapping(id)
                        .ok_or_else(|| error("DSH settings 覆盖无法静态解析"))?,
                );
            }
        }
    } else {
        let seq = file
            .document()
            .and_then(|d| d.as_sequence())
            .ok_or_else(|| error("DSH home patch 必须是数组"))?;
        for id in [PI, DEFAULT] {
            if let Some((_, row)) = find_row(&seq, id)? {
                if row
                    .get("disabled")
                    .is_some_and(|v| v.to_bool() != Some(false))
                {
                    return Err(error("DSH home 覆盖禁用了模型插件，请先处理"));
                }
                if row.contains_key("config") {
                    configs.insert(
                        id.into(),
                        row.get_mapping("config")
                            .ok_or_else(|| error("DSH home 配置无法静态解析"))?,
                    );
                }
            }
        }
    }
    drop(doc);
    Ok((file, configs))
}

pub(super) fn prepare(
    input: &mut Input,
    options: &Options,
    route: &Mapping,
    previous: Option<&Snapshot>,
) -> Result<()> {
    if let Some(previous) = previous.filter(|p| p.active) {
        check(previous, input)?;
    }
    for (change, settings) in [(&mut input.home_patch, false), (&mut input.settings, true)] {
        let Some(change) = change else { continue };
        let (file, configs) = configs(change.before.as_deref().unwrap_or_default(), settings)?;
        if let Some(pi) = configs.get(PI) {
            let providers = mapping(pi, "providers")?;
            if providers.contains_key(ROUTE) && previous.is_none_or(|p| !p.active) {
                return Err(error("DSH 全局覆盖已有同名路由，请先处理"));
            }
            providers.set(ROUTE, route.clone());
        }
        if let Some(default) = configs.get(DEFAULT).filter(|_| options.make_default) {
            default.set("provider", ROUTE);
            if previous.is_none_or(|p| !p.active) || options.model.is_some() {
                default.set(
                    "model",
                    options
                        .model
                        .as_deref()
                        .ok_or_else(|| error("请选择默认模型"))?,
                );
            }
        }
        change.after = file.to_string();
    }
    Ok(())
}

fn reconcile(raw: &str, snapshot: &Change, settings: bool, restore: bool) -> Result<String> {
    let (file, current) = configs(raw, settings)?;
    let (_, applied) = configs(&snapshot.after, settings)?;
    let (_, original) = configs(snapshot.before.as_deref().unwrap_or_default(), settings)?;
    for id in [PI, DEFAULT] {
        let Some(after) = applied.get(id) else {
            if current.contains_key(id) {
                return Err(conflict());
            }
            continue;
        };
        let current = current.get(id).ok_or_else(conflict)?;
        let before = original.get(id);
        if id == PI {
            let route = |map: &Mapping| map.get_mapping("providers").and_then(|m| m.get(ROUTE));
            check_route(route(current), route(after))?;
            if restore {
                restore_field(
                    &mapping(current, "providers")?,
                    ROUTE,
                    before.and_then(route),
                );
            }
        } else {
            for key in ["provider", "model"] {
                if !equal(before.and_then(|c| c.get(key)), after.get(key)) {
                    if !equal(current.get(key), after.get(key)) {
                        if key == "model" {
                            continue;
                        }
                        return Err(conflict_field("全局默认供应商"));
                    }
                    if restore {
                        restore_field(current, key, before.and_then(|c| c.get(key)));
                    }
                }
            }
        }
    }
    Ok(file.to_string())
}

pub(super) fn check(snapshot: &Snapshot, input: &Input) -> Result<()> {
    for (before, current, settings) in [
        (&snapshot.home_patch, &input.home_patch, false),
        (&snapshot.settings, &input.settings, true),
    ] {
        if let Some(before) = before {
            let current = current
                .as_ref()
                .and_then(|c| c.before.as_deref())
                .ok_or_else(conflict)?;
            reconcile(current, before, settings, false)?;
        } else if let Some(current) = current {
            if !configs(&current.after, settings)?.1.is_empty() {
                return Err(conflict());
            }
        }
    }
    Ok(())
}

pub(super) fn restore(snapshot: &Snapshot, input: &mut Input) -> Result<()> {
    for (snapshot, current, settings) in [
        (&snapshot.home_patch, &mut input.home_patch, false),
        (&snapshot.settings, &mut input.settings, true),
    ] {
        if let (Some(snapshot), Some(current)) = (snapshot, current) {
            current.after = reconcile(
                current.before.as_deref().unwrap_or_default(),
                snapshot,
                settings,
                true,
            )?;
        }
    }
    Ok(())
}
