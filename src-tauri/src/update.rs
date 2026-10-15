use crate::errors::code;
use crate::storage::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use url::Url;

pub const REPOSITORY_URL: &str = "https://github.com/lich13/lich13-switch";
pub const RELEASES_URL: &str = "https://github.com/lich13/lich13-switch/releases";
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/lich13/lich13-switch/releases/latest";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub release_url: String,
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: Option<String>,
    browser_download_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Release {
    tag_name: Option<String>,
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

pub fn normalize_version(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['+', '-'])
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or("0")
        .to_string()
}

fn version_parts(value: &str) -> Vec<u64> {
    normalize_version(value)
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let left = version_parts(left);
    let right = version_parts(right);
    (0..left.len().max(right.len()))
        .map(|index| {
            (
                left.get(index).copied().unwrap_or(0),
                right.get(index).copied().unwrap_or(0),
            )
        })
        .find_map(|(left, right)| (left != right).then(|| left.cmp(&right)))
        .unwrap_or(Ordering::Equal)
}

fn trusted_github_url(value: Option<&str>, fallback: &str, path_prefix: &str) -> String {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return fallback.to_string();
    };
    let Ok(parsed) = Url::parse(value) else {
        return fallback.to_string();
    };
    if parsed.scheme() == "https"
        && parsed.host_str() == Some("github.com")
        && parsed.path().starts_with(path_prefix)
    {
        value.to_string()
    } else {
        fallback.to_string()
    }
}

fn select_asset(assets: &[ReleaseAsset], platform: &str) -> Option<Asset> {
    let preferred = assets.iter().find(|asset| {
        let name = asset
            .name
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let url = asset.browser_download_url.as_deref().unwrap_or_default();
        let valid = !trusted_github_url(Some(url), "", "/lich13/lich13-switch/releases/download/")
            .is_empty();
        if !valid {
            return false;
        }
        match platform {
            "macos" | "darwin" => name.ends_with(".dmg") && !name.contains("x86_64"),
            "windows" | "win32" => name.ends_with(".exe") || name.contains("setup"),
            _ => false,
        }
    });
    let asset = preferred.or_else(|| {
        assets.iter().find(|asset| {
            !trusted_github_url(
                asset.browser_download_url.as_deref(),
                "",
                "/lich13/lich13-switch/releases/download/",
            )
            .is_empty()
        })
    })?;
    let name = asset.name.as_deref()?.trim();
    let url = trusted_github_url(
        asset.browser_download_url.as_deref(),
        "",
        "/lich13/lich13-switch/releases/download/",
    );
    (!name.is_empty() && !url.is_empty()).then(|| Asset {
        name: name.to_string(),
        url,
    })
}

fn build_info(current_version: &str, platform: &str, release: Release) -> Info {
    let latest_version = release
        .tag_name
        .as_deref()
        .map(normalize_version)
        .filter(|value| !value.is_empty());
    let release_url = trusted_github_url(
        release.html_url.as_deref(),
        RELEASES_URL,
        "/lich13/lich13-switch/releases/",
    );
    Info {
        has_update: latest_version
            .as_deref()
            .is_some_and(|latest| compare_versions(latest, current_version) == Ordering::Greater),
        current_version: normalize_version(current_version),
        latest_version,
        release_url,
        asset: select_asset(&release.assets, platform),
    }
}

pub async fn latest() -> Result<Info> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| AppError::new(code::UPDATE, "无法初始化更新检查"))?;
    let response = client
        .get(LATEST_RELEASE_API)
        .header(reqwest::header::USER_AGENT, "lich13-switch")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| AppError::new(code::UPDATE, "无法连接 GitHub"))?;
    if !response.status().is_success() {
        return Err(AppError::new(code::UPDATE, "GitHub 更新检查失败"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(AppError::new(code::UPDATE, "更新信息过大"));
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| AppError::new(code::UPDATE, "无法读取更新信息"))?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(AppError::new(code::UPDATE, "更新信息过大"));
    }
    let release: Release = serde_json::from_slice(&body)
        .map_err(|_| AppError::new(code::UPDATE, "更新信息格式无效"))?;
    Ok(build_info(
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        release,
    ))
}

pub fn open_repository() -> Result<()> {
    open::that(REPOSITORY_URL).map_err(|_| AppError::new(code::OPEN, "无法打开 GitHub"))
}

pub fn open_releases() -> Result<()> {
    open::that(RELEASES_URL).map_err(|_| AppError::new(code::OPEN, "无法打开 GitHub"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_release_versions_without_prefix_or_suffix() {
        assert_eq!(compare_versions("v0.12.1", "0.12.0"), Ordering::Greater);
        assert_eq!(compare_versions("0.12.0-beta.1", "0.12"), Ordering::Equal);
        assert_eq!(compare_versions("0.10", "0.10.0"), Ordering::Equal);
    }

    #[test]
    fn rejects_untrusted_release_urls() {
        assert_eq!(
            trusted_github_url(
                Some("https://example.invalid/download"),
                RELEASES_URL,
                "/lich13/lich13-switch/releases/"
            ),
            RELEASES_URL
        );
    }
}
