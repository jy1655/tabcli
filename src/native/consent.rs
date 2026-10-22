//! Workspace consent is evidence, not a provider permission mode. Provider stores are
//! read only; their schemas and the way a managed CLI accepts trust belong to adapters.
mod launch_dialog;
mod record;
mod store;
mod wait;

pub(in crate::native) use launch_dialog::complete_launch;
#[cfg(test)]
pub(in crate::native) use record::prepare;
pub(in crate::native) use record::{
    Assessment, ConsentState, SessionConsentObservation, applied, assess, authorized, observe,
    record, run,
};
pub(in crate::native) use store::{
    Environment, Evidence, Identity, LaunchTrust, Trust, identity, native_key, read_store,
    user_home,
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
