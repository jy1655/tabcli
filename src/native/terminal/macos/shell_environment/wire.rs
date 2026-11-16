//! Platform-neutral validation of the byte-preserving environment wire.
use crate::native::session::launch::host::FailureReason;
use std::collections::HashSet;
pub(super) const CAP: usize = 1024 * 1024;
type Variables = Vec<(Vec<u8>, Vec<u8>)>;
pub(super) fn decode(bytes: &[u8]) -> Result<Variables, FailureReason> {
    if bytes.len() > CAP {
        return Err(FailureReason::CaptureOversized);
    }
    if bytes.is_empty() {
        return Err(FailureReason::CaptureNoOutput);
    }
    if bytes.last() != Some(&0) {
        return Err(FailureReason::CaptureInvalid);
    }
    let mut names = HashSet::new();
    let mut variables = Vec::new();
    for entry in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        let Some(eq) = entry.iter().position(|b| *b == b'=') else {
            return Err(FailureReason::CaptureInvalid);
        };
        let name = &entry[..eq];
        if name.is_empty()
            || !(name[0].is_ascii_alphabetic() || name[0] == b'_')
            || !name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
            || !names.insert(name)
        {
            return Err(FailureReason::CaptureInvalid);
        }
        if [b"_".as_slice(), b"SHLVL", b"PWD", b"OLDPWD"].contains(&name) {
            continue;
        }
        variables.push((name.to_vec(), entry[eq + 1..].to_vec()));
    }
    Ok(variables)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_identifiers_duplicates_and_limits() {
        for bytes in [
            b"X=y".as_slice(),
            b"X=y\0X=z\0",
            b"=v\0",
            b"1X=y\0",
            b"A-B=c\0",
            b"X=y\0\0",
        ] {
            assert_eq!(decode(bytes).unwrap_err(), FailureReason::CaptureInvalid);
        }
        assert_eq!(decode(&[]).unwrap_err(), FailureReason::CaptureNoOutput);
        assert_eq!(
            decode(&vec![0; CAP + 1]).unwrap_err(),
            FailureReason::CaptureOversized
        );
        assert_eq!(
            decode(b"X=a=b\0_=x\0SHLVL=1\0PWD=p\0OLDPWD=q\0").unwrap(),
            vec![(b"X".to_vec(), b"a=b".to_vec())]
        );
    }
    #[test]
    fn values_are_exact_bytes() {
        assert_eq!(
            decode(b"X=\n'\"$\xff\0").unwrap(),
            vec![(b"X".to_vec(), b"\n'\"$\xff".to_vec())]
        );
    }
}
