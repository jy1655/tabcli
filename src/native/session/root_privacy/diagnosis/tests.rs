use super::super::{Action, FailurePolicy, apply_policy};
use super::*;

const SELECT: &str = "set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB";
const EMPTY: &str = "the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory)";
const UNKNOWN: &str = "cannot determine whether the directory holds a TAB entry";

const ABSENT: &str = "TAB creates it at the first session.";
const WRITABLE: &str = "select a writable directory meant for TAB";
#[cfg(target_os = "macos")]
const PRIVACY: &str = "check the terminal's macOS privacy access to this directory";

impl ErrorCode {
    fn error(self) -> io::Error {
        match self {
            Self::Native(code) => io::Error::from_raw_os_error(code),
            Self::Kind(kind) => io::Error::from(kind),
        }
    }

    #[cfg(target_os = "macos")]
    fn represents(self, error: &io::Error) -> bool {
        self.error().raw_os_error() == error.raw_os_error() && self.error().kind() == error.kind()
    }
}

// Untyped errors (for example an unresolved HOME) also occur in an anyhow chain.
#[derive(Debug, Copy, Clone)]
enum Failure {
    Io(ErrorCode),
    Message,
}

impl Failure {
    fn error(self) -> anyhow::Error {
        match self {
            Self::Io(code) => code.error().into(),
            Self::Message => anyhow::anyhow!("fixture error"),
        }
    }
}

fn io_causes() -> impl Iterator<Item = (ErrorCode, Cause)> {
    CAUSES
        .iter()
        .copied()
        .chain([(ErrorCode::Native(12345), Cause::Other)])
}

fn causes() -> impl Iterator<Item = (Option<Failure>, Option<Cause>)> {
    [(None, None), (Some(Failure::Message), None)]
        .into_iter()
        .chain(io_causes().map(|(code, cause)| (Some(Failure::Io(code)), Some(cause))))
}

// The same rows generate the inputs and an exhaustive match on Observation. Adding
// a variant therefore requires its fixture and its independent literal expectation.
macro_rules! observations {
    ($( $(#[$attr:meta])* $pattern:pat => $samples:expr, $text:expr, $remedy:expr; )*) => {
        fn observations() -> Vec<Observation> {
            use Observation::*;
            let mut all = Vec::new();
            $( $(#[$attr])* all.extend($samples); )*
            all
        }
        fn labels(issue: &Observation) -> (&'static str, &'static str) {
            use Observation::*;
            match issue { $( $(#[$attr])* $pattern => ($text, $remedy), )* }
        }
    };
}

observations! {
    Absent => [Absent], "state directory does not exist", ABSENT;
    Open { .. } => [Open { selected_non_directory: false }, Open { selected_non_directory: true }], if cfg!(windows) { "CreateFileW: open directory for READ_CONTROL and FILE_READ_ATTRIBUTES" }
        else { "open state directory" }, "check access to the state directory";
    #[cfg(windows)]
    NotDirectory => [NotDirectory], "expected a directory", SELECT;
    InspectPath => [InspectPath], "failed to inspect state directory tab", "check search access to its parent directories";
    CreatePath => [CreatePath], "failed to create state directory tab", "";
    Resolve => [Resolve], "resolve state directory", "";
    Inspect => [Inspect], "inspect state root", "";
    #[cfg(unix)]
    Owner { .. } => [Owner { actual: 12, expected: 34 }], "owner uid 12, expected 34", "select a TAB directory owned by uid 34";
    #[cfg(unix)]
    Mode(_) => [Mode(0o750)], "mode 0750, expected 0700", "restore owner permissions with chmod 700 'tab'";
    #[cfg(unix)]
    Reopen => [Reopen], "cannot reopen after restoring owner mode", "";
    #[cfg(unix)]
    Replaced => [Replaced], "directory changed during mode repair", "";
    #[cfg(unix)]
    Read => [Read], "read opened directory permissions", "";
    #[cfg(target_os = "macos")]
    OpenWithOwnerAccess => [OpenWithOwnerAccess], "owner mode permits access but the directory open was denied", "";
    #[cfg(target_os = "macos")]
    Acl(_) => [Acl("ACL entry 0 (deny, principal uuid)".into())], "ACL entry 0 (deny, principal uuid)", "run chmod -N 'tab' and retry";
    #[cfg(target_os = "macos")]
    AclRead(_) => io_causes().map(|(code, _)| AclRead(code.error())), "cannot read extended ACL", "check read access to the extended ACL";
    #[cfg(windows)]
    Owner { .. } => [Owner { actual: "BA".into(), expected: "U".into() }], "owner SID BA, expected U", "take ownership of this TAB directory as the intended user and retry";
    #[cfg(windows)]
    Dacl(_) => [Dacl("empty DACL".into())], "empty DACL", "grant the token user control of this TAB directory's DACL and retry";
    #[cfg(windows)]
    ReadToken => [ReadToken], "read Windows token user and default owner", "";
    #[cfg(windows)]
    ReadSecurity => [ReadSecurity], "read state root owner and DACL", "";
    #[cfg(windows)]
    Descriptor => [Descriptor], "build private Windows security descriptor", "";
    #[cfg(windows)]
    Verify => [Verify], "verify state root owner and DACL after repair", "";
}

struct Case<'a> {
    issue: &'a Observation,
    action: Action,
    entries: &'a Entries,
    flags: Flags,
    failure: Option<Failure>,
    cause: Option<Cause>,
    // A denied handle-open precedes the actual Windows security write.
    #[cfg(windows)]
    setter: bool,
}

impl Case<'_> {
    fn operation(&self) -> &'static str {
        match self.issue {
            #[cfg(unix)]
            Observation::Mode(_) => "fchmod: restore owner mode",
            #[cfg(target_os = "macos")]
            Observation::Acl(_) | Observation::AclRead(_) => "failed to remove the extended ACL",
            #[cfg(windows)]
            Observation::Owner { .. } if self.setter => "SetSecurityInfo: set owner",
            #[cfg(windows)]
            Observation::Dacl(_) if self.setter => "SetSecurityInfo: set DACL",
            #[cfg(windows)]
            Observation::Owner { .. } => {
                "NtOpenFile: reopen directory to set owner (access 0x80000)"
            }
            #[cfg(windows)]
            Observation::Dacl(_) => "NtOpenFile: reopen directory to set DACL (access 0x60080)",
            _ => "",
        }
    }

    fn result(&self) -> anyhow::Result<bool> {
        let failure = self.failure.map(|failure| {
            let error = failure.error();
            #[cfg(windows)]
            if self.setter {
                return error.context(SecurityWrite(
                    if matches!(self.issue, Observation::Owner { .. }) {
                        "set owner"
                    } else {
                        "set DACL"
                    },
                ));
            }
            if self.operation().is_empty() {
                error
            } else {
                error.context(self.operation())
            }
        });
        Err(self
            .issue
            .render(Path::new("tab"), self.entries, self.flags, failure)
            .context("state root tab"))
    }

    fn no_directory(&self) -> bool {
        match self.issue {
            Observation::Open { .. } => {
                matches!(self.cause, Some(Cause::Missing | Cause::NotDirectory))
            }
            Observation::Absent | Observation::CreatePath | Observation::Resolve => true,
            #[cfg(windows)]
            Observation::NotDirectory => true,
            _ => false,
        }
    }

    fn expectation(&self) -> (String, &'static str, &'static str) {
        let (label, default) = labels(self.issue);
        let label = if matches!(self.issue, Observation::Open { .. })
            && self.cause == Some(Cause::NotDirectory)
        {
            "expected a directory"
        } else {
            label
        };
        let mut detail = format!("state root tab: {label}");
        let cause = self.cause;
        #[cfg(target_os = "macos")]
        let cause = if let Observation::AclRead(err) = self.issue {
            detail.push_str(&format!(" ({err})"));
            cause.or_else(|| {
                io_causes()
                    .find(|(code, _)| code.represents(err))
                    .map(|(_, cause)| cause)
            })
        } else {
            cause
        };

        let mismatch = match self.issue {
            Observation::Open {
                selected_non_directory,
            } => *selected_non_directory,
            #[cfg(windows)]
            Observation::NotDirectory => true,
            Observation::Owner { .. } => true,
            #[cfg(unix)]
            Observation::Mode(_) => true,
            #[cfg(target_os = "macos")]
            Observation::Acl(_) => true,
            #[cfg(windows)]
            Observation::Dacl(_) => true,
            _ => false,
        };
        let status = if matches!(self.issue, Observation::Absent) {
            "state_root_absent"
        } else if mismatch {
            "state_root_not_private"
        } else {
            "state_root_unreadable"
        };

        #[cfg(target_os = "macos")]
        let flag = if cause != Some(Cause::ReadOnly)
            && (matches!(self.issue, Observation::Mode(_) | Observation::Acl(_))
                || matches!(self.issue, Observation::AclRead(_)) && self.failure.is_some())
        {
            self.flags
        } else {
            Flags::None
        };
        #[cfg(target_os = "macos")]
        let advice = match flag {
            Flags::SystemImmutable => {
                detail.push_str("; system immutable flag is set");
                Some("ask an administrator to clear the system immutable flag and retry")
            }
            Flags::UserImmutable => {
                detail.push_str("; user immutable flag is set");
                Some("clear the immutable flag with chflags nouchg 'tab' and retry")
            }
            Flags::None if cause == Some(Cause::NotPermitted) && !self.no_directory() => {
                detail.push_str("; macOS privacy controls may prevent access");
                Some(PRIVACY)
            }
            Flags::None => None,
        };

        let known = matches!(self.entries, Entries::Known);
        if !self.no_directory() {
            let membership = match self.entries {
                Entries::Known => "",
                Entries::Empty { .. } => EMPTY,
                Entries::Undetermined(None) => UNKNOWN,
                Entries::Undetermined(Some(_)) => {
                    "cannot determine whether the directory holds a TAB entry (list: code 5)"
                }
            };
            if !membership.is_empty() {
                detail.push_str(&format!("; {membership}"));
            }
        }
        let mut remedy = match cause {
            Some(Cause::ReadOnly) => WRITABLE,
            Some(Cause::Other | Cause::Missing | Cause::NotDirectory) => "",
            #[cfg(unix)]
            Some(Cause::NotPermitted) => default,
            Some(Cause::AccessDenied | Cause::PermissionDenied) | None => default,
        };
        if self.failure.is_some() {
            match self.issue {
                #[cfg(unix)]
                Observation::Mode(_) if remedy == default => remedy = "",
                #[cfg(target_os = "macos")]
                Observation::Acl(_) if remedy == default => remedy = "",
                #[cfg(windows)]
                Observation::Owner { .. } | Observation::Dacl(_)
                    if self.setter && remedy == default =>
                {
                    remedy = ""
                }
                _ => {}
            }
        }
        #[cfg(target_os = "macos")]
        if matches!(self.issue, Observation::AclRead(_))
            && cause != Some(Cause::AccessDenied)
            && remedy == default
        {
            remedy = "";
        }
        #[cfg(target_os = "macos")]
        if let Some(advice) = advice {
            remedy = advice;
        }
        if self.no_directory() || !known {
            remedy = SELECT;
        }
        if matches!(self.issue, Observation::Absent) {
            remedy = ABSENT;
        }
        #[cfg(target_os = "macos")]
        if (matches!(self.issue, Observation::Open { .. })
            || (matches!(self.issue, Observation::AclRead(_)) && self.failure.is_none()))
            && advice == Some(PRIVACY)
        {
            remedy = PRIVACY;
        }

        if !remedy.is_empty() {
            detail.push_str(&format!("; {remedy}"));
        }
        if let Some(failure) = self.failure {
            if !self.operation().is_empty() {
                detail.push_str(&format!(": {}", self.operation()));
            }
            detail.push_str(&format!(": {}", failure.error()));
        }
        (detail, remedy, status)
    }
}

// Exercise the renderer even with incidental flags and errors that current callers mask.
// Structural exclusions are kept here. Only the two eligibility gates mark an empty listing required. ACL-read repair requires EACCES.
// A security setter failure requires a preparing caller and an actual failure.
fn product(mut visit: impl FnMut(&Case<'_>, &str)) {
    for issue in observations() {
        for (code, cause) in causes() {
            #[cfg(target_os = "macos")]
            if let Observation::AclRead(err) = &issue
                && code.is_some()
                && err.raw_os_error() != Some(libc::EACCES)
            {
                continue;
            }
            for (action_name, action) in [
                ("inspect", Action::Inspect),
                ("prepare-existing", Action::Prepare { created: false }),
                ("prepare-created", Action::Prepare { created: true }),
            ] {
                for (membership, entries) in [
                    ("known", Entries::Known),
                    (
                        "empty",
                        Entries::Empty {
                            required_for_repair: false,
                        },
                    ),
                    (
                        "required",
                        Entries::Empty {
                            required_for_repair: true,
                        },
                    ),
                    ("unknown", Entries::Undetermined(None)),
                    (
                        "listing-error",
                        Entries::Undetermined(Some(anyhow::anyhow!("list: code 5"))),
                    ),
                ] {
                    if membership == "required" {
                        let eligible = match &issue {
                            #[cfg(target_os = "macos")]
                            Observation::Acl(_) => true,
                            #[cfg(target_os = "macos")]
                            Observation::AclRead(e) => e.raw_os_error() == Some(libc::EACCES),
                            #[cfg(windows)]
                            Observation::Owner { .. } => true,
                            _ => false,
                        };
                        if action != (Action::Prepare { created: false })
                            || !eligible
                            || code.is_some()
                        {
                            continue;
                        }
                    }
                    for (flag, flags) in [
                        ("none", Flags::None),
                        #[cfg(target_os = "macos")]
                        ("user", Flags::UserImmutable),
                        #[cfg(target_os = "macos")]
                        ("system", Flags::SystemImmutable),
                    ] {
                        if matches!(issue, Observation::Absent)
                            && (action != Action::Inspect
                                || code.is_some()
                                || membership != "unknown"
                                || flags != Flags::None)
                        {
                            continue;
                        }
                        let case = Case {
                            issue: &issue,
                            action,
                            entries: &entries,
                            flags,
                            failure: code,
                            cause,
                            #[cfg(windows)]
                            setter: false,
                        };
                        let id =
                            format!("{issue:?}, {code:?}, {action_name}, {membership}, {flag}");
                        visit(&case, &id);
                        #[cfg(windows)]
                        if action != Action::Inspect
                            && code.is_some()
                            && matches!(issue, Observation::Owner { .. } | Observation::Dacl(_))
                        {
                            visit(
                                &Case {
                                    setter: true,
                                    ..case
                                },
                                &format!("{id}, setter"),
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn whole_root_diagnosis_for_each_consumer() {
    let mut tested = 0;
    product(|case, id| {
        tested += 1;
        let (text, remedy, status) = case.expectation();
        let doctor = crate::native::doctor::state_root_diagnosis(case.result());
        assert_eq!(doctor.detail, text, "{id}");
        assert_eq!(doctor.next_action, remedy, "{id}");
        assert_eq!(doctor.reason_code, status, "{id}");
        let refuse = apply_policy(case.result(), FailurePolicy::Refuse, |_| {
            panic!("{id} warned")
        })
        .unwrap_err();
        assert_eq!(format!("{refuse:#}"), text, "{id}");
        let mut warning = None;
        let result = apply_policy(case.result(), FailurePolicy::Warn, |line| {
            warning = Some(line)
        });
        if matches!(
            case.entries,
            Entries::Empty {
                required_for_repair: true
            }
        ) {
            assert_eq!(format!("{:#}", result.unwrap_err()), text, "{id}");
            assert_eq!(warning, None, "{id}");
        } else {
            result.unwrap();
            assert_eq!(warning, Some(format!("warning: {text}")), "{id}");
        }
    });
    eprintln!("checked {tested} combinations for all three consumers");
}

#[test]
fn remedy_invariants() {
    product(|case, id| {
        let row = crate::native::doctor::state_root_diagnosis(case.result());
        let mutating = [
            "chmod ",
            "chflags ",
            "icacls ",
            "take ownership",
            "clear the system immutable flag",
            "grant the token user",
        ];
        assert!(
            matches!(case.entries, Entries::Known)
                || !mutating.iter().any(|s| row.detail.contains(s)),
            "directory-changing advice: {id}: {}",
            row.detail
        );
        let remedies = [
            "set AGENT_",
            "TAB creates",
            "check access",
            "check search",
            "check read",
            "check the terminal",
            "select a ",
            "restore owner permissions with",
            "run chmod",
            "clear the immutable flag with",
            "ask an administrator",
            "take ownership",
            "grant the token user",
        ];
        assert!(
            remedies
                .iter()
                .map(|s| row.detail.matches(s).count())
                .sum::<usize>()
                <= 1,
            "multiple remedies: {id}: {}",
            row.detail
        );
        let repeated = match case.operation() {
            "fchmod: restore owner mode" => "chmod 700",
            "failed to remove the extended ACL" => "chmod -N",
            "SetSecurityInfo: set owner" => "take ownership",
            "SetSecurityInfo: set DACL" => "grant the token user",
            _ => "",
        };
        assert!(
            case.failure.is_none() || repeated.is_empty() || !row.next_action.contains(repeated),
            "repeated operation: {id}: {}",
            row.detail
        );
        let absent = matches!(case.issue, Observation::Absent) && case.action == Action::Inspect;
        assert_eq!(
            row.reason_code == "state_root_absent",
            absent,
            "false absence: {id}: {}",
            row.detail
        );
        assert_eq!(
            row.next_action == ABSENT,
            absent,
            "false creation advice: {id}: {}",
            row.detail
        );
        assert!(
            !case.no_directory() || !row.detail.contains("TAB entry"),
            "membership without directory: {id}: {}",
            row.detail
        );
        assert!(
            !row.detail.contains(".:"),
            "error punctuation: {id}: {}",
            row.detail
        );
    });
}

#[test]
fn only_inspection_reports_a_missing_path_as_absent() {
    let parent = tempfile::tempdir().unwrap();
    let missing = parent.path().join("missing");
    for action in [
        Action::Inspect,
        Action::Prepare { created: false },
        Action::Prepare { created: true },
    ] {
        let row =
            crate::native::doctor::state_root_diagnosis(super::super::check(&missing, action));
        let inspecting = action == Action::Inspect;
        assert_eq!(
            row.reason_code,
            if inspecting {
                "state_root_absent"
            } else {
                "state_root_unreadable"
            }
        );
        assert_eq!(row.next_action, if inspecting { ABSENT } else { SELECT });
        assert!(!row.detail.contains(".:"), "{}", row.detail);
        assert!(!missing.exists());
    }
}
