//! Darwin sys/fcntl.h filesec properties and sys/acl.h. The extended stat
//! distinguishes an absent ACL property from an unreadable descriptor.
use super::*;
use std::{
    ffi::{c_int, c_void},
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

pub(super) fn first_acl_entry(file: &File) -> io::Result<Option<String>> {
    acl(file).map(|acl| acl.as_ref().map(first_entry))
}
