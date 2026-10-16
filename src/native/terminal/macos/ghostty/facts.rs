use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        outlives_owner: true,
        failed_start_identity: true,
        // Ghostty input text already delivers a paste.
        paste_envelope: PasteEnvelope::Raw,
        guards_dialog_input: false,
    },
    host_command: "native-ghostty-host",
    close_evidence: CloseEvidenceChoice::PresenceOnly,
    screen_read_refusal: Some("managed dialog input unsupported for this terminal"),
    guarded_input_refusal: Some("managed dialog input unsupported for this terminal"),
    #[cfg(target_os = "macos")]
    binding: None,
};
