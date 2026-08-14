use super::{FirstPartyCli, ProviderAdapter};
use anyhow::Result;
use semver::Version;

pub(super) static ADAPTER: PiAdapter = PiAdapter;
pub(super) const COMMAND: &str = "pi";

pub(super) struct PiAdapter;

impl ProviderAdapter for PiAdapter {
    fn cli(&self) -> FirstPartyCli {
        FirstPartyCli::Pi
    }

    fn command(&self) -> &'static str {
        COMMAND
    }

    fn minimum_version(&self) -> Version {
        Version::new(0, 84, 1)
    }

    fn yolo_args(&self) -> &'static [&'static str] {
        &[]
    }

    fn effort_args(&self, effort: &str) -> Result<Vec<String>> {
        Ok(vec!["--thinking".to_owned(), effort.to_owned()])
    }
}
