//! Session-flag hook configuration pinned to Codex rust-v0.163.0.
//! Injection records configuration, not runtime availability or completion authority.
//! Mac R0 (Codex 0.163.0, 2026-10-18): `state` inside the same `-c hooks=` value
//! is honoured without writing to the user's config (run C); a wrong hash raises
//! the picker for that hook only (run D). `codex app-server` + JSON-RPC `hooks/list`
//! reports key/currentHash/trustStatus without configuration writes (run F).
//! The question notice hook's matcher group, pinned to Codex rust-v0.166.0, was checked the same
//! way on the Mac with codex-cli 0.166.0 (2026-11-05): `hooks/list` reported it `trusted` with
//! the hash computed here, and `modified` for the hash of the same group without its matcher.
use super::codex_version;
use crate::native::session::{Reader, Store};
use crate::native::settings::{self, CodexLifecycleHooks, CodexQuestionNotice};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

const FILE: &str = "codex-launch-mode.json";
const VECTOR_VERSION: &str = "0.163.0";
/// The question notice hook's launch decision; every later read uses it, never the setting.
const NOTICE_FILE: &str = "codex-question-notice.json";
/// The only Codex version the notice hook's matcher, hash and payload were checked with.
const NOTICE_VECTOR_VERSION: &str = "0.166.0";
/// The tool the notice hook runs for. Codex 0.166.0 matches a pattern of ASCII letters, digits
/// and `_` exactly against the canonical tool name and its matcher aliases; this tool has none,
/// so the pattern cannot select `request_user_input` or a shell tool (`Bash`)
/// (hooks/src/engine/matcher.rs, core/src/tools/hook_names.rs).
pub(super) const QUESTION_TOOL: &str = "request_user_input_async";

pub(super) fn trust_hash(event_label: &str, command: &str) -> String {
    matched_trust_hash(event_label, None, command)
}

pub(super) fn matched_trust_hash(
    event_label: &str,
    matcher: Option<&str>,
    command: &str,
) -> String {
    // discovery.rs:981-998 normalizes through TOML, omitting absent optional fields; the
    // group's matcher is part of the identity (protocol/src/hooks.rs `MatcherGroup`).
    let mut identity = toml::map::Map::from_iter([
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
    ]);
    if let Some(matcher) = matcher {
        identity.insert("matcher".into(), toml::Value::String(matcher.into()));
    }
    let identity = toml::Value::Table(identity);
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

// The notice hook is one more group in the same value, with a matcher so that Codex runs it for
// the question tool only. The two lifecycle groups and their state entries are unchanged.
pub(super) fn hooks_override(bridge_executable: &Path, notice: bool) -> Option<String> {
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
    let (group, state) = if notice {
        (
            format!(",PreToolUse=[{{matcher='{QUESTION_TOOL}',hooks=[{handler}]}}]"),
            format!(
                ",'{}'={{trusted_hash='{}'}}",
                session_flags_state_key("pre_tool_use"),
                matched_trust_hash("pre_tool_use", Some(QUESTION_TOOL), &command)
            ),
        )
    } else {
        (String::new(), String::new())
    };
    Some(format!(
        "hooks={{Stop=[{{hooks=[{handler}]}}],UserPromptSubmit=[{{hooks=[{handler}]}}]{group},state={{'{stop}'={{trusted_hash='{}'}},'{ups}'={{trusted_hash='{}'}}{state}}}}}",
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

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum NoticeHook {
    Injected,
    NotInjected,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum NoticeReason {
    SettingsOff,
    LifecycleHooksNotInjected,
    #[serde(rename = "version_below_0_166")]
    VersionBelow0_166,
    LauncherUnavailable,
}

/// Whether this launch's `-c hooks=` value carried the question notice hook. It records
/// configuration: neither that Codex trusted or ran the hook nor that a child read the notice.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub(super) struct NoticeMode {
    pub(super) schema: u32,
    pub(super) notice_hook: NoticeHook,
    pub(super) reason: Option<NoticeReason>,
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
    let launcher = hook_command(executable).is_some();
    let reason = match &parsed {
        None => Some(Reason::VersionUnknown),
        Some(v) if *v < semver::Version::new(0, 163, 0) => Some(Reason::VersionBelow0_163),
        Some(v) if !v.pre.is_empty() => Some(Reason::Prerelease),
        _ if setting == CodexLifecycleHooks::Off => Some(Reason::SettingsOff),
        _ if !launcher => Some(Reason::LauncherUnavailable),
        _ => None,
    };
    let injected = reason.is_none();
    let launcher_unavailable = reason == Some(Reason::LauncherUnavailable);
    let store = Store::open_unchecked(directory);
    store.private(FILE).write_json(&LaunchMode {
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
    // The notice hook rides in the lifecycle value and keeps its own floor: 0.166.0 is the only
    // version measured. A prerelease never reaches here (the lifecycle decision refuses it).
    let notice_reason = match parsed {
        _ if launcher_unavailable => Some(NoticeReason::LauncherUnavailable),
        _ if !injected => Some(NoticeReason::LifecycleHooksNotInjected),
        Some(v) if v < semver::Version::new(0, 166, 0) => Some(NoticeReason::VersionBelow0_166),
        _ if settings::codex_question_notice(root)? == CodexQuestionNotice::Off => {
            Some(NoticeReason::SettingsOff)
        }
        _ => None,
    };
    let notice = notice_reason.is_none();
    // Written before the value is returned, so a launch never carries a hook its record omits.
    store.private(NOTICE_FILE).write_json(&NoticeMode {
        schema: 1,
        notice_hook: if notice {
            NoticeHook::Injected
        } else {
            NoticeHook::NotInjected
        },
        reason: notice_reason,
        vector_codex_version: NOTICE_VECTOR_VERSION.into(),
    })?;
    Ok(if injected {
        hooks_override(executable, notice)
    } else {
        None
    })
}

// Read the launch decision without re-probing settings/version.
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

// The notice hook decision of this launch. A session without the record was launched by an
// earlier binary, which injected no notice hook.
pub(super) fn read_notice_mode(directory: &Path) -> Result<Option<NoticeMode>> {
    let Some(text) = Reader::open_unchecked(directory)
        .private(NOTICE_FILE)
        .text()?
    else {
        return Ok(None);
    };
    let mode: NoticeMode =
        serde_json::from_str(&text).context("failed to parse Codex question notice mode")?;
    if mode.schema != 1
        || mode.vector_codex_version != NOTICE_VECTOR_VERSION
        || (mode.notice_hook == NoticeHook::Injected) != mode.reason.is_none()
    {
        bail!("Agent Bridge rejected invalid Codex question notice mode")
    }
    Ok(Some(mode))
}

pub(super) fn notice_hook_injected(directory: &Path) -> Result<bool> {
    Ok(read_notice_mode(directory)?.is_some_and(|mode| mode.notice_hook == NoticeHook::Injected))
}

#[cfg(test)]
mod tests;
