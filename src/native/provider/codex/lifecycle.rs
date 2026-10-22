//! Compatibility path for the Windows paste gate; turn records live in `turn`.
#[cfg(any(windows, test))]
pub(super) use super::turn::read_binding;
