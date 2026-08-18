use super::{FirstPartyCli, ProviderAdapter};
use anyhow::Result;
use semver::Version;

pub(super) static ADAPTER: CodexAdapter = CodexAdapter;
pub(super) const COMMAND: &str = "codex";

pub(super) struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn cli(&self) -> FirstPartyCli {
        FirstPartyCli::Codex
    }

    fn command(&self) -> &'static str {
        COMMAND
    }

    fn minimum_version(&self) -> Version {
        Version::new(0, 147, 0)
    }

    fn yolo_args(&self) -> &'static [&'static str] {
        &["--dangerously-bypass-approvals-and-sandbox"]
    }

    fn model_args(&self, model: &str) -> Vec<String> {
        vec!["--model".to_owned(), model.to_owned()]
    }

    fn effort_args(&self, effort: &str) -> Result<Vec<String>> {
        Ok(vec![
            "-c".to_owned(),
            format!("model_reasoning_effort={}", serde_json::to_string(effort)?),
        ])
    }
}
