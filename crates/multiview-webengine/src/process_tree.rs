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

impl ChromiumProcessType {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

/// One whole-tree reading.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessTreeSample {
    /// The root the tree was discovered from.
    pub root: Pid,
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
    /// Memory summed across the tree, in bytes.
    pub memory_bytes: u64,
    /// Whether every contributing process supplied `Pss` (else the sum mixes
    /// sources and is labelled as such).
    pub memory_source: MemorySource,
    /// Per-type CPU split of the **live** tree, every known type present (a type
    /// with no process is a real zero here, unlike an unmeasured GPU figure).
    pub by_type: BTreeMap<ChromiumProcessType, TypeAccounting>,
    /// Whether the reading is exact or estimated.
    pub accounting: Accounting,
}

impl ProcessTreeSample {
    /// The per-type entry for `kind`, which is always present for a single walk.
    #[must_use]
    pub fn type_usage(&self, kind: ChromiumProcessType) -> TypeAccounting {
        self.by_type.get(&kind).copied().unwrap_or_default()
    }

    /// The number of live descendant processes sampled (the root is not one of
    /// them).
    #[must_use]
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }
}

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

/// Walk the process tree rooted at `root` and read `proc/<pid>/...` files
/// through `fs`.
///
/// `previous` is the previous walk's sample (or `None` on the first sample); its
/// retained exited-child time is carried forward so a total never goes
/// backwards.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parse`] when the root process's own `/proc` entry
/// exists but cannot be read or parsed. A descendant that vanishes mid-walk is
/// skipped, not an error — a live tree changes under the reader.
pub fn sample_process_tree<F: FsRoot + ?Sized>(
    fs: &F,
    root: Pid,
    previous: Option<&ProcessTreeSample>,
) -> Result<ProcessTreeSample> {
    let retained = previous.map_or(0, |previous| previous.exited_cpu_ticks);

    let mut records: Vec<ProcRecord> = Vec::new();
    let root_ticks = read_root_cpu_ticks(fs, root)?;

    walk_tree(fs, root, &mut |record| records.push(record));

    let mut by_type: BTreeMap<ChromiumProcessType, TypeAccounting> = BTreeMap::new();
    for kind in CHROMIUM_PROCESS_TYPES {
        by_type.insert(kind, TypeAccounting::default());
    }

    let mut live_ticks = 0_u64;
    let mut memory_bytes = 0_u64;
    let mut every_pss = !records.is_empty();
    for record in &records {
        live_ticks = live_ticks.saturating_add(record.cpu_ticks);
        memory_bytes = memory_bytes.saturating_add(record.memory_bytes);
        every_pss &= record.memory_source.is_pss();
        if let Some(entry) = by_type.get_mut(&record.process_type) {
            entry.processes = entry.processes.saturating_add(1);
            entry.cpu_ticks = entry.cpu_ticks.saturating_add(record.cpu_ticks);
            entry.memory_bytes = entry.memory_bytes.saturating_add(record.memory_bytes);
        }
    }

    let total_live = root_ticks.saturating_add(live_ticks);
    let retained_total = retained.saturating_add(total_live);
    let monotonic = previous.map_or(retained_total, |previous| {
        previous.cpu_ticks.max(retained_total)
    });

    Ok(ProcessTreeSample {
        root,
        processes: records,
        cpu_ticks: monotonic,
        live_cpu_ticks: total_live,
        exited_cpu_ticks: retained,
        memory_bytes,
        memory_source: if every_pss {
            MemorySource::Pss
        } else {
            MemorySource::Rss
        },
        by_type,
        accounting: Accounting::Estimated,
    })
}

/// Add `delta` clock ticks to a retained exited-child total, saturating.
///
/// The sampler calls this when the live total of a tree drops between two walks
/// because a child exited: the difference is the departed child's CPU time,
/// which stays charged to the scope so the cumulative figure is monotonic.
#[must_use]
pub fn retain_exited(previous: &ProcessTreeSample, current: &ProcessTreeSample) -> u64 {
    let live_before = previous.live_cpu_ticks;
    let live_now = current.live_cpu_ticks;
    current
        .exited_cpu_ticks
        .saturating_add(live_before.saturating_sub(live_now))
}

/// Discover `pid`'s children.
///
/// `/proc/<pid>/task/<tid>/children` is used when the kernel provides it (the
/// usual case, and the only one that is race-free per thread). When it is absent
/// — older kernels, or a fixture without it — the fallback scans every
/// `/proc/<n>/stat` for a matching parent pid, which is the documented
/// alternative in ADR-0099 decision 8.
///
/// # Errors
///
/// [`Error::Io`] or [`Error::Parse`] when the fallback scan finds a `/proc`
/// entry that cannot be read or parsed.
pub fn children_of<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Result<Vec<Pid>> {
    let mut children: BTreeSet<Pid> = BTreeSet::new();
    let task_dir = format!("{PROC}/{pid}/task");
    for tid_name in fs.read_dir_names(&task_dir)? {
        if tid_name.parse::<u32>().is_err() {
            continue;
        }
        let path = format!("{task_dir}/{tid_name}/children");
        if let Ok(body) = fs.read_to_string(&path) {
            for token in body.split_whitespace() {
                if let Ok(child) = token.parse::<Pid>() {
                    children.insert(child);
                }
            }
        }
    }
    if children.is_empty() {
        for child in scan_ppid_children(fs, pid)? {
            children.insert(child);
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
    let mut children = Vec::new();
    for descendant in fs.read_dir_names(PROC)? {
        if let Ok(candidate) = descendant.parse::<Pid>() {
            if ppid_of(fs, candidate)? == Some(parent) {
                children.push(candidate);
            }
        }
    }
    children.sort();
    Ok(children)
}

/// Breadth-first walk of the descendants of `root`, calling `visit` on each.
///
/// The walk is cycle-safe (a malformed fixture cannot loop forever) and
/// fault-tolerant: a descendant whose `/proc` entry disappears between
/// discovery and read is skipped.
pub fn walk_tree<F: FsRoot + ?Sized, V: FnMut(ProcRecord)>(fs: &F, root: Pid, visit: &mut V) {
    let mut seen: BTreeSet<Pid> = BTreeSet::new();
    seen.insert(root);
    let mut queue: VecDeque<Pid> = VecDeque::new();
    queue.push_back(root);
    while let Some(pid) = queue.pop_front() {
        let Ok(children) = children_of(fs, pid) else {
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
    let cmdline = fs
        .read_to_string(&format!("{PROC}/{pid}/cmdline"))
        .unwrap_or_default();
    let raw_type = chromium_type_arg(&cmdline);
    let (memory_bytes, memory_source) = read_memory(fs, pid);
    Some(ProcRecord {
        pid,
        comm,
        cpu_ticks,
        process_type: chromium_process_type(raw_type.as_deref()),
        raw_type,
        memory_bytes,
        memory_source,
    })
}

/// The root's own `utime + stime`, erroring when its `/proc` entry cannot be
/// read or parsed (a live root must exist for the scope to mean anything).
fn read_root_cpu_ticks<F: FsRoot + ?Sized>(fs: &F, pid: Pid) -> Result<u64> {
    let path = format!("{PROC}/{pid}/stat");
    let stat = fs.read_to_string(&path)?;
    parse_stat_cpu_ticks(&stat)
        .ok_or_else(|| Error::parse(path, "could not read utime/stime after the comm paren"))
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

/// Narrow a finite, unit-interval `f64` fraction to `f32` through the `2^24`
/// integer grid, avoiding the `as` conversion the workspace lints forbid.
#[must_use]
pub(crate) fn frac_to_f32(value: f64) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let scaled = (value.clamp(0.0, 1.0) * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return 1.0;
    }
    let whole = scaled.trunc();
    if whole <= 0.0 {
        return 0.0;
    }
    let count = u32::try_from(f64_trunc_to_u32_grid(whole)).unwrap_or(0);
    f32_from_u32(count) / 16_777_216.0_f32
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

    #[test]
    fn comm_with_parens_and_spaces_parses_from_the_last_paren() {
        let body = "1234 (weird )(name) S 1 1234 1234 0 -1 0 0 0 0 0 100 50 0 0 20 0 8 0 999 0 0";
        assert_eq!(parse_stat_comm(body).as_deref(), Some("weird )(name"));
        assert_eq!(parse_stat_cpu_ticks(body), Some(150));
        assert_eq!(parse_stat_ppid(body), Some(Pid::new(1)));
    }

    #[test]
    fn malformed_stat_is_none_not_a_panic() {
        assert_eq!(parse_stat_comm("no parens"), None);
        assert_eq!(parse_stat_cpu_ticks("no parens"), None);
        assert_eq!(parse_stat_cpu_ticks("1 (c) S 1 2 3"), None);
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
