//! # multiview-webengine
//!
//! The **library core** of the web engine: resource scopes, the process-tree and
//! cgroup accounting sampler, the per-job usage record and the clip-job state
//! model (ADR-0102 decisions 8-10, ADR-0100 decision 4).
//!
//! This slice is deliberately small and inwards-facing. It launches no browser,
//! speaks no CDP, writes no cgroup file, and is not wired into `multiview-cli`
//! yet: it is the accounting substrate the later slices stand on. Three
//! properties make it usable from an engine that must never stall:
//!
//! * **No native dependencies and no `unsafe`.** Pure `std` plus `serde` and
//!   `thiserror`, so `cargo check --workspace` stays green on a GPU-free host.
//! * **No clock and no threads on any sampled path.** [`usage::Sampler`] takes
//!   the monotonic time as an argument and every read goes through the injected
//!   [`fsroot::FsRoot`], so a test drives a whole soak deterministically.
//! * **No fixed absolute path.** Production passes a root of `/`; tests pass a
//!   fixture directory or an in-memory tree. Nothing here touches the host's
//!   real `/proc` or `/sys` in a test.
//!
//! ## Why the process tree, not one process
//!
//! The measurements behind ADR-0102 found ~95% of a Chromium capture's CPU in
//! the GPU process, and an early sampler that matched a single process by
//! command line reported 0% CPU and 100 MB. Every descendant is therefore
//! walked, classified and summed, and the cumulative figure retains the CPU time
//! of children that exited between two samples so a total never goes backwards.
//!
//! ## Modules
//!
//! * [`scope`] — [`scope::ResourceScope`], [`scope::ScopeKind`] and the
//!   generation-guarded [`scope::ScopeRegistry`] that drops late samples.
//! * [`fsroot`] — the injected [`fsroot::FsRoot`] / [`fsroot::CgroupFs`] seam.
//! * [`process_tree`] — `/proc` walking, `stat`/`cmdline`/`smaps_rollup` parsing
//!   and the Chromium per-type CPU split.
//! * [`cgroup`] — cgroup v2 reads, the delegation capability probe and the
//!   ordered (never executed) boot-time leaf-move plan.
//! * [`usage`] — [`usage::ResourceUsage`] and its injected-clock
//!   [`usage::Sampler`].
//! * [`job`] — [`job::JobState`] with a guarded transition, and the bounded,
//!   coalescing [`job::JobQueue`].
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cgroup;
pub mod error;
pub mod fsroot;
pub mod job;
pub mod process_tree;
pub mod scope;
pub mod usage;

pub use cgroup::{
    delegation_status, parse_cpu_stat, plan_leaf_move, probe_child_name, probe_child_path,
    read_cgroup_usage, rollback_for, CgroupUsage, ControllerSet, CpuStat, DelegationReport,
    FailureAction, LeafMoveStep, NotDelegated, VerifyMembership, CGROUP_CONTROLLERS, CGROUP_PROCS,
    CGROUP_SUBTREE_CONTROL, CPU_STAT, DELEGATION_PROBE_PREFIX, ENGINE_LEAF, MEMORY_CURRENT,
    MEMORY_PEAK, PIDS_CURRENT, REQUIRED_CONTROLLERS,
};
pub use error::{Error, Result};
pub use fsroot::{CgroupFs, FsRoot, ParsePidError, Pid};
pub use job::{percent_u8, FillId, Job, JobId, JobPhase, JobQueue, JobState, Trigger};
pub use process_tree::{
    children_of, chromium_process_type, chromium_type_arg, exited_since, parse_stat_comm,
    parse_stat_cpu_ticks, parse_stat_ppid, parse_stat_starttime, read_memory, read_process,
    read_starttime, sample_process_tree, scan_ppid_children, ticks_to_seconds, walk_tree,
    ChromiumProcessType, ExpectedRoot, MemorySource, PpidIndex, PreviousSweep, ProcRecord,
    ProcessTicks, ProcessTreeSample, RootStat, TreeSample, TypeAccounting, TypeAccountingArray,
    TypeSecondsArray, CHROMIUM_PROCESS_TYPES, PROC, TYPE_COUNT, USER_HZ,
};
pub use scope::{Generation, ResourceScope, ScopeKind, ScopeOwner, ScopeRegistry};
pub use usage::{
    attribute, cpu_cores, monotonic_total, type_cpu_seconds, usage_from_cgroup, Accounting,
    ResourceUsage, Sampler, CPU_CORES_CAP,
};

// Browser capture runtime and immutable artifact service.
pub mod artifact;
pub mod capture;
pub mod cdp_wire;
pub mod fetch;
pub mod resource;
pub mod runner;
pub mod scheduler;
pub mod service;
pub mod service_journal;
