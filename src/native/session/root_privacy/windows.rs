use super::{Action, Entries, Flags, Observation, tab_entry};
use crate::native::consent::{windows_current_account, windows_owner_is_accepted, windows_sid};
use anyhow::{Context, Result};
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr,
};
use windows_sys::Wdk::{
    Foundation::OBJECT_ATTRIBUTES,
    Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_OPEN_FOR_BACKUP_INTENT, FILE_SYNCHRONOUS_IO_NONALERT, NtOpenFile,
    },
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::{
    Foundation::{ERROR_NO_MORE_FILES, LocalFree, RtlNtStatusToDosError, UNICODE_STRING},
    Security::{
        ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT, SetSecurityInfo,
        },
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CreateDirectoryW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileFullDirectoryInfo,
        FileFullDirectoryRestartInfo, GetFileInformationByHandleEx, READ_CONTROL, SYNCHRONIZE,
        WRITE_DAC, WRITE_OWNER,
    },
};

const SHARING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const READ_SECURITY: u32 = READ_CONTROL | FILE_READ_ATTRIBUTES;
const ADMINISTRATORS: &str = "S-1-5-32-544";

struct Descriptor(*mut c_void);
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
impl Descriptor {
    fn private(user: &str, assign_owner: bool) -> io::Result<Self> {
        let owner = if assign_owner {
            format!("O:{user}")
        } else {
            String::new()
        };
        let sddl: Vec<u16> = format!(
            "{owner}{}",
            crate::native::terminal::windows::private_sddl(user, true)
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(descriptor))
    }
    fn owner(&self) -> io::Result<*mut c_void> {
        let mut owner = ptr::null_mut();
        let mut defaulted = 0;
        if unsafe { GetSecurityDescriptorOwner(self.0, &mut owner, &mut defaulted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if owner.is_null() {
            return Err(io::Error::other("security descriptor has no owner"));
        }
        Ok(owner)
    }
    fn dacl(&self) -> io::Result<*mut ACL> {
        let mut dacl = ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(if present == 0 { ptr::null_mut() } else { dacl })
    }
}

pub(super) fn create_leaf(path: &Path) -> io::Result<()> {
    let account = windows_current_account().map_err(io::Error::other)?;
    let descriptor = Descriptor::private(&account.owners[0], false)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn reopen(root: &File, rights: u32, options: u32) -> io::Result<File> {
    // ReOpenFile adds FILE_NON_DIRECTORY_FILE on Windows even with backup
    // semantics. An empty name relative to the held directory reopens that
    // same object, including when the selected junction has been retargeted.
    let mut nul = 0u16;
    let name = UNICODE_STRING {
        Length: 0,
        MaximumLength: 2,
        Buffer: &mut nul,
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root.as_raw_handle(),
        ObjectName: &name,
        ..Default::default()
    };
    let mut handle = ptr::null_mut();
    let mut result = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtOpenFile(
            &mut handle,
            rights,
            &attributes,
            &mut result,
            SHARING,
            FILE_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT | options,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn has_tab_entry(root: &File) -> Result<bool> {
    let listing = reopen(
        root,
        FILE_LIST_DIRECTORY | SYNCHRONIZE,
        FILE_SYNCHRONOUS_IO_NONALERT,
    )
    .context("NtOpenFile: reopen directory for FILE_LIST_DIRECTORY")?;
    let mut buffer = vec![0u64; 8192];
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        if unsafe {
            GetFileInformationByHandleEx(
                listing.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(buffer.as_slice()) as u32,
            )
        } == 0
        {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                Ok(false)
            } else {
                Err(e).context("GetFileInformationByHandleEx: enumerate TAB entries")
            };
        }
        class = FileFullDirectoryInfo;
        let mut offset = 0;
        loop {
            let bytes = std::mem::size_of_val(buffer.as_slice()) - offset;
            if bytes < size_of::<FILE_FULL_DIR_INFO>() {
                anyhow::bail!("GetFileInformationByHandleEx: truncated directory information");
            }
            let entry = unsafe {
                &*buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_FULL_DIR_INFO>()
            };
            let name_len = entry.FileNameLength as usize;
            if !name_len.is_multiple_of(2)
                || name_len > bytes - offset_of!(FILE_FULL_DIR_INFO, FileName)
            {
                anyhow::bail!("GetFileInformationByHandleEx: invalid directory entry name length");
            }
            if entry.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                let name = unsafe {
                    std::slice::from_raw_parts(
                        buffer
                            .as_ptr()
                            .cast::<u8>()
                            .add(offset + offset_of!(FILE_FULL_DIR_INFO, FileName))
                            .cast::<u16>(),
                        name_len / 2,
                    )
                };
                let directory = entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                if String::from_utf16(name)
                    .is_ok_and(|name| tab_entry(&name, directory, !directory))
                {
                    return Ok(true);
                }
            }
            let next = entry.NextEntryOffset as usize;
            if next == 0 {
                break;
            }
            if next > bytes || !next.is_multiple_of(8) {
                anyhow::bail!("GetFileInformationByHandleEx: invalid directory entry offset");
            }
            offset += next;
        }
    }
}

struct Security {
    owner: String,
    dacl_issue: Option<String>,
}
impl Security {
    fn read(root: &File, user: &str) -> Result<Self> {
        let mut descriptor = ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                root.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32))
                .context("GetSecurityInfo: read owner and DACL");
        }
        let descriptor = Descriptor(descriptor);
        let owner = windows_sid(descriptor.owner()?)?;
        let dacl_issue = dacl_issue(&descriptor, user)?;
        Ok(Self { owner, dacl_issue })
    }

    fn judge(&self, owners: &[String]) -> Option<Observation> {
        if !windows_owner_is_accepted(&self.owner, owners) {
            return Some(Observation::Owner {
                actual: self.owner.clone(),
                expected: owners.join(" or "),
            });
        }
        self.dacl_issue.clone().map(Observation::Dacl)
    }
}

fn dacl_issue(descriptor: &Descriptor, user: &str) -> Result<Option<String>> {
    let dacl = descriptor.dacl()?;
    if dacl.is_null() {
        return Ok(Some("missing or null DACL".into()));
    }
    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let count = unsafe { (*dacl).AceCount };
    if count == 0 {
        return Ok(Some("empty DACL".into()));
    }
    let flags = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
    for index in 0..count {
        let mut ace = ptr::null_mut();
        if unsafe { GetAce(dacl, index.into(), &mut ace) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        // ACCESS_ALLOWED_ACE and ACCESS_DENIED_ACE put Mask and SidStart at the same offsets.
        if !matches!(header.AceType, 0 | 1)
            || usize::from(header.AceSize) < size_of::<ACCESS_DENIED_ACE>()
        {
            return Ok(Some(format!(
                "DACL entry {index} has type {}",
                header.AceType
            )));
        }
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        let sid = windows_sid(unsafe { ptr::addr_of_mut!((*ace).SidStart) }.cast())?;
        let mask = unsafe { (*ace).Mask };
        if index != 0
            || header.AceType != 0
            || header.AceFlags != flags
            || mask != FILE_ALL_ACCESS
            || sid != user
        {
            return Ok(Some(format!(
                "DACL entry {index}, SID {sid}, type {}, flags {:#x}, rights {mask:#x}",
                header.AceType, header.AceFlags
            )));
        }
    }
    Ok((control & SE_DACL_PROTECTED == 0).then(|| "DACL is unprotected".into()))
}

fn set(root: &File, descriptor: &Descriptor, owner: bool) -> Result<()> {
    let (rights, operation) = if owner {
        (WRITE_OWNER, "set owner")
    } else {
        (READ_SECURITY | WRITE_DAC, "set DACL")
    };
    let file = reopen(root, rights, 0).with_context(|| {
        format!("NtOpenFile: reopen directory to {operation} (access {rights:#x})")
    })?;
    let (information, sid, dacl) = if owner {
        (
            OWNER_SECURITY_INFORMATION,
            descriptor
                .owner()
                .context("read SID for SetSecurityInfo: set owner")?,
            ptr::null_mut(),
        )
    } else {
        (
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            descriptor
                .dacl()
                .context("read ACL for SetSecurityInfo: set DACL")?,
        )
    };
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            information,
            sid,
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("SetSecurityInfo: {operation}"));
    }
    Ok(())
}

pub(super) fn check(path: &Path, action: Action) -> Result<bool> {
    let root = OpenOptions::new()
        .access_mode(READ_SECURITY)
        .share_mode(SHARING)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(|error| {
            Observation::Open.render(path, &action.unopened(), Flags::None, Some(error.into()))
        })?;
    if !root
        .metadata()
        .context("read opened directory attributes")?
        .is_dir()
    {
        return Err(Observation::NotDirectory.render(
            path,
            &Entries::Undetermined(None),
            Flags::None,
            None,
        ));
    }
    let origin = std::cell::OnceCell::new();
    let entries = || origin.get_or_init(|| action.entries(|| has_tab_entry(&root)));
    let diagnose = |issue: Observation, error| issue.render(path, entries(), Flags::None, error);
    let owners = windows_current_account()
        .map_err(|e| diagnose(Observation::ReadToken, Some(e)))?
        .owners;
    let user = &owners[0];
    let before =
        Security::read(&root, user).map_err(|e| diagnose(Observation::ReadSecurity, Some(e)))?;
    let Some(issue) = before.judge(&owners) else {
        return Ok(false);
    };
    if action == Action::Inspect {
        return Err(diagnose(issue, None));
    }

    let owner_change = matches!(issue, Observation::Owner { .. });
    let descriptor = Descriptor::private(user, owner_change)
        .map_err(|e| diagnose(Observation::Descriptor, Some(e.into())))?;
    // Judge eligibility on the original directory before changing its grants.
    let owner_result = (|| -> Result<()> {
        if owner_change {
            if !matches!(entries(), Entries::Known) {
                return Err(issue.render(path, entries().required_for_repair(), Flags::None, None));
            }
            if before.owner != ADMINISTRATORS {
                return Err(diagnose(issue, None));
            }
            set(&root, &descriptor, true).map_err(|e| diagnose(issue, Some(e)))?;
            #[cfg(test)]
            crate::native::session::primitives::fault_point("after setting the state root owner")?;
        }
        Ok(())
    })();
    // The existing DACL repair applies even when ownership could not be repaired.
    let dacl_result = before.dacl_issue.map_or(Ok(()), |problem| {
        set(&root, &descriptor, false).map_err(|e| diagnose(Observation::Dacl(problem), Some(e)))
    });
    owner_result?;
    dacl_result?;
    let after = Security::read(&root, user).map_err(|e| diagnose(Observation::Verify, Some(e)))?;
    if let Some(issue) = after.judge(&owners) {
        return Err(diagnose(issue, None)).context("verify state root owner and DACL after repair");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopen_directory_for_dacl_repair() {
        let dir = tempfile::tempdir().unwrap();
        let root = OpenOptions::new()
            .access_mode(READ_SECURITY)
            .share_mode(SHARING)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir.path())
            .unwrap();
        // Same access as the failing DACL repair, before any descriptor setter.
        let writer = reopen(&root, WRITE_DAC, 0).expect("reopen directory for WRITE_DAC");
        drop(writer);
        std::fs::create_dir(dir.path().join("workspace-consent")).unwrap();
        assert!(has_tab_entry(&root).unwrap());
    }
}
