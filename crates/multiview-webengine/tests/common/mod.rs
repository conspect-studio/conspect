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

use std::cell::Cell;
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
    /// `starttime` (field 22 of `stat`), in clock ticks. Defaults to a value
    /// derived from the pid; change it with [`with_starttime`](Self::with_starttime)
    /// to model the kernel reusing a pid.
    pub starttime: u64,
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
            starttime: 1_000 + u64::from(pid),
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

    /// Set `starttime` (field 22), as a kernel would after a pid is reused.
    #[must_use]
    pub fn with_starttime(mut self, starttime: u64) -> Self {
        self.starttime = starttime;
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
    /// Kernel-shaped: exactly the 50 fields a real kernel writes after the
    /// closing `)` of `comm`, i.e. fields 3..52. `utime`/`stime` are fields
    /// 14/15 and `starttime` is field 22, so a parser that counts from the wrong
    /// token fails loudly. `minflt`/`cminflt`/`majflt`/`cmajflt` (fields 10..13)
    /// are distinct and nonzero, so a one-token error reads a different number
    /// rather than a row of zeros.
    #[must_use]
    pub fn stat_body(&self) -> String {
        kernel_stat_line(
            self.pid,
            &self.comm,
            self.ppid,
            self.utime,
            self.stime,
            self.starttime,
        )
    }
}

/// A kernel-shaped `/proc/<pid>/stat` line.
///
/// Exactly the 50 fields a real kernel writes after the closing `)` of `comm`
/// (fields 3..52). `utime`/`stime` are fields 14/15 and `starttime` is field 22.
/// `minflt`/`cminflt`/`majflt`/`cmajflt` (fields 10..13) are distinct and
/// nonzero, derived from the pid, so an off-by-one parser reads a different
/// number rather than a row of zeros.
#[must_use]
pub fn kernel_stat_line(
    pid: u32,
    comm: &str,
    parent: u32,
    utime: u64,
    stime: u64,
    starttime: u64,
) -> String {
    let mut fields: Vec<String> = vec![
        "S".to_owned(),
        parent.to_string(),
        pid.to_string(),
        pid.to_string(),
        "0".to_owned(),                       // tty_nr
        "-1".to_owned(),                      // tpgid
        "4194560".to_owned(),                 // flags
        (1_000 + u64::from(pid)).to_string(), // minflt
        (2_000 + u64::from(pid)).to_string(), // cminflt
        (3_000 + u64::from(pid)).to_string(), // majflt
        (4_000 + u64::from(pid)).to_string(), // cmajflt
        utime.to_string(),
        stime.to_string(),
        "0".to_owned(),        // cutime
        "0".to_owned(),        // cstime
        "20".to_owned(),       // priority
        "0".to_owned(),        // nice
        "5".to_owned(),        // num_threads
        "0".to_owned(),        // itrealvalue
        starttime.to_string(), // field 22
    ];
    // Fields 23..37 (vsize .. cnswap).
    for _ in 0..15 {
        fields.push("0".to_owned());
    }
    fields.push("17".to_owned()); // field 38: exit_signal
                                  // Fields 39..52 (processor .. exit_code).
    for _ in 0..14 {
        fields.push("0".to_owned());
    }
    format!("{pid} ({comm}) {}\n", fields.join(" "))
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

    /// Rewrite a process's `stat` in place.
    ///
    /// Models what the kernel does between two samples: CPU ticks advance, and a
    /// reused pid gets a new `starttime`.
    pub fn update_stat(&self, spec: &ProcSpec) {
        let base = self.dir.path().join("proc").join(spec.pid.to_string());
        fs::write(base.join("stat"), spec.stat_body()).expect("rewrite a stat");
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
    /// How many `mkdir` calls fail with `AlreadyExists` before one succeeds
    /// (models a stale probe directory left by a crashed run).
    pub eexist_creates: usize,
    /// Every path `mkdir` was asked to create, in order.
    pub create_paths: std::cell::RefCell<Vec<String>>,
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
        let attempt = self.create_paths.borrow().len();
        self.create_paths.borrow_mut().push(path.to_owned());
        if attempt < self.eexist_creates {
            return Err(Error::io(path, std::io::ErrorKind::AlreadyExists.into()));
        }
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

/// A [`FsRoot`] over a real fixture tree that counts the reads a sweep performs.
///
/// The efficiency tests use it to prove a sweep is O(processes): the number of
/// `stat` reads and `children` reads is asserted exactly, so a per-node `/proc`
/// ppid scan (which would read every pid for every node) fails the test.
pub struct CountingRoot<'a> {
    root: &'a Path,
    stat_reads: Cell<usize>,
    children_reads: Cell<usize>,
    total_reads: Cell<usize>,
}

impl<'a> CountingRoot<'a> {
    /// Wrap a fixture root.
    #[must_use]
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            stat_reads: Cell::new(0),
            children_reads: Cell::new(0),
            total_reads: Cell::new(0),
        }
    }

    /// How many `/proc/<pid>/stat` files have been read.
    #[must_use]
    pub fn stat_reads(&self) -> usize {
        self.stat_reads.get()
    }

    /// How many `/proc/<pid>/task/<tid>/children` files have been read.
    #[must_use]
    pub fn children_reads(&self) -> usize {
        self.children_reads.get()
    }

    /// How many file reads of any kind have happened.
    #[must_use]
    pub fn total_reads(&self) -> usize {
        self.total_reads.get()
    }

    /// Forget the counts, for a second measured sweep.
    pub fn reset(&self) {
        self.stat_reads.set(0);
        self.children_reads.set(0);
        self.total_reads.set(0);
    }
}

impl FsRoot for CountingRoot<'_> {
    fn read_to_string(&self, path: &str) -> Result<String> {
        self.total_reads
            .set(self.total_reads.get().saturating_add(1));
        if path.ends_with("/stat") {
            self.stat_reads.set(self.stat_reads.get().saturating_add(1));
        }
        if path.ends_with("/children") {
            self.children_reads
                .set(self.children_reads.get().saturating_add(1));
        }
        self.root.read_to_string(path)
    }

    fn exists(&self, path: &str) -> bool {
        // `Path::exists` is an inherent method, so the trait method is named
        // explicitly to keep the root-relative path argument.
        FsRoot::exists(self.root, path)
    }

    fn read_dir_names(&self, path: &str) -> Result<Vec<String>> {
        self.root.read_dir_names(path)
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
