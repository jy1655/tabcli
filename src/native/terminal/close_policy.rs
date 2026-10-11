//! Adapter declarations are portable: a persisted handle can name another platform.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) struct ClosePolicy {
    pub(super) outlives_owner: bool,
    pub(super) failed_start_identity: bool,
    pub(super) requires_app_incarnation: bool,
    pub(super) pre_close: PreClosePolicy,
}

#[derive(Clone, Copy)]
pub(super) enum PreClosePolicy {
    None,
    RecordIntent,
    StopForegroundGroup,
    StopOwnerAndShellGroups,
}

#[path = "macos/apple_terminal/close_policy.rs"]
pub(super) mod apple_terminal;
#[path = "macos/ghostty/close_policy.rs"]
pub(super) mod ghostty;
#[path = "macos/iterm2/close_policy.rs"]
pub(super) mod iterm2;
#[path = "macos/warp/close_policy.rs"]
pub(super) mod warp;
#[path = "macos/wezterm/close_policy.rs"]
pub(super) mod wezterm;
#[path = "windows/close_policy.rs"]
pub(super) mod windows;
