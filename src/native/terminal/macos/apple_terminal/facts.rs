use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(target_os = "macos")]
use crate::native::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        outlives_owner: true,
        failed_start_identity: false,
        paste_envelope: PasteEnvelope::Bracketed,
        guards_dialog_input: true,
    },
    host_command: "native-terminal-host",
    close_evidence: CloseEvidenceChoice::AppleTerminal,
    screen_read_refusal: None,
    guarded_input_refusal: None,
    #[cfg(target_os = "macos")]
    binding: Some(BindingFacts {
        host: "Terminal.app",
        cancelled: "Terminal.app launch was cancelled or timed out before its surface was bound",
        budget: BindingBudget::StartTimeout,
    }),
};
