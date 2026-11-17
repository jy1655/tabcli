//! OS process/TTY reads used by the shared proof; birth-only reads never require a TTY.
#[cfg(target_os = "linux")]
pub(in crate::native) use super::linux::process::{
    current_terminal_tty, live_native_process_identity, terminal_tty_device,
};
#[cfg(target_os = "macos")]
pub(in crate::native) use super::macos::process::{
    current_terminal_tty, live_native_process_identity, terminal_tty_device,
};
