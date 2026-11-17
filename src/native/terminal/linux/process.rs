//! procfs observation: only ENOENT/ESRCH at this PID's entry proves absence.
//! A zombie retains its incarnation. Birth-only callers never need a controlling TTY.
use super::super::{LinuxBirth, ownership::NativeProcessIdentity};
use anyhow::{Context, Result, bail};
use std::{fs, path::Path};

#[derive(Debug)]
struct Stat {
    pid: u32,
    parent: u32,
    group: u32,
    session: u32,
    tty: u64,
    foreground: i64,
    ticks: u64,
}
// The comm field is raw bytes cut at 15 bytes, so it can hold ')', newlines, and a split
// UTF-8 sequence; only the PID before it and the fields after its last ')' are text.
fn parse_stat(bytes: &[u8], pid: u32) -> Result<Stat> {
    let close = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .context("procfs stat has no closing comm delimiter")?;
    let open = bytes[..close]
        .windows(2)
        .position(|pair| pair == b" (")
        .context("procfs stat has no comm")?;
    let number = std::str::from_utf8(&bytes[..open]).context("procfs stat PID is not text")?;
    anyhow::ensure!(number.parse::<u32>()? == pid, "procfs returned another PID");
    // tail starts at field 3 (state).
    let tail =
        std::str::from_utf8(&bytes[close + 1..]).context("procfs stat fields are not text")?;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    anyhow::ensure!(
        fields.len() >= 20 && fields[0].len() == 1,
        "incomplete procfs stat"
    );
    let tty = fields[4].parse::<i32>()? as u32;
    // Linux new_decode_dev: never compare tty_nr's raw encoding to st_rdev.
    let major = (tty >> 8) & 0xfff;
    let minor = (tty & 0xff) | ((tty >> 12) & 0xfff00);
    Ok(Stat {
        pid,
        parent: fields[1].parse()?,
        group: fields[2].parse()?,
        session: fields[3].parse()?,
        tty: libc::makedev(major, minor),
        foreground: fields[5].parse()?,
        ticks: fields[19].parse()?,
    })
}
fn read_stat(pid: u32) -> Result<Option<Stat>> {
    read_stat_with(pid, || fs::read(format!("/proc/{pid}/stat")))
}
fn read_stat_with(
    pid: u32,
    read_pid_entry: impl FnOnce() -> std::io::Result<Vec<u8>>,
) -> Result<Option<Stat>> {
    anyhow::ensure!(
        pid > 0 && pid <= i32::MAX as u32,
        "Linux PID is out of range"
    );
    match read_pid_entry() {
        Ok(text) => parse_stat(&text, pid).map(Some),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect procfs process {pid}")),
    }
}
fn boot_id() -> Result<super::super::linux_birth::BootId> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .parse()
}
pub(in crate::native) fn process_birth(pid: u32) -> Result<Option<LinuxBirth>> {
    let boot_id = boot_id()?;
    Ok(read_stat(pid)?.map(|stat| LinuxBirth {
        boot_id,
        starttime_ticks: stat.ticks,
    }))
}
pub(in crate::native) fn live_native_process_identity(pid: u32) -> Result<NativeProcessIdentity> {
    live_native_process_identity_if_present(pid)?.context("native-session process is absent")
}
pub(in crate::native) fn live_native_process_identity_if_present(
    pid: u32,
) -> Result<Option<NativeProcessIdentity>> {
    let boot_id = boot_id()?;
    let Some(stat) = read_stat(pid)? else {
        return Ok(None);
    };
    if stat.tty == 0 || stat.session == 0 {
        bail!("native-session process has no controlling TTY")
    }
    Ok(Some(NativeProcessIdentity {
        birth: crate::native::terminal::ownership::ProcessBirth::Linux(LinuxBirth {
            boot_id,
            starttime_ticks: stat.ticks,
        }),
        pid: stat.pid,
        parent_pid: stat.parent,
        process_group: stat.group,
        terminal_tty_device: stat.tty,
        terminal_process_group: stat
            .foreground
            .try_into()
            .context("no terminal foreground group")?,
    }))
}
pub(in crate::native) fn current_terminal_tty() -> Result<String> {
    let mut bytes = [0 as libc::c_char; 4096];
    let error = unsafe { libc::ttyname_r(libc::STDIN_FILENO, bytes.as_mut_ptr(), bytes.len()) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error))
            .context("failed to resolve native-session controlling TTY");
    }
    Ok(unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
        .to_str()?
        .to_owned())
}
pub(in crate::native) fn terminal_tty_device(path: &Path) -> Result<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = fs::metadata(path)?;
    anyhow::ensure!(
        metadata.file_type().is_char_device(),
        "terminal TTY is not a character device: {}",
        path.display()
    );
    // Re-encode major/minor to the same canonical dev_t used for decoded tty_nr.
    Ok(libc::makedev(
        libc::major(metadata.rdev()),
        libc::minor(metadata.rdev()),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_parses_after_last_paren_and_decodes_device() {
        let mut fields = vec!["0"; 20];
        fields[0] = "Z";
        fields[1] = "7";
        fields[2] = "8";
        fields[3] = "9";
        fields[4] = "34817";
        fields[5] = "8";
        fields[19] = "123";
        let stat =
            parse_stat(format!("42 (a )\n b)) {}", fields.join(" ")).as_bytes(), 42).unwrap();
        assert_eq!(
            (stat.parent, stat.group, stat.session, stat.foreground),
            (7, 8, 9, 8)
        );
        assert_eq!((libc::major(stat.tty), libc::minor(stat.tty)), (136, 1));
        assert_eq!(stat.ticks, 123);
        assert!(parse_stat(b"42 (bad) S 1", 42).is_err());
        assert!(parse_stat(b"43 (bad) S 1", 42).is_err());
        // A comm cut inside a multibyte character is still this process's entry.
        let mut cut = b"42 (a\xea\xb0\x80\xeb) ".to_vec();
        cut.extend_from_slice(fields.join(" ").as_bytes());
        assert_eq!(parse_stat(&cut, 42).unwrap().ticks, 123);
    }
    #[test]
    fn only_pid_entry_enoent_or_esrch_is_absence() {
        for errno in [libc::ENOENT, libc::ESRCH] {
            assert!(
                read_stat_with(42, || Err(std::io::Error::from_raw_os_error(errno)))
                    .unwrap()
                    .is_none()
            );
        }
        for errno in [libc::EACCES, libc::EPERM, libc::EIO] {
            assert!(read_stat_with(42, || Err(std::io::Error::from_raw_os_error(errno))).is_err());
        }
        assert!(read_stat_with(42, || Ok(b"malformed".to_vec())).is_err());
    }
    #[test]
    fn real_procfs_birth_reads_a_comm_cut_inside_a_character() {
        // comm is the executed file name cut to 15 bytes: "a" + five 3-byte characters
        // ends with a split character, which is not UTF-8.
        let root = tempfile::tempdir().unwrap();
        let program = root
            .path()
            .join("a\u{ac00}\u{b098}\u{b2e4}\u{b77c}\u{b9c8}");
        std::os::unix::fs::symlink("/bin/sleep", &program).unwrap();
        let mut child = std::process::Command::new(&program)
            .arg("30")
            .spawn()
            .unwrap();
        let comm = fs::read(format!("/proc/{}/comm", child.id())).unwrap();
        assert!(std::str::from_utf8(&comm).is_err(), "{comm:?}");
        let birth = process_birth(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(birth.unwrap().is_some());
    }
    #[test]
    fn real_procfs_birth_does_not_require_a_tty() {
        let birth = process_birth(std::process::id()).unwrap().unwrap();
        assert_eq!(process_birth(std::process::id()).unwrap(), Some(birth));
        assert!(process_birth(i32::MAX as u32).unwrap().is_none());
        assert!(process_birth(0).is_err());
    }
}
