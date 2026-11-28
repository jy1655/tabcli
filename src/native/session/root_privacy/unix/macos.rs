//! Darwin sys/fcntl.h filesec properties and sys/acl.h. The extended stat
//! distinguishes an absent ACL property from an unreadable descriptor.
use super::*;
use std::{
    ffi::{CStr, c_int, c_void},
    os::fd::AsRawFd,
    ptr::{self, NonNull},
};

type Object = *mut c_void;
const FILESEC_MODE: c_int = 4;
const FILESEC_ACL: c_int = 5;

unsafe extern "C" {
    fn filesec_init() -> Object;
    fn filesec_free(sec: Object);
    fn filesec_get_property(sec: Object, property: c_int, value: Object) -> c_int;
    fn filesec_set_property(sec: Object, property: c_int, value: *const c_void) -> c_int;
    #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
    fn fstatx_np(fd: c_int, stat: *mut libc::stat, sec: Object) -> c_int;
    fn fchmodx_np(fd: c_int, sec: Object) -> c_int;
    fn acl_get_entry(acl: Object, index: c_int, entry: *mut Object) -> c_int;
    fn acl_get_qualifier(entry: Object) -> Object;
    fn acl_get_tag_type(entry: Object, tag: *mut c_int) -> c_int;
    fn acl_free(object: Object) -> c_int;
}
struct FileSec(NonNull<c_void>);
impl FileSec {
    fn new() -> io::Result<Self> {
        NonNull::new(unsafe { filesec_init() })
            .map(Self)
            .ok_or_else(io::Error::last_os_error)
    }
}
impl Drop for FileSec {
    fn drop(&mut self) {
        unsafe { filesec_free(self.0.as_ptr()) }
    }
}
struct AclAllocation(NonNull<c_void>);
impl Drop for AclAllocation {
    fn drop(&mut self) {
        unsafe {
            acl_free(self.0.as_ptr());
        }
    }
}

fn acl(file: &File) -> io::Result<Option<AclAllocation>> {
    let sec = FileSec::new()?;
    let mut stat = std::mem::MaybeUninit::uninit();
    if unsafe { fstatx_np(file.as_raw_fd(), stat.as_mut_ptr(), sec.0.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut acl: Object = ptr::null_mut();
    if unsafe { filesec_get_property(sec.0.as_ptr(), FILESEC_ACL, (&raw mut acl).cast()) } != 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    NonNull::new(acl)
        .map(|p| Some(AclAllocation(p)))
        .ok_or_else(|| io::Error::other("null extended ACL property"))
}

fn first_entry(acl: &AclAllocation) -> String {
    let mut entry = ptr::null_mut();
    if unsafe { acl_get_entry(acl.0.as_ptr(), 0, &mut entry) } != 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::EINVAL) {
            "extended ACL with zero entries".into()
        } else {
            format!("extended ACL entry unreadable ({error})")
        };
    }
    let mut tag = 0;
    let named = match unsafe { acl_get_tag_type(entry, &mut tag) } {
        0 if tag == 1 => "allow",
        0 if tag == 2 => "deny",
        _ => "unknown type",
    };
    let Some(identifier) = NonNull::new(unsafe { acl_get_qualifier(entry) }) else {
        return format!("ACL entry 0 ({named}, principal unreadable)");
    };
    let uuid = AclAllocation(identifier);
    let bytes = unsafe { uuid.0.cast::<[u8; 16]>().as_ref() };
    let mut text = String::new();
    use std::fmt::Write;
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        write!(text, "{byte:02x}").unwrap();
    }
    format!("ACL entry 0 ({named}, principal {text})")
}

fn has_tab_entry(root: &File) -> io::Result<bool> {
    // fdopendir owns the duplicate; readdir and fstatat stay on the opened root.
    let duplicate = root.try_clone()?;
    use std::os::fd::IntoRawFd;
    let stream = unsafe { libc::fdopendir(duplicate.as_raw_fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _ = duplicate.into_raw_fd();
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    loop {
        unsafe {
            *libc::__error() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let Ok(text) = name.to_str() else { continue };
        if !super::super::tab_entry(text, true, true) {
            continue;
        }
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe {
            libc::fstatat(
                root.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                continue;
            }
            return Err(error);
        }
        let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
        if super::super::tab_entry(text, kind == libc::S_IFDIR, kind == libc::S_IFREG) {
            return Ok(true);
        }
    }
}

pub(super) fn is_immutable(file: &File) -> bool {
    use std::os::darwin::fs::MetadataExt;
    file.metadata()
        .is_ok_and(|meta| meta.st_flags() & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE) != 0)
}

pub(super) fn clear_acl(file: &File) -> io::Result<()> {
    let sec = FileSec::new()?;
    let mode: libc::mode_t = 0o700;
    // _FILESEC_REMOVE_ACL is the Darwin sentinel (void *)1 used by chmod -N.
    let remove = ptr::without_provenance(1);
    if unsafe { filesec_set_property(sec.0.as_ptr(), FILESEC_MODE, (&raw const mode).cast()) } != 0
        || unsafe { filesec_set_property(sec.0.as_ptr(), FILESEC_ACL, remove) } != 0
        || unsafe { fchmodx_np(file.as_raw_fd(), sec.0.as_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn acl_remedy(path: &Path) -> String {
    format!(
        "run chmod -N '{}' and retry",
        path.display().to_string().replace('\'', "'\\''")
    )
}

pub(super) fn first_acl_entry(file: &File) -> io::Result<Option<String>> {
    acl(file).map(|acl| acl.as_ref().map(first_entry))
}

pub(super) fn acl_removal_allowed(file: &File, mode: u32) -> io::Result<bool> {
    // Search may need the owner's mode restored before it can see TAB entries.
    if mode != 0o700 {
        file.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    has_tab_entry(file)
}
