use super::{FirstPartyCli, ProviderAdapter};
use anyhow::Result;
use semver::Version;

pub(super) static ADAPTER: ClaudeAdapter = ClaudeAdapter;
pub(super) const COMMAND: &str = "claude";

pub(super) struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn cli(&self) -> FirstPartyCli {
        FirstPartyCli::Claude
    }

    fn command(&self) -> &'static str {
        COMMAND
    }

    fn minimum_version(&self) -> Version {
        Version::new(2, 1, 234)
    }

    fn yolo_args(&self) -> &'static [&'static str] {
        &["--dangerously-skip-permissions"]
    }

    fn model_args(&self, model: &str) -> Vec<String> {
        let model = if model == "Fable5" { "Fable" } else { model };
        vec!["--model".to_owned(), model.to_owned()]
    }

    fn effort_args(&self, effort: &str) -> Result<Vec<String>> {
        Ok(vec!["--effort".to_owned(), effort.to_owned()])
    }
}
