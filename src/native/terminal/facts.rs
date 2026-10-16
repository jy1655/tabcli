//! Adapter declarations are portable: a persisted handle can name another platform.
//! Each terminal kind is registered once, in its own `facts.rs`, and every shared
//! lookup (surface facts, host command, close evidence, declared refusals and the
//! macOS binding facts) reads that registration.
use super::TerminalKind;
#[cfg(target_os = "macos")]
use crate::native::session::launch::BindingFacts;

#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(in crate::native) struct SurfaceFacts {
    pub(in crate::native) outlives_owner: bool,
    pub(in crate::native) failed_start_identity: bool,
    pub(in crate::native) paste_envelope: PasteEnvelope,
    pub(in crate::native) guards_dialog_input: bool,
}

/// The close evidence a kind selects; resolved to an implementation only on macOS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(in crate::native) enum CloseEvidenceChoice {
    PresenceOnly,
    PresenceOnlyWithOwnerlessContext,
    AppleTerminal,
}

/// One kind's registration. A refusal of `None` means the kind supports the
/// operation; `Some` is the exact message the shared macOS dispatch answers with
/// before any adapter is reached.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(in crate::native) struct Registration {
    pub(in crate::native) facts: SurfaceFacts,
    pub(in crate::native) host_command: &'static str,
    pub(in crate::native) close_evidence: CloseEvidenceChoice,
    pub(in crate::native) screen_read_refusal: Option<&'static str>,
    pub(in crate::native) guarded_input_refusal: Option<&'static str>,
    #[cfg(target_os = "macos")]
    pub(in crate::native) binding: Option<BindingFacts>,
}

impl Registration {
    /// The binding facts of a kind whose launch host waits for a surface binding.
    /// Evaluated at compile time by the adapters that have one.
    #[cfg(target_os = "macos")]
    pub(in crate::native) const fn binding(&'static self) -> &'static BindingFacts {
        match &self.binding {
            Some(binding) => binding,
            None => panic!("this terminal kind declares no launch binding"),
        }
    }
}

impl TerminalKind {
    /// The one exhaustive lookup from a kind to its registration.
    pub(in crate::native) const fn registration(self) -> &'static Registration {
        match self {
            Self::Iterm2 => &iterm2::REGISTRATION,
            Self::AppleTerminal => &apple_terminal::REGISTRATION,
            Self::Ghostty => &ghostty::REGISTRATION,
            Self::Warp => &warp::REGISTRATION,
            Self::WezTerm => &wezterm::REGISTRATION,
            Self::WindowsConsole => &windows::REGISTRATION,
        }
    }
}

#[path = "macos/apple_terminal/facts.rs"]
pub(super) mod apple_terminal;
#[path = "macos/ghostty/facts.rs"]
pub(super) mod ghostty;
#[path = "macos/iterm2/facts.rs"]
pub(super) mod iterm2;
#[path = "macos/warp/facts.rs"]
pub(super) mod warp;
#[path = "macos/wezterm/facts.rs"]
pub(super) mod wezterm;
#[path = "windows/facts.rs"]
pub(super) mod windows;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum PasteEnvelope {
    Bracketed,
    Raw,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [TerminalKind; 6] = [
        TerminalKind::Iterm2,
        TerminalKind::AppleTerminal,
        TerminalKind::Ghostty,
        TerminalKind::Warp,
        TerminalKind::WezTerm,
        TerminalKind::WindowsConsole,
    ];

    // A kind that guards dialog input declares no refusal; a kind that refuses screen
    // reads cannot answer a dialog either. The messages themselves are asserted where
    // the shared dispatch answers with them.
    #[test]
    fn declared_refusals_agree_with_the_surface_facts() {
        for kind in KINDS {
            let registration = kind.registration();
            if registration.facts.guards_dialog_input {
                assert!(registration.screen_read_refusal.is_none(), "{kind:?}");
                assert!(registration.guarded_input_refusal.is_none(), "{kind:?}");
            } else {
                assert!(registration.guarded_input_refusal.is_some(), "{kind:?}");
            }
            if registration.screen_read_refusal.is_some() {
                assert!(registration.guarded_input_refusal.is_some(), "{kind:?}");
            }
        }
        let host_commands: std::collections::BTreeSet<_> = KINDS
            .iter()
            .map(|kind| kind.registration().host_command)
            .collect();
        assert_eq!(host_commands.len(), KINDS.len());
    }
}
