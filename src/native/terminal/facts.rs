//! Adapter declarations are portable: a persisted handle can name another platform.
//! Each terminal kind is registered once, in its own `facts.rs`, and every shared
//! lookup (surface facts, host command, launch host, close evidence, declared refusals
//! and the macOS binding facts) reads that registration.
use super::TerminalKind;
#[cfg(target_os = "macos")]
use super::TerminalSession;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::session::launch::BindingFacts;

#[derive(Clone, Copy)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(in crate::native) struct SurfaceFacts {
    pub(in crate::native) outlives_owner: bool,
    pub(in crate::native) failed_start_identity: bool,
    pub(in crate::native) paste_envelope: PasteEnvelope,
    pub(in crate::native) guards_dialog_input: bool,
}

/// The close evidence a kind selects; resolved to an implementation only on macOS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(in crate::native) enum CloseEvidenceChoice {
    PresenceOnly,
    PresenceOnlyWithOwnerlessContext,
    AppleTerminal,
    /// Whole host created exclusively by Bridge and recorded by incarnation.
    BridgeCreatedHost,
}

/// A kind whose launch runs the terminal-less launch host (`terminal/macos/launch_host.rs`)
/// as its surface program. The launcher records `launch-host.json` for it, its host command
/// takes `--launch-command`, the record names it, and doctor reports its recorded stage.
pub(in crate::native) struct LaunchHostFacts {
    /// Whether a bound surface names the terminal surface that the calling host runs in, as
    /// the terminal names that surface in the host's environment. The adapter owns this
    /// check; its legacy host decides with the same rule.
    #[cfg(target_os = "macos")]
    pub(in crate::native) binding_names_host: fn(&TerminalSession) -> bool,
}

/// One kind's registration. A refusal of `None` means the kind supports the
/// operation; `Some` is the exact message the shared macOS dispatch answers with
/// before any adapter is reached.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(in crate::native) struct Registration {
    pub(in crate::native) facts: SurfaceFacts,
    pub(in crate::native) host_command: &'static str,
    /// `Some` when the kind's launch runs the launch host.
    pub(in crate::native) launch_host: Option<LaunchHostFacts>,
    pub(in crate::native) close_evidence: CloseEvidenceChoice,
    pub(in crate::native) screen_read_refusal: Option<&'static str>,
    pub(in crate::native) guarded_input_refusal: Option<&'static str>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(in crate::native) binding: Option<BindingFacts>,
}

impl Registration {
    /// The binding facts of a kind whose launch host waits for a surface binding.
    /// Evaluated at compile time by the adapters that have one.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
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
            Self::Tmux => &tmux::REGISTRATION,
            Self::Herdr => &herdr::REGISTRATION,
        }
    }

    /// Whether this kind's launch runs the terminal-less launch host.
    pub(in crate::native) const fn uses_launch_host(self) -> bool {
        self.registration().launch_host.is_some()
    }
}

#[path = "macos/apple_terminal/facts.rs"]
pub(super) mod apple_terminal;
#[path = "macos/ghostty/facts.rs"]
pub(super) mod ghostty;
#[path = "macos/herdr/facts.rs"]
pub(super) mod herdr;
#[path = "macos/iterm2/facts.rs"]
pub(super) mod iterm2;
#[path = "linux/tmux/facts.rs"]
pub(super) mod tmux;
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

    const KINDS: [TerminalKind; 8] = [
        TerminalKind::Iterm2,
        TerminalKind::AppleTerminal,
        TerminalKind::Ghostty,
        TerminalKind::Warp,
        TerminalKind::WezTerm,
        TerminalKind::WindowsConsole,
        TerminalKind::Tmux,
        TerminalKind::Herdr,
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

    // A kind uses the launch host exactly when its host command takes the launch form, so
    // the fact and the argv parse cannot drift.
    #[test]
    fn launch_host_fact_agrees_with_the_host_command_parse() {
        use super::super::host::{HostCommand, parse};
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("session-facts");
        let launch_form = [
            directory.to_str().unwrap().to_owned(),
            "--launch-command".to_owned(),
            "script".to_owned(),
        ];
        let mut launch_hosts = Vec::new();
        for kind in KINDS {
            match parse(kind.registration().host_command, &launch_form) {
                Ok(HostCommand::Launch { kind: parsed, .. }) => {
                    assert_eq!(parsed, kind);
                    assert!(kind.uses_launch_host(), "{kind:?}");
                    launch_hosts.push(kind);
                }
                // tmux runs its own pane host, which takes the same flag.
                Ok(HostCommand::Tmux { .. }) => {
                    assert_eq!(kind, TerminalKind::Tmux);
                    assert!(!kind.uses_launch_host());
                }
                Ok(other) => panic!("{kind:?}: {other:?}"),
                Err(_) => assert!(!kind.uses_launch_host(), "{kind:?}"),
            }
        }
        assert_eq!(
            launch_hosts,
            [
                TerminalKind::Iterm2,
                TerminalKind::WezTerm,
                TerminalKind::Herdr
            ]
        );
    }
}
