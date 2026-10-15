use super::WindowsProcessIdentity;

/// A comparison used by process attestation; record and surface equality stay exact.
#[derive(Debug)]
pub(in crate::native) struct WindowsIncarnation(pub WindowsProcessIdentity);

impl PartialEq for WindowsIncarnation {
    fn eq(&self, other: &Self) -> bool {
        same_incarnation(&self.0, &other.0)
    }
}

fn same_incarnation(recorded: &WindowsProcessIdentity, live: &WindowsProcessIdentity) -> bool {
    recorded.creation_time == live.creation_time
        && recorded
            .executable_path
            .eq_ignore_ascii_case(&live.executable_path)
}
