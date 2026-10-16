use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(target_os = "macos")]
use crate::native::session::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        outlives_owner: false,
        failed_start_identity: true,
        paste_envelope: PasteEnvelope::Bracketed,
        guards_dialog_input: true,
    },
    host_command: "native-iterm2-host",
    close_evidence: CloseEvidenceChoice::PresenceOnly,
    screen_read_refusal: None,
    guarded_input_refusal: None,
    #[cfg(target_os = "macos")]
    binding: Some(BindingFacts {
        host: "iTerm2",
        cancelled: "iTerm2 launch was cancelled or timed out before surface binding",
        budget: BindingBudget::RemainingReceiptCapped,
    }),
};
