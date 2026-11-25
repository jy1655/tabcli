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
    launch_host: None,
    close_evidence: CloseEvidenceChoice::PresenceOnly,
    screen_read_refusal: Some("Ghostty's scripting dictionary exposes no screen text"),
    guarded_input_refusal: Some("managed dialog input unsupported for this terminal"),
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    binding: None,
};
