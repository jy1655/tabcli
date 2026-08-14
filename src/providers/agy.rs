use super::{FirstPartyCli, ProviderAdapter};
use anyhow::Result;
use semver::Version;

pub(super) static ADAPTER: AgyAdapter = AgyAdapter;
pub(super) const COMMAND: &str = "agy";

pub(super) struct AgyAdapter;

impl ProviderAdapter for AgyAdapter {
    fn cli(&self) -> FirstPartyCli {
        FirstPartyCli::Agy
    }

    fn command(&self) -> &'static str {
        COMMAND
    }

    fn minimum_version(&self) -> Version {
        Version::new(1, 1, 12)
    }

    fn yolo_args(&self) -> &'static [&'static str] {
        &["--dangerously-skip-permissions"]
    }

    fn effort_args(&self, effort: &str) -> Result<Vec<String>> {
        Ok(vec!["--effort".to_owned(), effort.to_owned()])
    }
}
