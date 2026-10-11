//! Adapter declarations are portable: a persisted handle can name another platform.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(in crate::native) struct SurfaceFacts {
    pub(in crate::native) outlives_owner: bool,
    pub(in crate::native) failed_start_identity: bool,
    pub(in crate::native) requires_app_incarnation: bool,
    pub(in crate::native) pre_close: PreClosePolicy,
    pub(in crate::native) paste_envelope: PasteEnvelope,
    pub(in crate::native) guards_dialog_input: bool,
}

#[derive(Clone, Copy)]
pub(in crate::native) enum PreClosePolicy {
    None,
    RecordIntent,
    StopForegroundGroup,
    StopOwnerAndShellGroups,
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
