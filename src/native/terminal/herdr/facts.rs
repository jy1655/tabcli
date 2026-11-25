use crate::native::terminal::facts::{
    CloseEvidenceChoice, LaunchHost, OwnerSurvival, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use crate::native::session::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        owner_survival: OwnerSurvival::CoreRetainsFinished,
        // The core id, the address and the launch host's incarnation name the terminal.
        failed_start_identity: true,
        // `terminal.send` brackets the text itself when the program asked for bracketed
        // paste, and drops an end marker inside it (herdr-core `input::paste`).
        paste_envelope: PasteEnvelope::Raw,
        guards_dialog_input: false,
    },
    host_command: "native-herdr-host",
    launch_host: Some(LaunchHost::new(
        !cfg!(any(target_os = "linux", windows)),
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        Some(crate::native::terminal::herdr::binding_names_this_host),
    )),
    close_evidence: CloseEvidenceChoice::PresenceOnly,
    // `terminal.read` with no history lines returns the visible screen only.
    screen_read_refusal: None,
    guarded_input_refusal: Some(
        "Herdr cannot compare the screen and send a key in one call; answer the dialog in its terminal",
    ),
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    binding: Some(BindingFacts {
        host: "Herdr",
        cancelled: "Herdr launch was cancelled or timed out before surface binding",
        budget: BindingBudget::RemainingReceiptCapped,
    }),
};
