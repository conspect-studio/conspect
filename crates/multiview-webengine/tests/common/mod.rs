//! Shared test scaffolding for the `multiview-webengine` integration tests.
//!
//! The crate's whole point is that no read reaches the host's real `/proc` or
//! `/sys`, so every integration test builds one of two roots: a tempdir tree
//! written file-by-file ([`ProcTree`]) or an in-memory fake
//! ([`FakeRoot`]). Both are read through `multiview_webengine::FsRoot`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc,
    dead_code,
    unreachable_pub
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use multiview_webengine::{CgroupFs, Error, FsRoot, Result};

/// A process to materialise under `<root>/proc/<pid>`.
pub struct ProcSpec {
    /// The process id, which is also its directory name.
    pub pid: u32,
    /// `comm`, i.e. the parenthesised field 2 of `stat`.
    pub comm: String,
    /// The discovering parent's pid (field 4 of `stat`).
    pub ppid: u32,
    /// `utime`, in clock ticks.
    pub utime: u64,
    /// `stime`, in clock ticks.
    pub stime: u64,
    /// The raw argv content written to `cmdline` (NULs included by the caller).
    pub cmdline: Option<String>,
    /// The `smaps_rollup` body, when the process should have one.
    pub smaps: Option<String>,
    /// Explicit children, written to `<pid>/task/<pid>/children`, when the
    /// caller wants the authoritative kernel strand instead of the `stat`
    /// fallback scan.
    pub children: Option<Vec<u32>>,
}

impl ProcSpec {
    /// A process with the minimum needed to be discovered, parented to pid 1.
    #[must_use]
    pub fn new(pid: u32, comm: &str, utime: u64, stime: u64) -> Self {
        Self {
            pid,
            comm: comm.to_owned(),
            ppid: 1,
            utime,
            stime,
            cmdline: None,
            smaps: None,
            children: None,
        }
    }

    /// Give this process a parent, so the `stat` ppid fallback can discover it.
    #[must_use]
    pub fn with_parent(mut self, parent: u32) -> Self {
        self.ppid = parent;
        self
    }

    /// Give this process a `--type=`-bearing argv (NUL-separated).
    #[must_use]
    pub fn with_type(mut self, chromium_type: &str) -> Self {
        let mut argv = String::from("/opt/chrome-headless-shell");
        for arg in ["--headless=new", "--no-sandbox"] {
            argv.push('\0');
            argv.push_str(arg);
        }
        argv.push_str("\0--type=");
        argv.push_str(chromium_type);
        self.cmdline = Some(argv);
        self
    }

    /// Give this process a raw argv body.
    #[must_use]
    pub fn with_cmdline(mut self, argv: &str) -> Self {
        self.cmdline = Some(argv.to_owned());
        self
    }

    /// Give this process a `Pss` `smaps_rollup`.
    #[must_use]
    pub fn with_pss_kib(mut self, pss_kib: u64) -> Self {
        self.smaps = Some(format!("Pss: {pss_kib} kB\nRss: {} kB\n", pss_kib * 2));
        self
    }

    /// Give this process an `Rss`-only `smaps_rollup` (the PSS fallback case).
    #[must_use]
    pub fn with_rss_only_kib(mut self, rss_kib: u64) -> Self {
        self.smaps = Some(format!("Rss: {rss_kib} kB\n"));
        self
    }

    /// Write this process's explicit children strand.
    #[must_use]
    pub fn with_children(mut self, children: &[u32]) -> Self {
        self.children = Some(children.to_vec());
        self
    }

    /// The `/proc/<pid>/stat` body this process materialises as.
    ///
    /// The field count matches a real kernel line through `stime` (field 15), so
    /// a parser that counts from the wrong place fails loudly.
    #[must_use]
    pub fn stat_body(&self) -> String {
        format!(
            "{} ({}) S {} {} {} 0 -1 4194560 0 0 0 0 {} {} 0 0 20 0 5 0 900 0 0\n",
            self.pid, self.comm, self.ppid, self.pid, self.pid, self.utime, self.stime
        )
    }
}

/// A tempdir holding a written `/proc` tree.
pub struct ProcTree {
    dir: tempfile::TempDir,
    specs: Vec<ProcSpec>,
}

impl ProcTree {
    /// Write a tree of `specs` under a fresh tempdir and return a handle to it.
    #[must_use]
    pub fn build(specs: Vec<ProcSpec>) -> Self {
        let dir = tempfile::tempdir().expect("a tempdir");
        for spec in &specs {
            let base = dir.path().join("proc").join(spec.pid.to_string());
            let task = base.join("task").join(spec.pid.to_string());
            fs::create_dir_all(&task).expect("proc dirs");
            fs::write(base.join("stat"), spec.stat_body()).expect("stat");
            if let Some(smaps) = &spec.smaps {
                fs::write(base.join("smaps_rollup"), smaps).expect("smaps_rollup");
            }
            if let Some(cmdline) = &spec.cmdline {
                fs::write(base.join("cmdline"), cmdline).expect("cmdline");
            }
            if let Some(children) = &spec.children {
                let body: Vec<String> = children.iter().map(u32::to_string).collect();
                fs::write(task.join("children"), body.join(" ")).expect("children");
            }
        }
        Self { dir, specs }
    }

    /// The root-relative `/proc` path this tree is read through.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// The spec for `pid`.
    #[must_use]
    pub fn spec(&self, pid: u32) -> &ProcSpec {
        self.specs
            .iter()
            .find(|spec| spec.pid == pid)
            .expect("a spec for that pid")
    }

    /// Move `pid`'s directory aside, as if the process had exited.
    pub fn remove(&self, pid: u32) {
        let base = self.dir.path().join("proc").join(pid.to_string());
        fs::remove_dir_all(&base).expect("remove a /proc entry");
    }

    /// The absolute path of a written file, for a direct assertion.
    #[must_use]
    pub fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative.trim_start_matches('/'))
    }
}

/// An in-memory [`FsRoot`] + [`CgroupFs`], for the cgroup cases where the
/// interesting behaviour is a *failed* `mkdir` or a *missing* controller.
#[derive(Default)]
pub struct FakeRoot {
    /// File contents, keyed by root-relative path.
    pub files: BTreeMap<String, String>,
    /// Directories that exist, as root-relative paths.
    pub dirs: BTreeSet<String>,
    /// Whether `mkdir` succeeds at all.
    pub allow_create: bool,
    /// Whether `rmdir` succeeds at all.
    pub allow_remove: bool,
    /// Every path a mutating operation was asked to touch.
    pub writes: Vec<String>,
}

impl FakeRoot {
    /// A root that permits every mutation (a fully delegated subtree shape).
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            allow_create: true,
            allow_remove: true,
            ..Self::default()
        }
    }

    /// Add a file, materialising every parent directory (as a real tree would).
    pub fn with_file(mut self, path: &str, body: &str) -> Self {
        let path = normalise(path);
        self.files.insert(path.clone(), body.to_owned());
        self.dirs.extend(ancestors_and_self(&path));
        self
    }

    /// Add a directory, materialising every parent directory.
    pub fn with_dir(mut self, path: &str) -> Self {
        self.dirs.extend(ancestors_and_self(&normalise(path)));
        self
    }

    /// Every write recorded so far.
    #[must_use]
    pub fn writes(&self) -> &[String] {
        &self.writes
    }
}

/// One `/`-joined, root-relative path (no leading or trailing separator).
#[must_use]
pub fn normalise(path: &str) -> String {
    path.trim_matches('/').to_owned()
}

/// `path` and every ancestor directory above it, longest last.
fn ancestors_and_self(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for part in path.split('/') {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        out.push(current.clone());
    }
    out
}

impl FsRoot for FakeRoot {
    fn read_to_string(&self, path: &str) -> Result<String> {
        self.files
            .get(&normalise(path))
            .cloned()
            .ok_or_else(|| Error::io(path, std::io::ErrorKind::NotFound.into()))
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalise(path);
        self.dirs.contains(&path) || self.files.contains_key(&path)
    }

    fn read_dir_names(&self, path: &str) -> Result<Vec<String>> {
        let prefix = format!("{}/", normalise(path));
        let mut names: Vec<String> = self
            .dirs
            .iter()
            .filter_map(|candidate| candidate.strip_prefix(&prefix))
            .filter_map(|rest| rest.split('/').next())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

impl CgroupFs for FakeRoot {
    fn create_dir(&self, path: &str) -> Result<()> {
        if !self.allow_create {
            return Err(Error::io(path, std::io::ErrorKind::PermissionDenied.into()));
        }
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        if !self.allow_remove {
            return Err(Error::io(path, std::io::ErrorKind::PermissionDenied.into()));
        }
        Ok(())
    }
}

/// A root that records every mutating operation, so a test can prove that a
/// reading function never writes (`memory.peak` in particular).
pub struct RecordingRoot {
    inner: FakeRoot,
    writes: std::cell::RefCell<Vec<String>>,
}

impl RecordingRoot {
    /// Wrap a root.
    #[must_use]
    pub fn new(inner: FakeRoot) -> Self {
        Self {
            inner,
            writes: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Every path a mutating operation was asked to touch.
    #[must_use]
    pub fn writes(&self) -> Vec<String> {
        self.writes.borrow().clone()
    }
}

impl FsRoot for RecordingRoot {
    fn read_to_string(&self, path: &str) -> Result<String> {
        self.inner.read_to_string(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn read_dir_names(&self, path: &str) -> Result<Vec<String>> {
        self.inner.read_dir_names(path)
    }
}

impl CgroupFs for RecordingRoot {
    fn create_dir(&self, path: &str) -> Result<()> {
        self.writes.borrow_mut().push(format!("create {path}"));
        self.inner.create_dir(path)
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        self.writes.borrow_mut().push(format!("remove {path}"));
        self.inner.remove_dir(path)
    }
}
