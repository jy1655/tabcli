use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        outlives_owner: false,
        failed_start_identity: true,
        // Console key events would expose bracketed-paste escapes as literal text.
        paste_envelope: PasteEnvelope::Raw,
        guards_dialog_input: true,
    },
    host_command: "native-console-host",
    close_evidence: CloseEvidenceChoice::PresenceOnly,
    screen_read_refusal: None,
    guarded_input_refusal: None,
    #[cfg(target_os = "macos")]
    binding: None,
};
