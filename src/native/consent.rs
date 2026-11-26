//! Workspace consent is evidence, not a provider permission mode. Provider stores are
//! read only; their schemas and the way a managed CLI accepts trust belong to adapters.
mod authority;
mod launch_dialog;
mod record;
mod store;
mod wait;
mod worktree;

#[cfg(test)]
pub(in crate::native) use authority::Consented;
pub(in crate::native) use authority::{Authority, authority, authorized};
pub(in crate::native) use launch_dialog::{
    Bypass, BypassStart, LaunchDialogAnswerer, LaunchDialogPolicy, LaunchDialogSupport,
    LaunchFacts, ProviderStart, complete_launch,
};
pub(in crate::native) use record::{
    Assessment, ConsentState, SessionConsentObservation, applied, assess, observe, record, run,
};
#[cfg(test)]
pub(in crate::native) use record::{prepare, prepare_at_home};
#[cfg(windows)]
pub(in crate::native) use store::windows_directory_is_private;
pub(in crate::native) use store::{
    Environment, Evidence, Identity, Trust, identity, native_key, read_store, user_home,
};
pub(in crate::native) use wait::{
    SleepBound, TrustPoll, TrustReadiness, wait_for_native_trust, wait_until_trusted,
};

#[cfg(all(test, windows))]
#[path = "consent/launch_tests.rs"]
mod launch_tests;
#[cfg(test)]
#[path = "consent/tests.rs"]
mod tests;
