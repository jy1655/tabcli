use crate::native::terminal::facts::{PasteEnvelope, PreClosePolicy, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: true,
    failed_start_identity: false,
    requires_app_incarnation: false,
    pre_close: PreClosePolicy::RecordIntent,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: false,
};
