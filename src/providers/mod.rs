mod agy;
mod claude;
mod codex;
mod pi;

use anyhow::Result;
use semver::Version;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirstPartyCli {
    Codex,
    Claude,
    Agy,
    Pi,
}

pub trait ProviderAdapter: Sync {
    fn cli(&self) -> FirstPartyCli;
    fn command(&self) -> &'static str;
    fn minimum_version(&self) -> Version;
    fn yolo_args(&self) -> &'static [&'static str];
    fn model_args(&self, model: &str) -> Vec<String>;
    fn effort_args(&self, effort: &str) -> Result<Vec<String>>;
}

const SUPPORTED_CLIS: [FirstPartyCli; 4] = [
    FirstPartyCli::Codex,
    FirstPartyCli::Claude,
    FirstPartyCli::Agy,
    FirstPartyCli::Pi,
];

pub const fn supported_clis() -> &'static [FirstPartyCli] {
    &SUPPORTED_CLIS
}

pub fn provider_adapter(cli: FirstPartyCli) -> &'static dyn ProviderAdapter {
    match cli {
        FirstPartyCli::Codex => &codex::ADAPTER,
        FirstPartyCli::Claude => &claude::ADAPTER,
        FirstPartyCli::Agy => &agy::ADAPTER,
        FirstPartyCli::Pi => &pi::ADAPTER,
    }
}

impl FirstPartyCli {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => codex::COMMAND,
            Self::Claude => claude::COMMAND,
            Self::Agy => agy::COMMAND,
            Self::Pi => pi::COMMAND,
        }
    }

    pub const fn command(self) -> &'static str {
        self.as_str()
    }

    pub fn minimum_version(self) -> Version {
        provider_adapter(self).minimum_version()
    }
}

impl FromStr for FirstPartyCli {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        supported_clis()
            .iter()
            .copied()
            .find(|cli| provider_adapter(*cli).command() == value)
            .ok_or_else(|| format!("unsupported CLI {value:?}; expected codex, claude, agy, or pi"))
    }
}
