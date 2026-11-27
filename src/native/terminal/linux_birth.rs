//! Linux birth is boot identity + procfs starttime ticks, never wall time. The same
//! optional `linux_birth` object is used by owner, terminal_shell and linux_host.
//! A present incomplete/malformed object fails decoding; it never becomes PID-only.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::native) struct BootId([u8; 16]);
impl std::fmt::Display for BootId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl BootId {
    /// The all-zero identity no kernel reports for a boot.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(in crate::native) fn is_nil(self) -> bool {
        self.0 == [0; 16]
    }
}
impl std::fmt::Debug for BootId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl Serialize for BootId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for BootId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}
impl std::str::FromStr for BootId {
    type Err = anyhow::Error;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        anyhow::ensure!(
            text.len() == 36
                && [8, 13, 18, 23]
                    .into_iter()
                    .all(|i| text.as_bytes()[i] == b'-'),
            "invalid Linux boot_id"
        );
        let hex = text
            .bytes()
            .enumerate()
            .filter_map(|(i, b)| (![8, 13, 18, 23].contains(&i)).then_some(b))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            hex.iter().all(u8::is_ascii_hexdigit),
            "invalid Linux boot_id"
        );
        let (pairs, []) = hex.as_chunks::<2>() else {
            anyhow::bail!("invalid Linux boot_id");
        };
        let mut bytes = [0; 16];
        for (byte, pair) in bytes.iter_mut().zip(pairs) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
        }
        Ok(Self(bytes))
    }
}
#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct LinuxBirth {
    pub(in crate::native) boot_id: BootId,
    pub(in crate::native) starttime_ticks: u64,
}
impl std::fmt::Debug for LinuxBirth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(boot {}, tick {})", self.boot_id, self.starttime_ticks)
    }
}

// A present null is malformed evidence, not an absent legacy identity.
pub(in crate::native) fn present<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<LinuxBirth>, D::Error> {
    LinuxBirth::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn birth_text_and_wire_keep_the_canonical_boot_id() {
        let text = "01234567-89ab-cdef-0123-456789abcdef";
        let birth = LinuxBirth {
            boot_id: text.parse().unwrap(),
            starttime_ticks: 7,
        };
        assert_eq!(format!("{birth:?}"), format!("(boot {text}, tick 7)"));
        assert_eq!(
            serde_json::to_value(birth).unwrap(),
            serde_json::json!({"boot_id": text, "starttime_ticks": 7})
        );
    }
}
