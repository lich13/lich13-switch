use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientId {
    #[default]
    Codex,
    Claude,
    Grok,
}
impl ClientId {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Grok => "Grok Build",
        }
    }
    pub fn port(self) -> u16 {
        match self {
            Self::Codex => 15722,
            Self::Claude => 15723,
            Self::Grok => 15724,
        }
    }
    pub fn address(self, port: u16) -> String {
        format!(
            "http://127.0.0.1:{port}{}",
            match self {
                Self::Codex => "/codex/v1",
                Self::Claude => "/claude",
                Self::Grok => "/grok/v1",
            }
        )
    }
    pub fn config(self, home: &Path) -> PathBuf {
        home.join(if self == Self::Claude {
            "settings.json"
        } else {
            "config.toml"
        })
    }
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
