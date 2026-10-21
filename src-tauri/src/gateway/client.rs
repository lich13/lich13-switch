use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_SHARED_PORT: u16 = 43721;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientId {
    #[default]
    Codex,
    Claude,
    Grok,
    Dsh,
}
impl ClientId {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Grok => "Grok Build",
            Self::Dsh => "DeepSeek Harness",
        }
    }
    pub fn legacy_port(self) -> Option<u16> {
        match self {
            Self::Codex => Some(15722),
            Self::Claude => Some(15723),
            Self::Grok => Some(15724),
            Self::Dsh => None,
        }
    }
    pub fn address(self, port: u16) -> String {
        format!(
            "http://127.0.0.1:{port}{}",
            match self {
                Self::Codex | Self::Grok | Self::Dsh => "/v1",
                Self::Claude => "",
            }
        )
    }
    /// Identification only; old ports do not create additional listeners.
    pub fn legacy_addresses(self, port: u16) -> Vec<String> {
        let prefix = match self {
            Self::Codex => "/codex/v1",
            Self::Claude => "/claude",
            Self::Grok => "/grok/v1",
            Self::Dsh => return vec![],
        };
        let mut ports = vec![
            Some(port),
            Self::Codex.legacy_port(),
            Self::Claude.legacy_port(),
            Self::Grok.legacy_port(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        ports.sort_unstable();
        ports.dedup();
        ports
            .into_iter()
            .flat_map(|port| {
                let mut addresses = vec![format!("http://127.0.0.1:{port}{prefix}")];
                if self == Self::Codex {
                    addresses.push(format!("http://127.0.0.1:{port}/v1"));
                }
                addresses
            })
            .collect()
    }
    pub fn config(self, home: &Path) -> PathBuf {
        if self == Self::Dsh {
            return home.join("profiles");
        }
        home.join(if self == Self::Claude {
            "settings.json"
        } else {
            "config.toml"
        })
    }
}

pub fn loopback_address(base: &str) -> bool {
    url::Url::parse(base)
        .ok()
        .is_some_and(|url| match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(host)) => host.is_loopback(),
            Some(url::Host::Ipv6(host)) => host.is_loopback(),
            None => false,
        })
}

pub fn claude_home() -> String {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
        .to_string_lossy()
        .into_owned()
}

pub fn grok_home() -> String {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".grok"))
        .to_string_lossy()
        .into_owned()
}
pub fn dsh_home() -> String {
    std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".dsh"))
        .to_string_lossy()
        .into_owned()
}
