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
/// The stat of a task whose signal state the kernel had already released when it was read:
/// `do_task_stat` (fs/proc/array.c) then cannot read the process group and session and prints
/// their initial -1. It does not prove that the process exited. A reaper releasing an exited
/// process makes it, and so does an exec in a thread other than the leader: that thread takes
/// over the leader's PID and start time and the old leader is released, while the process
/// lives on. Every reader refuses this entry; only a caller that holds a pidfd on the process
/// can ask it whether it exited (`pidfd::VerifiedPidfd::open`).
#[derive(Debug)]
pub(super) struct ReleasedTaskEntry;
impl std::fmt::Display for ReleasedTaskEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "procfs stat has an invalid process group \"-1\" and session \"-1\", printed for a task whose signal state was released",
        )
    }
}
impl std::error::Error for ReleasedTaskEntry {}

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
    anyhow::ensure!(
        number
            .parse::<u32>()
            .with_context(|| format!("procfs stat has an invalid PID {number:?}"))?
            == pid,
        "procfs returned another PID"
    );
    // tail starts at field 3 (state).
    let tail =
        std::str::from_utf8(&bytes[close + 1..]).context("procfs stat fields are not text")?;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    anyhow::ensure!(
        fields.len() >= 20 && fields[0].len() == 1,
        "incomplete procfs stat"
    );
    fn field<T: std::str::FromStr>(fields: &[&str], index: usize, name: &str) -> Result<T>
    where
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        fields[index]
            .parse()
            .with_context(|| format!("procfs stat has an invalid {name} {:?}", fields[index]))
    }
    if fields[2] == "-1" && fields[3] == "-1" {
        return Err(anyhow::Error::new(ReleasedTaskEntry));
    }
    let tty = field::<i32>(&fields, 4, "terminal")? as u32;
    // Linux new_decode_dev: never compare tty_nr's raw encoding to st_rdev.
    let major = (tty >> 8) & 0xfff;
    let minor = (tty & 0xff) | ((tty >> 12) & 0xfff00);
    Ok(Stat {
        pid,
        parent: field(&fields, 1, "parent")?,
        group: field(&fields, 2, "process group")?,
        session: field(&fields, 3, "session")?,
        tty: libc::makedev(major, minor),
        foreground: field(&fields, 5, "terminal process group")?,
        ticks: field(&fields, 19, "start time")?,
    })
}
fn read_stat(pid: u32) -> Result<Option<Stat>> {
    read_stat_with(pid, || fs::read(format!("/proc/{pid}/stat")))
}
pub(in crate::native::terminal) fn parent_pid(pid: u32) -> Result<Option<u32>> {
    Ok(read_stat(pid)?.map(|stat| stat.parent))
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
/// No process created after this call has an earlier birth: this boot, and the current
/// CLOCK_BOOTTIME in the clock ticks of procfs starttime, which the kernel floors from the
/// same boot-based clock.
pub(in crate::native) fn creation_floor() -> Result<LinuxBirth> {
    let boot_id = boot_id()?;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not read CLOCK_BOOTTIME");
    }
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    anyhow::ensure!(
        ticks_per_second > 0,
        "could not read the procfs clock tick rate"
    );
    let nanoseconds = u128::try_from(now.tv_sec)? * 1_000_000_000 + u128::try_from(now.tv_nsec)?;
    Ok(LinuxBirth {
        boot_id,
        starttime_ticks: u64::try_from(
            nanoseconds * u128::try_from(ticks_per_second)? / 1_000_000_000,
        )?,
    })
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
    // The repeated close of the real-tmux test once failed on the Ubuntu runner (2026-11-26)
    // with "invalid digit found in string" from this reader. The job log holds only that error;
    // an entry printed for a task whose signal state was released (fs/proc/array.c) is the
    // explanation reproduced from the kernel's code. Such an entry proves no exit, so the
    // reader refuses it, with a type that a caller holding a pidfd can recognise.
    #[test]
    fn a_task_being_released_is_refused_not_absent() {
        let entry = |state: &str, group: &str, session: &str| {
            format!(
                "42 (tmux: server) {state} 0 {group} {session} 0 -1 4210764 312 0 0 0 1 0 0 0 20 0 0 0 123456 0 0"
            )
            .into_bytes()
        };
        for state in ["X", "Z", "S"] {
            let error = read_stat_with(42, || Ok(entry(state, "-1", "-1"))).unwrap_err();
            assert!(
                error.downcast_ref::<ReleasedTaskEntry>().is_some(),
                "{error:#}"
            );
            assert_eq!(
                format!("{error:#}"),
                "procfs stat has an invalid process group \"-1\" and session \"-1\", printed for a task whose signal state was released"
            );
        }
        // State X with its ids is read as before: a birth, which proves no exit either.
        let collected = read_stat_with(42, || Ok(entry("X", "42", "42"))).unwrap();
        assert_eq!(collected.map(|stat| stat.ticks), Some(123456));
        // A zombie keeps its incarnation until it is reaped.
        let zombie = read_stat_with(42, || Ok(entry("Z", "42", "42"))).unwrap();
        assert_eq!(zombie.map(|stat| stat.ticks), Some(123456));
        // Any other field that is not a number is a malformed entry, and names that field.
        for (group, session, field) in [("-1", "42", "process group"), ("42", "-1", "session")] {
            let error = read_stat_with(42, || Ok(entry("S", group, session))).unwrap_err();
            assert!(error.downcast_ref::<ReleasedTaskEntry>().is_none());
            assert_eq!(
                format!("{error:#}"),
                format!("procfs stat has an invalid {field} \"-1\": invalid digit found in string")
            );
        }
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
        struct Reaped(std::process::Child);
        impl Drop for Reaped {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Reaped(
            std::process::Command::new(&program)
                .arg("30")
                .spawn()
                .unwrap(),
        );
        // The parent can run before exec renames the child, so wait for the new name.
        let expected = &program.file_name().unwrap().as_encoded_bytes()[..15];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let comm = fs::read(format!("/proc/{}/comm", child.0.id())).unwrap();
            if comm.strip_suffix(b"\n") == Some(expected) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{comm:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(std::str::from_utf8(expected).is_err());
        assert!(process_birth(child.0.id()).unwrap().is_some());
    }
    #[test]
    fn creation_floor_bounds_the_birth_of_later_processes_only() {
        let own = process_birth(std::process::id()).unwrap().unwrap();
        let floor = creation_floor().unwrap();
        assert_eq!(floor.boot_id, own.boot_id);
        assert!(floor.starttime_ticks >= own.starttime_ticks);
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let birth = process_birth(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        let birth = birth.unwrap().unwrap();
        assert_eq!(birth.boot_id, floor.boot_id);
        assert!(birth.starttime_ticks >= floor.starttime_ticks);
    }
    #[test]
    fn real_procfs_birth_does_not_require_a_tty() {
        let birth = process_birth(std::process::id()).unwrap().unwrap();
        assert_eq!(process_birth(std::process::id()).unwrap(), Some(birth));
        assert!(process_birth(i32::MAX as u32).unwrap().is_none());
        assert!(process_birth(0).is_err());
    }
}
