use crate::native::terminal::facts::{
    CloseEvidenceChoice, OwnerSurvival, PasteEnvelope, Registration, SurfaceFacts,
};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        // Repair settles an exited or failed console from its Records. A console root
        // can survive the owner of an active session, so Repair still closes it there.
        owner_survival: OwnerSurvival::NoRetention,
        failed_start_identity: true,
        // Console key events would expose bracketed-paste escapes as literal text.
        paste_envelope: PasteEnvelope::Raw,
        guards_dialog_input: true,
    },
    host_command: "native-console-host",
    launch_host: None,
    close_evidence: CloseEvidenceChoice::PresenceOnly,
    screen_read_refusal: None,
    guarded_input_refusal: None,
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    binding: None,
};
