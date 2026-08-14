use super::{CompletionMonitor, LaunchContext, LaunchPlan, NativeProviderAdapter};
use anyhow::Result;
use std::{ffi::OsString, path::Path};

pub(super) static ADAPTER: ClaudeAdapter = ClaudeAdapter;

pub(super) struct ClaudeAdapter;

impl NativeProviderAdapter for ClaudeAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let settings_path = context.directory.join("claude-settings.json");
        super::super::write_json_atomic(
            &settings_path,
            &super::claude_hook_settings(context.bridge_executable),
        )?;
        Ok(LaunchPlan {
            arguments: vec![
                OsString::from("--settings"),
                settings_path.into_os_string(),
                OsString::from("--name"),
                OsString::from(context.title),
            ],
            prompt_is_positional: true,
            completion_monitor: CompletionMonitor::Hook,
        })
    }
}

pub(super) fn hook_settings(executable: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": format!(
                        "{} native-hook claude",
                        super::super::shell_quote(executable.as_os_str())
                    ),
                    "timeout": 10
                }]
            }]
        }
    })
}
