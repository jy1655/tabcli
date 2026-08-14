use super::{CompletionMonitor, LaunchContext, LaunchPlan, NativeProviderAdapter};
use anyhow::Result;
use std::ffi::OsString;

pub(super) static ADAPTER: AgyAdapter = AgyAdapter;

pub(super) struct AgyAdapter;

impl NativeProviderAdapter for AgyAdapter {
    fn prepare_launch(&self, context: LaunchContext<'_>) -> Result<LaunchPlan> {
        let log_path = context.directory.join("agy.log");
        Ok(LaunchPlan {
            arguments: vec![
                OsString::from("--log-file"),
                log_path.as_os_str().to_owned(),
                OsString::from("--prompt-interactive"),
                OsString::from(context.prompt),
            ],
            prompt_is_positional: false,
            completion_monitor: CompletionMonitor::AgyTranscript { log_path },
        })
    }
}
