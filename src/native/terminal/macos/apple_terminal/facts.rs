use crate::native::terminal::facts::{PasteEnvelope, PreClosePolicy, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: true,
    failed_start_identity: false,
    pre_close: PreClosePolicy::StopOwnerAndShellGroups,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: true,
};
