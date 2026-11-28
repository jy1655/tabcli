//! Evidence from the opened root and its single user-facing remedy.
use std::{fmt, io, path::Path};

const SELECT_ROOT: &str = "set AGENT_BRIDGE_NATIVE_STATE_DIR to a directory meant for TAB";

pub(super) enum Entries {
    Known,
    Empty { required_for_repair: bool },
    Undetermined(Option<anyhow::Error>),
}

impl Entries {
    #[cfg(any(target_os = "macos", windows, test))]
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
        let status = match self {
            Self::Open if os.is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => Absent,
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
        let code = os.and_then(io::Error::raw_os_error);
        #[cfg(unix)]
        let read_only = code == Some(libc::EROFS);
        #[cfg(windows)]
        let read_only = code == Some(windows_sys::Win32::Foundation::ERROR_WRITE_PROTECT as i32);
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
            Flags::None if code == Some(libc::EPERM) => {
                detail.push_str("; macOS privacy controls may prevent access")
            }
            Flags::None => {}
        }

        let remedy = match entries {
            _ if status == Absent => "TAB creates it at the first session.".into(),
            Entries::Empty { .. } => {
                detail.push_str("; the directory holds no TAB entry (state-root.durable, settings.json, workspace-consent/ or a session-* directory)");
                SELECT_ROOT.into()
            }
            Entries::Undetermined(reason) => {
                detail.push_str("; cannot determine whether the directory holds a TAB entry");
                if let Some(reason) = reason {
                    detail.push_str(&format!(" ({reason:#})"));
                }
                SELECT_ROOT.into()
            }
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
            Entries::Known if code == Some(libc::EPERM) => {
                "check the terminal's macOS privacy access to this directory".into()
            }
            // Unclassified I/O failures give no evidence that editing permissions helps.
            Entries::Known if os.is_some_and(|e| e.kind() != io::ErrorKind::PermissionDenied) => {
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
                Self::Acl(_) => format!("run chmod -N {} and retry", quoted()),
                #[cfg(target_os = "macos")]
                Self::AclRead(_) if code == Some(libc::EACCES) => {
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
