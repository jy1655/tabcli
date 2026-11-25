//! Adapter declarations are portable: a persisted handle can name another platform.
//! Each terminal kind is registered once, in its own `facts.rs`, and every shared
//! lookup (surface facts, host command, launch host, close evidence, declared refusals
//! and binding facts) reads that registration.
use super::TerminalKind;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use super::TerminalSession;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::session::launch::BindingFacts;

#[derive(Clone, Copy)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(in crate::native) struct SurfaceFacts {
    pub(in crate::native::terminal) owner_survival: OwnerSurvival,
    pub(in crate::native) failed_start_identity: bool,
    pub(in crate::native) paste_envelope: PasteEnvelope,
    pub(in crate::native) guards_dialog_input: bool,
}

/// What may remain of the surface when its owner exits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native::terminal) enum OwnerSurvival {
    EndsWithOwner,
    SurfaceCanRemain,
    /// The core keeps a finished terminal until it is explicitly removed.
    CoreRetainsFinished,
}

/// The close evidence a kind selects on macOS and Linux, including creation-proven hosts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(in crate::native) enum CloseEvidenceChoice {
    PresenceOnly,
    PresenceOnlyWithOwnerlessContext,
    AppleTerminal,
    /// Whole host created exclusively by TAB and recorded by incarnation.
    BridgeCreatedHost,
}

/// The local program behind a registered launch-host command. The command form
/// and historical stage records remain readable on every platform.
#[derive(Clone, Copy)]
pub(in crate::native) struct LaunchHost {
    records_capture_stages: bool,
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(super) names_binding: Option<fn(&TerminalSession) -> bool>,
}

impl LaunchHost {
    pub(super) const fn new(
        records_capture_stages: bool,
        #[cfg(any(target_os = "macos", target_os = "linux", windows))] names_binding: Option<
            fn(&TerminalSession) -> bool,
        >,
    ) -> Self {
        Self {
            records_capture_stages,
            #[cfg(any(target_os = "macos", target_os = "linux", windows))]
            names_binding,
        }
    }
}

/// One kind's registration. A refusal of `None` means the kind supports the operation.
/// Dispatch code that reads a declared refusal returns its exact message before calling an adapter.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(in crate::native) struct Registration {
    pub(in crate::native) facts: SurfaceFacts,
    pub(in crate::native) host_command: &'static str,
    /// A recognized launch-host command and record kind, with its local handoff.
    pub(in crate::native) launch_host: Option<LaunchHost>,
    pub(in crate::native) close_evidence: CloseEvidenceChoice,
    pub(in crate::native) screen_read_refusal: Option<&'static str>,
    pub(in crate::native) guarded_input_refusal: Option<&'static str>,
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    pub(in crate::native) binding: Option<BindingFacts>,
}

impl Registration {
    /// The binding facts of a kind whose launch host waits for a surface binding.
    /// Evaluated at compile time by the adapters that have one.
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
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

    /// Whether the launch-host command form and historical stage record can name this kind.
    pub(in crate::native) const fn has_launch_host_form(self) -> bool {
        self.registration().launch_host.is_some()
    }

    /// The launcher initializes stages before the spawn fence for capture hosts.
    pub(in crate::native) const fn records_launch_stages(self) -> bool {
        match self.registration().launch_host {
            Some(host) => host.records_capture_stages,
            None => false,
        }
    }

    /// Whether this machine can run the registered capture or its doctor probe.
    pub(in crate::native) const fn captures_shell_environment(self) -> bool {
        cfg!(target_os = "macos") && self.records_launch_stages()
    }
}

#[path = "macos/apple_terminal/facts.rs"]
pub(super) mod apple_terminal;
#[path = "macos/ghostty/facts.rs"]
pub(super) mod ghostty;
#[path = "herdr/facts.rs"]
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

    #[test]
    fn only_herdr_retains_finished_surfaces_on_its_core() {
        for kind in KINDS {
            assert_eq!(
                kind.registration().facts.owner_survival == OwnerSurvival::CoreRetainsFinished,
                kind == TerminalKind::Herdr,
                "{kind:?}"
            );
        }
    }

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
                    assert!(kind.has_launch_host_form(), "{kind:?}");
                    launch_hosts.push(kind);
                }
                // tmux runs its own pane host, which takes the same flag.
                Ok(HostCommand::Tmux { .. }) => {
                    assert_eq!(kind, TerminalKind::Tmux);
                    assert!(!kind.has_launch_host_form());
                }
                Ok(other) => panic!("{kind:?}: {other:?}"),
                Err(_) => assert!(!kind.has_launch_host_form(), "{kind:?}"),
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
