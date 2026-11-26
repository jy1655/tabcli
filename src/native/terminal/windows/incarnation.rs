use super::WindowsProcessIdentity;

/// A comparison used by process attestation; record and surface equality stay exact.
#[derive(Debug)]
pub(in crate::native) struct WindowsIncarnation(pub WindowsProcessIdentity);

impl PartialEq for WindowsIncarnation {
    fn eq(&self, other: &Self) -> bool {
        same_incarnation(&self.0, &other.0)
    }
}

impl WindowsIncarnation {
    /// A recorded identity that Windows can produce: a creation time and an image path.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(in crate::native) fn is_possible(&self) -> bool {
        self.0.creation_time > 0 && !self.0.executable_path.is_empty()
    }

    /// Whether `live`, observed at the recorded PID, is a process created after the recorded
    /// one: a reuse of the PID. The same creation time under another image path is the same
    /// process recorded with a damaged path, never a reuse.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(in crate::native) fn succeeded_by(&self, live: &Self) -> bool {
        live.0.creation_time > self.0.creation_time
    }
}

fn same_incarnation(recorded: &WindowsProcessIdentity, live: &WindowsProcessIdentity) -> bool {
    recorded.creation_time == live.creation_time
        && recorded
            .executable_path
            .eq_ignore_ascii_case(&live.executable_path)
}
