//! Measures what one process-tree sweep costs in file reads.
//!
//! The operator's efficiency rule asks for the per-sweep read counts (kept from
//! the previous commit) plus heap allocations per sweep for a 200-process tree.
//! The read counts are measured here, against a **fixture** `/proc` in a tempdir,
//! never the host's.
//!
//! Run it with `cargo run -p multiview-webengine --example sweep_cost`; pass
//! `--why-no-allocator` for why the heap figure is not measured here.
//!
//! Two shapes are measured, mirroring the efficiency test:
//!
//! * **bare** — no `task/<tid>/children` strands at all, so every node falls back
//!   to the one `/proc`-wide `ppid` scan (the expensive case);
//! * **strands** — every process has a `children` file, so no scan happens.
//!
//! Tree construction and argument parsing are outside the measured region; the
//! measured region is exactly one `sample_process_tree` call.
//!
//! `print_stdout` is allowed for this one example target: a cost probe's whole
//! output is its report, and it is never part of the engine's data plane.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::path::Path;

use multiview_webengine::{sample_process_tree, Error, FsRoot, Pid, Result};

/// The size of the tree measured: root `1` plus `2..=SIZE`.
const SIZE: u32 = 200;

/// The fixture path, counting the reads a sweep makes through it.
struct CountingRoot<'a> {
    root: &'a Path,
    stat_reads: Cell<u64>,
    children_reads: Cell<u64>,
    total_reads: Cell<u64>,
}

impl<'a> CountingRoot<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            stat_reads: Cell::new(0),
            children_reads: Cell::new(0),
            total_reads: Cell::new(0),
        }
    }
}

impl FsRoot for CountingRoot<'_> {
    fn read_to_string(&self, path: &str) -> Result<String> {
        self.total_reads.set(self.total_reads.get() + 1);
        if path.ends_with("/stat") {
            self.stat_reads.set(self.stat_reads.get() + 1);
        }
        if path.ends_with("/children") {
            self.children_reads.set(self.children_reads.get() + 1);
        }
        FsRoot::read_to_string(self.root, path)
    }

    fn exists(&self, path: &str) -> bool {
        FsRoot::exists(self.root, path)
    }

    fn read_dir_names(&self, path: &str) -> Result<Vec<String>> {
        self.root.read_dir_names(path)
    }
}

/// Write `SIZE` processes under `root/proc`, with `1` as the root.
///
/// Kernel-shaped `stat` lines (50 fields) with distinct activity fields, so a
/// parser that counted from the wrong token would read different numbers.
fn write_tree(root: &Path, with_strands: bool) -> Result<()> {
    let proc = root.join("proc");
    let children: Vec<String> = (2..=SIZE).map(|pid| pid.to_string()).collect();
    for pid in 1..=SIZE {
        let base = proc.join(pid.to_string());
        let task = base.join("task").join(pid.to_string());
        std::fs::create_dir_all(&task).map_err(|source| Error::io("fixture", source))?;
        std::fs::write(base.join("stat"), stat_line(pid, 1))
            .map_err(|source| Error::io("fixture", source))?;
        std::fs::write(base.join("smaps_rollup"), "Pss: 64 kB\nRss: 128 kB\n")
            .map_err(|source| Error::io("fixture", source))?;
        std::fs::write(base.join("cmdline"), "/opt/chrome\0--type=renderer\0")
            .map_err(|source| Error::io("fixture", source))?;
        if with_strands {
            let body = if pid == 1 {
                children.join(" ")
            } else {
                String::new()
            };
            std::fs::write(task.join("children"), body)
                .map_err(|source| Error::io("fixture", source))?;
        }
    }
    Ok(())
}

/// A kernel-shaped `/proc/<pid>/stat` line: exactly the 50 fields after `comm`.
fn stat_line(pid: u32, parent: u32) -> String {
    let mut fields: Vec<String> = vec![
        "S".to_owned(),
        parent.to_string(),
        pid.to_string(),
        pid.to_string(),
        "0".to_owned(),
        "-1".to_owned(),
        "4194560".to_owned(),
        (1_000 + u64::from(pid)).to_string(),
        (2_000 + u64::from(pid)).to_string(),
        (3_000 + u64::from(pid)).to_string(),
        (4_000 + u64::from(pid)).to_string(),
        "3".to_owned(), // utime
        "2".to_owned(), // stime
        "0".to_owned(),
        "0".to_owned(),
        "20".to_owned(),
        "0".to_owned(),
        "5".to_owned(),
        "0".to_owned(),
        (1_000 + u64::from(pid)).to_string(), // starttime: field 22
    ];
    for _ in 0..15 {
        fields.push("0".to_owned());
    }
    fields.push("17".to_owned());
    for _ in 0..14 {
        fields.push("0".to_owned());
    }
    format!("{pid} (chrome) {}\n", fields.join(" "))
}

/// Measure one sweep over a freshly written `with_strands` tree.
fn measure(label: &str, with_strands: bool) -> Result<String> {
    let dir = tempfile::tempdir().map_err(|source| Error::io("tempdir", source))?;
    write_tree(dir.path(), with_strands)?;
    let root = CountingRoot::new(dir.path());

    let Some(sample) = sample_process_tree(&root, Pid::new(1), None)?.into_sample() else {
        return Err(Error::missing(
            "proc/1/stat: the fixture root vanished mid-sweep",
        ));
    };

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{label}: {} descendants | {} stat reads, {} children reads, {} total file reads",
        sample.process_count(),
        root.stat_reads.get(),
        root.children_reads.get(),
        root.total_reads.get(),
    );
    Ok(out)
}

/// Why the heap figure is reported as unmeasured rather than estimated.
const WHY_NO_ALLOCATOR: &str = "\
Heap allocations per sweep are NOT measured here, and the reason is structural \
rather than an omission:

* the crate is `#![forbid(unsafe_code)]` (it is checked as LGPL-clean, GPU-free \
pure `std`), and installing a counting allocator needs `unsafe impl GlobalAlloc`;
* the workspace sets `clippy`/`rustc` `unsafe_code = \"forbid\"` for **every** \
target, so an `#[allow(unsafe_code)]` inside an example, test or bench is \
rejected as `E0453: allow(unsafe_code) incompatible with previous forbid` — the \
level would have to be relaxed at the Cargo manifest level, which changes the \
whole crate's lint contract for every target to serve one measurement.

A global allocator also cannot see zero-sized allocations, so it would have been \
a lower bound by construction. The read counts below are exact.";

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--why-no-allocator") {
        println!("{WHY_NO_ALLOCATOR}");
        return Ok(());
    }

    print!("{}", measure("bare    (ppid scan)", false)?);
    print!("{}", measure("strands (no scan) ", true)?);
    Ok(())
}
