//! Linux birth is boot identity + procfs starttime ticks, never wall time. The same
//! optional `linux_birth` object is used by owner, terminal_shell and linux_host.
//! A present incomplete/malformed object fails decoding; it never becomes PID-only.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) struct BootId([u8; 16]);
impl Serialize for BootId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let hex = self
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        serializer.serialize_str(&format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        ))
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
        let mut bytes = [0; 16];
        for (i, pair) in hex.chunks_exact(2).enumerate() {
            bytes[i] = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
        }
        Ok(Self(bytes))
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::native) struct LinuxBirth {
    pub(in crate::native) boot_id: BootId,
    pub(in crate::native) starttime_ticks: u64,
}

// A present null is malformed evidence, not an absent legacy identity.
pub(in crate::native) fn present<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<LinuxBirth>, D::Error> {
    LinuxBirth::deserialize(d).map(Some)
}
