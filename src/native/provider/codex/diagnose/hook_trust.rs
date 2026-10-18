//! Current-installation probe, never evidence that a managed session ran a hook.
use super::super::{ADAPTER, hooks};
use crate::native::doctor::{Availability, Check, Context};
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
            Check::new("codex_hook_trust",
                if trusted { Availability::Available } else { Availability::Unavailable },
                if trusted { "codex_hook_trust_observed" } else { "codex_hook_trust_attention" },
                "Current installed Codex hooks/list probe with this Bridge executable's proposed override; not an observation of hook execution or this session's effective configuration.",
                "Inspect trustStatus and hash_matches; normalization drift means currentHash differs. Bridge never answers the hook-review picker.")
                .evidence(evidence)
        }
        Err(error) => Check::new("codex_hook_trust", Availability::Unavailable,
            "codex_hook_trust_unavailable", format!("Current hook-trust probe unavailable: {error:#}"),
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
    let override_value = hooks::hooks_override(&bridge).context("hook launcher unavailable")?;
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
    summarize(&result?, &bridge)
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

fn summarize(result: &Value, bridge: &Path) -> Result<Value> {
    let command = hooks::hook_command(bridge).context("hook launcher unavailable")?;
    let data = result["data"]
        .as_array()
        .context("hooks/list data missing")?;
    let mut rows = Vec::new();
    for label in ["user_prompt_submit", "stop"] {
        let key = hooks::session_flags_state_key(label);
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
        let expected = hooks::trust_hash(label, &command);
        rows.push(json!({"key":key,"trustStatus":status,"currentHash":hash,
            "expected_hash":expected,"hash_matches":hash == expected,
            "diagnostic":if hash == expected { None } else { Some("normalization drift") }}));
    }
    Ok(json!({"source":"current_probe","bridge_executable":bridge,"hooks":rows}))
}

#[cfg(test)]
mod tests;
