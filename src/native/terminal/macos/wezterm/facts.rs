use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(target_os = "macos")]
use crate::native::session::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        outlives_owner: true,
        failed_start_identity: false,
        paste_envelope: PasteEnvelope::Bracketed,
        guards_dialog_input: false,
    },
    host_command: "native-wezterm-host",
    close_evidence: CloseEvidenceChoice::PresenceOnlyWithOwnerlessContext,
    // WezTerm reads its screen through `get-text`; only the one-call dialog answer is missing.
    screen_read_refusal: None,
    guarded_input_refusal: Some(
        "WezTerm cannot compare the screen and send a key in one call; answer the dialog in its pane",
    ),
    #[cfg(target_os = "macos")]
    binding: Some(BindingFacts {
        host: "WezTerm",
        cancelled: "WezTerm launch was cancelled or timed out before surface binding",
        budget: BindingBudget::RemainingReceiptCapped,
    }),
};
