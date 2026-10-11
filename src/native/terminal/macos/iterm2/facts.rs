use crate::native::terminal::facts::{PasteEnvelope, PreClosePolicy, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: false,
    failed_start_identity: true,
    pre_close: PreClosePolicy::None,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: true,
};

#[cfg(target_os = "macos")]
use crate::native::launch::{BindingBudget, BindingFacts};

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) const BINDING: BindingFacts = BindingFacts {
    host: "iTerm2",
    cancelled: "iTerm2 launch was cancelled or timed out before surface binding",
    budget: BindingBudget::RemainingReceiptCapped,
};
