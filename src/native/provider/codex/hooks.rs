//! Session-flag hook configuration pinned to Codex rust-v0.163.0.
//! Injection records configuration, not runtime availability or completion authority.
//! Mac R0 (Codex 0.163.0, 2026-10-18): `state` inside the same `-c hooks=` value
//! is honoured without writing to the user's config (run C); a wrong hash raises
//! the picker for that hook only (run D). `codex app-server` + JSON-RPC `hooks/list`
//! reports key/currentHash/trustStatus without writing (run F), the doctor probe
//! planned for R4. That probe is not implemented in this round.
use super::codex_version;
use crate::native::session::{Reader, Store};
use crate::native::settings::{self, CodexLifecycleHooks};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

const FILE: &str = "codex-launch-mode.json";
const VECTOR_VERSION: &str = "0.163.0";

pub(super) fn trust_hash(event_label: &str, command: &str) -> String {
    // discovery.rs:777-793 normalizes through TOML, omitting absent optional fields.
    let identity = toml::Value::Table(toml::map::Map::from_iter([
        ("event_name".into(), toml::Value::String(event_label.into())),
        (
            "hooks".into(),
            toml::Value::Array(vec![toml::Value::Table(toml::map::Map::from_iter([
                ("type".into(), toml::Value::String("command".into())),
                ("command".into(), toml::Value::String(command.into())),
                ("timeout".into(), toml::Value::Integer(30)),
                ("async".into(), toml::Value::Boolean(false)),
            ]))]),
        ),
    ]));
    let mut json = serde_json::to_value(identity).expect("hook identity is JSON representable");
    json.sort_all_objects();
    let bytes = serde_json::to_vec(&json).expect("hook identity serializes");
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{digest}")
}

pub(super) fn session_flags_state_key(label: &str) -> String {
    #[cfg(windows)]
    let source = r"C:\<session-flags>\config.toml";
    // Verified in Mac R0 on Codex 0.163.0; matches discovery.rs:421,425-434.
    #[cfg(unix)]
    let source = "/<session-flags>/config.toml";
    format!("{source}:{label}:0:0")
}

pub(super) fn hook_command(bridge_executable: &Path) -> Option<String> {
    let path = bridge_executable.to_str()?;
    if path.is_empty() {
        return None;
    }
    #[cfg(windows)]
    {
        if !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:\\-".contains(&b))
        {
            return None;
        }
        Some(format!("{path} native-hook codex"))
    }
    #[cfg(unix)]
    {
        Some(format!(
            "{} native-hook codex",
            crate::native::terminal::shell_quote(bridge_executable.as_os_str())
        ))
    }
}

pub(super) fn hooks_override(bridge_executable: &Path) -> Option<String> {
    let command = hook_command(bridge_executable)?;
    #[cfg(windows)]
    let quoted = format!("'{command}'");
    // JSON escapes also work in TOML basic strings. DEL additionally needs escaping
    // because TOML forbids that literal control character.
    #[cfg(unix)]
    let quoted = serde_json::to_string(&command)
        .expect("command serializes")
        .replace('\u{7f}', "\\u007f");
    let handler = format!("{{type='command',command={quoted},timeout=30,async=false}}");
    let stop = session_flags_state_key("stop");
    let ups = session_flags_state_key("user_prompt_submit");
    Some(format!(
        "hooks={{Stop=[{{hooks=[{handler}]}}],UserPromptSubmit=[{{hooks=[{handler}]}}],state={{'{stop}'={{trusted_hash='{}'}},'{ups}'={{trusted_hash='{}'}}}}}}",
        trust_hash("stop", &command),
        trust_hash("user_prompt_submit", &command)
    ))
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum LifecycleHooks {
    Injected,
    NotInjected,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    #[serde(rename = "version_below_0_163")]
    VersionBelow0_163,
    VersionUnknown,
    Prerelease,
    SettingsOff,
    LauncherUnavailable,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct LaunchMode {
    pub(super) schema: u32,
    pub(super) lifecycle_hooks: LifecycleHooks,
    pub(super) reason: Option<Reason>,
    pub(super) codex_version: Option<String>,
    pub(super) vector_codex_version: String,
}

pub(super) fn prepare(directory: &Path, executable: &Path) -> Result<Option<String>> {
    let version = match Reader::open_unchecked(directory).manifest() {
        Ok(manifest) => Some(manifest.provider_version),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let root = directory
        .parent()
        .context("Codex session has no state root")?;
    let setting = settings::codex_lifecycle_hooks(root)?;
    let parsed = version.as_deref().and_then(|v| codex_version(v).ok());
    let value = hooks_override(executable);
    let reason = match parsed {
        None => Some(Reason::VersionUnknown),
        Some(v) if v < semver::Version::new(0, 163, 0) => Some(Reason::VersionBelow0_163),
        Some(v) if !v.pre.is_empty() => Some(Reason::Prerelease),
        _ if setting == CodexLifecycleHooks::Off => Some(Reason::SettingsOff),
        _ if value.is_none() => Some(Reason::LauncherUnavailable),
        _ => None,
    };
    let injected = reason.is_none();
    Store::open_unchecked(directory)
        .private(FILE)
        .write_json(&LaunchMode {
            schema: 1,
            lifecycle_hooks: if injected {
                LifecycleHooks::Injected
            } else {
                LifecycleHooks::NotInjected
            },
            reason,
            codex_version: version,
            vector_codex_version: VECTOR_VERSION.into(),
        })?;
    Ok(if injected { value } else { None })
}

// R2/R4 will consume this adapter-private record instead of re-probing settings/version.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "reader staged for subsequent lifecycle-hook rounds"
    )
)]
pub(super) fn read_launch_mode(directory: &Path) -> Result<Option<LaunchMode>> {
    let Some(text) = Reader::open_unchecked(directory).private(FILE).text()? else {
        return Ok(None);
    };
    let mode: LaunchMode =
        serde_json::from_str(&text).context("failed to parse Codex launch mode")?;
    if mode.schema != 1
        || mode.vector_codex_version != VECTOR_VERSION
        || (mode.lifecycle_hooks == LifecycleHooks::Injected) != mode.reason.is_none()
    {
        bail!("Agent Bridge rejected invalid Codex launch mode")
    }
    Ok(Some(mode))
}

#[cfg(test)]
mod tests;
