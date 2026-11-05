//! Current-installation probe, never evidence that a managed session ran a hook.
use super::super::{ADAPTER, hooks};
use crate::native::diagnostics::vocabulary::{CheckId, Reason};
use crate::native::diagnostics::{Availability, Check, Context};
use crate::native::provider::NativeProviderAdapter;
use crate::native::{process_env, provider_process};
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT: u64 = 1024 * 1024;

pub(super) fn check(context: &Context<'_>) -> Check {
    let result = probe(context);
    match result {
        Ok(evidence) => {
            let trusted = evidence["hooks"].as_array().unwrap().iter().all(|h| {
                h["trustStatus"] == "trusted" && h["hash_matches"] == true
            });
            Check::new(CheckId::CODEX_HOOK_TRUST,
                if trusted { Availability::Available } else { Availability::Unavailable },
                if trusted { Reason::CODEX_HOOK_TRUST_OBSERVED } else { Reason::CODEX_HOOK_TRUST_ATTENTION },
                "Current installed Codex hooks/list probe with this Bridge executable's proposed override; not an observation of hook execution or this session's effective configuration.",
                "Inspect trustStatus and hash_matches; normalization drift means currentHash differs. Bridge never answers the hook-review picker.")
                .evidence(evidence)
        }
        Err(error) => Check::new(CheckId::CODEX_HOOK_TRUST, Availability::Unavailable,
            Reason::CODEX_HOOK_TRUST_UNAVAILABLE, format!("Current hook-trust probe unavailable: {error:#}"),
            "Inspect the installed Codex and workspace; this probe grants no session readiness or completion authority.")
            .evidence(json!({"source":"current_probe","error":format!("{error:#}")})),
    }
}

fn probe(context: &Context<'_>) -> Result<Value> {
    let executable = context.executable.context("Codex executable unavailable")?;
    if !context.workspace.is_dir() {
        bail!("workspace unavailable");
    }
    let bridge = std::env::current_exe()?;
    let settings = crate::native::session::Reader::state_root().and_then(|root| settings(&root));
    let notice = proposes_notice(
        context.current_version,
        settings,
        hooks::hook_command(&bridge).is_some(),
    );
    let override_value =
        hooks::hooks_override(&bridge, notice).context("hook launcher unavailable")?;
    // This command constructor handles PowerShell shims without creating a forwarder.
    // Batch shims need a disk forwarder in the ordinary launcher; refuse them here.
    if executable
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("cmd") || s.eq_ignore_ascii_case("bat"))
    {
        bail!(
            "batch shim unavailable for the file-free hook-trust probe; use the native Codex executable"
        );
    }
    let mut command =
        process_env::provider_version_command(executable, ADAPTER.probe_environment_removals())?;
    command
        .args(["-c", &override_value, "app-server"])
        .current_dir(context.workspace)
        .env_remove(crate::native::session::primitives::SESSION_DIR_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    provider_process::configure_process_tree(&mut command);
    let deadline = Instant::now() + TIMEOUT;
    let mut child = command
        .spawn()
        .context("failed to start Codex app-server probe")?;
    let tree = match provider_process::ProviderProcessTree::attach(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let result = (|| {
        tree.resume(&child)?;
        exchange(
            child.stdin.take().context("probe stdin missing")?,
            child.stdout.take().context("probe stdout missing")?,
            context.workspace,
            deadline,
        )
    })();
    tree.terminate();
    let _ = child.kill();
    let _ = child.wait();
    summarize(&result?, &bridge, notice)
}

type Settings = (
    crate::native::settings::CodexLifecycleHooks,
    crate::native::settings::CodexQuestionNotice,
);

// The two settings a new launch reads from this state root.
fn settings(root: &Path) -> Result<Settings> {
    Ok((
        crate::native::settings::codex_lifecycle_hooks(root)?,
        crate::native::settings::codex_question_notice(root)?,
    ))
}

// The probe proposes the notice hook exactly when the launch rule would inject it for the
// installed Codex, these settings and this executable. Settings it cannot read enter as their own
// defaults: a launch fails on them instead, and the probe still reports the hooks a launch with
// the defaults would carry. The two lifecycle hooks are proposed whatever the rule says of them.
fn proposes_notice(version: Option<&str>, settings: Result<Settings>, launcher: bool) -> bool {
    let (lifecycle, notice) = settings.unwrap_or_default();
    matches!(
        hooks::Eligibility::new(version, lifecycle, launcher).notice(|| Ok(notice)),
        Ok(None)
    )
}

// Own both streams so even a blocked write/read cannot hold the caller past its
// deadline. The process owner terminates the complete tree on every return path.
fn exchange(
    mut stdin: impl Write + Send + 'static,
    stdout: impl Read + Send + 'static,
    workspace: &Path,
    deadline: Instant,
) -> Result<Value> {
    let workspace = workspace.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new().name("codex-hook-trust-probe".into()).spawn(move || {
        let result = (|| {
            for request in [
                json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"tabcli","version":env!("CARGO_PKG_VERSION")}}}),
                json!({"method":"initialized"}),
                json!({"id":2,"method":"hooks/list","params":{"cwds":[workspace]}}),
            ] {
                serde_json::to_writer(&mut stdin, &request)?;
                stdin.write_all(b"\n")?;
                stdin.flush()?;
            }
            let mut reader = BufReader::new(stdout.take(MAX_OUTPUT + 1));
            let mut total = 0;
            let mut initialized = false;
            loop {
                let mut line = String::new();
                let count = reader.read_line(&mut line)?;
                total += count as u64;
                if total > MAX_OUTPUT { bail!("probe output exceeded limit"); }
                if count == 0 { bail!("probe ended before hooks/list response"); }
                let value: Value = serde_json::from_str(&line).context("malformed JSON-RPC reply")?;
                if !value.is_object() { bail!("malformed JSON-RPC reply object"); }
                if value["id"] == 1 || value["id"] == 2 {
                    if value.get("error").is_some() { bail!("Codex JSON-RPC error: {}", value["error"]); }
                    let result = value.get("result").context("JSON-RPC result missing")?;
                    if value["id"] == 1 { initialized = true; }
                    else {
                        if !initialized { bail!("initialize response missing"); }
                        return Ok(result.clone());
                    }
                }
            }
        })();
        let _ = sender.send(result);
    })?;
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .context("hooks/list probe timed out or responder disconnected")?
}

fn summarize(result: &Value, bridge: &Path, notice: bool) -> Result<Value> {
    let command = hooks::hook_command(bridge).context("hook launcher unavailable")?;
    let data = result["data"]
        .as_array()
        .context("hooks/list data missing")?;
    let mut rows = Vec::new();
    for injected in hooks::Hook::injected(notice) {
        let key = injected.state_key();
        let mut matches = Vec::new();
        for entry in data {
            for hook in entry["hooks"]
                .as_array()
                .context("hooks/list hooks missing")?
            {
                if hook["key"] == key {
                    matches.push(hook);
                }
            }
        }
        if matches.len() != 1 {
            bail!("Bridge hook key missing or ambiguous: {key}");
        }
        let hook = matches[0];
        let hash = hook["currentHash"]
            .as_str()
            .context("currentHash missing")?;
        let status = hook["trustStatus"]
            .as_str()
            .context("trustStatus missing")?;
        let expected = injected.trust_hash(&command);
        rows.push(json!({"key":key,"trustStatus":status,"currentHash":hash,
            "expected_hash":expected,"hash_matches":hash == expected,
            "diagnostic":if hash == expected { None } else { Some("normalization drift") }}));
    }
    Ok(json!({"source":"current_probe","bridge_executable":bridge,"hooks":rows}))
}

#[cfg(test)]
mod tests;
