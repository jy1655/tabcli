use crate::native::terminal::facts::{
    CloseEvidenceChoice, LaunchHost, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::session::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        // A finished Herdr terminal stays on the core until it is removed.
        outlives_owner: true,
        // The core id, the address and the launch host's incarnation name the terminal.
        failed_start_identity: true,
        // `terminal.send` brackets the text itself when the program asked for bracketed
        // paste, and drops an end marker inside it (herdr-core `input::paste`).
        paste_envelope: PasteEnvelope::Raw,
        guards_dialog_input: false,
    },
    host_command: "native-herdr-host",
    #[cfg(target_os = "linux")]
    launch_host: Some(LaunchHost::BoundBash(
        crate::native::terminal::herdr::binding_names_this_host,
    )),
    #[cfg(windows)]
    launch_host: Some(LaunchHost::BoundPowerShell(
        crate::native::terminal::herdr::binding_names_this_host,
        &WINDOWS_BINDING,
    )),
    #[cfg(not(any(target_os = "linux", windows)))]
    launch_host: Some(LaunchHost::ShellCapture {
        #[cfg(target_os = "macos")]
        binding_names_host: crate::native::terminal::herdr::binding_names_this_host,
    }),
    // A terminal that ended outside TAB (its tab closed, `hc kill`, or its program
    // exited) stays on the core, finished, until something calls `terminal.rm`.
    close_evidence: CloseEvidenceChoice::PresenceWithEndedSurface,
    // `terminal.read` with no history lines returns the visible screen only.
    screen_read_refusal: None,
    guarded_input_refusal: Some(
        "Herdr cannot compare the screen and send a key in one call; answer the dialog in its terminal",
    ),
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    binding: Some(BindingFacts {
        host: "Herdr",
        cancelled: "Herdr launch was cancelled or timed out before surface binding",
        budget: BindingBudget::RemainingReceiptCapped,
    }),
};

/// The binding the Windows launch host waits for.
#[cfg(windows)]
const WINDOWS_BINDING: BindingFacts = BindingFacts {
    host: "Herdr",
    cancelled: "Herdr launch was cancelled or timed out before surface binding",
    budget: BindingBudget::RemainingReceiptCapped,
};
