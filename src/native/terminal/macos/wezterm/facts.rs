use crate::native::terminal::facts::{
    CloseEvidenceChoice, LaunchHost, OwnerSurvival, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(target_os = "macos")]
use crate::native::session::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        owner_survival: OwnerSurvival::SurfaceCanRemain,
        failed_start_identity: false,
        paste_envelope: PasteEnvelope::Bracketed,
        guards_dialog_input: false,
    },
    host_command: "native-wezterm-host",
    launch_host: Some(LaunchHost::new(
        true,
        #[cfg(target_os = "macos")]
        Some(crate::native::terminal::macos::wezterm::binding_names_this_host),
        #[cfg(any(target_os = "linux", windows))]
        None,
    )),
    close_evidence: CloseEvidenceChoice::PresenceOnlyWithOwnerlessContext,
    // WezTerm reads its screen through `get-text`; only the one-call dialog answer is missing.
    screen_read_refusal: None,
    guarded_input_refusal: Some(
        "WezTerm cannot compare the screen and send a key in one call; answer the dialog in its pane",
    ),
    #[cfg(any(target_os = "linux", windows))]
    binding: None,
    #[cfg(target_os = "macos")]
    binding: Some(BindingFacts {
        host: "WezTerm",
        cancelled: "WezTerm launch was cancelled or timed out before surface binding",
        budget: BindingBudget::RemainingReceiptCapped,
    }),
};
