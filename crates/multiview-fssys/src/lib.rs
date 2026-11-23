//! Filesystem space measurement for recorder workers. Never call on the output clock.
use std::{io, path::Path};

/// Capacity and space available to an unprivileged writer, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub avail_bytes: u64,
    pub total_bytes: u64,
}

/// Query filesystem capacity. This syscall may block; call only off-clock.
///
/// # Errors
/// Returns an I/O error for invalid paths or a failed filesystem query.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn statvfs_avail(path: &Path) -> io::Result<DiskSpace> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut result = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: path is a NUL-terminated CString and result points to writable storage.
    let rc = unsafe { libc::statvfs(path.as_ptr(), result.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statvfs initialized the entire output structure.
    let result = unsafe { result.assume_init() };
    let unit = u64::try_from(result.f_frsize).unwrap_or(u64::MAX);
    let available = u64::try_from(result.f_bavail).unwrap_or(u64::MAX);
    let total = u64::try_from(result.f_blocks).unwrap_or(u64::MAX);
    Ok(DiskSpace {
        avail_bytes: available.saturating_mul(unit),
        total_bytes: total.saturating_mul(unit),
    })
}

/// Unsupported platforms fail closed instead of claiming free space.
///
/// # Errors
/// Always returns an unsupported-platform error.
#[cfg(not(unix))]
pub fn statvfs_avail(_path: &Path) -> io::Result<DiskSpace> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "filesystem space probe unsupported",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_path_is_plausible_and_missing_path_fails() {
        #[cfg(unix)]
        {
            let space = statvfs_avail(Path::new(".")).expect("current directory");
            assert!(space.total_bytes > 0);
            assert!(space.avail_bytes <= space.total_bytes);
            assert!(statvfs_avail(Path::new("/this-path-must-not-exist-conspect-fssys")).is_err());
        }
    }
}
