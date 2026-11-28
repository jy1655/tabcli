//! Evidence from the opened root and its single user-facing remedy.
use std::{fmt, io, path::Path};

#[cfg(target_os = "macos")]
const PRIVACY_ACCESS: &str = "check the terminal's macOS privacy access to this directory";
const SELECT_ROOT: &str = "set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB";

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) enum Cause {
    Missing,
    AccessDenied,
    PermissionDenied,
    #[cfg(unix)]
    NotPermitted,
    NotDirectory,
    ReadOnly,
    Other,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum ErrorCode {
    Native(i32),
    Kind(io::ErrorKind),
}

// The consumer matrix takes its inputs from these same discriminators. Keep native
// codes before kind fallbacks: on Darwin EPERM and EACCES have the same ErrorKind.
const CAUSES: &[(ErrorCode, Cause)] = {
    use ErrorCode::{Kind, Native};
    #[cfg(windows)]
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
        ERROR_WRITE_PROTECT,
    };
    &[
        #[cfg(unix)]
        (Native(libc::ENOENT), Cause::Missing),
        #[cfg(unix)]
        (Native(libc::EACCES), Cause::AccessDenied),
        #[cfg(unix)]
        (Native(libc::EPERM), Cause::NotPermitted),
        #[cfg(unix)]
        (Native(libc::ENOTDIR), Cause::NotDirectory),
        #[cfg(unix)]
        (Native(libc::EROFS), Cause::ReadOnly),
        #[cfg(windows)]
        (Native(ERROR_FILE_NOT_FOUND as i32), Cause::Missing),
        #[cfg(windows)]
        (Native(ERROR_PATH_NOT_FOUND as i32), Cause::Missing),
        #[cfg(windows)]
        (Native(ERROR_ACCESS_DENIED as i32), Cause::AccessDenied),
        #[cfg(windows)]
        (Native(ERROR_DIRECTORY as i32), Cause::NotDirectory),
        #[cfg(windows)]
        (Native(ERROR_WRITE_PROTECT as i32), Cause::ReadOnly),
        (Kind(io::ErrorKind::NotFound), Cause::Missing),
        (
            Kind(io::ErrorKind::PermissionDenied),
            Cause::PermissionDenied,
        ),
        (Kind(io::ErrorKind::NotADirectory), Cause::NotDirectory),
    ]
};

impl Cause {
    pub(super) fn of(error: &io::Error) -> Self {
        CAUSES
            .iter()
            .find_map(|&(code, cause)| {
                let matches = match code {
                    ErrorCode::Native(n) => error.raw_os_error() == Some(n),
                    ErrorCode::Kind(kind) => error.kind() == kind,
                };
                matches.then_some(cause)
            })
            .unwrap_or(Self::Other)
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub(super) struct SecurityWrite(pub(super) &'static str);

#[cfg(windows)]
impl fmt::Display for SecurityWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SetSecurityInfo: {}", self.0)
    }
}

pub(super) enum Entries {
    Known,
    Empty { required_for_repair: bool },
    Undetermined(Option<anyhow::Error>),
}

impl Entries {
    #[cfg(any(target_os = "macos", windows))]
    pub(super) fn required_for_repair(&self) -> &Self {
        match self {
            Self::Empty { .. } => &Self::Empty {
                required_for_repair: true,
            },
            other => other,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Flags {
    None,
    #[cfg(target_os = "macos")]
    UserImmutable,
    #[cfg(target_os = "macos")]
    SystemImmutable,
}

#[derive(Debug)]
pub(in crate::native::session) enum Observation {
    Absent,
    Open,
    NotDirectory,
    InspectPath,
    CreatePath,
    Inspect,
    Resolve,
    #[cfg(unix)]
    Reopen,
    #[cfg(unix)]
    Replaced,
    #[cfg(unix)]
    Read,
    #[cfg(target_os = "macos")]
    OpenWithOwnerAccess,
    #[cfg(windows)]
    ReadToken,
    #[cfg(windows)]
    ReadSecurity,
    #[cfg(windows)]
    Descriptor,
    #[cfg(windows)]
    Verify,
    #[cfg(unix)]
    Owner {
        actual: u32,
        expected: u32,
    },
    #[cfg(unix)]
    Mode(u32),
    #[cfg(target_os = "macos")]
    Acl(String),
    #[cfg(target_os = "macos")]
    AclRead(io::Error),
    #[cfg(windows)]
    Owner {
        actual: String,
        expected: String,
    },
    #[cfg(windows)]
    Dacl(String),
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub(in crate::native) enum RootStatus {
    NotPrivate,
    Unreadable,
    Absent,
}

#[derive(Debug)]
pub(in crate::native) struct Diagnosis {
    detail: String,
    pub(in crate::native) remedy: String,
    pub(in crate::native) status: RootStatus,
    pub(super) refuse_writers: bool,
}

impl fmt::Display for Diagnosis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)?;
        if !self.remedy.is_empty() {
            write!(f, "; {}", self.remedy)?;
        }
        Ok(())
    }
}
impl std::error::Error for Diagnosis {}

impl Observation {
    fn describe(&self, path: &Path) -> String {
        match self {
            Self::Absent => "state directory does not exist".into(),
            #[cfg(windows)]
            Self::Open => {
                "CreateFileW: open directory for READ_CONTROL and FILE_READ_ATTRIBUTES".into()
            }
            #[cfg(unix)]
            Self::Open => "open state directory".into(),
            Self::NotDirectory => "expected a directory".into(),
            Self::InspectPath => format!("failed to inspect state directory {}", path.display()),
            Self::CreatePath => format!("failed to create state directory {}", path.display()),
            Self::Inspect => "inspect state root".into(),
            Self::Resolve => "resolve state directory".into(),
            #[cfg(unix)]
            Self::Owner { actual, expected } => format!("owner uid {actual}, expected {expected}"),
            #[cfg(unix)]
            Self::Mode(mode) => format!("mode {mode:04o}, expected 0700"),
            #[cfg(unix)]
            Self::Reopen => "cannot reopen after restoring owner mode".into(),
            #[cfg(unix)]
            Self::Replaced => "directory changed during mode repair".into(),
            #[cfg(unix)]
            Self::Read => "read opened directory permissions".into(),
            #[cfg(target_os = "macos")]
            Self::OpenWithOwnerAccess => {
                "owner mode permits access but the directory open was denied".into()
            }
            #[cfg(target_os = "macos")]
            Self::Acl(entry) => entry.clone(),
            #[cfg(target_os = "macos")]
            Self::AclRead(error) => format!("cannot read extended ACL ({error})"),
            #[cfg(windows)]
            Self::Owner { actual, expected } => format!("owner SID {actual}, expected {expected}"),
            #[cfg(windows)]
            Self::Dacl(entry) => entry.clone(),
            #[cfg(windows)]
            Self::ReadToken => "read Windows token user and default owner".into(),
            #[cfg(windows)]
            Self::ReadSecurity => "read state root owner and DACL".into(),
            #[cfg(windows)]
            Self::Descriptor => "build private Windows security descriptor".into(),
            #[cfg(windows)]
            Self::Verify => "verify state root owner and DACL after repair".into(),
        }
    }

    pub(super) fn render(
        &self,
        path: &Path,
        entries: &Entries,
        flags: Flags,
        failure: Option<anyhow::Error>,
    ) -> anyhow::Error {
        use RootStatus::*;
        let os = failure.as_ref().and_then(|e| e.downcast_ref::<io::Error>());
        #[cfg(target_os = "macos")]
        let os = os.or(match self {
            Self::AclRead(e) => Some(e),
            _ => None,
        });
        let cause = os.map(Cause::of);
        let status = match self {
            Self::Absent => Absent,
            Self::NotDirectory | Self::Owner { .. } => NotPrivate,
            #[cfg(unix)]
            Self::Mode(_) => NotPrivate,
            #[cfg(target_os = "macos")]
            Self::Acl(_) => NotPrivate,
            #[cfg(windows)]
            Self::Dacl(_) => NotPrivate,
            _ => Unreadable,
        };
        let mut detail = self.describe(path);
        let no_directory = matches!(
            self,
            Self::Absent | Self::NotDirectory | Self::CreatePath | Self::Resolve
        ) || (matches!(self, Self::Open) && cause == Some(Cause::Missing));
        let read_only = cause == Some(Cause::ReadOnly);
        #[cfg(unix)]
        let quoted = || format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        #[cfg(not(target_os = "macos"))]
        let _ = flags;
        #[cfg(target_os = "macos")]
        let flags = if !read_only
            && (matches!(self, Self::Mode(_) | Self::Acl(_))
                || (matches!(self, Self::AclRead(_)) && failure.is_some()))
        {
            flags
        } else {
            Flags::None
        };
        #[cfg(target_os = "macos")]
        match flags {
            Flags::UserImmutable => detail.push_str("; user immutable flag is set"),
            Flags::SystemImmutable => detail.push_str("; system immutable flag is set"),
            Flags::None if !no_directory && cause == Some(Cause::NotPermitted) => {
                detail.push_str("; macOS privacy controls may prevent access")
            }
            Flags::None => {}
        }

        if !no_directory {
            match entries {
                Entries::Known => {}
                Entries::Empty { .. } => detail.push_str("; the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory)"),
                Entries::Undetermined(reason) => {
                    detail.push_str("; cannot determine whether the directory holds a TAB entry");
                    if let Some(reason) = reason {
                        detail.push_str(&format!(" ({reason:#})"));
                    }
                }
            }
        }
        let remedy = match entries {
            _ if status == Absent => "TAB creates it at the first session.".into(),
            _ if no_directory => SELECT_ROOT.into(),
            #[cfg(target_os = "macos")]
            _ if (matches!(self, Self::Open)
                || (matches!(self, Self::AclRead(_)) && failure.is_none()))
                && flags == Flags::None
                && cause == Some(Cause::NotPermitted) =>
            {
                PRIVACY_ACCESS.into()
            }
            Entries::Empty { .. } | Entries::Undetermined(_) => SELECT_ROOT.into(),
            Entries::Known if read_only => "select a writable directory meant for TAB".into(),
            #[cfg(target_os = "macos")]
            Entries::Known if flags == Flags::SystemImmutable => {
                "ask an administrator to clear the system immutable flag and retry".into()
            }
            #[cfg(target_os = "macos")]
            Entries::Known if flags == Flags::UserImmutable => format!(
                "clear the immutable flag with chflags nouchg {} and retry",
                quoted()
            ),
            #[cfg(target_os = "macos")]
            Entries::Known if cause == Some(Cause::NotPermitted) => PRIVACY_ACCESS.into(),
            // Unclassified I/O failures give no evidence that editing permissions helps.
            Entries::Known
                if matches!(
                    cause,
                    Some(Cause::Other | Cause::Missing | Cause::NotDirectory)
                ) =>
            {
                String::new()
            }
            #[cfg(windows)]
            Entries::Known
                if failure
                    .as_ref()
                    .is_some_and(|error| error.is::<SecurityWrite>()) =>
            {
                String::new()
            }
            Entries::Known => match self {
                Self::Open => "check access to the state directory".into(),
                Self::NotDirectory => SELECT_ROOT.into(),
                Self::InspectPath => "check search access to its parent directories".into(),
                #[cfg(unix)]
                Self::Owner { expected, .. } => {
                    format!("select a TAB directory owned by uid {expected}")
                }
                #[cfg(unix)]
                Self::Mode(_) if failure.is_none() => {
                    format!("restore owner permissions with chmod 700 {}", quoted())
                }
                #[cfg(target_os = "macos")]
                Self::Acl(_) if failure.is_none() => format!("run chmod -N {} and retry", quoted()),
                #[cfg(target_os = "macos")]
                Self::AclRead(_) if cause == Some(Cause::AccessDenied) => {
                    "check read access to the extended ACL".into()
                }
                #[cfg(windows)]
                Self::Owner { .. } => {
                    "take ownership of this TAB directory as the intended user and retry".into()
                }
                #[cfg(windows)]
                Self::Dacl(_) => {
                    "grant the token user control of this TAB directory's DACL and retry".into()
                }
                _ => String::new(),
            },
        };
        let diagnosis = Diagnosis {
            detail,
            remedy,
            status,
            refuse_writers: matches!(
                entries,
                Entries::Empty {
                    required_for_repair: true
                }
            ),
        };
        match failure {
            Some(error) => error.context(diagnosis),
            None => diagnosis.into(),
        }
    }
}

#[cfg(test)]
mod tests;
