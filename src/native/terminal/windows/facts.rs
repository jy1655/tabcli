use crate::native::terminal::facts::{PasteEnvelope, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: false,
    failed_start_identity: true,
    // Console key events would expose bracketed-paste escapes as literal text.
    paste_envelope: PasteEnvelope::Raw,
    guards_dialog_input: true,
};

pub(in crate::native) const HOST_COMMAND: &str = "native-console-host";
