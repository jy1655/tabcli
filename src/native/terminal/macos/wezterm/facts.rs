use crate::native::terminal::facts::{PasteEnvelope, SurfaceFacts};

pub(in crate::native::terminal) const FACTS: SurfaceFacts = SurfaceFacts {
    outlives_owner: true,
    failed_start_identity: false,
    paste_envelope: PasteEnvelope::Bracketed,
    guards_dialog_input: false,
};

#[cfg(target_os = "macos")]
use crate::native::launch::{BindingBudget, BindingFacts};

#[cfg(target_os = "macos")]
pub(in crate::native::terminal) const BINDING: BindingFacts = BindingFacts {
    host: "WezTerm",
    cancelled: "WezTerm launch was cancelled or timed out before surface binding",
    budget: BindingBudget::RemainingReceiptCapped,
};
