use crate::native::terminal::facts::{PasteEnvelope, PreClosePolicy, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: true,
    failed_start_identity: true,
    requires_app_incarnation: false,
    pre_close: PreClosePolicy::None,
    // Ghostty input text already delivers a paste.
    paste_envelope: PasteEnvelope::Raw,
    guards_dialog_input: false,
};
