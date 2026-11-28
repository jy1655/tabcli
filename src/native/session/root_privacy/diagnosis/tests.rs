use super::super::{FailurePolicy, apply_policy};
use super::*;

const SELECT: &str = "set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB";
const EMPTY: &str = "the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory)";
const UNKNOWN: &str = "cannot determine whether the directory holds a TAB entry";

// Keep this match exhaustive: a new judgment needs an expected observation
// and next action in the consumer table.
fn expected(issue: &Observation) -> (&'static str, &'static str) {
    match issue {
        Observation::Open => (
            if cfg!(windows) {
                "CreateFileW: open directory for READ_CONTROL and FILE_READ_ATTRIBUTES"
            } else {
                "open state directory"
            },
            "check access to the state directory",
        ),
        Observation::NotDirectory => ("expected a directory", SELECT),
        Observation::InspectPath => (
            "failed to inspect state directory tab",
            "check search access to its parent directories",
        ),
        Observation::CreatePath => ("failed to create state directory tab", ""),
        Observation::Resolve => ("resolve state directory", ""),
        Observation::Inspect => ("inspect state root", ""),
        #[cfg(unix)]
        Observation::Owner { .. } => (
            "owner uid 12, expected 34",
            "select a TAB directory owned by uid 34",
        ),
        #[cfg(unix)]
        Observation::Mode(_) => (
            "mode 0750, expected 0700",
            "restore owner permissions with chmod 700 'tab'",
        ),
        #[cfg(unix)]
        Observation::Reopen => ("cannot reopen after restoring owner mode", ""),
        #[cfg(unix)]
        Observation::Replaced => ("directory changed during mode repair", ""),
        #[cfg(unix)]
        Observation::Read => ("read opened directory permissions", ""),
        #[cfg(target_os = "macos")]
        Observation::OpenWithOwnerAccess => (
            "owner mode permits access but the directory open was denied",
            "",
        ),
        #[cfg(target_os = "macos")]
        Observation::Acl(_) => (
            "ACL entry 0 (deny, principal uuid)",
            "run chmod -N 'tab' and retry",
        ),
        #[cfg(target_os = "macos")]
        Observation::AclRead(_) => (
            "cannot read extended ACL (Permission denied (os error 13))",
            "check read access to the extended ACL",
        ),
        #[cfg(windows)]
        Observation::Owner { .. } => (
            "owner SID BA, expected U",
            "take ownership of this TAB directory as the intended user and retry",
        ),
        #[cfg(windows)]
        Observation::Dacl(_) => (
            "empty DACL",
            "grant the token user control of this TAB directory's DACL and retry",
        ),
        #[cfg(windows)]
        Observation::ReadToken => ("read Windows token user and default owner", ""),
        #[cfg(windows)]
        Observation::ReadSecurity => ("read state root owner and DACL", ""),
        #[cfg(windows)]
        Observation::Descriptor => ("build private Windows security descriptor", ""),
        #[cfg(windows)]
        Observation::Verify => ("verify state root owner and DACL after repair", ""),
    }
}

struct Row {
    issue: Observation,
    flags: Flags,
    failure: Option<(i32, &'static str)>,
    detail: String,
    remedy: &'static str,
}

impl Row {
    fn new(issue: Observation) -> Self {
        let (detail, remedy) = expected(&issue);
        Self {
            issue,
            flags: Flags::None,
            failure: None,
            detail: detail.into(),
            remedy,
        }
    }
    fn failed(mut self, code: i32, operation: &'static str, remedy: &'static str) -> Self {
        self.failure = Some((code, operation));
        self.remedy = remedy;
        self
    }
}

#[test]
fn whole_root_diagnosis_for_each_consumer() {
    use Observation::*;
    #[cfg(target_os = "macos")]
    let acl = || Acl("ACL entry 0 (deny, principal uuid)".into());
    let mut rows: Vec<_> = [
        Open,
        NotDirectory,
        Resolve,
        InspectPath,
        CreatePath,
        Inspect,
        #[cfg(unix)]
        Owner {
            actual: 12,
            expected: 34,
        },
        #[cfg(unix)]
        Mode(0o750),
        #[cfg(unix)]
        Reopen,
        #[cfg(unix)]
        Replaced,
        #[cfg(unix)]
        Read,
        #[cfg(target_os = "macos")]
        OpenWithOwnerAccess,
        #[cfg(target_os = "macos")]
        acl(),
        #[cfg(target_os = "macos")]
        AclRead(io::Error::from_raw_os_error(libc::EACCES)),
        #[cfg(windows)]
        Owner {
            actual: "BA".into(),
            expected: "U".into(),
        },
        #[cfg(windows)]
        Dacl("empty DACL".into()),
        #[cfg(windows)]
        ReadToken,
        #[cfg(windows)]
        ReadSecurity,
        #[cfg(windows)]
        Descriptor,
        #[cfg(windows)]
        Verify,
    ]
    .into_iter()
    .map(Row::new)
    .collect();
    #[cfg(unix)]
    rows.push(Row::new(Mode(0o750)).failed(
        libc::EROFS,
        "fchmod: restore owner mode",
        "select a writable directory meant for TAB",
    ));
    #[cfg(unix)]
    rows.push(Row::new(Mode(0o750)).failed(libc::EACCES, "fchmod: restore owner mode", ""));
    #[cfg(windows)]
    {
        rows.push(Row::new(Dacl("empty DACL".into())).failed(
            19,
            "SetSecurityInfo: set DACL",
            "select a writable directory meant for TAB",
        ));
        rows.push(Row::new(Dacl("empty DACL".into())).failed(
            5,
            "NtOpenFile: reopen directory to set DACL (access 0x60080)",
            "grant the token user control of this TAB directory's DACL and retry",
        ));
    }
    #[cfg(target_os = "macos")]
    {
        for (flags, suffix, remedy) in [
            (
                Flags::UserImmutable,
                "user immutable flag is set",
                "clear the immutable flag with chflags nouchg 'tab' and retry",
            ),
            (
                Flags::SystemImmutable,
                "system immutable flag is set",
                "ask an administrator to clear the system immutable flag and retry",
            ),
            (
                Flags::None,
                "macOS privacy controls may prevent access",
                "check the terminal's macOS privacy access to this directory",
            ),
        ] {
            for (issue, op) in [
                (acl(), "failed to remove the extended ACL"),
                (Mode(0o750), "fchmodat: restore owner mode"),
            ] {
                let mut row = Row::new(issue).failed(libc::EPERM, op, remedy);
                row.flags = flags;
                row.detail.push_str(&format!("; {suffix}"));
                rows.push(row);
            }
        }
        for (code, suffix, remedy) in [
            (
                libc::EPERM,
                "; macOS privacy controls may prevent access",
                "check the terminal's macOS privacy access to this directory",
            ),
            (libc::EIO, "", ""),
        ] {
            let error = io::Error::from_raw_os_error(code);
            let detail = format!("cannot read extended ACL ({error}){suffix}");
            rows.push(Row {
                issue: AclRead(error),
                flags: Flags::None,
                failure: None,
                detail,
                remedy,
            });
        }
    }
    for row in rows {
        for entries in [
            Entries::Known,
            Entries::Empty {
                required_for_repair: false,
            },
            Entries::Undetermined(None),
            Entries::Undetermined(Some(anyhow::anyhow!("list: code 5"))),
        ] {
            let (membership, remedy) = match &entries {
                Entries::Known => ("", row.remedy),
                Entries::Empty { .. } => (EMPTY, SELECT),
                Entries::Undetermined(None) => (UNKNOWN, SELECT),
                Entries::Undetermined(Some(_)) => (
                    "cannot determine whether the directory holds a TAB entry (list: code 5)",
                    SELECT,
                ),
            };
            let mut detail = format!("state root tab: {}", row.detail);
            for text in [membership, remedy].into_iter().filter(|s| !s.is_empty()) {
                detail.push_str(&format!("; {text}"));
            }
            if let Some((code, op)) = row.failure {
                detail.push_str(&format!(": {op}: {}", io::Error::from_raw_os_error(code)));
            }
            let result = |entries: &Entries| {
                Err(row
                    .issue
                    .render(
                        Path::new("tab"),
                        entries,
                        row.flags,
                        row.failure.map(|(code, op)| {
                            anyhow::Error::new(io::Error::from_raw_os_error(code)).context(op)
                        }),
                    )
                    .context("state root tab"))
            };
            let doctor = crate::native::doctor::state_root_diagnosis(result(&entries));
            assert_eq!(doctor.detail, detail);
            assert_eq!(doctor.next_action, remedy);
            let refusal = apply_policy(result(&entries), FailurePolicy::Refuse, |_| {
                panic!("strict writer warned")
            })
            .unwrap_err();
            assert_eq!(format!("{refusal:#}"), detail);
            #[cfg(not(any(windows, target_os = "macos")))]
            let needs_entry = false;
            #[cfg(target_os = "macos")]
            let needs_entry = match &row.issue {
                #[cfg(target_os = "macos")]
                Observation::Acl(_) => true,
                #[cfg(target_os = "macos")]
                Observation::AclRead(error) => error.raw_os_error() == Some(libc::EACCES),
                _ => false,
            };
            #[cfg(windows)]
            let needs_entry = matches!(row.issue, Observation::Owner { .. });
            let writer_entries = if needs_entry {
                entries.required_for_repair()
            } else {
                &entries
            };
            let mut warning = None;
            let writer = apply_policy(result(writer_entries), FailurePolicy::Warn, |line| {
                warning = Some(line)
            });
            if needs_entry && matches!(entries, Entries::Empty { .. }) {
                assert_eq!(format!("{:#}", writer.unwrap_err()), detail);
                assert_eq!(warning, None);
            } else {
                writer.unwrap();
                assert_eq!(warning.unwrap(), format!("warning: {detail}"));
            }
        }
    }
}
