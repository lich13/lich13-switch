//! Public Token tariffs and explicit model spellings, independently adapted from
//! Wei-Shaw/sub2api a3298b46ebe239fb0720b068a1eb3383955bdac2.
use super::model::decimal;
use chrono::{Datelike, Timelike};
use rust_decimal::Decimal;
use serde_json::Value;
use std::{collections::BTreeMap, sync::OnceLock};

pub const VERSION: &str = "sub2api-a3298b4";
pub fn catalog() -> &'static BTreeMap<String, Value> {
    static CARDS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();
    CARDS.get_or_init(|| {
        serde_json::from_str(include_str!("../../resources/usage-builtin-prices.json"))
            .expect("bundled Token prices")
    })
}

pub fn aliases() -> &'static BTreeMap<String, String> {
    static ALIASES: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    ALIASES.get_or_init(|| {
        let mut aliases = BTreeMap::new();
        for model in catalog().keys() {
            aliases.insert(model.replace('.', "-"), model.clone());
        }
        for (alias, model) in [
            ("gemini-4-argon-f", "gemini-4-argon"),
            ("gpt-6", "gpt-6-astra"),
            ("gpt6", "gpt-6-astra"),
            ("gpt-5.6", "gpt-5.6-sol"),
            ("gpt-5-6", "gpt-5.6-sol"),
            ("gpt-5.3", "gpt-5.3-codex"),
            ("gpt-5.3-codex-spark", "gpt-5.3-codex"),
            ("glm-5.3flash", "glm-5.3-flash"),
            ("glm-5turbo", "glm-5-turbo"),
            ("glm-4.5x", "glm-4.5-x"),
            ("glm-4.5air", "glm-4.5-air"),
            ("glm-4.5airx", "glm-4.5-airx"),
            ("glm-4-32b", "glm-4-32b-0414-128k"),
            ("minimax-m-2", "minimax-m2"),
            ("gemini-4-argon-cyber", "gemini-4-argon-cyber-permissive"),
            ("deepseek-flash", "deepseek-v4-flash"),
            ("deepseek-v4.1-flash", "deepseek-v4-flash"),
            ("deepseek-chat", "deepseek-v4-flash"),
            ("deepseek-reasoner", "deepseek-v4-flash"),
            ("k3", "kimi-k3"),
            ("k3-256k", "kimi-k3"),
            ("grok", "grok-4.6"),
            ("grok-latest", "grok-4.6"),
            ("grok-4.5-latest", "grok-4.5"),
            ("grok-4.6-latest", "grok-4.6"),
            ("grok-4.7-latest", "grok-4.7"),
            ("grok-4.7-build-fast", "grok-4.7-fast"),
            ("grok-build", "grok-build-0.1"),
            ("grok-build-latest", "grok-build-0.1"),
            ("grok-composer", "grok-build-0.1"),
            ("grok-composer-2.5-fast", "grok-build-0.1"),
            ("composer-2.5", "grok-build-0.1"),
            ("grok-4.20-0309-reasoning", "grok-4.20"),
            ("grok-4.20-0309-non-reasoning", "grok-4.20"),
            ("grok-4.20-multi-agent-0309", "grok-4.20"),
            ("grok-4.20-reasoning", "grok-4.20"),
            ("grok-4.20-non-reasoning", "grok-4.20"),
        ] {
            aliases.insert(alias.into(), model.into());
        }
        for model in [
            "gemini-3.6-flash",
            "gemini-3.7-flash",
            "gemini-3.8-flash",
            "gemini-4-argon",
            "gemini-4-argon-f",
            "claude-opus-5-5",
            "claude-sonnet-5-5",
        ] {
            let target = aliases.get(model).cloned().unwrap_or_else(|| model.into());
            for tier in ["high", "medium", "low"] {
                aliases.insert(format!("{model}-{tier}"), target.clone());
            }
            if model.starts_with("gemini-") {
                aliases.insert(format!("{model}-tiered"), target);
            }
        }
        aliases
    })
}

fn dated(suffix: &str) -> bool {
    let date = suffix.replace('-', "");
    (date.len() == 8 && date.starts_with("20") || matches!(date.len(), 4 | 6))
        && date.bytes().all(|c| c.is_ascii_digit())
}
fn codex_suffix(suffix: &str) -> bool {
    matches!(
        suffix,
        "codex"
            | "codex-high"
            | "codex-low"
            | "codex-medium"
            | "codex-xhigh"
            | "codex-max"
            | "low"
            | "medium"
            | "high"
            | "xhigh"
            | "max"
            | "openai-compact"
    )
}
/// Only published cards and enumerated aliases gain suffix handling. Unknown
/// families never inherit a tariff merely because their names share a keyword.
pub fn canonical(id: &str) -> Option<String> {
    let id = id.trim().to_ascii_lowercase();
    let bare = id.rsplit('/').next()?;
    if matches!(bare, "kimi-k2-0905" | "kimi-k2-0711" | "kimi-k3[1m]") {
        return None;
    }
    if catalog().contains_key(bare) {
        return Some(bare.into());
    }
    if let Some(model) = aliases().get(bare) {
        return Some(model.clone());
    }
    let variants = crate::models_dev::names(&id, true);
    for variant in variants {
        if let Some(model) = aliases().get(&variant) {
            return Some(model.clone());
        }
        if catalog().contains_key(&variant) {
            return Some(variant);
        }
        for (alias, model) in aliases() {
            if variant
                .strip_prefix(&format!("{alias}-"))
                .is_some_and(|suffix| {
                    dated(suffix) || alias.starts_with("gpt-") && codex_suffix(suffix)
                })
            {
                return Some(model.clone());
            }
        }
        for model in catalog().keys() {
            for spelling in [model.clone(), model.replace('.', "-")] {
                if let Some(suffix) = variant.strip_prefix(&format!("{spelling}-")) {
                    let codex = model.starts_with("gpt-") && codex_suffix(suffix);
                    if dated(suffix) || codex {
                        return Some(model.clone());
                    }
                }
            }
        }
    }
    None
}
fn rate(v: &Value, key: &str) -> Option<Decimal> {
    let v = v.get(key)?;
    decimal(v.as_str().unwrap_or(&v.to_string()))
}
fn set(v: &mut Value, key: &str, amount: Decimal) {
    v[key] = Value::String(amount.normalize().to_string());
}

/// Materialize request-time policies in the quote. Persisted snapshots then
/// remain reproducible without reapplying a new clock or a later policy.
pub fn apply(model: &str, data: &mut Value, at: i64) {
    let Some(model) = canonical(model) else {
        return;
    };
    let cache_explicit = data.get("cache_creation_input_token_cost").is_some();
    for field in [
        "cache_creation_input_token_cost",
        "cache_read_input_token_cost",
    ] {
        if data.get(field).is_none() {
            set(data, field, Decimal::ZERO);
        }
    }
    if data["litellm_provider"].as_str() == Some("xai") {
        data["long_context_threshold_inclusive"] = Value::Bool(true);
    }
    if model.starts_with("deepseek-") {
        let card = &catalog()[&model];
        let peak = chrono::DateTime::from_timestamp_millis(at).is_some_and(|utc| {
            let local = utc + chrono::Duration::hours(8);
            local.weekday().number_from_monday() <= 5 && matches!(utc.hour(), 1..=3 | 6..=9)
        });
        let factor = Decimal::from(if peak { 2 } else { 1 });
        for key in [
            "input_cost_per_token",
            "output_cost_per_token",
            "cache_read_input_token_cost",
        ] {
            if let Some(value) = rate(card, key) {
                set(data, key, value * factor);
            }
        }
        data["time_price_multiplier"] = Value::String(factor.to_string());
        return;
    }
    let premium = model.starts_with("gpt-5.6-")
        || matches!(
            model.as_str(),
            "gpt-6-sol" | "gpt-6-luna" | "gpt-6.1-sol" | "gpt-6.1-terra"
        );
    if premium
        && (!cache_explicit || model.starts_with("gpt-5.6-"))
        && rate(data, "cache_creation_input_token_cost").is_none_or(|v| v <= Decimal::ZERO)
    {
        if let Some(input) = rate(data, "input_cost_per_token") {
            set(
                data,
                "cache_creation_input_token_cost",
                input * Decimal::new(125, 2),
            );
        }
    }
    let factor = match model.as_str() {
        "gpt-5.5" => Some(Decimal::new(25, 1)),
        "gpt-5.4" | "gpt-6-astra" | "gpt-6-sol" | "gpt-6-luna" | "gpt-6.1-sol"
        | "gpt-6.1-terra" => Some(Decimal::from(2)),
        _ if premium => Some(Decimal::from(2)),
        _ => None,
    };
    if let Some(factor) = factor {
        for key in [
            "input_cost_per_token",
            "output_cost_per_token",
            "cache_read_input_token_cost",
            "cache_creation_input_token_cost",
        ] {
            if let Some(value) = rate(data, key) {
                set(data, &format!("{key}_priority"), value * factor);
            }
        }
    }
    if model == "gpt-6-astra" {
        data["ultrafast_multiplier"] = Value::String("6".into());
    }
    if model == "claude-opus-5-5" && data.get("fast_multiplier").is_none() {
        data["fast_multiplier"] = Value::String("2".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{model::Tokens, pricing::Quote};
    fn quote(model: &str, at: &str) -> Quote {
        let id = canonical(model).expect("known tariff");
        let mut data = catalog()[&id].clone();
        apply(
            &id,
            &mut data,
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .timestamp_millis(),
        );
        Quote {
            model: id,
            data: Some(data),
            version: VERSION.into(),
            source: "sub2api_builtin".into(),
            multiplier: "1".into(),
        }
    }
    const OFF_PEAK: &str = "2026-10-25T02:00:00Z";
    fn tokens(input: u64, output: u64) -> Tokens {
        Tokens {
            input: Some(input),
            output: Some(output),
            ..Tokens::default()
        }
    }
    #[test]
    fn published_family_amounts_and_explicit_variants() {
        // 1,000 input + 100 output, expected USD from the pinned upstream cards.
        for (model, cost) in [
            ("gemini-4-argon-f", "0.003"),
            ("gemini-4-argon-high", "0.003"),
            ("gemini-4-argon-cyber-permissive", "0.006"),
            ("gemini-3.8-flash", "0.001125"),
            ("claude-opus-5.5", "0.006"),
            ("claude-sonnet-5-5-high", "0.003"),
            ("claude-fable-5-1", "0.015"),
            ("gpt-6", "0.015"),
            ("gpt-6-high", "0.015"),
            ("gpt-5.6-codex-high", "0.008"),
            ("gpt-6.1-sol-codex-high", "0.003"),
            ("gpt-6.1-terra", "0.0015"),
            ("gpt-5.4-mini", "0.0012"),
            ("gpt-5.5-pro", "0.048"),
            ("gpt-5.6-luna", "0.00032"),
            ("grok-4.7-fast", "0.0052"),
            ("grok-4.20-reasoning", "0.0015"),
            ("glm-5.3", "0.00184"),
            ("glm-4.7-flash", "0"),
            ("deepseek-v4-pro-0813", "0.000858"),
            ("deepseek-chat", "0.00021"),
            ("k3-256k", "0.0045"),
            ("minimax-m3", "0.00084"),
            ("jev-latest", "0.000042"),
        ] {
            let mut usage = tokens(1000, 100);
            usage.reasoning = Some(75);
            assert_eq!(
                quote(model, OFF_PEAK).calculate(&usage, None).unwrap().cost,
                cost,
                "{model}"
            );
        }
        for unknown in [
            "gemini-4-unknown",
            "gpt-99",
            "deepseek-future",
            "gemini-4-argon-cyber-unknown",
            "claude-opus-999",
        ] {
            assert!(canonical(unknown).is_none(), "{unknown}");
        }
    }
    #[test]
    fn tiers_ttl_context_boundaries_and_peak_times() {
        let base = quote("gpt-6.1-sol", OFF_PEAK);
        for (n, cost) in [(272000, "0.544"), (272001, "1.088004")] {
            assert_eq!(base.calculate(&tokens(n, 0), None).unwrap().cost, cost);
        }
        assert_eq!(
            base.calculate(&tokens(272001, 0), Some("priority"))
                .unwrap()
                .cost,
            "2.176008"
        );
        assert_eq!(
            base.calculate(&tokens(272001, 0), Some("flex"))
                .unwrap()
                .cost,
            "0.544002"
        );
        assert_eq!(
            quote("gpt-6-astra", OFF_PEAK)
                .calculate(&tokens(1000, 100), Some("ultrafast"))
                .unwrap()
                .cost,
            "0.09"
        );
        assert_eq!(
            quote("gpt-5.5", OFF_PEAK)
                .calculate(&tokens(1000, 100), Some("fast"))
                .unwrap()
                .cost,
            "0.02"
        );
        let ttl = Tokens {
            cache_write: Some(100),
            cache_write_5m: Some(30),
            cache_write_1h: Some(70),
            ..tokens(0, 0)
        };
        assert_eq!(
            quote("claude-opus-5-5", OFF_PEAK)
                .calculate(&ttl, Some("fast"))
                .unwrap()
                .cost,
            "0.00142"
        );
        for (n, cost) in [(100000, "0.01"), (100001, "0.0500005")] {
            assert_eq!(
                quote("claude-haiku-5-5", OFF_PEAK)
                    .calculate(&tokens(n, 0), None)
                    .unwrap()
                    .cost,
                cost
            );
        }
        let grok = quote("grok-4.7", OFF_PEAK);
        let high = grok.calculate(&tokens(200000, 0), None).unwrap();
        assert!(decimal(&high.cost).unwrap() > Decimal::new(4, 1));
        assert_eq!(
            grok.calculate(&tokens(199999, 0), None).unwrap().cost,
            "0.399998"
        );
        for (at, cost) in [
            ("2026-10-26T00:59:59Z", "0.00021"),
            ("2026-10-26T01:00:00Z", "0.00042"),
            ("2026-10-26T04:00:00Z", "0.00021"),
            ("2026-10-26T06:00:00Z", "0.00042"),
            ("2026-10-26T10:00:00Z", "0.00021"),
            (OFF_PEAK, "0.00021"),
        ] {
            let snapshot = quote("deepseek-v4-flash", at)
                .calculate(&tokens(1000, 100), None)
                .unwrap();
            assert_eq!(snapshot.cost, cost, "{at}");
            assert!(snapshot
                .basis
                .unwrap()
                .get("time_price_multiplier")
                .is_some());
        }
    }
    #[test]
    fn image_token_tariffs_use_existing_token_counts() {
        let image = Tokens {
            image_input: Some(400),
            image_output: Some(100),
            ..tokens(1000, 100)
        };
        assert_eq!(
            quote("gpt-image-2.5-flare", OFF_PEAK)
                .calculate(&image, None)
                .unwrap()
                .cost,
            "0.0092"
        );
        assert_eq!(
            quote("doubao-embedding-vision", OFF_PEAK)
                .calculate(
                    &Tokens {
                        image_input: Some(400),
                        ..tokens(1000, 0)
                    },
                    None
                )
                .unwrap()
                .cost,
            "0.0001596"
        );
        assert_eq!(
            quote("gemini-4-argon-f", OFF_PEAK)
                .calculate(&image, None)
                .unwrap()
                .cost,
            "0.003"
        );
        let cached = super::super::model::parse_tokens(
            &serde_json::json!({
                "input_tokens": 1200, "output_tokens": 100,
                "input_tokens_details": {
                    "image_tokens": 500, "cached_tokens": 200,
                    "cached_tokens_details": {"image_tokens": 100}
                },
                "output_tokens_details": {"image_tokens": 100}
            }),
            false,
        );
        assert_eq!(cached.input, Some(1000));
        assert_eq!(cached.image_input, Some(400));
        assert_eq!(cached.image_cache_read, Some(100));
        assert_eq!(cached.total(), Some(1300));
        let snapshot = quote("gpt-image-2.5-flare", OFF_PEAK)
            .calculate(&cached, None)
            .unwrap();
        assert_eq!(snapshot.cost, "0.009525");
        assert_eq!(
            snapshot.rates["cache_read_input_image_token_cost"],
            "0.000002"
        );
        let at = chrono::DateTime::parse_from_rfc3339(OFF_PEAK)
            .unwrap()
            .timestamp_millis();
        let mut record = super::super::model::Record {
            id: "fixture-image-cache".into(),
            source: "proxy".into(),
            client: "codex".into(),
            started_at: at,
            finished_at: Some(at),
            completed: true,
            ..Default::default()
        };
        record.attempts.push(super::super::model::Attempt {
            id: "fixture-image-cache-attempt".into(),
            tokens: cached.clone(),
            price: Some(snapshot),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let mut store = super::super::store::Store::open(dir.path()).unwrap();
        store.write_batch(&[record.clone()], None).unwrap();
        assert_eq!(store.detail(&record.id).unwrap().attempts[0].tokens, cached);
    }
}
