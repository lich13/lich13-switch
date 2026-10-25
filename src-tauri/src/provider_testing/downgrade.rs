//! User-selected model hierarchy; unknown identifiers never imply recovery.
use super::compat::{integer, string};
use regex::Regex;
use serde_json::Value;
use std::cmp::Ordering;
use std::sync::LazyLock;

pub const LEGACY_TARGET: &str = "gpt-5.6-luna";
pub const DEFAULT_TEST_MODEL: &str = LEGACY_TARGET;

pub fn valid_report(report: &Value) -> bool {
    integer(&report["used_outputs"]).is_some_and(|used| used >= 1)
        && !string(&report["prediction"]).trim().is_empty()
        && !matches!(string(&report["decision"]), "unscorable" | "not_model")
}
pub fn normalize(model: &str) -> String {
    let value = model.trim().to_ascii_lowercase();
    let value = value
        .rsplit('/')
        .next()
        .unwrap_or(&value)
        .split(':')
        .next()
        .unwrap_or("");
    static CLAUDE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^claude-(opus|sonnet|haiku|fable)-(\d+)-(\d+)(?:-\d{8})?$").unwrap()
    });
    if let Some(c) = CLAUDE.captures(value) {
        return format!("claude-{}-{}.{}", &c[1], &c[2], &c[3]);
    }
    static OLDER_CLAUDE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^claude-(\d+)-(\d+)-(opus|sonnet|haiku)(?:-\d{8})?$").unwrap()
    });
    if let Some(c) = OLDER_CLAUDE.captures(value) {
        return format!("claude-{}-{}.{}", &c[3], &c[1], &c[2]);
    }
    value.into()
}
fn family(model: &str) -> Option<u8> {
    // A provider-defined alias sharing a prefix is not a known model identity.
    static FAMILIES: LazyLock<Vec<(Regex, u8)>> = LazyLock::new(|| {
        [
            (
                r"^claude-(?:opus|sonnet|haiku|fable)-[0-9]+(?:\.[0-9]+)*(?:-[0-9]{8})?$",
                4,
            ),
            (
                r"^gpt-(?:[0-9]+(?:\.[0-9]+)*(?:-(?:astra|sol|luna|terra))?|4o(?:-mini)?)$",
                3,
            ),
            (r"^grok-[0-9]+(?:\.[0-9]+)*(?:-(?:fast|code))?$", 2),
            (
                r"^deepseek-(?:v[0-9]+(?:\.[0-9]+)*(?:-(?:pro|flash))?|r1)(?:-[0-9]{4,8})?$",
                1,
            ),
        ]
        .into_iter()
        .map(|(pattern, rank)| (Regex::new(pattern).unwrap(), rank))
        .collect()
    });
    FAMILIES
        .iter()
        .find_map(|(pattern, rank)| pattern.is_match(model).then_some(*rank))
}
fn tier(model: &str) -> Option<(String, u8)> {
    static MODEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:gpt-([0-9]+(?:\.[0-9]+)*)-(astra|sol|luna)|claude-(opus|sonnet|haiku)-([0-9]+(?:\.[0-9]+)*)(?:-[0-9]{8})?|grok-([0-9]+(?:\.[0-9]+)*)|deepseek-v([0-9]+(?:\.[0-9]+)*)-(pro|flash)(?:-[0-9]{4,8})?)$").unwrap()
    });
    let c = MODEL.captures(model)?;
    if let Some(v) = c.get(1) {
        return Some((
            v.as_str().into(),
            match &c[2] {
                "astra" => 3,
                "sol" => 2,
                _ => 0,
            },
        ));
    }
    if let Some(v) = c.get(4) {
        return Some((
            v.as_str().into(),
            match &c[3] {
                "opus" => 3,
                "sonnet" => 2,
                _ => 1,
            },
        ));
    }
    if let Some(v) = c.get(5) {
        return Some((v.as_str().into(), 0));
    }
    Some((
        c[6].split('.').take(2).collect::<Vec<_>>().join("."),
        if &c[7] == "pro" { 2 } else { 1 },
    ))
}
fn compare_versions(left: &str, right: &str) -> Ordering {
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        let (a, b) = (left.next(), right.next());
        if a.is_none() && b.is_none() {
            return Ordering::Equal;
        }
        // Compare unsigned decimal segments without a machine-integer limit.
        // Missing trailing segments and all-zero segments are equivalent.
        let a = a.unwrap_or("0").trim_start_matches('0');
        let b = b.unwrap_or("0").trim_start_matches('0');
        let order = a.len().cmp(&b.len()).then_with(|| a.cmp(b));
        if order != Ordering::Equal {
            return order;
        }
    }
}

/// None is unclassified, not a successful recovery check.
pub fn classify(requested: &str, predicted: &str) -> Option<bool> {
    let requested = normalize(requested);
    let predicted = normalize(predicted);
    let (a, b) = (family(&requested)?, family(&predicted)?);
    if a != b {
        return Some(b < a);
    }
    if a == 3 && predicted == LEGACY_TARGET {
        return Some(true);
    }
    if a == 2 {
        static GROK: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^grok-([0-9]+(?:\.[0-9]+)*)(-(?:fast|code))?$").unwrap());
        let (r, p) = (GROK.captures(&requested)?, GROK.captures(&predicted)?);
        if r.get(2).map(|m| m.as_str()) != p.get(2).map(|m| m.as_str()) {
            return None;
        }
        return Some(compare_versions(&p[1], &r[1]) == Ordering::Less);
    }
    let ((rv, rt), (pv, pt)) = (tier(&requested)?, tier(&predicted)?);
    Some(match pt.cmp(&rt) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal if a == 3 && rt == 2 => false,
        Ordering::Equal => compare_versions(&pv, &rv) == Ordering::Less,
    })
}
#[cfg(test)]
pub fn is_downgrade(requested: &str, predicted: &str) -> bool {
    classify(requested, predicted) == Some(true)
}
pub fn verdict(requested: &str, report: &Value) -> &'static str {
    if !valid_report(report) {
        return "unknown";
    }
    let predicted = string(&report["prediction"]);
    // Explicit membership from the pinned bank. A new merged identity is not guessed.
    let display = string(&report["prediction_name"]);
    if display.contains('/') {
        if predicted == "gpt-6-astra" && display == "gpt-6-astra/6.1-sol" {
            return if ["gpt-6-astra", "gpt-6.1-sol"]
                .iter()
                .all(|m| classify(requested, m) == Some(true))
            {
                "downgrade"
            } else {
                "unknown"
            };
        }
        return "unknown";
    }
    match classify(requested, predicted) {
        Some(true) => "downgrade",
        Some(false) => "notDowngrade",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn downgrade_vectors_preserve_legacy_exception_and_rank_tiers_first() {
        let vectors = [
            ("gpt-6.1-sol", "gpt-6-astra", false),
            ("gpt-6-luna", "gpt-5.6-luna", true),
            ("gpt-6-astra", "gpt-5.6-luna", true),
            ("gpt-5.6-luna", "gpt-5.6-luna", true),
            ("gpt-6-astra", "gpt-6-sol", true),
            ("gpt-6-sol", "gpt-6-luna", true),
            ("gpt-6-sol", "gpt-6-astra", false),
            ("gpt-6-sol", "gpt-5.6-sol", false),
            ("gpt-6.1-sol", "gpt-6-sol", false),
            ("gpt-6-sol", "gpt-6.1-sol", false),
            ("gpt-6.1-sol", "gpt-5.6-sol", false),
            ("gpt-5.6-sol", "gpt-6-sol", false),
            ("gpt-6-luna", "gpt-5.9-sol", false),
            ("gpt-6.1-astra", "gpt-6-astra", true),
            ("gpt-6-astra", "gpt-6.0-astra", false),
            ("gpt-5.10-sol", "gpt-5.9-sol", false),
            ("  GPT-6-Sol \t", "gpt-6-sol", false),
            ("gpt-6-sol", "claude-sonnet-5", false),
            ("gpt-6-sol", "", false),
            ("gpt-6-sol", "gpt-5.6-terra", false),
            ("gpt-6-terra", "gpt-6-luna", false),
            ("gpt-6-sol", "gpt-5.5", false),
            ("gpt-6", "gpt-6-luna", false),
            ("gpt-6-unknown", "gpt-6-luna", false),
            ("gpt-6-sol", "gpt-6-unknown", false),
            ("gpt-6-sol", " \n\t", false),
            ("", " GPT-5.6-LUNA ", false),
            ("claude-sonnet-5", "gpt-5.6-luna", true),
            ("gpt-6.0.0-sol", "gpt-06-sol", false),
            ("gpt-6-sol", "gpt-6.0.1-sol", false),
            ("gpt-6.-sol", "gpt-5-sol", false),
            ("gpt-6-sol", "gpt--5-sol", false),
            ("gpt-6-sol", "gpt-５-sol", false),
            ("gpt-6-sol", "prefix-gpt-5-sol", false),
            ("gpt-6-sol", "gpt-5-sol-extra", false),
            (
                "gpt-184467440737095516160-sol",
                "gpt-184467440737095516159-sol",
                false,
            ),
        ];
        for (requested, predicted, expected) in vectors {
            assert_eq!(
                is_downgrade(requested, predicted),
                expected,
                "{requested} / {predicted}"
            );
        }
    }

    #[test]
    fn invalid_reports_are_rejected_without_panicking() {
        for report in [
            Value::Null,
            json!({}),
            json!({"used_outputs":0,"prediction":LEGACY_TARGET}),
            json!({"used_outputs":1,"prediction":" \t"}),
            json!({"used_outputs":1,"prediction":42}),
        ] {
            assert!(!valid_report(&report));
        }
        assert!(valid_report(
            &json!({"used_outputs":1,"prediction":"gpt-6-sol"})
        ));
        assert!(valid_report(
            &json!({"used_outputs":"1","prediction":"gpt-6-sol"})
        ));
    }

    #[test]
    fn agreed_families_aliases_and_merged_candidates() {
        for (requested, predicted, expected) in [
            ("claude-opus-4-8", "claude-sonnet-5", Some(true)),
            ("claude-sonnet-5", "claude-opus-4.6", Some(false)),
            (
                "claude-3-5-sonnet-20241022",
                "claude-sonnet-3.4",
                Some(true),
            ),
            ("claude-opus-4.8", "claude-fable-5", None),
            ("claude-fable-5", "gpt-6-astra", Some(true)),
            ("gpt-6-astra", "grok-4.7", Some(true)),
            ("grok-4.7", "deepseek-v4.1-pro", Some(true)),
            ("deepseek-v4-pro", "gpt-5.6-luna", Some(false)),
            ("grok-4.7", "grok-4.6", Some(true)),
            ("grok-4.7-fast", "grok-4.6-fast", Some(true)),
            ("grok-4.7-fast", "grok-4.6-code", None),
            ("grok-4.7", "grok-4.6-fast", None),
            ("deepseek-v4.1-pro", "deepseek-v4-flash", Some(true)),
            ("deepseek-v4.1-flash", "deepseek-v4-pro", Some(false)),
            ("deepseek-v4.1-flash", "deepseek-v4-flash", Some(true)),
            ("deepseek-v4-pro-0813", "deepseek-v4-pro", Some(false)),
            ("deepseek-v4.1.3-pro", "deepseek-v4.1.2-pro", Some(false)),
            ("claude-custom-alias", "gpt-6-sol", None),
            ("gpt-custom-alias", "grok-4.7", None),
            ("grok-custom-alias", "deepseek-v4-pro", None),
            ("gpt-6-sol", "deepseek-custom-alias", None),
            ("gpt-5.4", "grok-4.7", Some(true)),
            ("router-alias", "gpt-5.6-luna", None),
            ("gpt-6-sol", "qwen-3", None),
        ] {
            assert_eq!(
                classify(requested, predicted),
                expected,
                "{requested} / {predicted}"
            );
        }
        let report = json!({"prediction":"gpt-6-astra","prediction_name":"gpt-6-astra/6.1-sol","used_outputs":1,"probability":null,"decision":"partial"});
        assert_eq!(verdict("claude-sonnet-5", &report), "downgrade");
        assert_eq!(verdict("gpt-6.1-astra", &report), "downgrade");
        assert_eq!(verdict("gpt-6-astra", &report), "unknown");
        assert_eq!(verdict("gpt-6-luna", &report), "unknown");
    }

    #[test]
    fn global_detection_default_matches_reference() {
        assert_eq!(super::super::DEFAULT_DETECTION_MODEL, "gpt-6-luna");
    }
}
