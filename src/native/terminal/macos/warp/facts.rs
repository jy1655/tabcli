use crate::native::terminal::facts::{PasteEnvelope, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: true,
    failed_start_identity: false,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: false,
};

pub(in crate::native) const HOST_COMMAND: &str = "native-warp-host";
