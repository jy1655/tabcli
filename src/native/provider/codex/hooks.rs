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

// The matcher-less identity under the name the vector tests use; production hashes go through
// `Hook::trust_hash`.
#[cfg(test)]
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

/// The hooks Bridge injects into a new launch, in the order of the `-c hooks=` value. The set is
/// closed: the value, the hook-trust probe's rows and the native hook dispatch all go through it,
/// so a new member is a compile error in the dispatch until it is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hook {
    Stop,
    UserPromptSubmit,
    /// The question notice: one group with a matcher, so that Codex runs it for the question
    /// tool only, injected only when the launch decides on it.
    QuestionNotice,
}

impl Hook {
    const ALL: [Self; 3] = [Self::Stop, Self::UserPromptSubmit, Self::QuestionNotice];

    /// Codex's event name: the group's table name in the value and `hook_event_name` in a payload.
    const fn event(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::QuestionNotice => "PreToolUse",
        }
    }

    /// The event label in the session-flags state key and in the trust hash identity.
    const fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::QuestionNotice => "pre_tool_use",
        }
    }

    const fn matcher(self) -> Option<&'static str> {
        match self {
            Self::Stop | Self::UserPromptSubmit => None,
            Self::QuestionNotice => Some(QUESTION_TOOL),
        }
    }

    /// The member a hook payload's event name names; any other name is none of Bridge's hooks.
    pub(super) fn from_event(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|hook| hook.event() == name)
    }

    /// The hooks a launch injects, in value order: the lifecycle hooks, and the notice hook only
    /// with `notice`.
    pub(super) fn injected(notice: bool) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |hook| notice || *hook != Self::QuestionNotice)
    }

    pub(super) fn state_key(self) -> String {
        session_flags_state_key(self.label())
    }

    pub(super) fn trust_hash(self, command: &str) -> String {
        matched_trust_hash(self.label(), self.matcher(), command)
    }
}

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
    let mut groups = Vec::new();
    let mut states = Vec::new();
    for hook in Hook::injected(notice) {
        let matcher = hook
            .matcher()
            .map(|matcher| format!("matcher='{matcher}',"))
            .unwrap_or_default();
        groups.push(format!("{}=[{{{matcher}hooks=[{handler}]}}]", hook.event()));
        states.push(format!(
            "'{}'={{trusted_hash='{}'}}",
            hook.state_key(),
            hook.trust_hash(&command)
        ));
    }
    Some(format!(
        "hooks={{{},state={{{}}}}}",
        groups.join(","),
        states.join(",")
    ))
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum LifecycleHooks {
    Injected,
    NotInjected,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
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

/// The first Codex version a new launch injects the lifecycle hooks for. A floor is eligibility;
/// `VECTOR_VERSION` is the version the trust hashes were measured with. They are equal today, but
/// neither is derived from the other.
const LIFECYCLE_FLOOR: semver::Version = semver::Version::new(0, 163, 0);
/// The first Codex version a new launch injects the question notice hook for: the only version
/// measured. Like the lifecycle floor, it is independent of `NOTICE_VECTOR_VERSION`.
const NOTICE_FLOOR: semver::Version = semver::Version::new(0, 166, 0);

/// The one rule deciding which hooks a new launch injects, in the vocabulary of the two launch
/// records: no reason injects, a reason does not. `prepare` records its decisions and the
/// hook-trust probe proposes the notice hook from it. The lifecycle hooks are decided first;
/// the notice hook rides in their value, so it is decided after them and its setting is read
/// only when every other notice gate passes.
pub(super) struct Eligibility {
    version: Option<semver::Version>,
    lifecycle: Option<Reason>,
}

impl Eligibility {
    /// `version` is the Codex version output the launch records; one that does not parse is
    /// unknown. `setting` is the lifecycle hooks setting, and `launcher` is whether this Bridge
    /// executable has a hook command.
    pub(super) fn new(version: Option<&str>, setting: CodexLifecycleHooks, launcher: bool) -> Self {
        let version = version.and_then(|version| codex_version(version).ok());
        let reason = match &version {
            None => Some(Reason::VersionUnknown),
            Some(v) if *v < LIFECYCLE_FLOOR => Some(Reason::VersionBelow0_163),
            Some(v) if !v.pre.is_empty() => Some(Reason::Prerelease),
            _ if setting == CodexLifecycleHooks::Off => Some(Reason::SettingsOff),
            _ if !launcher => Some(Reason::LauncherUnavailable),
            _ => None,
        };
        Self {
            version,
            lifecycle: reason,
        }
    }

    /// Why the lifecycle hooks are not injected; `None` when they are.
    pub(super) fn lifecycle(&self) -> Option<Reason> {
        self.lifecycle
    }

    /// Why the notice hook is not injected; `None` when it is. A prerelease never reaches the
    /// floor check: the lifecycle decision refuses it. `setting` runs only after every other gate
    /// passes, and its failure is the decision's failure.
    pub(super) fn notice(
        &self,
        setting: impl FnOnce() -> Result<CodexQuestionNotice>,
    ) -> Result<Option<NoticeReason>> {
        let reason = match (&self.lifecycle, &self.version) {
            (Some(Reason::LauncherUnavailable), _) => Some(NoticeReason::LauncherUnavailable),
            (Some(_), _) => Some(NoticeReason::LifecycleHooksNotInjected),
            (None, Some(v)) if *v < NOTICE_FLOOR => Some(NoticeReason::VersionBelow0_166),
            _ => None,
        };
        if reason.is_some() {
            return Ok(reason);
        }
        Ok((setting()? == CodexQuestionNotice::Off).then_some(NoticeReason::SettingsOff))
    }
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
    let eligibility = Eligibility::new(
        version.as_deref(),
        settings::codex_lifecycle_hooks(root)?,
        hook_command(executable).is_some(),
    );
    let reason = eligibility.lifecycle();
    let injected = reason.is_none();
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
    // The notice setting is read only after the lifecycle record is published.
    let notice_reason = eligibility.notice(|| settings::codex_question_notice(root))?;
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
