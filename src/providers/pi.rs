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

    fn model_args(&self, model: &str) -> Vec<String> {
        let model = if model == "Fable" {
            "anthropic/claude-fable-5"
        } else {
            model
        };
        vec!["--model".to_owned(), model.to_owned()]
    }

    fn effort_args(&self, effort: &str) -> Result<Vec<String>> {
        Ok(vec!["--thinking".to_owned(), effort.to_owned()])
    }
}
