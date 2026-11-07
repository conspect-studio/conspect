//! Process-tree sampling from a `/proc` tree.
//!
//! This is the **estimated** accounting path of ADR-0099 decision 8: without a
//! delegated cgroup, Conspect discovers a browser's descendants from `/proc`,
//! sums their CPU time and memory, and keeps the CPU time of children that
//! exited between two samples so a reported total never goes backwards.
//!
//! Three measured facts shape the code:
//!
//! * **Whole tree or nothing.** An early sampler matched a single Chromium
//!   process by command line and reported 0% CPU / 100 MB, because ~95% of the
//!   CPU sits in the GPU process. Every descendant is therefore sampled, and the
//!   per-type split exists so a regression to "one process" is visible in a
//!   test.
//! * **`comm` is hostile.** `/proc/<pid>/stat` field 2 is parenthesised and may
//!   itself contain spaces and `)`. All fields are parsed after the **last**
//!   `)`.
//! * **argv is hostile too.** Chromium's `--type=` arrives NUL-separated, but
//!   `chrome-headless-shell` can rewrite argv into a single space-separated
//!   string. Both shapes are handled.
//!
//! Nothing here reads a fixed absolute path: the `/proc` root is passed in (see
//! [`FsRoot`](crate::fsroot::FsRoot)).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::error::{Error, Result};
use crate::fsroot::{FsRoot, Pid};
pub use crate::usage::Accounting;

/// Chromium process types as reported by the `--type=` switch.
///
/// The variant set is exactly the six types a Chromium-class browser launches
/// under `--headless=new` / `chrome-headless-shell`; anything else is
/// [`Unknown`](Self::Unknown) rather than a guess, and the raw string is kept
/// beside it by the caller so a new upstream type is visible in diagnostics
/// instead of silently miscounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChromiumProcessType {
    /// The browser process itself (also the case for a root argv with no `--type=`).
    Browser,
    /// A renderer (one per site instance).
    Renderer,
    /// The GPU process, where the measured bulk of the CPU sits.
    GpuProcess,
    /// A utility process (network, audio, storage, ...).
    Utility,
    /// The zygote that forks renderers.
    Zygote,
    /// A legacy Pepper plugin host.
    Ppapi,
    /// A descendant that is not a recognised Chromium type (or has no argv).
    Unknown,
}

/// Every variant, in a stable order, for exhaustive reporting and tests.
pub const CHROMIUM_PROCESS_TYPES: [ChromiumProcessType; 7] = [
    ChromiumProcessType::Browser,
    ChromiumProcessType::Renderer,
    ChromiumProcessType::GpuProcess,
    ChromiumProcessType::Utility,
    ChromiumProcessType::Zygote,
    ChromiumProcessType::Ppapi,
    ChromiumProcessType::Unknown,
];

/// The number of process types, and therefore the length of a per-type array.
pub const TYPE_COUNT: usize = CHROMIUM_PROCESS_TYPES.len();

impl ChromiumProcessType {
    /// This variant's index in [`CHROMIUM_PROCESS_TYPES`].
    ///
    /// The per-type accounting is a fixed array indexed by this, so a sweep does
    /// not allocate a map.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Browser => 0,
            Self::Renderer => 1,
            Self::GpuProcess => 2,
            Self::Utility => 3,
            Self::Zygote => 4,
            Self::Ppapi => 5,
            Self::Unknown => 6,
        }
    }

    /// The `--type=` value this variant corresponds to, or `None` for
    /// [`Browser`](Self::Browser) (which carries no `--type=`) and
    /// [`Unknown`](Self::Unknown).
    #[must_use]
    pub const fn chromium_label(self) -> Option<&'static str> {
        match self {
            // The browser process carries no `--type=` (it IS the process that
            // was launched), and an unrecognised value has no label to claim.
            Self::Browser | Self::Unknown => None,
            Self::Renderer => Some("renderer"),
            Self::GpuProcess => Some("gpu-process"),
            Self::Utility => Some("utility"),
            Self::Zygote => Some("zygote"),
            Self::Ppapi => Some("ppapi"),
        }
    }

    /// Whether this variant is one of the six recognised Chromium types
    /// (including [`Browser`](Self::Browser), which is recognised by absence).
    #[must_use]
    pub const fn is_recognised(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// What one descendant process contributed to a sample.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcRecord {
    /// The process id.
    pub pid: Pid,
    /// `comm` from `/proc/<pid>/stat` (may contain spaces and `)`).
    pub comm: String,
    /// `starttime` (field 22 of `stat`): the tick the process started.
    ///
    /// With the pid this identifies a process across sweeps, so the kernel
    /// reusing a pid for a *new* process is not mistaken for the same one.
    /// `0` when the field could not be parsed.
    pub starttime: u64,
    /// `utime + stime` in clock ticks.
    pub cpu_ticks: u64,
    /// Classified Chromium role.
    pub process_type: ChromiumProcessType,
    /// The raw `--type=` value, when argv carried one. Kept so an unrecognised
    /// upstream type is diagnosable.
    pub raw_type: Option<String>,
    /// The memory figure used for this process, in bytes.
    pub memory_bytes: u64,
    /// How that memory figure was obtained.
    pub memory_source: MemorySource,
}

/// Where a process's memory figure came from.
///
/// `#[non_exhaustive]`: a later slice may add a source (a cgroup's
/// `memory.current` for a delegated process), so consumers carry a wildcard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemorySource {
    /// `Pss` from `smaps_rollup` — the proportional-share figure, so the tree
    /// sum does not double-count shared pages.
    Pss,
    /// `Rss` from `smaps_rollup`, used when the kernel did not report `Pss`.
    Rss,
    /// No `smaps_rollup` was readable at all (non-Linux, or the process died):
    /// memory is unknown for this process, never a fabricated zero.
    Unavailable,
}

impl MemorySource {
    /// Whether this source is the preferred `Pss` reading.
    #[must_use]
    pub const fn is_pss(self) -> bool {
        matches!(self, Self::Pss)
    }
}

/// The identity a scope was **registered** with: the root pid and the
/// `starttime` the caller read right after spawning it.
///
/// A pid alone is not an identity: between registration and the first sample the
/// kernel can reap the browser and hand its pid to an unrelated process, and a
/// sampler that compares `starttime` only from the second sample on would charge
/// that impostor's whole tree to the dead scope on the first one. Carrying the
/// registered `starttime` into
/// [`sample_process_tree`] closes that window: the comparison happens on
/// **every** sample, the first included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedRoot {
    /// The root pid the scope was registered for.
    pub pid: Pid,
    /// The root's `starttime` (field 22 of `/proc/<pid>/stat`) at registration.
    pub starttime: u64,
}

impl ExpectedRoot {
    /// The identity of `pid`, reading its `starttime` now.
    ///
    /// This is what a caller uses immediately after spawning the browser: the
    /// window between the spawn and this read is the only one a pid could be
    /// recycled in, and the kernel cannot recycle a pid whose process is still
    /// running, so the read returns the spawned process's own identity.
    ///
    /// `None` when `/proc/<pid>/stat` cannot be read or its `starttime` does not
    /// parse — a caller that cannot establish an identity must not silently fall
    /// back to a bare pid, which is the bug this type exists to prevent. The
    /// `Result`-shaped [`try_read`](Self::try_read) is preferred when the caller
    /// wants the reason.
    #[must_use]
    pub fn read<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Option<Self> {
        let starttime = read_starttime(fs, pid)?;
        Some(Self { pid, starttime })
    }

    /// The identity of `pid`, reading its `starttime` now.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::Parse`] when the process's `stat` cannot be read
    /// or its `starttime` field cannot be parsed.
    pub fn try_read<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Result<Self> {
        let path = format!("{PROC}/{pid}/stat");
        let body = fs.read_to_string(&path)?;
        let starttime = parse_stat_starttime(&body)
            .ok_or_else(|| Error::parse(&path, "could not read starttime (field 22)"))?;
        Ok(Self { pid, starttime })
    }

    /// Whether `root_stat` is still the process this identity was registered
    /// for. A `0` registered `starttime` (a kernel that did not report the
    /// field) degrades the check to the pid alone, exactly as
    /// [`ProcRecord::starttime`] does for a descendant.
    #[must_use]
    pub const fn matches(&self, pid: Pid, root_stat: &RootStat) -> bool {
        self.pid.as_u32() == pid.as_u32()
            && (self.starttime == 0 || self.starttime == root_stat.starttime)
    }
}

/// The root process's own identity and reading, taken from one `stat` read.
///
/// Its `starttime` is what makes the root's identity checkable across sweeps: a
/// pid the kernel has reused carries a different one, so the new process is not
/// folded into the old scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootStat {
    /// `utime + stime` of the root itself, in clock ticks.
    pub cpu_ticks: u64,
    /// `stat` field 22 for the root; `0` when the field could not be parsed.
    pub starttime: u64,
}

/// One whole-tree reading.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessTreeSample {
    /// The root the tree was discovered from.
    pub root: Pid,
    /// The root's `starttime` at this sweep (field 22 of its `stat`; `0` when the
    /// field could not be parsed).
    ///
    /// The root's identity, recorded from the **first** sample on, so a later
    /// sweep can tell a live root from a pid the kernel reused for a new
    /// process. See [`TreeSample::RootGone`].
    pub root_starttime: u64,
    /// Every live descendant (the root is accounted separately, not repeated here).
    pub processes: Vec<ProcRecord>,
    /// Live `utime + stime` across the tree **and** the retained time of exited
    /// children, in clock ticks.
    ///
    /// Holds the maximum seen by the sampler across a run, so a kernel counter
    /// reset cannot make a reported total go backwards.
    pub cpu_ticks: u64,
    /// Live `utime + stime` of the tree discovered by this walk, in clock ticks.
    /// This one *does* go down when a child exits, and is what the per-type
    /// split is derived from.
    pub live_cpu_ticks: u64,
    /// CPU time of children that exited, accumulated by the caller across walks,
    /// in clock ticks.
    pub exited_cpu_ticks: u64,
    /// Memory summed across the tree **including the root**, in bytes.
    ///
    /// The root's own `smaps_rollup` is part of the sum: a browser that is alive
    /// and readable must not be reported as having no memory figure merely
    /// because it launched no descendants.
    pub memory_bytes: u64,
    /// Whether every contributing process supplied `Pss` (else the sum mixes
    /// sources and is labelled as such). The root is a contributor too, so a
    /// root whose memory could not be read weakens the whole sum.
    pub memory_source: MemorySource,
    /// Per-type CPU split of the **live** tree, a fixed array indexed by
    /// [`ChromiumProcessType::index`], so a sweep allocates no map. Every type
    /// is present (a type with no process is a real zero here, unlike an
    /// unmeasured GPU figure).
    pub by_type: TypeAccountingArray,
    /// Whether the reading is exact or estimated.
    pub accounting: Accounting,
}

impl ProcessTreeSample {
    /// The per-type entry for `kind`, which is always present for a single walk.
    #[must_use]
    pub fn type_usage(&self, kind: ChromiumProcessType) -> TypeAccounting {
        self.by_type.get(kind.index()).copied().unwrap_or_default()
    }

    /// The number of live descendant processes sampled (the root is not one of
    /// them).
    #[must_use]
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }
}

/// Per-type accounting for one walk, indexed by [`ChromiumProcessType::index`].
pub type TypeAccountingArray = [TypeAccounting; TYPE_COUNT];

/// Per-type CPU seconds for one walk, indexed by [`ChromiumProcessType::index`].
pub type TypeSecondsArray = [f64; TYPE_COUNT];

/// Per-type CPU and memory subtotal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeAccounting {
    /// How many live descendants of this type were sampled.
    pub processes: u32,
    /// Their summed `utime + stime`, in clock ticks.
    pub cpu_ticks: u64,
    /// Their summed memory, in bytes.
    pub memory_bytes: u64,
}

/// A CPU time in seconds, as reported by [`sample_process_tree`].
///
/// Clock ticks are converted with the kernel's `USER_HZ` (100 on every Linux
/// this project targets), so the value is comparable to `cpu.stat`'s
/// `usage_usec` without asking libc for `_SC_CLK_TCK` (which would add a native
/// dependency this crate must not have).
pub const USER_HZ: u64 = 100;

/// Clock ticks to seconds.
#[must_use]
pub fn ticks_to_seconds(ticks: u64) -> f64 {
    let ticks = u64_to_f64(ticks);
    let hz = u64_to_f64(USER_HZ);
    ticks / hz
}

/// One process's identity and CPU time, as remembered from a sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessTicks {
    /// `stat` field 22: distinguishes a reused pid from the same process.
    pub starttime: u64,
    /// `utime + stime` at that sweep, in clock ticks.
    pub cpu_ticks: u64,
}

/// What the next sweep needs from the previous one.
///
/// Deliberately **not** a whole [`ProcessTreeSample`]: a sweep keeps only the
/// per-pid identity map and the two running totals, so it never clones every
/// `ProcRecord` (and its strings) it just produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviousSweep {
    /// The root's identity at the previous sweep: the pid and the `starttime`
    /// its `stat` carried.
    ///
    /// `None` on a first sweep (nothing to compare against yet). Held here
    /// rather than only in the returned sample so the **walk itself** can refuse
    /// to fold a reused pid into a scope: if the kernel recycled the root pid,
    /// this is the only record of what the old root looked like.
    pub root: Option<(Pid, u64)>,
    /// Every live descendant at the previous sweep, keyed by pid.
    pub live: BTreeMap<Pid, ProcessTicks>,
    /// The cumulative total the previous sweep published.
    pub cpu_ticks: u64,
    /// The retained exited total the previous sweep carried.
    pub exited_cpu_ticks: u64,
}

impl PreviousSweep {
    /// `(len, size_bytes)` of the retained per-pid map.
    ///
    /// The cost test's measurement: over a fixed tree, a sweep that only
    /// **replaces** the previous state leaves both unchanged, while a sweep that
    /// accumulates into the map grows them. (`BTreeMap` has no `capacity()`;
    /// `size_of` is its allocation proxy, and the map's own node footprint
    /// dominates it.) `by_type` needs no such accessor — it is a fixed-length
    /// array, so its length is always [`TYPE_COUNT`] and it is inline, never
    /// heap-allocated.
    #[must_use]
    pub fn live_map_size(&self) -> (usize, usize) {
        (self.live.len(), std::mem::size_of::<Self>())
    }

    /// Remember `sample` for the next sweep.
    #[must_use]
    pub fn from_sample(sample: &ProcessTreeSample) -> Self {
        Self {
            root: Some((sample.root, sample.root_starttime)),
            live: sample
                .processes
                .iter()
                .map(|record| {
                    (
                        record.pid,
                        ProcessTicks {
                            starttime: record.starttime,
                            cpu_ticks: record.cpu_ticks,
                        },
                    )
                })
                .collect(),
            cpu_ticks: sample.cpu_ticks,
            exited_cpu_ticks: sample.exited_cpu_ticks,
        }
    }
}

/// CPU ticks of processes that were live in `previous` and are gone now.
///
/// Identity is `(pid, starttime)`: a pid that has disappeared, or whose
/// `starttime` changed because the kernel reused the pid for a **new** process,
/// counts as gone and its last recorded ticks are retained. A process that is
/// still live is never booked as exited — including one whose own counter went
/// down, which is held by the monotonic total rather than charged twice.
///
/// No allocation: the previous totals are summed and the still-live entries are
/// subtracted.
#[must_use]
pub fn exited_since(previous: &BTreeMap<Pid, ProcessTicks>, current: &[ProcRecord]) -> u64 {
    let total_before = previous
        .values()
        .fold(0_u64, |sum, entry| sum.saturating_add(entry.cpu_ticks));
    let still_live = current.iter().fold(0_u64, |sum, record| {
        let matched = previous
            .get(&record.pid)
            .filter(|before| before.starttime == record.starttime)
            .map_or(0, |before| before.cpu_ticks);
        sum.saturating_add(matched)
    });
    total_before.saturating_sub(still_live)
}

/// The outcome of sampling a process tree (ADR-0099 decision 8).
///
/// Sampling is **not** infallible-by-construction: the root can end between two
/// sweeps, or the kernel can reuse its pid for an unrelated process. Both mean
/// the scope is over, and neither may be folded into the old owner's figures —
/// the caller retires the scope and registers a new generation instead
/// ([`scope::ScopeRegistry`](crate::scope::ScopeRegistry)).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
// The large arm is the point: `Sampled` is the normal outcome of every sweep and
// a sweep already allocates its record `Vec`, so the enum is left un-boxed to
// avoid one extra indirection per sample on the accounting path. `RootGone`
// carries nothing because there is nothing honest to carry — no reading exists
// for a scope that ended.
#[allow(clippy::large_enum_variant)]
pub enum TreeSample {
    /// The root is the same process it was at the previous sweep (or this is the
    /// first sweep of the scope): the reading is the scope's own.
    Sampled(ProcessTreeSample),
    /// The root's `starttime` changed — the kernel reused the pid for a **new**
    /// process — so this sweep belongs to nobody.
    ///
    /// The caller retires the scope and registers a new generation. The new
    /// process's figures are deliberately not reported here: charging them to
    /// the old scope is exactly the misattribution the generation counter
    /// exists to prevent.
    RootGone,
}

impl TreeSample {
    /// The reading, when the root is still the process the scope was opened for.
    #[must_use]
    pub const fn sample(&self) -> Option<&ProcessTreeSample> {
        match self {
            Self::Sampled(sample) => Some(sample),
            Self::RootGone => None,
        }
    }

    /// The reading, consuming the result.
    #[must_use]
    pub fn into_sample(self) -> Option<ProcessTreeSample> {
        match self {
            Self::Sampled(sample) => Some(sample),
            Self::RootGone => None,
        }
    }

    /// Whether the scope this sample was taken for has ended.
    #[must_use]
    pub const fn root_is_gone(&self) -> bool {
        matches!(self, Self::RootGone)
    }

    /// The reading's process count, or `0` when the root is gone.
    ///
    /// Present so a diagnostics or test call site that does not care which arm
    /// it got still reads naturally; a call site that must not ignore the ended
    /// scope matches on the enum instead.
    #[must_use]
    pub fn process_count(&self) -> usize {
        self.sample().map_or(0, ProcessTreeSample::process_count)
    }

    /// The reading's per-type entry for `kind`, or the zero default when the
    /// root is gone.
    #[must_use]
    pub fn type_usage(&self, kind: ChromiumProcessType) -> TypeAccounting {
        self.sample()
            .map_or_else(TypeAccounting::default, |sample| sample.type_usage(kind))
    }

    /// The scopes' per-type CPU split, or zeroes when the root is gone.
    #[must_use]
    pub fn by_type(&self) -> TypeAccountingArray {
        self.sample()
            .map_or([TypeAccounting::default(); TYPE_COUNT], |sample| {
                sample.by_type
            })
    }

    /// The reading's CPU time in seconds, or `0.0` when the root is gone.
    ///
    /// None of these conveniences may be read as "the scope used nothing": a
    /// root that is gone has **no** figure, and a caller that needs to tell that
    /// apart matches on [`TreeSample`] rather than using these.
    #[must_use]
    pub fn cpu_seconds(&self) -> f64 {
        self.sample()
            .map_or(0.0, |sample| ticks_to_seconds(sample.cpu_ticks))
    }
}

/// Walk the process tree rooted at `root` and read `proc/<pid>/...` files
/// through `fs`.
///
/// `previous` is the previous sweep's state (or `None` on the first sample); the
/// CPU time of processes that have exited since is retained so a total never
/// goes backwards, and the published total is held at its high-water mark.
///
/// The root's identity is checked **first**, on **every** sweep. `expected` is
/// the identity the scope was registered with (see [`ExpectedRoot`]); when the
/// live root's `starttime` differs from it the result is
/// [`TreeSample::RootGone`] — the walk does not even run, let alone report the
/// impostor's CPU as the old scope's. That check is what makes the **first**
/// sample safe too: with only `previous` to compare against, a pid recycled
/// between registration and the first sweep had nothing to be caught by.
///
/// The `previous` sweep's root identity is checked as well, so a caller that has
/// no registration-time identity still gets the cross-sweep guard.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parse`] when the root process's own `/proc` entry
/// exists but cannot be read or parsed. A descendant that vanishes mid-walk is
/// skipped, not an error — a live tree changes under the reader.
pub fn sample_process_tree<F: FsRoot + ?Sized>(
    fs: &F,
    root: Pid,
    expected: Option<&ExpectedRoot>,
    previous: Option<&PreviousSweep>,
) -> Result<TreeSample> {
    let root_stat = read_root_stat(fs, root)?;
    if let Some(expected) = expected {
        if !expected.matches(root, &root_stat) {
            // The registered process is not this pid any more (or the pid now
            // names a different process): the scope ended before this sweep.
            return Ok(TreeSample::RootGone);
        }
    }
    if let Some((previous_root, previous_starttime)) = previous.and_then(|state| state.root) {
        if previous_root == root && previous_starttime != root_stat.starttime {
            // The pid is the same but the process is not: the scope ended.
            return Ok(TreeSample::RootGone);
        }
    }

    let (root_memory_bytes, root_memory_source) = read_memory(fs, root);

    let mut records: Vec<ProcRecord> = Vec::new();
    walk_tree(fs, root, &mut |record| records.push(record));

    let mut by_type: TypeAccountingArray = [TypeAccounting::default(); TYPE_COUNT];

    let mut live_ticks = 0_u64;
    let mut memory_bytes = root_memory_bytes;
    // The sum always has at least one contributor: the root itself. A live root
    // whose memory is readable must not be reported as having no figure merely
    // because it has no descendants yet.
    let mut every_pss = root_memory_source.is_pss();
    let mut every_unavailable = root_memory_source == MemorySource::Unavailable;
    for record in &records {
        live_ticks = live_ticks.saturating_add(record.cpu_ticks);
        memory_bytes = memory_bytes.saturating_add(record.memory_bytes);
        every_pss &= record.memory_source.is_pss();
        every_unavailable &= record.memory_source == MemorySource::Unavailable;
        if let Some(entry) = by_type.get_mut(record.process_type.index()) {
            entry.processes = entry.processes.saturating_add(1);
            entry.cpu_ticks = entry.cpu_ticks.saturating_add(record.cpu_ticks);
            entry.memory_bytes = entry.memory_bytes.saturating_add(record.memory_bytes);
        }
    }

    let memory_source = if every_unavailable {
        // No process in the tree — root included — supplied a figure. Unknown,
        // never `Pss` or `Rss`.
        MemorySource::Unavailable
    } else if every_pss {
        MemorySource::Pss
    } else {
        // A mixed sum (Pss with an `Rss` or `Unavailable` contributor) is
        // reported as the weaker measured source.
        MemorySource::Rss
    };

    let retained = previous.map_or(0, |previous| {
        previous
            .exited_cpu_ticks
            .saturating_add(exited_since(&previous.live, &records))
    });

    let total_live = root_stat.cpu_ticks.saturating_add(live_ticks);
    let retained_total = retained.saturating_add(total_live);
    let monotonic = previous.map_or(retained_total, |previous| {
        previous.cpu_ticks.max(retained_total)
    });

    Ok(TreeSample::Sampled(ProcessTreeSample {
        root,
        root_starttime: root_stat.starttime,
        processes: records,
        cpu_ticks: monotonic,
        live_cpu_ticks: total_live,
        exited_cpu_ticks: retained,
        memory_bytes,
        memory_source,
        by_type,
        accounting: Accounting::Estimated,
    }))
}

/// A `/proc`-wide parent map, built at most **once per sweep**.
///
/// `/proc/<pid>/task/<tid>/children` is preferred per node (the kernel
/// maintains it, and it is race-free per thread). This index is the fallback for
/// a node whose strand is missing or unreadable: the documented alternative in
/// ADR-0099 decision 8. Building it costs one `stat` read per pid, so the walk
/// builds it lazily and reuses it, keeping a sweep O(processes) rather than
/// O(processes x all pids).
#[derive(Clone, Debug, Default)]
pub struct PpidIndex {
    children: BTreeMap<Pid, Vec<Pid>>,
}

impl PpidIndex {
    /// Scan every `/proc/<n>/stat` and index its `ppid`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::Parse`] for a `/proc` entry that exists but
    /// cannot be read or parsed for a reason other than having vanished.
    pub fn build<F: FsRoot + ?Sized>(fs: &F) -> Result<Self> {
        let mut children: BTreeMap<Pid, Vec<Pid>> = BTreeMap::new();
        for entry in fs.read_dir_names(PROC)? {
            let Ok(pid) = entry.parse::<Pid>() else {
                continue;
            };
            if let Some(parent) = ppid_of(fs, pid)? {
                children.entry(parent).or_default().push(pid);
            }
        }
        for list in children.values_mut() {
            list.sort();
            list.dedup();
        }
        Ok(Self { children })
    }

    /// The children of `parent` in this index, ascending.
    #[must_use]
    pub fn children_of(&self, parent: Pid) -> Vec<Pid> {
        self.children.get(&parent).cloned().unwrap_or_default()
    }
}

/// Discover `pid`'s children.
///
/// Each thread's `task/<tid>/children` strand is used when the kernel provides
/// it (the usual case). A strand that is **missing or unreadable** falls back to
/// the `/proc` ppid scan *for that node*; a strand that exists but is empty is
/// authoritative "no children", so no scan happens.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parse`] when `pid`'s task directory cannot be
/// listed, or when the fallback scan finds a `/proc` entry that cannot be read
/// or parsed.
pub fn children_of<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Result<Vec<Pid>> {
    let mut index: Option<PpidIndex> = None;
    children_of_indexed(fs, pid, &mut index)
}

/// [`children_of`] against an index that is built at most once and reused.
fn children_of_indexed<F: FsRoot + ?Sized>(
    fs: &F,
    pid: Pid,
    index: &mut Option<PpidIndex>,
) -> Result<Vec<Pid>> {
    let task_dir = format!("{PROC}/{pid}/task");
    let tids = fs.read_dir_names(&task_dir)?;
    let mut children: BTreeSet<Pid> = BTreeSet::new();
    // No thread directory at all is itself "no authoritative strand", so the
    // scan is the only way to discover this node's children.
    let mut fall_back = tids.is_empty();
    for tid_name in tids {
        if tid_name.parse::<u32>().is_err() {
            continue;
        }
        let path = format!("{task_dir}/{tid_name}/children");
        match fs.read_to_string(&path) {
            Ok(body) => {
                for token in body.split_whitespace() {
                    if let Ok(child) = token.parse::<Pid>() {
                        children.insert(child);
                    }
                }
            }
            Err(_) => fall_back = true,
        }
    }
    if fall_back {
        if index.is_none() {
            *index = Some(PpidIndex::build(fs)?);
        }
        if let Some(index) = index.as_ref() {
            for child in index.children_of(pid) {
                children.insert(child);
            }
        }
    }
    Ok(children.into_iter().collect())
}

/// The `/proc` scan fallback: every pid whose `stat` ppid equals `parent`.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parse`] on an unreadable `/proc` entry.
pub fn scan_ppid_children<F: FsRoot + ?Sized>(fs: &F, parent: Pid) -> Result<Vec<Pid>> {
    Ok(PpidIndex::build(fs)?.children_of(parent))
}

/// Breadth-first walk of the descendants of `root`, calling `visit` on each.
///
/// The walk is cycle-safe (a malformed fixture cannot loop forever) and
/// fault-tolerant: a descendant whose `/proc` entry disappears between
/// discovery and read is skipped. The ppid fallback index is created lazily and
/// shared across the whole walk, so a sweep scans `/proc` at most once.
pub fn walk_tree<F: FsRoot + ?Sized, V: FnMut(ProcRecord)>(fs: &F, root: Pid, visit: &mut V) {
    let mut seen: BTreeSet<Pid> = BTreeSet::new();
    seen.insert(root);
    let mut queue: VecDeque<Pid> = VecDeque::new();
    queue.push_back(root);
    let mut index: Option<PpidIndex> = None;
    while let Some(pid) = queue.pop_front() {
        let Ok(children) = children_of_indexed(fs, pid, &mut index) else {
            continue;
        };
        for child in children {
            if !seen.insert(child) {
                continue;
            }
            queue.push_back(child);
            if let Some(record) = read_process(fs, child) {
                visit(record);
            }
        }
    }
}

/// Read and classify one process. `None` when its `/proc` entry is already gone
/// or its `stat` cannot be parsed.
#[must_use]
pub fn read_process<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Option<ProcRecord> {
    let stat = fs.read_to_string(&format!("{PROC}/{pid}/stat")).ok()?;
    let cpu_ticks = parse_stat_cpu_ticks(&stat)?;
    let comm = parse_stat_comm(&stat).unwrap_or_default();
    // Unknown `starttime` degrades identity to the pid alone; a real kernel
    // line always carries it.
    let starttime = parse_stat_starttime(&stat).unwrap_or(0);
    let cmdline = fs
        .read_to_string(&format!("{PROC}/{pid}/cmdline"))
        .unwrap_or_default();
    let raw_type = chromium_type_arg(&cmdline);
    let (memory_bytes, memory_source) = read_memory(fs, pid);
    Some(ProcRecord {
        pid,
        comm,
        starttime,
        cpu_ticks,
        process_type: chromium_process_type(raw_type.as_deref()),
        raw_type,
        memory_bytes,
        memory_source,
    })
}

/// The root's own identity and CPU time, from one `stat` read.
///
/// Errors when the root's `/proc` entry cannot be read or its CPU fields cannot
/// be parsed: a live root must exist for the scope to mean anything. A missing
/// `starttime` is **not** an error — it degrades the identity check to the pid
/// alone, exactly as [`read_process`] does for a descendant.
fn read_root_stat<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Result<RootStat> {
    let path = format!("{PROC}/{pid}/stat");
    let stat = fs.read_to_string(&path)?;
    let cpu_ticks = parse_stat_cpu_ticks(&stat)
        .ok_or_else(|| Error::parse(&path, "could not read utime/stime after the comm paren"))?;
    Ok(RootStat {
        cpu_ticks,
        starttime: parse_stat_starttime(&stat).unwrap_or(0),
    })
}

fn ppid_of<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Result<Option<Pid>> {
    let path = format!("{PROC}/{pid}/stat");
    match fs.read_to_string(&path) {
        Ok(body) => Ok(parse_stat_ppid(&body)),
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(other) => Err(other),
    }
}

/// The `/proc` mount point, as a root-relative path for [`FsRoot`].
pub const PROC: &str = "proc";

/// Parse `comm` (field 2) from a `/proc/<pid>/stat` body.
///
/// The field is bounded by the first `(` after the pid and the **last** `)` in
/// the line, so a comm containing `)` or `( ` cannot truncate it.
#[must_use]
pub fn parse_stat_comm(body: &str) -> Option<String> {
    let open = body.find('(')?;
    let close = body.rfind(')')?;
    if close <= open {
        return None;
    }
    Some(body.get(open.saturating_add(1)..close)?.to_owned())
}

/// Parse `utime + stime` (fields 14 + 15) from a `/proc/<pid>/stat` body.
///
/// All fields are counted **after the last `)`**, so field 3 (`state`) is token
/// 0 and `utime`/`stime` are tokens 11 and 12. `None` when the body has no `)`
/// or too few fields.
#[must_use]
pub fn parse_stat_cpu_ticks(body: &str) -> Option<u64> {
    let (_, tail) = body.rsplit_once(')')?;
    let mut fields = tail.split_whitespace();
    let utime = fields.nth(11)?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

/// Parse `ppid` (field 4) from a `/proc/<pid>/stat` body, counted after the last
/// `)` exactly like [`parse_stat_cpu_ticks`].
#[must_use]
pub fn parse_stat_ppid(body: &str) -> Option<Pid> {
    let (_, tail) = body.rsplit_once(')')?;
    tail.split_whitespace().nth(1)?.parse::<Pid>().ok()
}

/// Parse `starttime` (field 22) from a `/proc/<pid>/stat` body.
///
/// Counted after the last `)` exactly like [`parse_stat_cpu_ticks`]: field 3
/// (`state`) is token 0, so `starttime` is token 19. `None` when the body is too
/// short or the field does not parse — a real kernel line always carries all 52
/// fields.
#[must_use]
pub fn parse_stat_starttime(body: &str) -> Option<u64> {
    let (_, tail) = body.rsplit_once(')')?;
    tail.split_whitespace().nth(19)?.parse::<u64>().ok()
}

/// Read `starttime` (field 22 of `/proc/<pid>/stat`) for `pid` through `fs`.
///
/// This is the helper a caller uses to build the [`ExpectedRoot`] identity a
/// process-tree scope is registered with, immediately after spawning the
/// browser. `None` when the `stat` cannot be read or its `starttime` does not
/// parse — never `0`, which would silently degrade a real identity check to a
/// pid-only one.
#[must_use]
pub fn read_starttime<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Option<u64> {
    let body = fs.read_to_string(&format!("{PROC}/{pid}/stat")).ok()?;
    parse_stat_starttime(&body)
}

/// The `--type=` value from a process `cmdline`, if any.
///
/// Chromium passes argv NUL-separated, but `chrome-headless-shell` has been
/// observed rewriting argv into one space-separated string, so both separators
/// are treated as argument boundaries. A token is only read when it starts with
/// `--type=` at a boundary, so a page that *mentions* the switch inside an
/// argument cannot forge a process type.
///
/// Empty values (`--type=`) yield `None`.
#[must_use]
pub fn chromium_type_arg(cmdline: &str) -> Option<String> {
    for token in cmdline.split(['\0', ' ']) {
        if let Some(value) = token.strip_prefix("--type=") {
            let value = value.trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Classify a raw `--type=` value.
#[must_use]
pub fn chromium_process_type(raw: Option<&str>) -> ChromiumProcessType {
    match raw {
        None => ChromiumProcessType::Browser,
        Some("renderer") => ChromiumProcessType::Renderer,
        Some("gpu-process") => ChromiumProcessType::GpuProcess,
        Some("utility") => ChromiumProcessType::Utility,
        Some("zygote") => ChromiumProcessType::Zygote,
        Some("ppapi") => ChromiumProcessType::Ppapi,
        Some(_) => ChromiumProcessType::Unknown,
    }
}

/// The `(memory, source)` pair for `pid` from `smaps_rollup`, preferring `Pss`.
///
/// `Pss` is the proportional-share figure, so summing a tree does not
/// double-count shared pages; `Rss` is the fallback and is reported as the
/// fallback (ADR-0099 decision 8's "PSS, falling back to RSS, and report that").
/// [`MemorySource::Unavailable`] when `smaps_rollup` cannot be read — an unknown
/// memory figure is never reported as zero bytes.
#[must_use]
pub fn read_memory<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> (u64, MemorySource) {
    let Ok(body) = fs.read_to_string(&format!("{PROC}/{pid}/smaps_rollup")) else {
        return (0, MemorySource::Unavailable);
    };
    if let Some(pss) = smaps_field_bytes(&body, "Pss:") {
        return (pss, MemorySource::Pss);
    }
    if let Some(rss) = smaps_field_bytes(&body, "Rss:") {
        return (rss, MemorySource::Rss);
    }
    (0, MemorySource::Unavailable)
}

/// One `smaps_rollup` field, whose value is in kibibytes, as bytes.
#[must_use]
pub fn smaps_field_bytes(body: &str, label: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
}

/// Read a whitespace-delimited `u64` field from a small status-style file.
#[must_use]
pub fn read_u64<F: FsRoot + ?Sized>(fs: &F, path: &str, label: &str) -> Option<u64> {
    let body = fs.read_to_string(path).ok()?;
    body.lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Read a bare `u64` from a whole file (a cgroup `memory.current`, say).
#[must_use]
pub fn read_bare_u64<F: FsRoot + ?Sized>(fs: &F, path: &str) -> Option<u64> {
    fs.read_to_string(path).ok()?.trim().parse::<u64>().ok()
}

/// Lossless `u64 -> f64` widening (values below `2^53` are exact), avoiding the
/// `as` conversion the workspace lints forbid.
#[must_use]
pub(crate) fn u64_to_f64(value: u64) -> f64 {
    u32::try_from(value).map_or_else(
        |_| {
            let high = u32::try_from((value >> 32) & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            let low = u32::try_from(value & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            high * 4_294_967_296.0 + low
        },
        f64::from,
    )
}

/// Narrow a finite, non-negative `f64` to `f32` on the `2^24` integer grid over
/// `0.0..=cap`, saturating at `cap`.
///
/// Goes through an integer grid rather than an `as` conversion, which the
/// workspace lints forbid.
#[must_use]
pub(crate) fn ratio_to_f32(value: f64, cap: f32) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let cap64 = f64::from(cap);
    if cap64 <= 0.0 {
        return 0.0;
    }
    let scaled = (value.clamp(0.0, cap64) / cap64 * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return cap;
    }
    let whole = scaled.trunc();
    if whole <= 0.0 {
        return 0.0;
    }
    let count = u32::try_from(f64_trunc_to_u32_grid(whole)).unwrap_or(0);
    f32_from_u32(count) * cap / 16_777_216.0_f32
}

/// Truncate a finite, non-negative `f64` below `2^24` to a `u64` by reading its
/// IEEE-754 fields (no `as` cast).
#[must_use]
fn f64_trunc_to_u32_grid(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
    if exponent >= 52 {
        significand << (exponent - 52)
    } else {
        significand >> (52 - exponent)
    }
}

/// Exact `u32 -> f32` for values `<= 2^24`, composing two lossless halves.
#[must_use]
fn f32_from_u32(value: u32) -> f32 {
    let high = u16::try_from((value >> 16) & 0xFFFF).map_or(f32::INFINITY, f32::from);
    let low = u16::try_from(value & 0xFFFF).map_or(f32::INFINITY, f32::from);
    high * 65_536.0_f32 + low
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kernel-shaped `stat` line: exactly the 50 fields a real kernel writes
    /// after the comm's `)`, i.e. fields 3..52.
    ///
    /// `minflt`/`cminflt`/`majflt`/`cmajflt` (fields 10..13) are distinct and
    /// nonzero, so a parser that counts from the wrong token reads a different
    /// number rather than a row of zeros.
    fn kernel_stat(
        pid: u32,
        comm: &str,
        parent: u32,
        utime: u64,
        stime: u64,
        starttime: u64,
    ) -> String {
        format!(
            "{pid} ({comm}) S {parent} {pid} {pid} 0 -1 4194560 {minflt} {cminflt} {majflt} {cmajflt} \
             {utime} {stime} 0 0 20 0 5 0 {starttime} \
             0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
            minflt = 1_000 + pid,
            cminflt = 2_000 + pid,
            majflt = 3_000 + pid,
            cmajflt = 4_000 + pid,
        )
    }

    #[test]
    fn comm_with_parens_and_spaces_parses_from_the_last_paren() {
        let body = kernel_stat(1234, "weird )(name", 1, 100, 50, 999);
        assert_eq!(parse_stat_comm(&body).as_deref(), Some("weird )(name"));
        assert_eq!(parse_stat_cpu_ticks(&body), Some(150));
        assert_eq!(parse_stat_ppid(&body), Some(Pid::new(1)));
        assert_eq!(parse_stat_starttime(&body), Some(999));
    }

    #[test]
    fn a_kernel_shaped_stat_line_has_the_activity_fields_an_off_by_one_parser_would_miss() {
        let body = kernel_stat(42, "chrome", 1, 7, 3, 555);
        let fields: Vec<&str> = body
            .rsplit_once(')')
            .expect("a comm paren")
            .1
            .split_whitespace()
            .collect();
        assert_eq!(fields.len(), 50, "fields 3..52, as the kernel writes them");
        // minflt, cminflt, majflt, cmajflt at tokens 7..10.
        let minor: Vec<u64> = fields
            .get(7..11)
            .expect("four activity fields")
            .iter()
            .map(|field| field.parse::<u64>().expect("numeric"))
            .collect();
        assert!(minor.iter().all(|value| *value != 0), "{minor:?}");
        let distinct: BTreeSet<u64> = minor.iter().copied().collect();
        assert_eq!(distinct.len(), 4, "distinct, so an offset is visible");
        // The correct reading is utime + stime; a parser one token early would
        // read cmajflt + utime, which must not coincide.
        assert_eq!(parse_stat_cpu_ticks(&body), Some(10));
        assert_ne!(
            minor.get(3).copied().unwrap_or_default() + 7,
            10,
            "the off-by-one reading differs from the correct one"
        );
    }

    #[test]
    fn malformed_stat_is_none_not_a_panic() {
        assert_eq!(parse_stat_comm("no parens"), None);
        assert_eq!(parse_stat_cpu_ticks("no parens"), None);
        assert_eq!(parse_stat_cpu_ticks("1 (c) S 1 2 3"), None);
        assert_eq!(parse_stat_starttime("1 (c) S 1 2 3"), None);
    }

    #[test]
    fn every_type_has_a_distinct_index_matching_the_constant_order() {
        for (expected, kind) in CHROMIUM_PROCESS_TYPES.iter().enumerate() {
            assert_eq!(kind.index(), expected, "{kind:?}");
        }
        assert_eq!(TYPE_COUNT, CHROMIUM_PROCESS_TYPES.len());
    }

    #[test]
    fn exited_since_counts_disappeared_processes_and_reused_pids() {
        let mut previous = BTreeMap::new();
        previous.insert(
            Pid::new(10),
            ProcessTicks {
                starttime: 5,
                cpu_ticks: 30,
            },
        );
        previous.insert(
            Pid::new(11),
            ProcessTicks {
                starttime: 6,
                cpu_ticks: 40,
            },
        );
        previous.insert(
            Pid::new(12),
            ProcessTicks {
                starttime: 7,
                cpu_ticks: 50,
            },
        );

        let record = |pid: u32, starttime: u64, cpu_ticks: u64| ProcRecord {
            pid: Pid::new(pid),
            comm: "chrome".to_owned(),
            starttime,
            cpu_ticks,
            process_type: ChromiumProcessType::Browser,
            raw_type: None,
            memory_bytes: 0,
            memory_source: MemorySource::Unavailable,
        };
        // 10 is still the SAME process; 11 has a new starttime (a reused pid);
        // 12 has disappeared.
        let current = vec![record(10, 5, 35), record(11, 99, 2)];
        assert_eq!(
            exited_since(&previous, &current),
            40 + 50,
            "the reused pid's old time and the departed process are both retained"
        );
        // A process still live but with a LOWER counter is not booked as exited.
        let reset = vec![record(10, 5, 1), record(11, 6, 40), record(12, 7, 50)];
        assert_eq!(exited_since(&previous, &reset), 0);
    }

    #[test]
    fn type_arg_handles_nul_and_space_rewritten_argv() {
        let nul = "/usr/bin/chrome\0--headless=new\0--type=renderer\0--no-sandbox";
        assert_eq!(chromium_type_arg(nul).as_deref(), Some("renderer"));
        let spaced = "/usr/bin/chrome --headless=new --type=gpu-process --no-sandbox";
        assert_eq!(chromium_type_arg(spaced).as_deref(), Some("gpu-process"));
        assert_eq!(chromium_type_arg("/usr/bin/chrome --headless"), None);
        assert_eq!(chromium_type_arg("--type="), None);
        // A mention inside another argument is not a type.
        assert_eq!(chromium_type_arg("--user-data-dir=/x--type=renderer"), None);
    }

    #[test]
    fn every_known_type_classifies_and_unknowns_stay_unknown() {
        for kind in CHROMIUM_PROCESS_TYPES {
            // Browser is recognised by the ABSENCE of `--type=`, and Unknown has
            // no label at all, so only the five labelled types round-trip.
            if let Some(label) = kind.chromium_label() {
                assert_eq!(
                    chromium_process_type(Some(label)),
                    kind,
                    "{kind:?} must round-trip through its --type= label"
                );
            }
        }
        assert!(ChromiumProcessType::Browser.is_recognised());
        assert!(!ChromiumProcessType::Unknown.is_recognised());
        assert_eq!(ChromiumProcessType::Unknown.chromium_label(), None);
        assert_eq!(
            chromium_process_type(Some("network.mojom.NetworkService")),
            ChromiumProcessType::Unknown
        );
        assert_eq!(chromium_process_type(None), ChromiumProcessType::Browser);
    }

    #[test]
    fn ticks_to_seconds_uses_user_hz() {
        assert!((ticks_to_seconds(250) - 2.5).abs() < 1e-9);
    }
}
