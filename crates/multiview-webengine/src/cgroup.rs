//! cgroup v2 reads, the delegation capability probe, and the **plan** for the
//! boot-time move into a leaf cgroup (ADR-0099 decisions 8, 9 and 10).
//!
//! Two hard rules shape this module:
//!
//! * **Reads only.** `memory.peak` is a lifetime peak *because* nothing ever
//!   writes it, so this crate never writes any cgroup file. The one move that
//!   does write — the boot-time relocation into `engine/` — is returned as an
//!   ordered [`LeafMoveStep`] list by [`plan_leaf_move`] and executed by a later
//!   slice, in `main`, before any thread or child exists.
//! * **Capability, not configuration.** Delegation is decided by trying to
//!   create *and remove* a child directory under the target cgroup and by
//!   reading `cgroup.controllers` — never by reading the service unit. The probe
//!   runs against an injected [`CgroupFs`], so tests exercise both answers in a
//!   tempdir and never touch the real hierarchy.
//!
//! Order matters in [`plan_leaf_move`]: a cgroup that holds processes cannot
//! enable a domain controller for its children (`cpu`, `memory` and `pids` are
//! all domain controllers on the kernels this targets), so the pid move must
//! precede the `cgroup.subtree_control` write. A failed step means Conspect
//! stays where it is and uses process-tree accounting with the
//! `CgroupDelegationUnavailable` warning.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::fsroot::{CgroupFs, FsRoot};
use crate::process_tree::{read_bare_u64, u64_to_f64};

/// The controllers that must be delegated for exact accounting, hard limits and
/// kill-the-group teardown: CPU time, memory, and a pid ceiling.
pub const REQUIRED_CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];

/// Kernel-boot counter file (microseconds).
pub const CPU_STAT: &str = "cpu.stat";
/// Current memory usage file (bytes).
pub const MEMORY_CURRENT: &str = "memory.current";
/// Lifetime peak memory file (bytes). Read, **never** written.
pub const MEMORY_PEAK: &str = "memory.peak";
/// Current pid count file.
pub const PIDS_CURRENT: &str = "pids.current";
/// Controller list for a cgroup's children.
pub const CGROUP_CONTROLLERS: &str = "cgroup.controllers";
/// Controller enable list, written by the plan only.
pub const CGROUP_SUBTREE_CONTROL: &str = "cgroup.subtree_control";
/// The file a process is moved with.
pub const CGROUP_PROCS: &str = "cgroup.procs";

/// The fixed, non-secret name of the leaf cgroup the boot-time move creates.
pub const ENGINE_LEAF: &str = "engine";

/// The reserved prefix of the child-directory name the delegation probe creates
/// and removes, so a probe can never collide with a real generation scope.
///
/// A full name is this prefix plus the process id and a per-process counter
/// (see [`probe_child_name`]), so two probes — even in the same process — never
/// contend for one directory.
pub const DELEGATION_PROBE_PREFIX: &str = "conspect-delegation-probe-";

/// A fresh, unique probe child name.
///
/// `conspect-delegation-probe-<pid>-<counter>`: the pid separates processes and
/// the counter separates calls within one process, without a clock.
#[must_use]
pub fn probe_child_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{DELEGATION_PROBE_PREFIX}{}-{counter}", std::process::id())
}

/// The cgroup path of the probe child `name` under `root`.
#[must_use]
pub fn probe_child_path(root: &str, name: &str) -> String {
    merge_cgroup_path(root, name)
}

/// Join a cgroup directory and a child name with exactly one `/`.
#[must_use]
pub fn merge_cgroup_path(root: &str, child: &str) -> String {
    format!(
        "{}/{}",
        root.trim_end_matches('/'),
        child.trim_start_matches('/')
    )
}

/// The `cpu.stat` fields the sampler reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuStat {
    /// `usage_usec`: total CPU time charged to the cgroup, in microseconds.
    pub usage_usec: u64,
    /// `user_usec`, when the kernel reports it.
    pub user_usec: Option<u64>,
    /// `system_usec`, when the kernel reports it.
    pub system_usec: Option<u64>,
    /// `nr_periods`, when the kernel reports it.
    pub nr_periods: Option<u64>,
    /// `nr_throttled`, when the kernel reports it.
    pub nr_throttled: Option<u64>,
    /// `throttled_usec`, when the kernel reports it.
    pub throttled_usec: Option<u64>,
}

impl CpuStat {
    /// `usage_usec` as seconds.
    #[must_use]
    pub fn usage_seconds(&self) -> f64 {
        u64_to_f64(self.usage_usec) / 1_000_000.0
    }
}

/// One cgroup reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CgroupUsage {
    /// `cpu.stat`.
    pub cpu: CpuStat,
    /// `memory.current`, in bytes. `None` when the file could not be read.
    pub memory_current_bytes: Option<u64>,
    /// `memory.peak`, in bytes. `None` on a kernel without the file (pre-5.19)
    /// or when it could not be read — unknown, never a fabricated zero.
    pub memory_peak_bytes: Option<u64>,
    /// `pids.current`. `None` when the file could not be read.
    pub pids_current: Option<u64>,
}

impl CgroupUsage {
    /// Total CPU seconds charged to the cgroup.
    #[must_use]
    pub fn cpu_seconds_total(&self) -> f64 {
        self.cpu.usage_seconds()
    }
}

/// Read one cgroup's usage files under `dir` (a root-relative path for `fs`).
///
/// `cpu.stat` is the only required file: a cgroup that holds a live browser
/// always has one, and without it there is no accounting to report at all.
/// Every other field is optional so a kernel that lacks `memory.peak` still
/// yields a usable sample.
///
/// # Errors
///
/// [`Error::Io`] when `cpu.stat` cannot be read, [`Error::Parse`] when it is
/// present but carries no `usage_usec`.
pub fn read_cgroup_usage<F: FsRoot + ?Sized>(fs: &F, dir: &str) -> Result<CgroupUsage> {
    let cpu_path = merge_cgroup_path(dir, CPU_STAT);
    let body = fs.read_to_string(&cpu_path)?;
    let cpu = parse_cpu_stat(&body).ok_or_else(|| Error::parse(cpu_path, "no usage_usec field"))?;
    Ok(CgroupUsage {
        cpu,
        memory_current_bytes: read_bare_u64(fs, &merge_cgroup_path(dir, MEMORY_CURRENT)),
        memory_peak_bytes: read_bare_u64(fs, &merge_cgroup_path(dir, MEMORY_PEAK)),
        pids_current: read_bare_u64(fs, &merge_cgroup_path(dir, PIDS_CURRENT)),
    })
}

/// Parse a cgroup v2 `cpu.stat` body.
///
/// `usage_usec` is required; every other key is optional. An unknown key is
/// ignored (the kernel adds keys over time). `None` when `usage_usec` is absent.
#[must_use]
pub fn parse_cpu_stat(body: &str) -> Option<CpuStat> {
    let mut stat = CpuStat::default();
    let mut saw_usage = false;
    for line in body.lines() {
        let mut fields = line.split_whitespace();
        let (Some(key), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "usage_usec" => {
                stat.usage_usec = value;
                saw_usage = true;
            }
            "user_usec" => stat.user_usec = Some(value),
            "system_usec" => stat.system_usec = Some(value),
            "nr_periods" => stat.nr_periods = Some(value),
            "nr_throttled" => stat.nr_throttled = Some(value),
            "throttled_usec" => stat.throttled_usec = Some(value),
            _ => {}
        }
    }
    if saw_usage {
        Some(stat)
    } else {
        None
    }
}

/// Whether `root` may be treated as a delegated cgroup subtree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelegationReport {
    /// A child cgroup could be created and removed, and `cgroup.controllers`
    /// lists every [`REQUIRED_CONTROLLERS`] entry.
    Delegated,
    /// Delegation is unavailable. The reason names what was observed so the
    /// `CgroupDelegationUnavailable` warning can say what is lost and how to fix
    /// it (ADR-0099 decision 9).
    NotDelegated(NotDelegated),
}

impl DelegationReport {
    /// Whether the subtree is delegated.
    #[must_use]
    pub const fn is_delegated(&self) -> bool {
        matches!(self, Self::Delegated)
    }

    /// The reason delegation is unavailable, when it is.
    #[must_use]
    pub const fn reason(&self) -> Option<NotDelegated> {
        match self {
            Self::Delegated => None,
            Self::NotDelegated(reason) => Some(*reason),
        }
    }
}

/// Why delegation is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotDelegated {
    /// `cgroup.controllers` could not be read under `root`.
    ControllersUnreadable,
    /// `cgroup.controllers` does not list every required controller; the bitmask
    /// is what it did list, and `required` is what was missing.
    ControllersMissing {
        /// The controllers present.
        present: ControllerSet,
        /// The missing controllers, as a mask over [`REQUIRED_CONTROLLERS`].
        missing: ControllerSet,
    },
    /// A child cgroup could not be created (no delegation, or a read-only
    /// hierarchy).
    CannotCreateChild,
    /// The probe child was created but could not be removed.
    CannotRemoveChild,
    /// `root` is not a directory (no cgroup v2 mount there).
    NotADirectory,
}

/// A bitmask over [`REQUIRED_CONTROLLERS`].
///
/// A mask rather than a list so a report is `Copy`, branch-free to compare, and
/// stable in an event payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControllerSet(u8);

impl ControllerSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// The bit for `name`, when `name` is one of the required controllers.
    #[must_use]
    pub fn bit(name: &str) -> Option<u8> {
        REQUIRED_CONTROLLERS
            .iter()
            .position(|required| *required == name)
            .and_then(|index| 1_u8.checked_shl(u32::try_from(index).ok()?))
    }

    /// Build a set from a `cgroup.controllers` body.
    #[must_use]
    pub fn from_controllers(body: &str) -> Self {
        let mut mask = 0_u8;
        for name in body.split_whitespace() {
            if let Some(bit) = Self::bit(name) {
                mask |= bit;
            }
        }
        Self(mask)
    }

    /// The set of required controllers this set is missing.
    #[must_use]
    pub const fn missing_from_required(self) -> Self {
        Self(Self::all_bits() & !self.0)
    }

    /// Whether every required controller is present.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.missing_from_required().0 == 0
    }

    /// Whether `name`'s bit is set.
    #[must_use]
    pub fn contains(self, name: &str) -> bool {
        Self::bit(name).is_some_and(|bit| self.0 & bit != 0)
    }

    /// Whether no required controller is present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw mask.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The names of the controllers this set contains, in [`REQUIRED_CONTROLLERS`]
    /// order.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        REQUIRED_CONTROLLERS
            .iter()
            .copied()
            .filter(|name| self.contains(name))
            .collect()
    }

    const fn all_bits() -> u8 {
        1 | 2 | 4
    }
}

/// Probe `root` for delegation (ADR-0099 decision 9).
///
/// The probe creates a uniquely named [`probe_child_name`] under `root` and
/// removes it again. `root` must be a directory; a `create_dir` failure is
/// reported as [`NotDelegated::CannotCreateChild`] whatever its cause (a
/// read-only hierarchy, a missing delegation, a permission denial) because the
/// operator action — the systemd drop-in — is the same for all of them.
///
/// A directory that already exists for a freshly generated name can only be a
/// stale probe from a crashed run, so an `EEXIST` create is retried **once**
/// with another fresh name rather than reported as "not delegated". Every
/// failure path after a create makes a best-effort `rmdir`, so a failed probe
/// leaves nothing behind.
///
/// # Errors
///
/// [`Error::Io`] when listing `root` itself fails for a reason other than
/// absence through the underlying root implementation (see
/// [`FsRoot::read_dir_names`]).
pub fn delegation_status<F: CgroupFs + ?Sized>(fs: &F, root: &str) -> Result<DelegationReport> {
    if !fs.exists(root) {
        return Ok(DelegationReport::NotDelegated(NotDelegated::NotADirectory));
    }
    let Ok(controllers) = fs.read_to_string(&merge_cgroup_path(root, CGROUP_CONTROLLERS)) else {
        return Ok(DelegationReport::NotDelegated(
            NotDelegated::ControllersUnreadable,
        ));
    };
    let present = ControllerSet::from_controllers(&controllers);
    if !present.is_complete() {
        return Ok(DelegationReport::NotDelegated(
            NotDelegated::ControllersMissing {
                present,
                missing: present.missing_from_required(),
            },
        ));
    }

    let mut probe = probe_child_path(root, &probe_child_name());
    match fs.create_dir(&probe) {
        Ok(()) => {}
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::AlreadyExists => {
            // A stale probe holds the name: one retry with a fresh one.
            probe = probe_child_path(root, &probe_child_name());
            if fs.create_dir(&probe).is_err() {
                let _ = fs.remove_dir(&probe);
                return Ok(DelegationReport::NotDelegated(
                    NotDelegated::CannotCreateChild,
                ));
            }
        }
        Err(_) => {
            return Ok(DelegationReport::NotDelegated(
                NotDelegated::CannotCreateChild,
            ))
        }
    }
    if fs.remove_dir(&probe).is_err() {
        // Best effort: a failed probe must not leave its directory behind.
        let _ = fs.remove_dir(&probe);
        return Ok(DelegationReport::NotDelegated(
            NotDelegated::CannotRemoveChild,
        ));
    }
    Ok(DelegationReport::Delegated)
}

/// One step of the boot-time leaf move. **Nothing here executes**: the plan is
/// returned as data so `main` can run it before any thread or child exists, and
/// so a test can assert the order.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LeafMoveStep {
    /// Create the leaf cgroup below `parent`.
    CreateLeaf {
        /// The leaf's absolute path.
        path: PathBuf,
    },
    /// Write our own pid into the leaf's `cgroup.procs`, moving this process (and
    /// thereby every future child) into the leaf.
    MoveSelf {
        /// The pid to write.
        pid: u32,
        /// The `cgroup.procs` file to write it to.
        path: PathBuf,
    },
    /// Read the parent's `cgroup.procs` and require it to be empty: a domain
    /// controller cannot be enabled while the parent holds processes.
    ///
    /// Failure here means **abort and stay**: this process remains in its
    /// current cgroup, keeps process-tree accounting, raises the
    /// `CgroupDelegationUnavailable` warning, and — crucially — **no
    /// controller is enabled**, because the enable steps follow this one and
    /// a failed verify must not fall through to them (see
    /// [`LeafMoveStep::on_failure`]).
    VerifyParentEmpty {
        /// The parent's `cgroup.procs` file.
        path: PathBuf,
    },
    /// Read `cgroup.subtree_control` in the parent and require the controllers to
    /// be absent, so a re-run does not fail on a controller already enabled.
    VerifyDelegationInPlace {
        /// The parent's `cgroup.subtree_control` file.
        path: PathBuf,
    },
    /// Enable one controller in the parent's `cgroup.subtree_control` (`+cpu`,
    /// `+memory`, `+pids`), in that order.
    EnableController {
        /// The controller name.
        controller: &'static str,
        /// The `cgroup.subtree_control` file.
        path: PathBuf,
    },
}

/// What the executor does when a leaf-move step fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureAction {
    /// Abort the move and stay where the process is.
    ///
    /// The process keeps its current cgroup and process-tree accounting, and the
    /// caller raises the `CgroupDelegationUnavailable` warning. No
    /// `cgroup.subtree_control` controller is enabled: a domain controller
    /// cannot be enabled while an ancestor holds processes, so continuing past a
    /// failed verify would be both wrong and futile.
    AbortAndStay,
}

impl LeafMoveStep {
    /// A short, stable label for logs and tests.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::CreateLeaf { .. } => "create_leaf",
            Self::MoveSelf { .. } => "move_self",
            Self::VerifyParentEmpty { .. } => "verify_parent_empty",
            Self::VerifyDelegationInPlace { .. } => "verify_delegation_in_place",
            Self::EnableController { .. } => "enable_controller",
        }
    }

    /// What the executor must do when this step fails.
    ///
    /// ADR-0099 decision 8: "if any step fails, Conspect stays where it is, uses
    /// process-tree accounting, and raises the warning." Exposed as data so the
    /// abort-and-stay semantics are testable rather than living only in a
    /// comment: a failed [`VerifyParentEmpty`](Self::VerifyParentEmpty) aborts
    /// the plan, and nothing after it — in particular no controller enable —
    /// runs.
    #[must_use]
    pub const fn on_failure(&self) -> FailureAction {
        FailureAction::AbortAndStay
    }
}

/// The ordered steps of ADR-0099 decision 8's boot-time move into `engine/`.
///
/// The order is the decision's, and it is load-bearing: create the leaf, move
/// this pid into it, confirm the service cgroup no longer holds a process, then
/// (and only then) enable `cpu memory pids` in the parent's
/// `cgroup.subtree_control`. `pid` is the caller's own pid (the later slice reads
/// it in `main`, before any thread or child exists, which is what removes the
/// migration race).
#[must_use]
pub fn plan_leaf_move(parent: &str, pid: u32) -> Vec<LeafMoveStep> {
    let leaf = merge_cgroup_path(parent, ENGINE_LEAF);
    let mut steps = vec![
        LeafMoveStep::CreateLeaf {
            path: PathBuf::from(&leaf),
        },
        LeafMoveStep::MoveSelf {
            pid,
            path: PathBuf::from(merge_cgroup_path(&leaf, CGROUP_PROCS)),
        },
        LeafMoveStep::VerifyParentEmpty {
            path: PathBuf::from(merge_cgroup_path(parent, CGROUP_PROCS)),
        },
        LeafMoveStep::VerifyDelegationInPlace {
            path: PathBuf::from(merge_cgroup_path(parent, CGROUP_SUBTREE_CONTROL)),
        },
    ];
    for controller in REQUIRED_CONTROLLERS {
        steps.push(LeafMoveStep::EnableController {
            controller,
            path: PathBuf::from(merge_cgroup_path(parent, CGROUP_SUBTREE_CONTROL)),
        });
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_stat_parses_the_reported_fields() {
        let body = "usage_usec 1234567\nuser_usec 1000000\nsystem_usec 234567\nnr_periods 42\nnr_throttled 3\nthrottled_usec 999\nunknown_key 7\n";
        let stat = parse_cpu_stat(body).expect("usage_usec present");
        assert_eq!(stat.usage_usec, 1_234_567);
        assert_eq!(stat.user_usec, Some(1_000_000));
        assert_eq!(stat.system_usec, Some(234_567));
        assert_eq!(stat.nr_periods, Some(42));
        assert_eq!(stat.nr_throttled, Some(3));
        assert_eq!(stat.throttled_usec, Some(999));
        assert!((stat.usage_seconds() - 1.234_567).abs() < 1e-9);
    }

    #[test]
    fn cpu_stat_without_usage_is_none() {
        assert!(parse_cpu_stat("user_usec 5\n").is_none());
        assert!(parse_cpu_stat("").is_none());
    }

    #[test]
    fn controller_set_reports_what_is_missing() {
        let present = ControllerSet::from_controllers("cpu memory pids\n");
        assert!(present.is_complete());
        assert!(present.contains("cpu") && present.contains("memory") && present.contains("pids"));

        let partial = ControllerSet::from_controllers("cpu pids\n");
        assert!(!partial.is_complete());
        assert_eq!(partial.missing_from_required().names(), vec!["memory"]);
        assert_eq!(partial.names(), vec!["cpu", "pids"]);
        assert_eq!(
            ControllerSet::EMPTY.missing_from_required().names().len(),
            3
        );
    }

    #[test]
    fn plan_leaf_move_is_ordered_create_move_verify_then_enable() {
        let steps = plan_leaf_move("/sys/fs/cgroup/conspect.service", 4242);
        let labels: Vec<&str> = steps.iter().map(LeafMoveStep::label).collect();
        assert_eq!(
            labels,
            vec![
                "create_leaf",
                "move_self",
                "verify_parent_empty",
                "verify_delegation_in_place",
                "enable_controller",
                "enable_controller",
                "enable_controller",
            ]
        );
        assert_eq!(
            steps.first(),
            Some(&LeafMoveStep::CreateLeaf {
                path: PathBuf::from("/sys/fs/cgroup/conspect.service/engine"),
            })
        );
        assert_eq!(
            steps.get(1),
            Some(&LeafMoveStep::MoveSelf {
                pid: 4242,
                path: PathBuf::from("/sys/fs/cgroup/conspect.service/engine/cgroup.procs"),
            })
        );
        let enabled: Vec<&str> = steps
            .iter()
            .filter_map(|step| match step {
                LeafMoveStep::EnableController { controller, .. } => Some(*controller),
                _ => None,
            })
            .collect();
        assert_eq!(enabled, REQUIRED_CONTROLLERS.to_vec());
    }

    #[test]
    fn merge_cgroup_path_normalises_slashes() {
        assert_eq!(merge_cgroup_path("/a/b/", "/c"), "/a/b/c");
        assert_eq!(merge_cgroup_path("/a/b", "c"), "/a/b/c");
    }
}
