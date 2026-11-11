//! The filesystem seam: **every** read in this crate goes through a root.
//!
//! The sampler must be testable without a live browser, without a delegated
//! cgroup and without the host's real `/proc` or `/sys`. [`FsRoot`] is the small
//! trait that makes that true: production passes a [`Path`] that *is* `/`, tests
//! pass a fixture directory or an in-memory tree. Nothing in this crate opens an
//! absolute path directly.
//!
//! Reads are best-effort by design: a file that is absent (a vanished PID, a
//! kernel without `memory.peak`, a non-Linux host) is reported as absent, never
//! as a fabricated zero and never as a panic.
//!
//! [`CgroupFs`] adds the two mutation operations the delegation probe needs
//! (strict `mkdir` / `rmdir`, cgroup v2 semantics) so
//! [`delegation_status`](crate::cgroup::delegation_status) can be decided by a
//! **capability**, not by reading a unit file — and can be exercised in a
//! tempdir without ever writing to the real hierarchy.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// A POSIX process id.
///
/// A newtype so a bare integer can never be mistaken for a jiffie count, a
/// generation or a percentage as those flow through the sampler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Pid(u32);

impl Pid {
    /// Wrap a raw process id.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw process id.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The error type of [`Pid::from_str`](std::str::FromStr::from_str).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsePidError;

impl std::fmt::Display for ParsePidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not a process id")
    }
}

impl std::error::Error for ParsePidError {}

impl std::str::FromStr for Pid {
    type Err = ParsePidError;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        text.parse::<u32>().map(Self).map_err(|_| ParsePidError)
    }
}

/// A read-only view of a file tree.
///
/// Every path passed in is **root-relative**: `proc/1/stat`, `sys/fs/cgroup/...`.
/// An implementation may resolve that under a fixture directory (tests) or the
/// filesystem root (production) — the caller cannot tell the difference, which
/// is exactly the property the crate's tests rely on.
pub trait FsRoot {
    /// Read a whole file as UTF-8 text.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file could not be read, including
    /// [`io::ErrorKind::NotFound`] for an absent file.
    fn read_to_string(&self, path: &str) -> Result<String>;

    /// Whether `path` exists.
    fn exists(&self, path: &str) -> bool;

    /// The names directly under `path`, in a deterministic order.
    ///
    /// A missing directory yields an empty list rather than an error: this is a
    /// scan of a live `/proc` that changes under the reader.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory exists but cannot be listed.
    fn read_dir_names(&self, path: &str) -> Result<Vec<String>>;
}

/// The two mutation operations a cgroup capability probe needs.
///
/// cgroup v2 `mkdir` fails with `EBUSY`/`EPERM` when the caller is not
/// delegated a subtree, and `rmdir` fails while the directory holds processes —
/// so "can I create *and* remove a child directory here?" is the delegation
/// test ADR-0102 decision 9 specifies. Both operations are strict (`mkdir`
/// instead of `create_dir_all`, `rmdir` instead of `remove_dir_all`) so the
/// probe cannot silently succeed by creating parents.
pub trait CgroupFs: FsRoot {
    /// Create the single directory `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory could not be created.
    fn create_dir(&self, path: &str) -> Result<()>;

    /// Remove the empty directory `path`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory could not be removed.
    fn remove_dir(&self, path: &str) -> Result<()>;
}

/// Resolve a root-relative path against `root`.
///
/// Returns `None` for a path that escapes the root (any `..` component), so a
/// fixture root can never be walked out of by a malformed scope path.
#[must_use]
pub fn resolve(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut out = PathBuf::from(root);
    for component in Path::new(relative.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

impl FsRoot for Path {
    fn read_to_string(&self, path: &str) -> Result<String> {
        let resolved = resolve(self, path).ok_or_else(|| Error::missing(path))?;
        std::fs::read_to_string(&resolved).map_err(|source| Error::io(path, source))
    }

    fn exists(&self, path: &str) -> bool {
        resolve(self, path).is_some_and(|resolved| resolved.exists())
    }

    fn read_dir_names(&self, path: &str) -> Result<Vec<String>> {
        let Some(resolved) = resolve(self, path) else {
            return Ok(Vec::new());
        };
        let entries = match std::fs::read_dir(&resolved) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(Error::io(path, source)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::io(path, source))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

impl CgroupFs for Path {
    fn create_dir(&self, path: &str) -> Result<()> {
        let resolved = resolve(self, path).ok_or_else(|| Error::missing(path))?;
        std::fs::create_dir(&resolved).map_err(|source| Error::io(path, source))
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        let resolved = resolve(self, path).ok_or_else(|| Error::missing(path))?;
        std::fs::remove_dir(&resolved).map_err(|source| Error::io(path, source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_joins_relative_paths_under_the_root() {
        let resolved = resolve(Path::new("/fixture"), "/proc/1/stat");
        assert_eq!(resolved.as_deref(), Some(Path::new("/fixture/proc/1/stat")));
        assert_eq!(
            resolve(Path::new("/"), "proc/1/stat").as_deref(),
            Some(Path::new("/proc/1/stat"))
        );
    }

    #[test]
    fn resolve_refuses_to_escape_the_root() {
        assert!(resolve(Path::new("/fixture"), "../etc/passwd").is_none());
        assert!(resolve(Path::new("/fixture"), "proc/../../etc").is_none());
    }

    #[test]
    fn pid_round_trips_through_text() {
        let pid: Pid = "4242".parse().expect("numeric");
        assert_eq!(pid.as_u32(), 4242);
        assert_eq!(pid.to_string(), "4242");
        assert!("x".parse::<Pid>().is_err());
    }
}
