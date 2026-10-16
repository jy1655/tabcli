//! Which macOS terminal a launch uses: the explicit choice, else the host that the
//! caller's environment names, else Terminal.app. Pure: the macOS module reads the
//! environment, every target compiles and tests the decision.
use super::TerminalKind;

pub(super) fn classify_macos_terminal(
    term_program: Option<&str>,
    term: Option<&str>,
    has_iterm_session_id: bool,
    has_term_session_id: bool,
) -> Option<TerminalKind> {
    if let Some(value) = term_program {
        return match value.to_ascii_lowercase().as_str() {
            "ghostty" => Some(TerminalKind::Ghostty),
            "warpterminal" | "warp" => Some(TerminalKind::Warp),
            // WezTerm sets TERM_PROGRAM=WezTerm for every program it starts
            // (config/src/config.rs, `apply_cmd_defaults`).
            "wezterm" => Some(TerminalKind::WezTerm),
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

pub(super) fn select_macos_terminal(
    preferred: Option<TerminalKind>,
    term_program: Option<&str>,
    term: Option<&str>,
    has_iterm_session_id: bool,
    has_term_session_id: bool,
) -> TerminalKind {
    if let Some(preferred) = preferred {
        return preferred;
    }
    match classify_macos_terminal(
        term_program,
        term,
        has_iterm_session_id,
        has_term_session_id,
    ) {
        None => TerminalKind::AppleTerminal,
        Some(kind) => kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            classify_macos_terminal(Some("WarpTerminal"), Some("xterm-256color"), false, true),
            Some(TerminalKind::Warp)
        );
        assert_eq!(
            classify_macos_terminal(Some("WezTerm"), Some("xterm-256color"), false, false),
            Some(TerminalKind::WezTerm)
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
        assert_eq!(
            select_macos_terminal(None, Some("ghostty"), Some("xterm-ghostty"), false, false,),
            TerminalKind::Ghostty
        );
        assert_eq!(
            select_macos_terminal(
                None,
                Some("WarpTerminal"),
                Some("xterm-256color"),
                false,
                false,
            ),
            TerminalKind::Warp
        );
    }

    #[test]
    fn ghostty_host_routes_to_its_native_adapter_without_terminal_fallback() {
        assert_eq!(
            select_macos_terminal(None, Some("ghostty"), Some("xterm-ghostty"), false, false),
            TerminalKind::Ghostty
        );
        assert_eq!(
            TerminalKind::Ghostty.supported_on_this_platform(),
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn explicit_warp_target_is_independent_of_the_invoking_host() {
        for (host, program, term, iterm, terminal) in [
            (
                "Warp",
                Some("WarpTerminal"),
                Some("xterm-256color"),
                false,
                false,
            ),
            (
                "iTerm2",
                Some("iTerm.app"),
                Some("xterm-256color"),
                true,
                true,
            ),
            (
                "Terminal.app",
                Some("Apple_Terminal"),
                Some("xterm-256color"),
                false,
                true,
            ),
            (
                "WezTerm",
                Some("WezTerm"),
                Some("xterm-256color"),
                false,
                false,
            ),
            ("empty", None, None, false, false),
            (
                "conflicting",
                Some("Apple_Terminal"),
                Some("xterm-ghostty"),
                true,
                true,
            ),
        ] {
            assert_eq!(
                select_macos_terminal(Some(TerminalKind::Warp), program, term, iterm, terminal),
                TerminalKind::Warp,
                "explicit Warp target was changed by the {host} invoking environment"
            );
        }
    }

    #[test]
    fn an_explicit_wezterm_target_wins_in_every_invoking_terminal() {
        // TERM_PROGRAM, TERM, whether ITERM_SESSION_ID and TERM_SESSION_ID are set, and
        // what the invoking terminal selects when nothing is asked for.
        let xterm = Some("xterm-256color");
        let hosts = [
            (Some("WezTerm"), xterm, false, false, TerminalKind::WezTerm),
            (Some("iTerm.app"), xterm, true, true, TerminalKind::Iterm2),
            (
                Some("Apple_Terminal"),
                xterm,
                false,
                true,
                TerminalKind::AppleTerminal,
            ),
            (
                Some("vscode"),
                xterm,
                false,
                false,
                TerminalKind::AppleTerminal,
            ),
            (None, None, false, false, TerminalKind::AppleTerminal),
            // A multiplexer started in iTerm2 and attached from WezTerm keeps both.
            (Some("WezTerm"), xterm, true, true, TerminalKind::WezTerm),
        ];
        for (term_program, term, iterm, apple, detected) in hosts {
            let select =
                |preferred| select_macos_terminal(preferred, term_program, term, iterm, apple);
            assert_eq!(select(None), detected, "{term_program:?}");
            assert_eq!(
                select(Some(TerminalKind::WezTerm)),
                TerminalKind::WezTerm,
                "{term_program:?}"
            );
            // And the other way: a terminal asked for from inside WezTerm is that one.
            assert_eq!(
                select(Some(TerminalKind::Iterm2)),
                TerminalKind::Iterm2,
                "{term_program:?}"
            );
        }
    }
}
