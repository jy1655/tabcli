use crate::native::terminal::facts::{PasteEnvelope, PreClosePolicy, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: false,
    failed_start_identity: true,
    requires_app_incarnation: false,
    pre_close: PreClosePolicy::None,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: true,
};
