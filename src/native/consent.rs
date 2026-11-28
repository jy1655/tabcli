//! Workspace consent is evidence, not a provider permission mode. Provider stores are
//! read only; their schemas and the way a managed CLI accepts trust belong to adapters.
mod authority;
mod launch_dialog;
mod record;
mod store;
mod wait;
mod worktree;

pub(in crate::native) use authority::{
    Authority, LaunchGrant, admit_launch, authority, authorized,
};
#[cfg(test)]
pub(in crate::native) use authority::{Consented, Refusal};
pub(in crate::native) use launch_dialog::{
    Bypass, BypassStart, LaunchDialogAnswerer, LaunchDialogPolicy, LaunchDialogSupport,
    LaunchFacts, ProviderStart, complete_launch,
};
pub(in crate::native) use record::{
    Assessment, ConsentState, SessionConsentObservation, apply_option, assess,
    check_command_workspace, observe, option_unavailable, record, run, session_applied,
};
#[cfg(test)]
pub(in crate::native) use record::{after_main_identified, applied, prepare, prepare_at_home};
pub(in crate::native) use store::{
    Environment, Evidence, Identity, Trust, identity, native_key, read_store, user_home,
};
#[cfg(windows)]
pub(in crate::native) use store::{
    WindowsAccount, windows_current_account, windows_directory_is_private,
    windows_directory_is_private_to, windows_process_account,
};
pub(in crate::native) use wait::{
    SleepBound, TrustPoll, TrustReadiness, wait_for_native_trust, wait_until_trusted,
};
pub(in crate::native) use worktree::is_link;
#[cfg(test)]
pub(in crate::native) use worktree::tests::registered;

#[cfg(all(test, windows))]
#[path = "consent/launch_tests.rs"]
mod launch_tests;
#[cfg(test)]
#[path = "consent/tests.rs"]
mod tests;
