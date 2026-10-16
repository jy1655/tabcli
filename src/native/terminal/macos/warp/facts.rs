use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        outlives_owner: true,
        failed_start_identity: false,
        paste_envelope: PasteEnvelope::Bracketed,
        guards_dialog_input: false,
    },
    host_command: "native-warp-host",
    close_evidence: CloseEvidenceChoice::PresenceOnlyWithOwnerlessContext,
    screen_read_refusal: Some(
        "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit",
    ),
    guarded_input_refusal: Some(
        "managed Warp screen reads and guarded input are unsupported; Warp Control input.insert/input.replace do not submit",
    ),
    #[cfg(target_os = "macos")]
    binding: None,
};
