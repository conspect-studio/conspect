//! Real-time resource accounting of one producer's whole process tree.
//!
//! A [`Tracker`] is bound to the producer root (`pid` + `starttime`, so a reused
//! pid is never mistaken for it). Each [`Tracker::sample`] scans `/proc` once
//! and attributes every process that is the root, a `ppid` descendant of it, a
//! member of the root's process group, or a previously attributed process that
//! was reparented (identity = pid + starttime). CPU of members that already
//! exited is retained so the rate never drops when a short-lived child exits.
//!
//! A capped scan sets `truncated`. Known members are always read directly, so
//! they are retired only when really gone; the unknown part of the tree is
//! rediscovered through `/proc/<pid>/task/<pid>/children` (bounded by the
//! tracked cap). An orphan reparented away *and* unseen by a capped scan is
//! inherently undiscoverable, so a truncated sample is only an estimate: its IO
//! totals/rates and `cpu_cores` are `None` (`io_complete = false`) and the rate
//! baseline is reset, so a later uncapped sample never reports a lifetime burst.
//!
//! Unknown is never reported as zero: PSS falls back to RSS with an explicit
//! [`MemoryBasis`], unreadable counters are `None`, and GPU is always `None`
//! (no NVML dependency). Memory per sample is bounded by [`MAX_TRACKED`].
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Upper bound on processes attributed per sample.
pub const MAX_TRACKED: usize = 4096;
/// Sampling period of the producer pump.
pub const PERIOD: Duration = Duration::from_secs(1);

/// Where the memory figure comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBasis {
    /// Every process reported PSS.
    Pss,
    /// Some processes lacked PSS and contributed RSS instead.
    PssWithRssFallback,
    /// No process reported PSS; the figure is RSS (an over-count of shared pages).
    Rss,
    /// Nothing readable.
    Unknown,
}

/// GPU usage; never measured in this build, so always unknown.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct GpuUsage {
    /// Utilisation percent.
    pub percent: f64,
}

/// One point-in-time attribution of the producer tree.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ResourceSample {
    /// Live attributed processes (root included).
    pub processes: u32,
    /// Live threads, if every stat was parseable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    /// Cumulative CPU seconds of the tree, including exited members.
    pub cpu_seconds: f64,
    /// CPU cores in use over the last interval (may exceed 1.0); `None` on the first sample.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<f64>,
    /// A member exited since the previous sample, so `cpu_cores` is an
    /// estimate (a lower bound from its last observed CPU).
    pub cpu_rate_estimated: bool,
    /// Memory of live members, or `None` when nothing was readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Basis of `memory_bytes`.
    pub memory_basis: MemoryBasis,
    /// Cumulative storage read bytes (incl. exited members), if readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_bytes: Option<u64>,
    /// Cumulative storage write bytes (incl. exited members), if readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_bytes: Option<u64>,
    /// Whether every member's IO counters were readable; when `false` the
    /// IO totals and rates are `None` rather than an undercount.
    pub io_complete: bool,
    /// Read rate over the last interval (approximate above 2^53 bytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_bytes_per_sec: Option<f64>,
    /// Write rate over the last interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_bytes_per_sec: Option<f64>,
    /// Always `None`: GPU is unknown, not zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuUsage>,
    /// The root was gone (exited or pid reused) at this sample.
    pub root_lost: bool,
    /// The `/proc` scan or tracked set hit its cap; unseen members may exist, so
    /// IO totals/rates and `cpu_cores` are `None` and the rate baseline resets.
    pub truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Ident {
    pid: u32,
    start: u64,
}
#[derive(Clone, Copy, Debug)]
struct Stat {
    ident: Ident,
    ppid: u32,
    pgrp: u32,
    ticks: u64,
    threads: u32,
    rss_pages: u64,
}
#[derive(Clone, Copy, Default)]
struct Retained {
    ticks: u64,
    read: Option<u64>,
    write: Option<u64>,
}

/// Parse `/proc/<pid>/stat` content (comm may contain spaces and parentheses).
fn parse_stat(pid: u32, text: &str) -> Option<Stat> {
    let rest = text.get(text.rfind(')')? + 1..)?;
    let f: Vec<&str> = rest.split_ascii_whitespace().collect();
    let at = |i: usize| f.get(i).copied();
    Some(Stat {
        ident: Ident {
            pid,
            start: at(19)?.parse().ok()?,
        },
        ppid: at(1)?.parse().ok()?,
        pgrp: at(2)?.parse().ok()?,
        ticks: at(11)?
            .parse::<u64>()
            .ok()?
            .checked_add(at(12)?.parse().ok()?)?,
        threads: at(17)?.parse().ok()?,
        rss_pages: at(21)?.parse::<i64>().ok()?.max(0).try_into().ok()?,
    })
}
fn kv(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key))?
        .trim_start_matches(':')
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()
}

/// Tracks one producer tree. Create right after spawning the root.
pub struct Tracker {
    proc_root: PathBuf,
    root: Ident,
    ticks_per_sec: f64,
    page: u64,
    known: HashSet<Ident>,
    last_seen: HashMap<Ident, Retained>,
    retired: Retained,
    retired_io_unknown: bool,
    scan_cap: usize,
    max_tracked: usize,
    previous: Option<(Instant, u64, Option<u64>, Option<u64>)>,
    buf: String,
}

impl Tracker {
    /// Bind to `root_pid` of the real `/proc`. `None` if the root is already gone.
    #[must_use]
    pub fn new(root_pid: u32) -> Option<Self> {
        Self::with_proc_root(Path::new("/proc"), root_pid)
    }
    /// As [`Tracker::new`] over an alternate procfs tree (fixtures).
    #[must_use]
    pub fn with_proc_root(proc_root: &Path, root_pid: u32) -> Option<Self> {
        let text = fs::read_to_string(proc_root.join(root_pid.to_string()).join("stat")).ok()?;
        let root = parse_stat(root_pid, &text)?.ident;
        Some(Self {
            proc_root: proc_root.to_owned(),
            root,
            ticks_per_sec: f64::from(
                u32::try_from(rustix::param::clock_ticks_per_second())
                    .unwrap_or(100)
                    .max(1),
            ),
            page: u64::try_from(rustix::param::page_size()).unwrap_or(4096),
            known: HashSet::from([root]),
            last_seen: HashMap::new(),
            retired: Retained::default(),
            retired_io_unknown: false,
            scan_cap: MAX_TRACKED * 16,
            max_tracked: MAX_TRACKED,
            previous: None,
            buf: String::new(),
        })
    }

    /// Override the scan cap and tracked-member cap (tests simulate truncation
    /// without tens of thousands of fixture files).
    pub fn set_caps(&mut self, scan_cap: usize, max_tracked: usize) {
        self.scan_cap = scan_cap.max(1);
        self.max_tracked = max_tracked.max(1);
    }

    fn read(&mut self, pid: u32, file: &str) -> Option<&str> {
        self.buf.clear();
        let path = self.proc_root.join(pid.to_string()).join(file);
        let mut f = fs::File::open(path).ok()?;
        std::io::Read::read_to_string(&mut f, &mut self.buf).ok()?;
        Some(&self.buf)
    }

    /// Scan once and attribute the tree. Never panics; allocation is bounded.
    #[allow(clippy::too_many_lines)]
    pub fn sample(&mut self, now: Instant) -> ResourceSample {
        let mut all: Vec<Stat> = Vec::new();
        let mut seen: HashSet<Ident> = HashSet::new();
        let mut truncated = false;
        let mut scan_capped = false;
        if let Ok(dir) = fs::read_dir(&self.proc_root) {
            for entry in dir.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                else {
                    continue;
                };
                if all.len() >= self.scan_cap {
                    truncated = true;
                    scan_capped = true;
                    break;
                }
                if let Ok(text) = fs::read_to_string(entry.path().join("stat")) {
                    if let Some(s) = parse_stat(pid, &text) {
                        seen.insert(s.ident);
                        all.push(s);
                    }
                }
            }
        }
        // The root and every known member are read directly, so a capped scan
        // can never hide them (and never fakes a lost root).
        let mut direct: Vec<u32> = vec![self.root.pid];
        direct.extend(self.known.iter().map(|i| i.pid));
        let mut root_alive = false;
        for pid in direct {
            let Some(s) = fs::read_to_string(self.proc_root.join(pid.to_string()).join("stat"))
                .ok()
                .and_then(|t| parse_stat(pid, &t))
            else {
                continue;
            };
            if s.ident == self.root {
                root_alive = true;
            }
            if seen.insert(s.ident) {
                all.push(s);
            }
        }
        let mut members: HashSet<Ident> = HashSet::new();
        let by_parent: HashMap<u32, Vec<&Stat>> = all.iter().fold(HashMap::new(), |mut m, s| {
            m.entry(s.ppid).or_default().push(s);
            m
        });
        // Seed the walk with the root and every live known identity so the
        // descendants of reparented members are found too.
        let mut stack: Vec<u32> = Vec::new();
        if root_alive {
            members.insert(self.root);
            stack.push(self.root.pid);
        }
        for s in &all {
            if self.known.contains(&s.ident) && members.insert(s.ident) {
                stack.push(s.ident.pid);
            }
        }
        while let Some(p) = stack.pop() {
            for s in by_parent.get(&p).into_iter().flatten() {
                if members.len() >= self.max_tracked {
                    truncated = true;
                } else if members.insert(s.ident) {
                    stack.push(s.ident.pid);
                }
            }
        }
        if root_alive {
            for s in &all {
                if s.pgrp == self.root.pid {
                    if members.len() >= self.max_tracked {
                        truncated = true;
                        break;
                    }
                    members.insert(s.ident);
                }
            }
        }
        if scan_capped {
            // The ppid view is incomplete: follow the kernel's direct child
            // lists from every member, reading each child's stat directly.
            let mut queue: Vec<u32> = members.iter().map(|i| i.pid).collect();
            while let Some(p) = queue.pop() {
                let kids: Vec<u32> = self
                    .read(p, &format!("task/{p}/children"))
                    .map(|t| {
                        t.split_ascii_whitespace()
                            .filter_map(|x| x.parse().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                for kid in kids {
                    let Some(st) =
                        fs::read_to_string(self.proc_root.join(kid.to_string()).join("stat"))
                            .ok()
                            .and_then(|t| parse_stat(kid, &t))
                    else {
                        continue;
                    };
                    if members.contains(&st.ident) {
                        continue;
                    }
                    if members.len() >= self.max_tracked {
                        truncated = true;
                        continue;
                    }
                    members.insert(st.ident);
                    if seen.insert(st.ident) {
                        all.push(st);
                    }
                    queue.push(kid);
                }
            }
        }
        let live: Vec<Stat> = all
            .iter()
            .filter(|s| members.contains(&s.ident))
            .copied()
            .collect();
        let live_ids: HashSet<Ident> = live.iter().map(|s| s.ident).collect();
        // Every known member is read directly above, so one absent from `live`
        // is truly gone (never merely hidden by a cap): retire it, which also
        // keeps `last_seen` and `known` bounded by the tracked cap.
        let mut retired_now = false;
        let gone: Vec<Ident> = self
            .last_seen
            .keys()
            .filter(|i| !live_ids.contains(i))
            .copied()
            .collect();
        for g in gone {
            if let Some(r) = self.last_seen.remove(&g) {
                retired_now = true;
                self.retired.ticks = self.retired.ticks.saturating_add(r.ticks);
                self.retired.read = add(self.retired.read, r.read);
                self.retired.write = add(self.retired.write, r.write);
                if r.read.is_none() || r.write.is_none() {
                    self.retired_io_unknown = true;
                }
            }
        }
        self.known.clone_from(&live_ids);
        let (mut threads, mut mem) = (Some(0u32), Some(0u64));
        let (mut pss_n, mut rss_n) = (0usize, 0usize);
        let mut io_complete = !self.retired_io_unknown && !truncated;
        for s in &live {
            threads = threads.map(|t| t.saturating_add(s.threads));
            let rss = s.rss_pages.saturating_mul(self.page);
            let pss = self
                .read(s.ident.pid, "smaps_rollup")
                .and_then(|t| kv(t, "Pss"))
                .map(|k| k.saturating_mul(1024));
            let bytes = if let Some(p) = pss {
                pss_n += 1;
                p
            } else {
                rss_n += 1;
                rss
            };
            mem = mem.map(|m| m.saturating_add(bytes));
            let (r, w) = self.read(s.ident.pid, "io").map_or((None, None), |t| {
                (kv(t, "read_bytes"), kv(t, "write_bytes"))
            });
            if r.is_none() || w.is_none() {
                io_complete = false;
            }
            self.last_seen.insert(
                s.ident,
                Retained {
                    ticks: s.ticks,
                    read: r,
                    write: w,
                },
            );
        }
        // Totals over the live members and the retired ones.
        let mut ticks = self.retired.ticks;
        let (mut read, mut write) = (
            self.retired.read.unwrap_or(0),
            self.retired.write.unwrap_or(0),
        );
        for r in self.last_seen.values() {
            ticks = ticks.saturating_add(r.ticks);
            read = read.saturating_add(r.read.unwrap_or(0));
            write = write.saturating_add(r.write.unwrap_or(0));
        }
        // Unknown is not zero: any member without readable IO makes the
        // aggregate unknown rather than an undercount.
        let (read, write) = if io_complete {
            (Some(read), Some(write))
        } else {
            (None, None)
        };
        let (memory_bytes, memory_basis) = match (pss_n, rss_n) {
            (0, 0) => (None, MemoryBasis::Unknown),
            (_, 0) => (mem, MemoryBasis::Pss),
            (0, _) => (mem, MemoryBasis::Rss),
            _ => (mem, MemoryBasis::PssWithRssFallback),
        };
        let cpu_seconds = to_f64(ticks) / self.ticks_per_sec;
        let (mut cpu_cores, mut rd_rate, mut wr_rate) = (None, None, None);
        // A truncated sample may omit members: no rate is derived from it and
        // the baseline resets so the next sample cannot show a lifetime burst.
        if let Some((t0, ticks0, r0, w0)) = self.previous.filter(|_| !truncated) {
            let dt = now.saturating_duration_since(t0).as_secs_f64();
            if dt > 0.0 {
                cpu_cores = Some(to_f64(ticks.saturating_sub(ticks0)) / self.ticks_per_sec / dt);
                let per_sec =
                    |a: Option<u64>, b: Option<u64>| Some(to_f64(a?.checked_sub(b?)?) / dt);
                rd_rate = per_sec(read, r0);
                wr_rate = per_sec(write, w0);
            }
        }
        self.previous = if truncated {
            None
        } else {
            Some((now, ticks, read, write))
        };
        ResourceSample {
            processes: u32::try_from(live.len()).unwrap_or(u32::MAX),
            threads,
            cpu_seconds,
            cpu_cores,
            // A member that exited between samples contributes only the CPU
            // seen at its last sample, so the interval rate is a lower bound.
            cpu_rate_estimated: retired_now,
            memory_bytes,
            memory_basis,
            read_bytes: read,
            write_bytes: write,
            io_complete,
            read_bytes_per_sec: rd_rate,
            write_bytes_per_sec: wr_rate,
            gpu: None,
            root_lost: !root_alive,
            truncated,
        }
    }
}

/// `u64` to `f64` without a lossy cast; exact below 2^53, otherwise the
/// nearest representable value (relative error below 2^-52).
fn to_f64(v: u64) -> f64 {
    let hi = u32::try_from(v >> 32).unwrap_or(u32::MAX);
    let lo = u32::try_from(v & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    f64::from(hi) * 4_294_967_296.0 + f64::from(lo)
}
fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
    }
}

/// 1 Hz pacing around a [`Tracker`] for the producer pump.
pub struct Monitor {
    tracker: Tracker,
    next: Instant,
}
impl Monitor {
    /// Monitor `root_pid`; `None` if it already vanished.
    #[must_use]
    pub fn new(root_pid: u32) -> Option<Self> {
        Some(Self {
            tracker: Tracker::new(root_pid)?,
            next: Instant::now(),
        })
    }
    /// Sample if the period elapsed (the first call samples immediately).
    pub fn poll(&mut self, now: Instant) -> Option<ResourceSample> {
        if now < self.next {
            return None;
        }
        self.next = now + PERIOD;
        Some(self.tracker.sample(now))
    }
    /// One last sample, taken once when the leader is seen exited but not yet
    /// reaped or swept (its zombie `stat` is still readable).
    pub fn finish(&mut self, now: Instant) -> ResourceSample {
        self.next = now + PERIOD;
        self.tracker.sample(now)
    }
}

/// Read-only cgroup capability probe. `true` means this process cannot manage a
/// delegated cgroup subtree (no writable own cgroup), so tree accounting uses
/// the `/proc` walk; nothing is faked.
#[must_use]
pub fn delegation_unavailable() -> bool {
    let Ok(text) = fs::read_to_string("/proc/self/cgroup") else {
        return true;
    };
    let Some(path) = text.lines().find_map(|l| l.strip_prefix("0::")) else {
        return true;
    };
    let dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    !(dir.join("cgroup.controllers").exists()
        && rustix::fs::access(&dir, rustix::fs::Access::WRITE_OK).is_ok())
}
