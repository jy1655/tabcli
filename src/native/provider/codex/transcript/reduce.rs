//! Bounded reduction of one rollout record longer than `LINE_BYTES` (#269). Every string
//! longer than `ELIDE_BYTES` becomes `""` once its bytes pass the checks serde_json applies
//! to a `String`; the caller parses the reduced record and decides whether to skip it.
//! Remove together with the transcript watcher.
use super::{LINE_BYTES, Observation};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};

pub(super) const ELIDE_BYTES: usize = 4 * 1024;

pub(super) struct Reducer {
    out: Vec<u8>,
    elide: usize,
    string: Option<Str>,
    utf8: Utf8,
}
struct Str {
    start: usize,
    len: usize,
    cut: bool,
    escape: Escape,
}
#[derive(Clone, Copy)]
enum Escape {
    Plain,
    Backslash,
    Hex { left: u8, value: u16, trail: bool },
    AfterLead,
    AfterLeadSlash,
}
#[derive(Default)]
struct Utf8 {
    bytes: [u8; 4],
    have: usize,
    need: usize,
}
enum Emit {
    Byte,
    Nothing,
    Cut(usize),
}
impl Reducer {
    /// Reduces the retained 8 MiB prefix in place: the write position never passes the
    /// read position, so the record is never held twice.
    pub(super) fn from_prefix(raw: Vec<u8>) -> Observation<Self> {
        Self::reduce(raw, ELIDE_BYTES)
    }
    fn reduce(mut raw: Vec<u8>, elide: usize) -> Observation<Self> {
        let mut reducer = Self {
            out: Vec::new(),
            elide,
            string: None,
            utf8: Utf8::default(),
        };
        let mut len = 0;
        let mut read = 0;
        while read < raw.len() {
            let byte = raw[read];
            read += 1;
            match reducer.step(byte, len)? {
                Emit::Byte => {
                    raw[len] = byte;
                    len += 1;
                }
                Emit::Nothing => {}
                Emit::Cut(start) => len = start,
            }
        }
        raw.truncate(len);
        raw.shrink_to_fit();
        reducer.out = raw;
        Ok(reducer)
    }
    pub(super) fn push(&mut self, byte: u8) -> Observation<()> {
        match self.step(byte, self.out.len())? {
            Emit::Byte => {
                if self.out.len() == self.out.capacity() {
                    // Grow geometrically, but never past the most `step` can keep.
                    let room = (LINE_BYTES + ELIDE_BYTES).saturating_sub(self.out.len());
                    self.out
                        .reserve_exact(self.out.capacity().max(64).min(room.max(1)));
                }
                self.out.push(byte);
            }
            Emit::Nothing => {}
            Emit::Cut(start) => self.out.truncate(start),
        }
        Ok(())
    }
    pub(super) fn finish(self) -> Observation<Vec<u8>> {
        if self.utf8.need != 0 {
            return Err("invalid_utf8");
        }
        if self.string.is_some() {
            return Err("invalid_json");
        }
        Ok(self.out)
    }
    #[cfg(test)]
    pub(super) fn retained(&self) -> usize {
        self.out.capacity()
    }
    /// `len` is the reduced length so far. Bytes outside a string, and both quotes, are
    /// kept for good; string content stays only while the string fits `elide`.
    fn step(&mut self, byte: u8, len: usize) -> Observation<Emit> {
        self.utf8.push(byte)?;
        let Some(string) = self.string.as_mut() else {
            if byte == b'"' {
                self.string = Some(Str {
                    start: len + 1,
                    len: 0,
                    cut: false,
                    escape: Escape::Plain,
                });
            }
            return kept(len);
        };
        let Some(escape) = advance(string.escape, byte)? else {
            self.string = None;
            return kept(len);
        };
        string.escape = escape;
        if string.cut {
            Ok(Emit::Nothing)
        } else if string.len == self.elide {
            string.cut = true;
            Ok(Emit::Cut(string.start))
        } else {
            string.len += 1;
            Ok(Emit::Byte)
        }
    }
}
/// A reduced record longer than `LINE_BYTES` stays oversized.
fn kept(len: usize) -> Observation<Emit> {
    if len >= LINE_BYTES {
        Err("oversized")
    } else {
        Ok(Emit::Byte)
    }
}
/// serde_json's validated `String` rules (read.rs `parse_escape`/`parse_unicode_escape`):
/// no raw control byte, one of `"\/bfnrt` or `u` and four hex digits after a backslash,
/// and every UTF-16 surrogate paired. `None` closes the string.
fn advance(state: Escape, byte: u8) -> Observation<Option<Escape>> {
    Ok(Some(match (state, byte) {
        (Escape::Plain, b'"') => return Ok(None),
        (Escape::Plain, b'\\') => Escape::Backslash,
        (Escape::Plain, 0x00..0x20) => return Err("invalid_json"),
        (Escape::Plain, _) => Escape::Plain,
        (Escape::Backslash, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
            Escape::Plain
        }
        (Escape::Backslash, b'u') => Escape::Hex {
            left: 4,
            value: 0,
            trail: false,
        },
        (Escape::Hex { left, value, trail }, _) => {
            let digit = char::from(byte).to_digit(16).ok_or("invalid_json")?;
            let value = (value << 4) | digit as u16;
            if left > 1 {
                Escape::Hex {
                    left: left - 1,
                    value,
                    trail,
                }
            } else if trail != (0xDC00..=0xDFFF).contains(&value) {
                return Err("invalid_json");
            } else if (0xD800..=0xDBFF).contains(&value) {
                Escape::AfterLead
            } else {
                Escape::Plain
            }
        }
        (Escape::AfterLead, b'\\') => Escape::AfterLeadSlash,
        (Escape::AfterLeadSlash, b'u') => Escape::Hex {
            left: 4,
            value: 0,
            trail: true,
        },
        _ => return Err("invalid_json"),
    }))
}
impl Utf8 {
    /// Checks the whole line, inside and outside strings, as `str::from_utf8` would.
    fn push(&mut self, byte: u8) -> Observation<()> {
        if self.need == 0 {
            self.need = match byte {
                0x00..=0x7F => return Ok(()),
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => return Err("invalid_utf8"),
            };
            self.have = 0;
        } else if byte & 0xC0 != 0x80 {
            return Err("invalid_utf8");
        }
        self.bytes[self.have] = byte;
        self.have += 1;
        if self.have == self.need {
            std::str::from_utf8(&self.bytes[..self.need]).map_err(|_| "invalid_utf8")?;
            self.need = 0;
        }
        Ok(())
    }
}

/// `Value` keeps only the last of repeated keys, so it cannot show that a discriminator
/// was written twice: count the top-level `type` and `payload` and the `type` inside
/// `payload` of the already validated text. A shape that is not an object counts as one.
pub(super) fn repeated_discriminator(text: &str) -> bool {
    struct Keys {
        nested: bool,
    }
    impl<'de> DeserializeSeed<'de> for Keys {
        type Value = bool;
        fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
            deserializer.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Keys {
        type Value = bool;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let (mut kinds, mut payloads, mut repeated) = (0, 0, false);
            while let Some(key) = map.next_key::<String>()? {
                kinds += usize::from(key == "type");
                if key == "payload" && !self.nested {
                    payloads += 1;
                    repeated |= map.next_value_seed(Keys { nested: true })?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(repeated || kinds > 1 || payloads > 1)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    Keys { nested: false }
        .deserialize(&mut deserializer)
        .unwrap_or(true)
}
#[cfg(test)]
mod tests;
