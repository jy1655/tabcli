//! Settings that a user changes with `tabcli settings`. They are one private
//! record in the state root; a missing record, or a missing key, means the default.
use crate::native::session::{Reader, RecordReader, RecordStore, Store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::str::FromStr;

const FILE: &str = "settings.json";
// A setting's wire values, parser, default and usage share this declaration. The
// record and its callers still own storage shape, read timing and error policy.
macro_rules! setting {
    (
        $(#[$doc:meta])*
        pub(super) enum $type:ident {
            $(#[$first_doc:meta])* $first:ident => $first_wire:literal,
            $(#[$second_doc:meta])* $second:ident => $second_wire:literal,
        }
        name: $name:literal; synopsis: $synopsis:ident;
    ) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
        pub(super) enum $type {
            $(#[$first_doc])*
            #[serde(rename = $first_wire)]
            $first,
            $(#[$second_doc])*
            #[serde(rename = $second_wire)]
            $second,
        }

        impl $type {
            const NAME: &'static str = $name;
            const ARGUMENT: &'static str = concat!($name, " <", $first_wire, "|", $second_wire, ">");
            const SYNOPSIS: &'static str = setting!(@synopsis $synopsis, $name, $first_wire, $second_wire);

            pub(super) const fn as_str(self) -> &'static str {
                match self {
                    Self::$first => $first_wire,
                    Self::$second => $second_wire,
                }
            }
        }

        impl FromStr for $type {
            type Err = anyhow::Error;

            fn from_str(value: &str) -> Result<Self> {
                match value {
                    $first_wire => Ok(Self::$first),
                    $second_wire => Ok(Self::$second),
                    _ => bail!(concat!($name, " is ", $first_wire, " or ", $second_wire, ", not {:?}"), value),
                }
            }
        }
    };
    (@synopsis optional, $name:literal, $first:literal, $second:literal) => {
        concat!("settings [", $name, " <", $first, "|", $second, ">] [--json]")
    };
    (@synopsis required, $name:literal, $first:literal, $second:literal) => {
        concat!("settings ", $name, " <", $first, "|", $second, "> [--json]")
    };
}

setting! {
    /// A creation preference, never cleanup authority. Each terminal handle records
    /// the surface that was actually created, even if this setting changes later.
    pub(super) enum MacosOpenMode {
        #[default]
        TabFirst => "tab-first",
        NewWindow => "new-window",
    }
    name: "macos-open-mode"; synopsis: required;
}

setting! {
    /// The Windows Terminal window in which a managed console opens its tab on native
    /// Windows. It has no effect on another platform.
    pub(super) enum WindowsTabWindow {
        /// The window named `agent-bridge`, which Windows Terminal creates when it does not
        /// exist. No window the user works in is touched.
        #[default]
        Dedicated => "dedicated",
        /// The most recently used Windows Terminal window. Windows Terminal selects the new
        /// tab and offers no way to select the earlier one again, so the tab keeps the
        /// keyboard inside that window until the user switches back.
        Current => "current",
    }
    name: "windows-tab-window"; synopsis: optional;
}

setting! {
    /// A launch admission preference, never close authority: whether `ask`, `reopen` and
    /// `self-test` start a session whose workspace is the Codex memory workspace or lies
    /// inside it. Codex's background memory consolidation delegated through Bridge from
    /// there and left sessions open (issue #216), so the default refuses.
    pub(super) enum CodexMemoryWorkspace {
        #[default]
        Refuse => "refuse",
        Allow => "allow",
    }
    name: "codex-memory-workspace"; synopsis: required;
}

setting! {
    /// Whether new Codex launches inject lifecycle hooks; not evidence of execution.
    pub(super) enum CodexLifecycleHooks {
        #[default]
        On => "on",
        Off => "off",
    }
    name: "codex-lifecycle-hooks"; synopsis: required;
}

setting! {
    /// Whether new Claude launches refuse a model's main-loop `AskUserQuestion` call before its
    /// dialog and return the question in the Result. Each session records its own launch decision;
    /// the setting is not evidence that a call was refused.
    pub(super) enum ClaudeQuestionDeflection {
        #[default]
        On => "on",
        Off => "off",
    }
    name: "claude-question-deflection"; synopsis: required;
}

setting! {
    /// Whether new Codex launches inject the `PreToolUse` hook that lets the model's
    /// `request_user_input_async` call run and adds a notice: no reply can arrive during the turn, so
    /// finish what does not depend on the answer and end the turn with the question. Off unless the
    /// user turns it on: the question tool is asynchronous by design and the notice is advice a
    /// caller may not need. Each session records its own launch decision; the setting is not
    /// evidence that a child read the notice.
    pub(super) enum CodexQuestionNotice {
        On => "on",
        #[default]
        Off => "off",
    }
    name: "codex-question-notice"; synopsis: required;
}

// Help/describe put the optional Windows setting first. Invalid-argument usage
// retains its older macOS-first order below.
pub(super) const SYNOPSIS: &[&str] = &[
    WindowsTabWindow::SYNOPSIS,
    MacosOpenMode::SYNOPSIS,
    CodexMemoryWorkspace::SYNOPSIS,
    CodexLifecycleHooks::SYNOPSIS,
    ClaudeQuestionDeflection::SYNOPSIS,
    CodexQuestionNotice::SYNOPSIS,
];

fn usage() -> String {
    let leading = [
        MacosOpenMode::ARGUMENT,
        WindowsTabWindow::ARGUMENT,
        CodexMemoryWorkspace::ARGUMENT,
        CodexLifecycleHooks::ARGUMENT,
        ClaudeQuestionDeflection::ARGUMENT,
    ]
    .join(", ");
    format!(
        "settings takes no argument to show the settings, or {leading} or {} to change one; --json is accepted",
        CodexQuestionNotice::ARGUMENT,
    )
}

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_tab_window: Option<WindowsTabWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    macos_open_mode: Option<MacosOpenMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_memory_workspace: Option<CodexMemoryWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_lifecycle_hooks: Option<CodexLifecycleHooks>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_question_deflection: Option<ClaudeQuestionDeflection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_question_notice: Option<CodexQuestionNotice>,
}

fn read(root: &Path) -> Result<Record> {
    let path = Reader::open_unchecked(root).private(FILE).path().to_owned();
    let Some(text) = RecordReader::at(&path).text()? else {
        return Ok(Record {
            schema: 1,
            windows_tab_window: None,
            macos_open_mode: None,
            codex_memory_workspace: None,
            codex_lifecycle_hooks: None,
            claude_question_deflection: None,
            codex_question_notice: None,
        });
    };
    let record: Record = serde_json::from_str(&text)
        .with_context(|| format!("invalid settings record {}", path.display()))?;
    if record.schema != 1 {
        bail!(
            "unsupported settings schema {} in {}",
            record.schema,
            path.display()
        );
    }
    Ok(record)
}

pub(super) fn windows_tab_window(root: &Path) -> Result<WindowsTabWindow> {
    Ok(read(root)?.windows_tab_window.unwrap_or_default())
}

pub(super) fn macos_open_mode(root: &Path) -> Result<MacosOpenMode> {
    Ok(read(root)?.macos_open_mode.unwrap_or_default())
}

pub(super) fn codex_memory_workspace(root: &Path) -> Result<CodexMemoryWorkspace> {
    Ok(read(root)?.codex_memory_workspace.unwrap_or_default())
}

pub(super) fn codex_lifecycle_hooks(root: &Path) -> Result<CodexLifecycleHooks> {
    Ok(read(root)?.codex_lifecycle_hooks.unwrap_or_default())
}

pub(super) fn claude_question_deflection(root: &Path) -> Result<ClaudeQuestionDeflection> {
    Ok(read(root)?.claude_question_deflection.unwrap_or_default())
}

pub(super) fn codex_question_notice(root: &Path) -> Result<CodexQuestionNotice> {
    Ok(read(root)?.codex_question_notice.unwrap_or_default())
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let settings = apply(&Reader::state_root()?, args)?;
    println!("{}", serde_json::to_string_pretty(&settings)?);
    Ok(())
}

// Changes the named setting, if one is named, and reports every setting as it now is.
fn apply(root: &Path, args: &[String]) -> Result<serde_json::Value> {
    let named: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|argument| *argument != "--json")
        .collect();
    if args.len() - named.len() > 1 {
        bail!(usage());
    }
    match named.as_slice() {
        [] => {}
        [MacosOpenMode::NAME, value] => {
            let value = value.parse::<MacosOpenMode>()?;
            Store::create_state_root(root, &Reader::home_directories())?;
            let mut record = read(root)?;
            record.macos_open_mode = Some(value);
            RecordStore::at(Reader::open_unchecked(root).private(FILE).path())
                .write_json(&record)?;
        }
        [WindowsTabWindow::NAME, value] => {
            let value = value.parse::<WindowsTabWindow>()?;
            Store::create_state_root(root, &Reader::home_directories())?;
            let mut record = read(root)?;
            record.windows_tab_window = Some(value);
            RecordStore::at(Reader::open_unchecked(root).private(FILE).path())
                .write_json(&record)?;
        }
        [CodexLifecycleHooks::NAME, value] => {
            let value = value.parse::<CodexLifecycleHooks>()?;
            Store::create_state_root(root, &Reader::home_directories())?;
            let mut record = read(root)?;
            record.codex_lifecycle_hooks = Some(value);
            RecordStore::at(Reader::open_unchecked(root).private(FILE).path())
                .write_json(&record)?;
        }
        [CodexMemoryWorkspace::NAME, value] => {
            let value = value.parse::<CodexMemoryWorkspace>()?;
            Store::create_state_root(root, &Reader::home_directories())?;
            let mut record = read(root)?;
            record.codex_memory_workspace = Some(value);
            RecordStore::at(Reader::open_unchecked(root).private(FILE).path())
                .write_json(&record)?;
        }
        [ClaudeQuestionDeflection::NAME, value] => {
            let value = value.parse::<ClaudeQuestionDeflection>()?;
            Store::create_state_root(root, &Reader::home_directories())?;
            let mut record = read(root)?;
            record.claude_question_deflection = Some(value);
            RecordStore::at(Reader::open_unchecked(root).private(FILE).path())
                .write_json(&record)?;
        }
        [CodexQuestionNotice::NAME, value] => {
            let value = value.parse::<CodexQuestionNotice>()?;
            Store::create_state_root(root, &Reader::home_directories())?;
            let mut record = read(root)?;
            record.codex_question_notice = Some(value);
            RecordStore::at(Reader::open_unchecked(root).private(FILE).path())
                .write_json(&record)?;
        }
        _ => bail!(usage()),
    }
    Ok(serde_json::json!({
        "settings_file": Reader::open_unchecked(root).private(FILE).path().to_owned(),
        "windows_tab_window": windows_tab_window(root)?.as_str(),
        "macos_open_mode": macos_open_mode(root)?.as_str(),
        "codex_memory_workspace": codex_memory_workspace(root)?.as_str(),
        "codex_lifecycle_hooks": codex_lifecycle_hooks(root)?.as_str(),
        "claude_question_deflection": claude_question_deflection(root)?.as_str(),
        "codex_question_notice": codex_question_notice(root)?.as_str(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::session::primitives::read_json;
    use std::fs;

    #[test]
    fn setting_values_keep_their_parse_serde_order_and_defaults() {
        macro_rules! check {
            ($ty:ty, $name:literal, [$first:literal, $second:literal], $default:literal) => {{
                assert_eq!(<$ty>::default().as_str(), $default);
                for wire in [$first, $second] {
                    let value = wire.parse::<$ty>().unwrap();
                    assert_eq!(value.as_str(), wire);
                    assert_eq!(serde_json::to_string(&value).unwrap(), format!("{wire:?}"));
                    let decoded: $ty = serde_json::from_str(&format!("{wire:?}")).unwrap();
                    assert_eq!(decoded, value);
                }
                assert_eq!(
                    "invalid".parse::<$ty>().unwrap_err().to_string(),
                    concat!($name, " is ", $first, " or ", $second, ", not \"invalid\"")
                );
                assert_eq!(
                    serde_json::from_str::<$ty>("\"invalid\"")
                        .unwrap_err()
                        .to_string(),
                    concat!(
                        "unknown variant `invalid`, expected `",
                        $first,
                        "` or `",
                        $second,
                        "` at line 1 column 9"
                    )
                );
            }};
        }
        check!(
            MacosOpenMode,
            "macos-open-mode",
            ["tab-first", "new-window"],
            "tab-first"
        );
        check!(
            WindowsTabWindow,
            "windows-tab-window",
            ["dedicated", "current"],
            "dedicated"
        );
        check!(
            CodexMemoryWorkspace,
            "codex-memory-workspace",
            ["refuse", "allow"],
            "refuse"
        );
        check!(
            CodexLifecycleHooks,
            "codex-lifecycle-hooks",
            ["on", "off"],
            "on"
        );
        check!(
            ClaudeQuestionDeflection,
            "claude-question-deflection",
            ["on", "off"],
            "on"
        );
        check!(
            CodexQuestionNotice,
            "codex-question-notice",
            ["on", "off"],
            "off"
        );
    }

    #[test]
    fn usage_keeps_its_order_and_invalid_requests_do_not_create_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let expected = "settings takes no argument to show the settings, or macos-open-mode <tab-first|new-window>, windows-tab-window <dedicated|current>, codex-memory-workspace <refuse|allow>, codex-lifecycle-hooks <on|off>, claude-question-deflection <on|off> or codex-question-notice <on|off> to change one; --json is accepted";
        for invalid in [
            vec!["unknown", "value"],
            vec!["--json", "--json"],
            vec!["macos-open-mode"],
        ] {
            assert_eq!(
                apply(&root, &arguments(&invalid)).unwrap_err().to_string(),
                expected
            );
            assert!(!root.exists());
        }
    }

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn lifecycle_hooks_default_on_and_updates_preserve_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        assert_eq!(apply(&root, &[]).unwrap()["codex_lifecycle_hooks"], "on");
        assert!(!root.exists());
        for invalid in [
            vec!["codex-lifecycle-hooks"],
            vec!["codex-lifecycle-hooks", "ON"],
            vec!["codex-lifecycle-hooks", "off", "extra"],
        ] {
            assert!(apply(&root, &arguments(&invalid)).is_err());
        }
        assert!(!root.exists());
        apply(&root, &arguments(&["codex-memory-workspace", "allow"])).unwrap();
        for value in ["off", "on"] {
            assert_eq!(
                apply(&root, &arguments(&["codex-lifecycle-hooks", value])).unwrap()["codex_lifecycle_hooks"],
                value
            );
            assert_eq!(
                codex_memory_workspace(&root).unwrap(),
                CodexMemoryWorkspace::Allow
            );
        }
        fs::write(
            root.join(FILE),
            r#"{"schema":1,"codex_lifecycle_hooks":"invalid"}"#,
        )
        .unwrap();
        assert!(codex_lifecycle_hooks(&root).is_err());
    }

    #[test]
    fn question_deflection_defaults_on_and_updates_preserve_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        assert_eq!(
            claude_question_deflection(&root).unwrap(),
            ClaudeQuestionDeflection::On
        );
        assert_eq!(
            apply(&root, &[]).unwrap()["claude_question_deflection"],
            "on"
        );
        for invalid in [
            vec!["claude-question-deflection"],
            vec!["claude-question-deflection", "ON"],
            vec!["claude-question-deflection", "yes"],
            vec!["claude-question-deflection", "off", "extra"],
        ] {
            assert!(apply(&root, &arguments(&invalid)).is_err(), "{invalid:?}");
        }
        assert!(!root.exists(), "an invalid value must not create the root");
        assert_eq!(
            apply(&root, &arguments(&["claude-question-deflection", "maybe"]))
                .unwrap_err()
                .to_string(),
            "claude-question-deflection is on or off, not \"maybe\""
        );
        apply(&root, &arguments(&["codex-lifecycle-hooks", "off"])).unwrap();
        for value in ["off", "on", "off"] {
            assert_eq!(
                apply(
                    &root,
                    &arguments(&["claude-question-deflection", value, "--json"])
                )
                .unwrap()["claude_question_deflection"],
                value
            );
            assert_eq!(
                codex_lifecycle_hooks(&root).unwrap(),
                CodexLifecycleHooks::Off
            );
        }
        assert_eq!(
            claude_question_deflection(&root).unwrap(),
            ClaudeQuestionDeflection::Off
        );
        assert_eq!(
            read_json::<serde_json::Value>(&root.join(FILE)).unwrap(),
            serde_json::json!({
                "schema": 1,
                "codex_lifecycle_hooks": "off",
                "claude_question_deflection": "off",
            })
        );
        // A record written before this key existed keeps the default; an unknown value is
        // an error for the getter, as for every setting.
        fs::write(
            root.join(FILE),
            r#"{"schema":1,"codex_lifecycle_hooks":"off"}"#,
        )
        .unwrap();
        assert_eq!(
            claude_question_deflection(&root).unwrap(),
            ClaudeQuestionDeflection::On
        );
        fs::write(
            root.join(FILE),
            r#"{"schema":1,"claude_question_deflection":"sometimes"}"#,
        )
        .unwrap();
        assert!(claude_question_deflection(&root).is_err());
    }

    #[test]
    fn codex_question_notice_defaults_off_and_updates_preserve_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        assert_eq!(
            codex_question_notice(&root).unwrap(),
            CodexQuestionNotice::Off
        );
        assert_eq!(apply(&root, &[]).unwrap()["codex_question_notice"], "off");
        for invalid in [
            vec!["codex-question-notice"],
            vec!["codex-question-notice", "ON"],
            vec!["codex-question-notice", "yes"],
            vec!["codex-question-notice", "off", "extra"],
        ] {
            assert!(apply(&root, &arguments(&invalid)).is_err(), "{invalid:?}");
        }
        assert!(!root.exists(), "an invalid value must not create the root");
        assert_eq!(
            apply(&root, &arguments(&["codex-question-notice", "maybe"]))
                .unwrap_err()
                .to_string(),
            "codex-question-notice is on or off, not \"maybe\""
        );
        apply(&root, &arguments(&["claude-question-deflection", "off"])).unwrap();
        for value in ["on", "off", "on"] {
            assert_eq!(
                apply(
                    &root,
                    &arguments(&["codex-question-notice", value, "--json"])
                )
                .unwrap()["codex_question_notice"],
                value
            );
            assert_eq!(
                claude_question_deflection(&root).unwrap(),
                ClaudeQuestionDeflection::Off
            );
        }
        assert_eq!(
            read_json::<serde_json::Value>(&root.join(FILE)).unwrap(),
            serde_json::json!({
                "schema": 1,
                "claude_question_deflection": "off",
                "codex_question_notice": "on",
            })
        );
        // The two providers' settings are independent keys of one record.
        assert_eq!(
            codex_lifecycle_hooks(&root).unwrap(),
            CodexLifecycleHooks::On
        );
        fs::write(
            root.join(FILE),
            r#"{"schema":1,"claude_question_deflection":"off"}"#,
        )
        .unwrap();
        assert_eq!(
            codex_question_notice(&root).unwrap(),
            CodexQuestionNotice::Off
        );
        fs::write(
            root.join(FILE),
            r#"{"schema":1,"codex_question_notice":"sometimes"}"#,
        )
        .unwrap();
        assert!(codex_question_notice(&root).is_err());
    }

    #[test]
    fn the_tab_window_is_dedicated_until_the_user_chooses_the_current_one() {
        let directory = tempfile::tempdir().unwrap();
        // The state root is created by the first change, not by a look at the settings.
        let root = directory.path().join("native-sessions");
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Dedicated
        );
        let shown = apply(&root, &arguments(&["--json"])).unwrap();
        assert_eq!(shown["windows_tab_window"], "dedicated");
        assert!(!root.exists());

        let changed = apply(&root, &arguments(&["windows-tab-window", "current"])).unwrap();
        assert_eq!(changed["windows_tab_window"], "current");
        assert_eq!(changed["settings_file"], serde_json::json!(root.join(FILE)));
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Current
        );
        assert_eq!(
            read_json::<serde_json::Value>(&root.join(FILE)).unwrap(),
            serde_json::json!({ "schema": 1, "windows_tab_window": "current" })
        );

        let restored = apply(
            &root,
            &arguments(&["windows-tab-window", "dedicated", "--json"]),
        )
        .unwrap();
        assert_eq!(restored["windows_tab_window"], "dedicated");
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Dedicated
        );
    }

    #[test]
    fn an_unknown_setting_or_value_changes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("native-sessions");
        for invalid in [
            &["macos-open-mode"][..],
            &["macos-open-mode", "current"],
            &["macos-open-mode", "tab-first", "extra"],
            &["windows-tab-window"][..],
            &["windows-tab-window", "new"],
            &["windows-tab-window", "current", "extra"],
            &["codex-memory-workspace"][..],
            &["codex-memory-workspace", "refused"],
            &["codex-memory-workspace", "Allow"],
            &["codex-memory-workspace", "allow", "extra"],
            &["tab-window", "current"],
            &["--json", "--json"],
        ] {
            assert!(apply(&root, &arguments(invalid)).is_err(), "{invalid:?}");
        }
        assert!(!root.exists());
    }

    #[test]
    fn a_settings_record_that_cannot_be_understood_is_an_error_not_a_default() {
        let directory = tempfile::tempdir().unwrap();
        for record in [
            r#"{"schema":2,"windows_tab_window":"current"}"#,
            r#"{"schema":1,"windows_tab_window":"somewhere"}"#,
            r#"{"schema":1,"macos_open_mode":"somewhere"}"#,
            r#"{"schema":1,"codex_memory_workspace":"sometimes"}"#,
            r#"{"schema":2,"codex_memory_workspace":"allow"}"#,
            "{",
        ] {
            fs::write(directory.path().join(FILE), record).unwrap();
            assert!(
                codex_memory_workspace(directory.path()).is_err(),
                "{record}"
            );
            assert!(windows_tab_window(directory.path()).is_err(), "{record}");
            assert!(macos_open_mode(directory.path()).is_err(), "{record}");
            assert!(apply(directory.path(), &[]).is_err(), "{record}");
        }
    }

    #[test]
    fn macos_defaults_to_tabs_and_both_settings_survive_independent_updates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("native-sessions");
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::TabFirst);
        assert_eq!(apply(&root, &[]).unwrap()["macos_open_mode"], "tab-first");
        assert!(!root.exists(), "reading defaults must not write settings");

        apply(&root, &arguments(&["windows-tab-window", "current"])).unwrap();
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::TabFirst);
        apply(&root, &arguments(&["macos-open-mode", "new-window"])).unwrap();
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::NewWindow);
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Current
        );
        apply(&root, &arguments(&["windows-tab-window", "dedicated"])).unwrap();
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::NewWindow);
        apply(&root, &arguments(&["macos-open-mode", "tab-first"])).unwrap();
        let reloaded = apply(&root, &[]).unwrap();
        assert_eq!(reloaded["macos_open_mode"], "tab-first");
        assert_eq!(reloaded["windows_tab_window"], "dedicated");
    }

    #[test]
    fn the_codex_memory_workspace_is_refused_until_the_user_allows_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("native-sessions");
        assert_eq!(
            codex_memory_workspace(&root).unwrap(),
            CodexMemoryWorkspace::Refuse
        );
        assert_eq!(
            apply(&root, &[]).unwrap(),
            serde_json::json!({
                "settings_file": root.join(FILE),
                "windows_tab_window": "dedicated",
                "macos_open_mode": "tab-first",
                "codex_memory_workspace": "refuse",
                "codex_lifecycle_hooks": "on",
                "claude_question_deflection": "on",
                "codex_question_notice": "off",
            })
        );
        assert!(!root.exists(), "reading defaults must not write settings");
        let error = apply(&root, &arguments(&["codex-memory-workspace", "always"])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "codex-memory-workspace is refuse or allow, not \"always\""
        );
        assert!(!root.exists(), "an invalid value must not create the root");

        apply(&root, &arguments(&["windows-tab-window", "current"])).unwrap();
        apply(&root, &arguments(&["macos-open-mode", "new-window"])).unwrap();
        let changed = apply(
            &root,
            &arguments(&["codex-memory-workspace", "allow", "--json"]),
        )
        .unwrap();
        assert_eq!(changed["codex_memory_workspace"], "allow");
        assert_eq!(
            read_json::<serde_json::Value>(&root.join(FILE)).unwrap(),
            serde_json::json!({
                "schema": 1,
                "windows_tab_window": "current",
                "macos_open_mode": "new-window",
                "codex_memory_workspace": "allow",
            })
        );
        assert_eq!(
            codex_memory_workspace(&root).unwrap(),
            CodexMemoryWorkspace::Allow
        );
        apply(&root, &arguments(&["codex-memory-workspace", "refuse"])).unwrap();
        assert_eq!(
            codex_memory_workspace(&root).unwrap(),
            CodexMemoryWorkspace::Refuse
        );
        assert_eq!(
            windows_tab_window(&root).unwrap(),
            WindowsTabWindow::Current
        );
        assert_eq!(macos_open_mode(&root).unwrap(), MacosOpenMode::NewWindow);
        // A record written before this key existed keeps the default.
        fs::write(
            root.join(FILE),
            r#"{"schema":1,"macos_open_mode":"tab-first"}"#,
        )
        .unwrap();
        assert_eq!(
            codex_memory_workspace(&root).unwrap(),
            CodexMemoryWorkspace::Refuse
        );
    }
}
