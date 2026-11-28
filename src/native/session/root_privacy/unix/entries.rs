use super::tab_entry;
use std::{ffi::CStr, fs::File, io, os::fd::AsRawFd};

// POSIX readdir uses a null result for both EOF and failure. errno is a
// thread-local macro in C; its backing libc accessor differs between Unix ABIs.
unsafe extern "C" {
    #[cfg_attr(
        any(target_vendor = "apple", target_os = "freebsd"),
        link_name = "__error"
    )]
    #[cfg_attr(
        any(
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "android",
            target_os = "cygwin",
            target_os = "nuttx",
            target_env = "newlib"
        ),
        link_name = "__errno"
    )]
    #[cfg_attr(
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "hurd",
            target_os = "redox",
            target_os = "emscripten",
            target_os = "l4re"
        ),
        link_name = "__errno_location"
    )]
    #[cfg_attr(
        any(target_os = "solaris", target_os = "illumos"),
        link_name = "___errno"
    )]
    #[cfg_attr(target_os = "aix", link_name = "_Errno")]
    #[cfg_attr(target_os = "nto", link_name = "__get_errno_ptr")]
    #[cfg_attr(target_os = "haiku", link_name = "_errnop")]
    fn errno_location() -> *mut libc::c_int;
}

pub(super) fn has_tab_entry(root: &File) -> io::Result<bool> {
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
    unsafe {
        libc::rewinddir(stream.0);
    }
    loop {
        unsafe {
            *errno_location() = 0;
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
        if !tab_entry(text, true, true) {
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
        if tab_entry(text, kind == libc::S_IFDIR, kind == libc::S_IFREG) {
            return Ok(true);
        }
    }
}
