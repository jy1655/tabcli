use std::{path::Path, str::FromStr};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub(super) mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unsupported as platform;
#[cfg(target_os = "windows")]
use windows as platform;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TerminalKind {
    Iterm2,
    AppleTerminal,
    Ghostty,
    WindowsConsole,
}

impl TerminalKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Iterm2 => "iterm2",
            Self::AppleTerminal => "apple-terminal",
            Self::Ghostty => "ghostty",
            Self::WindowsConsole => "windows-console",
        }
    }

    pub(super) const fn display_name(self) -> &'static str {
        match self {
            Self::Iterm2 => "iTerm2",
            Self::AppleTerminal => "Terminal.app",
            Self::Ghostty => "Ghostty",
            Self::WindowsConsole => "Windows Console",
        }
    }
}

impl FromStr for TerminalKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "ghostty" => Ok(Self::Ghostty),
            "windows-console" | "windows" | "console" => Ok(Self::WindowsConsole),
            "iterm" | "iterm.app" | "iterm2" => Ok(Self::Iterm2),
            "apple-terminal" | "apple_terminal" | "default" | "terminal" | "terminal.app" => {
                Ok(Self::AppleTerminal)
            }
            _ => Err(format!(
                "unsupported terminal {value:?}; expected ghostty, iterm2, or terminal"
            )),
        }
    }
}

const fn legacy_iterm2_kind() -> TerminalKind {
    TerminalKind::Iterm2
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct TerminalSession {
    #[serde(default = "legacy_iterm2_kind", rename = "terminal")]
    pub(super) kind: TerminalKind,
    #[serde(alias = "iterm_session_id", rename = "session_id")]
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) managed_session_id: Option<String>,
}

impl TerminalSession {
    pub(super) fn verify_managed_session(&self, expected: &str) -> Result<()> {
        match self.managed_session_id.as_deref() {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => {
                bail!("terminal handle belongs to managed session {actual}, not {expected}")
            }
            None if self.kind == TerminalKind::Iterm2 => Ok(()),
            None => bail!("terminal handle is missing its managed session binding"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) enum CloseOutcome {
    Closed,
    Missing,
}

pub(super) fn select(preferred: Option<TerminalKind>) -> Result<TerminalKind> {
    platform::select(preferred)
}

pub(super) fn open_tab(kind: TerminalKind, command: &str) -> Result<TerminalSession> {
    platform::open_tab(kind, command)
}

pub(super) fn send_file(session: &TerminalSession, prompt_path: &Path) -> Result<()> {
    platform::send_file(session, prompt_path)
}

pub(super) fn close_session(session: &TerminalSession) -> Result<CloseOutcome> {
    platform::close_session(session)
}

#[cfg(target_os = "windows")]
pub(super) fn windows_console_control(action: &str, pid: u32, input: Option<&str>) -> Result<()> {
    windows::console_control(action, pid, input)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn classify_macos_terminal(
    term_program: Option<&str>,
    term: Option<&str>,
    has_iterm_session_id: bool,
    has_term_session_id: bool,
) -> Option<TerminalKind> {
    if let Some(value) = term_program {
        return match value.to_ascii_lowercase().as_str() {
            "ghostty" => Some(TerminalKind::Ghostty),
            "iterm" | "iterm.app" | "iterm2" => Some(TerminalKind::Iterm2),
            "apple_terminal" | "terminal" | "terminal.app" => Some(TerminalKind::AppleTerminal),
            _ => None,
        };
    }
    if term.is_some_and(|value| value.eq_ignore_ascii_case("xterm-ghostty")) {
        return Some(TerminalKind::Ghostty);
    }
    if has_iterm_session_id {
        return Some(TerminalKind::Iterm2);
    }
    has_term_session_id.then_some(TerminalKind::AppleTerminal)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn select_macos_terminal(
    preferred: Option<TerminalKind>,
    term_program: Option<&str>,
    term: Option<&str>,
    has_iterm_session_id: bool,
    has_term_session_id: bool,
) -> TerminalKind {
    preferred
        .or_else(|| {
            classify_macos_terminal(
                term_program,
                term,
                has_iterm_session_id,
                has_term_session_id,
            )
        })
        .unwrap_or(TerminalKind::AppleTerminal)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    #[cfg(target_os = "macos")]
    use super::macos;
    use super::{TerminalKind, TerminalSession, classify_macos_terminal, select_macos_terminal};

    #[test]
    fn macos_terminal_detection_recognizes_each_supported_host() {
        assert_eq!(
            classify_macos_terminal(Some("ghostty"), Some("xterm-ghostty"), false, true),
            Some(TerminalKind::Ghostty)
        );
        assert_eq!(
            classify_macos_terminal(Some("iTerm.app"), Some("xterm-256color"), true, true),
            Some(TerminalKind::Iterm2)
        );
        assert_eq!(
            classify_macos_terminal(Some("Apple_Terminal"), Some("xterm-256color"), false, true),
            Some(TerminalKind::AppleTerminal)
        );
        assert_eq!(
            classify_macos_terminal(Some("vscode"), Some("xterm-256color"), false, false),
            None
        );
        assert_eq!(
            classify_macos_terminal(Some("vscode"), Some("xterm-ghostty"), false, true),
            None
        );
    }

    #[test]
    fn macos_terminal_selection_honors_explicit_choice_and_falls_back_safely() {
        assert_eq!(
            select_macos_terminal(
                Some(TerminalKind::Ghostty),
                Some("Apple_Terminal"),
                Some("xterm-256color"),
                false,
                true,
            ),
            TerminalKind::Ghostty
        );
        assert_eq!(
            select_macos_terminal(None, Some("vscode"), Some("xterm-256color"), false, false,),
            TerminalKind::AppleTerminal
        );
        assert_eq!(
            select_macos_terminal(None, None, None, false, false),
            TerminalKind::AppleTerminal
        );
    }

    #[test]
    fn explicit_terminal_names_have_stable_canonical_values() {
        for alias in ["iterm", "iTerm2", "iTerm.app"] {
            assert_eq!(TerminalKind::from_str(alias), Ok(TerminalKind::Iterm2));
        }
        for alias in ["terminal", "Terminal.app", "apple-terminal", "default"] {
            assert_eq!(
                TerminalKind::from_str(alias),
                Ok(TerminalKind::AppleTerminal)
            );
        }
        assert_eq!(TerminalKind::from_str("Ghostty"), Ok(TerminalKind::Ghostty));
        assert_eq!(
            TerminalKind::from_str("windows-console"),
            Ok(TerminalKind::WindowsConsole)
        );
        assert!(TerminalKind::from_str("vscode").is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_selects_its_managed_console_transport() {
        assert_eq!(super::select(None).unwrap(), TerminalKind::WindowsConsole);
        assert_eq!(
            super::select(Some(TerminalKind::WindowsConsole)).unwrap(),
            TerminalKind::WindowsConsole
        );
        assert!(super::select(Some(TerminalKind::Iterm2)).is_err());
    }

    #[test]
    fn terminal_session_records_preserve_legacy_iterm_state() {
        let legacy: TerminalSession =
            serde_json::from_str(r#"{"iterm_session_id":"legacy-session"}"#).unwrap();
        assert_eq!(legacy.kind, TerminalKind::Iterm2);
        assert_eq!(legacy.id, "legacy-session");
        assert_eq!(legacy.tab_id, None);
        assert_eq!(legacy.window_id, None);
        assert_eq!(legacy.managed_session_id, None);

        let ghostty = TerminalSession {
            kind: TerminalKind::Ghostty,
            id: "terminal-id".to_owned(),
            tab_id: Some("tab-id".to_owned()),
            window_id: Some("window-id".to_owned()),
            managed_session_id: None,
        };
        assert_eq!(
            serde_json::to_value(&ghostty).unwrap(),
            serde_json::json!({
                "terminal": "ghostty",
                "session_id": "terminal-id",
                "tab_id": "tab-id",
                "window_id": "window-id"
            })
        );

        let bound: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal",
            "session_id": "/dev/ttys001",
            "window_id": "1001",
            "managed_session_id": "session-owner123"
        }))
        .unwrap();
        assert!(bound.verify_managed_session("session-owner123").is_ok());
        assert!(bound.verify_managed_session("session-other456").is_err());
        let bound = serde_json::to_value(bound).unwrap();
        assert_eq!(bound["managed_session_id"], "session-owner123");
        assert!(ghostty.verify_managed_session("session-owner123").is_err());
    }

    #[test]
    fn terminal_records_discard_legacy_visible_title_metadata() {
        let session: TerminalSession = serde_json::from_value(serde_json::json!({
            "terminal": "apple-terminal",
            "session_id": "/dev/ttys001",
            "window_id": "1001",
            "managed_session_id": "session-owner123",
            "ownership_title": "legacy-visible-marker"
        }))
        .unwrap();

        let record = serde_json::to_value(session).unwrap();
        assert!(record.get("ownership_title").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_open_uses_separate_create_discover_queue_and_enter_transactions() {
        let create = macos::ghostty::CREATE_SURFACE_SCRIPT;
        assert!(create.contains("set targetWindow to new window"));
        assert!(create.contains("set targetTab to new tab in targetWindow"));
        assert!(create.contains("id of targetTab"));
        assert!(create.contains("id of targetWindow"));
        assert!(!create.contains("focused terminal"));
        assert!(!create.contains("input text"));
        assert!(!create.contains("send key"));

        let discover = macos::ghostty::DISCOVER_TERMINAL_SCRIPT;
        assert!(discover.contains("first window whose id is wantedWindowId"));
        assert!(discover.contains("if id of candidateTab is wantedTabId then"));
        assert!(discover.contains("focused terminal of targetTab"));
        assert!(discover.contains("return \"not-ready\""));
        assert!(!discover.contains("new tab"));
        assert!(!discover.contains("input text"));
        assert!(!discover.contains("send key"));

        let queue = macos::ghostty::QUEUE_COMMAND_SCRIPT;
        for proof in ["wantedTerminalId", "wantedTabId", "wantedWindowId"] {
            assert!(queue.contains(proof));
        }
        assert!(queue.contains("input text bridgeCommand to targetTerminal"));
        assert!(queue.contains("if errorNumber is -10000 then return \"not-ready\""));
        assert_eq!(
            queue
                .matches("input text bridgeCommand to targetTerminal")
                .count(),
            1
        );
        assert!(!queue.contains("send key"));
        assert!(!queue.contains("new tab"));
        assert!(!queue.contains("focused terminal"));

        let press_enter = macos::ghostty::PRESS_ENTER_SCRIPT;
        for proof in ["wantedTerminalId", "wantedTabId", "wantedWindowId"] {
            assert!(press_enter.contains(proof));
        }
        assert!(!press_enter.contains("input text"));
        assert_eq!(
            press_enter
                .matches("send key \"enter\" to targetTerminal")
                .count(),
            1
        );
        assert!(!press_enter.contains("new tab"));
        assert!(!press_enter.contains("focused terminal"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_adapter_targets_stable_ids_without_interpolating_input() {
        assert!(macos::ghostty::CREATE_SURFACE_SCRIPT.contains("new tab in targetWindow"));
        assert!(macos::ghostty::CREATE_SURFACE_SCRIPT.contains("id of targetTab"));
        assert!(macos::ghostty::CREATE_SURFACE_SCRIPT.contains("id of targetWindow"));
        assert!(macos::ghostty::DISCOVER_TERMINAL_SCRIPT.contains("item 1 of argv"));
        assert!(macos::ghostty::DISCOVER_TERMINAL_SCRIPT.contains("id of targetTerminal"));
        assert!(macos::ghostty::QUEUE_COMMAND_SCRIPT.contains("item 4 of argv"));
        assert!(macos::ghostty::PRESS_ENTER_SCRIPT.contains("item 3 of argv"));
        assert!(macos::ghostty::SEND_FILE_SCRIPT.contains("input text promptText"));
        assert!(macos::ghostty::SEND_FILE_SCRIPT.contains("send key \"enter\""));
        assert!(macos::ghostty::CLOSE_TAB_SCRIPT.contains("close tab targetTab"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ghostty_open_never_uses_surface_configuration_initial_input() {
        for script in [
            macos::ghostty::CREATE_SURFACE_SCRIPT,
            macos::ghostty::DISCOVER_TERMINAL_SCRIPT,
            macos::ghostty::QUEUE_COMMAND_SCRIPT,
            macos::ghostty::PRESS_ENTER_SCRIPT,
        ] {
            assert!(!script.contains("new surface configuration"));
            assert!(!script.contains("initial input"));
            assert!(!script.contains("bridgeConfiguration"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apple_terminal_adapter_targets_only_its_created_tty() {
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("item 1 of argv"));
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("do script bridgeCommand"));
        assert!(
            !macos::apple_terminal::OPEN_TAB_SCRIPT
                .contains("do script bridgeCommand in front window")
        );
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("System Events"));
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("front window"));
        assert!(!macos::apple_terminal::OPEN_TAB_SCRIPT.contains("selected tab"));
        assert!(
            macos::apple_terminal::OPEN_TAB_SCRIPT
                .contains("set targetTab to do script bridgeCommand")
        );
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("tty of targetTab"));
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("windowIdForTty(targetTty)"));
        assert!(macos::apple_terminal::OPEN_TAB_SCRIPT.contains("id of targetWindow"));
        assert!(
            macos::apple_terminal::SEND_FILE_SCRIPT.contains("tty of candidateTab is wantedTty")
        );
        assert!(
            macos::apple_terminal::SEND_FILE_SCRIPT.contains("do script promptText in targetTab")
        );
        assert!(macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("set controlC to character id 3"));
        assert!(
            macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("do script controlC in targetTab")
        );
        assert!(
            macos::apple_terminal::CLOSE_TAB_SCRIPT
                .contains("if (count of tabs of targetWindow) is not 1 then error")
        );
        assert_eq!(
            macos::apple_terminal::CLOSE_TAB_SCRIPT
                .matches("if (count of tabs of targetWindow) is not 1 then error")
                .count(),
            2,
            "Terminal.app must re-check the one-tab invariant immediately before close"
        );
        let close_script = macos::apple_terminal::CLOSE_TAB_SCRIPT;
        let process_stopped = close_script
            .find("if busy of targetTab then error")
            .expect("busy-process finality check");
        let final_window = close_script
            .rfind("if id of targetWindow is not wantedWindowId then return \"missing\"")
            .expect("final window identity check");
        let final_tty = close_script
            .rfind("if tty of targetTab is not wantedTty then return \"missing\"")
            .expect("final tty identity check");
        let final_tab_count = close_script
            .rfind("if (count of tabs of targetWindow) is not 1 then error")
            .expect("final one-tab check");
        let close_window = close_script
            .rfind("close targetWindow")
            .expect("native window close");
        assert!(
            process_stopped < final_window
                && final_window < final_tty
                && final_tty < final_tab_count
                && final_tab_count < close_window,
            "Terminal.app must repeat the full proof in order immediately before close"
        );
        assert!(macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("close targetWindow"));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("System Events"));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("keystroke \"w\""));
        assert!(!macos::apple_terminal::CLOSE_TAB_SCRIPT.contains("close targetTab"));
    }
}
