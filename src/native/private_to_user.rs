use std::{fs::Metadata, os::unix::fs::MetadataExt};

/// Unix owner/mode evidence: owned by the supplied effective UID, with no
/// group/other write bits. The caller supplies metadata from its chosen source.
pub(in crate::native) fn owned_and_not_writable_by_others(
    metadata: &Metadata,
    current_uid: u32,
) -> bool {
    metadata.uid() == current_uid && metadata.mode() & 0o022 == 0
}

/// Unix owner/mode evidence: owned by the supplied effective UID, with no
/// group/other read, write, or execute/search bits.
#[cfg(any(target_os = "macos", test))]
pub(in crate::native) fn owned_and_not_accessible_by_others(
    metadata: &Metadata,
    current_uid: u32,
) -> bool {
    metadata.uid() == current_uid && metadata.mode() & 0o077 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn owner_and_mode_policy_table() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("evidence");
        crate::native::session::primitives::write_private(&path, b"evidence").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().uid(), unsafe {
            libc::geteuid()
        });
        for (mode, writable, accessible) in [
            (0o000, true, true),
            (0o400, true, true),
            (0o600, true, true),
            (0o700, true, true),
            (0o640, true, false),
            (0o604, true, false),
            (0o610, true, false),
            (0o601, true, false),
            (0o644, true, false),
            (0o750, true, false),
            (0o755, true, false),
            (0o620, false, false),
            (0o602, false, false),
            (0o666, false, false),
            (0o777, false, false),
        ] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let metadata = fs::metadata(&path).unwrap();
            let uid = metadata.uid();
            assert_eq!(
                owned_and_not_writable_by_others(&metadata, uid),
                writable,
                "{mode:o}"
            );
            assert_eq!(
                owned_and_not_accessible_by_others(&metadata, uid),
                accessible,
                "{mode:o}"
            );
            assert!(
                !owned_and_not_writable_by_others(&metadata, uid.wrapping_add(1)),
                "{mode:o}"
            );
            assert!(
                !owned_and_not_accessible_by_others(&metadata, uid.wrapping_add(1)),
                "{mode:o}"
            );
        }
    }
}
