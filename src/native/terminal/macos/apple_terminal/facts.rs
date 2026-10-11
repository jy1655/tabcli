use crate::native::terminal::facts::{PasteEnvelope, PreClosePolicy, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: true,
    failed_start_identity: false,
    pre_close: PreClosePolicy::StopOwnerAndShellGroups,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: true,
};

#[cfg(target_os = "macos")]
use crate::native::launch::{BindingBudget, BindingFacts};

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) const BINDING: BindingFacts = BindingFacts {
    host: "Terminal.app",
    cancelled: "Terminal.app launch was cancelled or timed out before its surface was bound",
    budget: BindingBudget::StartTimeout,
};
