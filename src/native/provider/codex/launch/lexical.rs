//! Pinned Codex screen lexemes. Callers own readiness, span and overlay policy.
// Empty chat composer; see docs/verification/2026-10-02-windows.md.
pub(super) const COMPOSER_ROW: &str = "› Ask Codex to do anything";
pub(super) const REVIEW_HOOKS: &str = "Review hooks";
pub(super) const TRUST_ALL_AND_CONTINUE: &str = "Trust all and continue";

pub(super) fn contains_hook_review_label(screen: &str) -> bool {
    screen.contains(REVIEW_HOOKS) || screen.contains(TRUST_ALL_AND_CONTINUE)
}

// Draft lexemes are used only by the Windows paste route and its portable tests.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) fn numbered_label(text: &str) -> Option<&str> {
    let (n, label) = text.split_once(". ")?;
    (!n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())).then_some(label)
}
// Pinned startup-hook and trust-directory options, including wrapped label prefixes.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) fn pinned_option(label: &str) -> bool {
    [
        REVIEW_HOOKS,
        TRUST_ALL_AND_CONTINUE,
        "Continue without trusting",
        "Trust and continue",
        "Open existing task",
        "Open restricted",
        "Back to Agent Command Center",
        "Keep current directory",
        "Quit",
        "Don't trust",
    ]
    .iter()
    .any(|prefix| label.starts_with(prefix))
}
// Fragments survive wrapping and clipped frames; key names come from the keymap.
// The caller decides whether a matching row is content or unresolved UI.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) fn confirmation_hint(text: &str) -> bool {
    let text = text.trim().strip_prefix("› ").unwrap_or(text.trim());
    fn action(value: &str) -> bool {
        matches!(
            value,
            "confirm" | "continue" | "skip" | "quit" | "back" | "cancel"
        )
    }
    fn key_token(value: &str) -> bool {
        let key = value.rsplit('+').next().unwrap_or(value);
        key.chars().count() == 1
            || matches!(
                key,
                "enter"
                    | "esc"
                    | "escape"
                    | "tab"
                    | "space"
                    | "backspace"
                    | "delete"
                    | "insert"
                    | "home"
                    | "end"
                    | "pageup"
                    | "pagedown"
                    | "pgup"
                    | "pgdn"
                    | "up"
                    | "down"
                    | "left"
                    | "right"
            )
            || key
                .strip_prefix('f')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
    }
    fn keyed_action(value: &str) -> bool {
        value.rsplit_once(' ').is_some_and(|(keys, verb)| {
            keys.split_whitespace().next().is_some()
                && keys.split_whitespace().all(key_token)
                && action(verb)
        })
    }
    // Keep terminal separators too: trimming a wrapped row removes their final space.
    let separated = (text.contains(" · ") || text.ends_with(" ·"))
        && text.split('·').any(|part| {
            let part = part.trim();
            action(part) || keyed_action(part)
        });
    action(text) || separated || keyed_action(text) || text.contains("and create sandbox")
}
