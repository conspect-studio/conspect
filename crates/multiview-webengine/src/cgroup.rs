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
//!
//! **The plan is forward-only.** [`plan_leaf_move`] names exactly the steps an
//! executor runs, in order; the undo of each step is derived by
//! [`rollback_for`], which walks every step *before* the failure and asks
//! [`LeafMoveStep::undo`] for its inverse, in reverse order. Nothing in the
//! forward plan is a rollback step, so an executor that walks the plan never
//! confuses "a step to run" with "a step to run on failure".

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
/// with another fresh name rather than reported as "not delegated".
///
/// Cleanup removes **only what this call created**: a failed `create_dir` means
/// some other caller owns that directory, so it is left exactly where it was.
/// When this call did create its probe, every failure path after that makes a
/// best-effort `rmdir`, so a failed probe of ours leaves nothing behind.
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
    // Only ever remove what THIS call created. A `create_dir` that fails leaves
    // whatever already occupied the name alone; removing it would delete another
    // caller's (or a concurrent probe's) directory.
    let created = match fs.create_dir(&probe) {
        Ok(()) => true,
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::AlreadyExists => {
            // A stale probe holds the name: one retry with a fresh one. The
            // retry's failure is not ours to clean up either — we did not create
            // that directory.
            probe = probe_child_path(root, &probe_child_name());
            match fs.create_dir(&probe) {
                Ok(()) => true,
                Err(_) => {
                    return Ok(DelegationReport::NotDelegated(
                        NotDelegated::CannotCreateChild,
                    ))
                }
            }
        }
        Err(_) => {
            return Ok(DelegationReport::NotDelegated(
                NotDelegated::CannotCreateChild,
            ))
        }
    };
    if fs.remove_dir(&probe).is_err() {
        if created {
            // Best effort: a probe we created must not leave its directory
            // behind. Guarded by `created`, so this can only ever remove the
            // directory this call made.
            let _ = fs.remove_dir(&probe);
        }
        return Ok(DelegationReport::NotDelegated(
            NotDelegated::CannotRemoveChild,
        ));
    }
    Ok(DelegationReport::Delegated)
}

/// One step of the boot-time leaf move. **Nothing here executes**: the plan is
/// returned as data so `main` can run it before any thread or child exists, and
/// so a test can assert the order.
///
/// The forward plan ([`plan_leaf_move`]) contains only the six forward variants;
/// [`MoveSelfBack`](Self::MoveSelfBack), [`RemoveLeaf`](Self::RemoveLeaf) and
/// [`DisableController`](Self::DisableController) are produced **only** by
/// [`undo`](Self::undo) and appear only in a [`rollback_for`] sequence.
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
        /// The **parent's** `cgroup.procs` file: carried so
        /// [`undo`](Self::undo) can produce a [`MoveSelfBack`](Self::MoveSelfBack)
        /// without having to un-merge the leaf path back out.
        back_to: PathBuf,
    },
    /// Read the parent's `cgroup.procs` and require it to be empty: a domain
    /// controller cannot be enabled while the parent holds processes.
    ///
    /// A verify has **no undo** ([`undo`](Self::undo) is `None`): it changed
    /// nothing, so there is nothing to restore. A failure here is handled by
    /// [`rollback_for`], which reverses the real mutations that preceded it —
    /// [`MoveSelfBack`](Self::MoveSelfBack) then
    /// [`RemoveLeaf`](Self::RemoveLeaf) — and then the executor **aborts and
    /// stays**: this process remains in its current cgroup, keeps process-tree
    /// accounting, raises the `CgroupDelegationUnavailable` warning, and **no
    /// controller is enabled**, because the enable steps follow this one and a
    /// failed verify must not fall through to them (see
    /// [`LeafMoveStep::on_failure`]).
    VerifyParentEmpty {
        /// The parent's `cgroup.procs` file.
        path: PathBuf,
    },
    /// Read `cgroup.subtree_control` in the parent and require the controllers to
    /// be absent, so a re-run does not fail on a controller already enabled.
    ///
    /// Like every verify this has no undo: it reads.
    VerifyDelegationInPlace {
        /// The parent's `cgroup.subtree_control` file.
        path: PathBuf,
    },
    /// Enable one controller in the parent's `cgroup.subtree_control` (`+cpu`,
    /// `+memory`, `+pids`), in that order.
    ///
    /// Its undo is [`DisableController`](Self::DisableController), which is why a
    /// failure at a later enable can restore the earlier ones instead of leaving
    /// them on under a parent that no longer delegates.
    EnableController {
        /// The controller name.
        controller: &'static str,
        /// The `cgroup.subtree_control` file.
        path: PathBuf,
    },
    /// **Undo only.** Write our pid back into `parent`'s `cgroup.procs`, undoing
    /// a completed [`MoveSelf`](Self::MoveSelf).
    ///
    /// "Abort and stay" is not an undo: once the move has happened, aborting
    /// *without* this step would leave the process in a leaf under a parent that
    /// still holds processes — exactly the bad state the verify exists to catch.
    ///
    /// In a rollback it comes **before** [`RemoveLeaf`](Self::RemoveLeaf),
    /// because a cgroup that holds a process cannot be removed.
    MoveSelfBack {
        /// The pid to write back (the caller's own).
        pid: u32,
        /// The **parent's** `cgroup.procs` file.
        to: PathBuf,
    },
    /// **Undo only.** Remove the leaf directory created by a completed
    /// [`CreateLeaf`](Self::CreateLeaf).
    ///
    /// Only meaningful once nothing is left in the leaf (the
    /// [`MoveSelfBack`](Self::MoveSelfBack) precedes it), because a cgroup that
    /// holds a process cannot be removed.
    RemoveLeaf {
        /// The leaf's absolute path.
        path: PathBuf,
    },
    /// **Undo only.** Disable one controller in the parent's
    /// `cgroup.subtree_control` by writing `-cpu`, `-memory` or `-pids`,
    /// undoing a completed [`EnableController`](Self::EnableController).
    ///
    /// A rollback disables **every** enabled controller before it moves the
    /// process back: an enabled domain controller under a parent that holds this
    /// process is the state the forward order exists to avoid, so leaving one on
    /// would make the "abort and stay" position indistinguishable from a
    /// half-applied plan.
    DisableController {
        /// The controller name to remove from the enable list.
        controller: &'static str,
        /// The `cgroup.subtree_control` file.
        path: PathBuf,
    },
    /// **Undo only.** Run `then` only if `check` holds when the executor reaches
    /// this step; otherwise skip it.
    ///
    /// Produced by [`rollback_for`] for a failure *at*
    /// [`MoveSelf`](Self::MoveSelf), where the executor cannot tell whether the
    /// (atomic) kernel migration happened: the process is moved back only if
    /// `/proc/<pid>/cgroup` shows it in the leaf. A completed `MoveSelf` still
    /// gets the unconditional [`MoveSelfBack`](Self::MoveSelfBack).
    IfMember {
        /// The membership to verify first.
        check: VerifyMembership,
        /// The step to run when it holds.
        then: Box<Self>,
    },
}

/// A membership check the executor evaluates **at run time**, before a
/// conditional undo: "is `pid` currently in the cgroup at `path`?".
///
/// It exists because a failed write is not proof of an unmoved process. The
/// kernel's migration is atomic, but the executor only sees an error return and
/// cannot always know whether the write reached the kernel (a `write(2)`
/// interrupted after the migration, a lost reply). The undo for a failure *at*
/// [`MoveSelf`](LeafMoveStep::MoveSelf) is therefore conditional on the process
/// actually being in the leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyMembership {
    /// The process whose `/proc/<pid>/cgroup` is read.
    pub pid: u32,
    /// The cgroup directory (absolute, e.g. `/sys/fs/cgroup/svc/engine`) the
    /// process must be in for the check to hold.
    pub path: PathBuf,
}

impl VerifyMembership {
    /// Whether `proc_cgroup` (the body of `/proc/<pid>/cgroup`) places the
    /// process in [`path`](Self::path), where `mount` is the cgroup v2 mount
    /// point (`/sys/fs/cgroup`).
    ///
    /// Pure: the executor reads the file and asks. Only the unified (`0::`)
    /// line counts; anything unreadable, unparsable or outside `mount` is
    /// `false`, so a doubtful answer never triggers a write.
    #[must_use]
    pub fn holds(&self, proc_cgroup: &str, mount: &str) -> bool {
        let Some(relative) = self.path.to_str().and_then(|path| {
            path.strip_prefix(mount.trim_end_matches('/'))
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        }) else {
            return false;
        };
        let want = if relative.is_empty() { "/" } else { relative };
        proc_cgroup
            .lines()
            .filter_map(|line| line.strip_prefix("0::"))
            .any(|actual| actual.trim_end() == want)
    }
}

/// What the executor does when a leaf-move step fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureAction {
    /// Run the step's undo steps first, then abort and stay where the process
    /// was before the plan started.
    ///
    /// The process keeps its original cgroup and process-tree accounting, and
    /// the caller raises the `CgroupDelegationUnavailable` warning. No
    /// `cgroup.subtree_control` controller is enabled: a domain controller
    /// cannot be enabled while an ancestor holds processes, so continuing past a
    /// failed verify would be both wrong and futile.
    ///
    /// The undo steps are **not** implied by this value — they are the concrete
    /// [`LeafMoveStep`]s [`rollback_for`] derives from the failed plan, so the
    /// executor runs no hidden logic and a test can assert the exact sequence.
    AbortAfterRollback,
}

impl LeafMoveStep {
    /// A short, stable label for logs and tests.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::CreateLeaf { .. } => "create_leaf",
            Self::MoveSelf { .. } => "move_self",
            Self::MoveSelfBack { .. } => "move_self_back",
            Self::RemoveLeaf { .. } => "remove_leaf",
            Self::VerifyParentEmpty { .. } => "verify_parent_empty",
            Self::VerifyDelegationInPlace { .. } => "verify_delegation_in_place",
            Self::EnableController { .. } => "enable_controller",
            Self::DisableController { .. } => "disable_controller",
            Self::IfMember { .. } => "if_member",
        }
    }

    /// What the executor must do when this step fails.
    ///
    /// ADR-0099 decision 8: "if any step fails, Conspect stays where it is, uses
    /// process-tree accounting, and raises the warning." "Where it is" means
    /// where it was *before the plan*, so a step that has already changed
    /// something must be undone first: [`rollback_for`] reverses the completed
    /// mutations, and nothing after the failed step — in particular no further
    /// controller enable — runs. The undo steps themselves only ever abort
    /// (there is nothing further to undo), which is what makes the sequence
    /// terminate.
    #[must_use]
    pub const fn on_failure(&self) -> FailureAction {
        FailureAction::AbortAfterRollback
    }

    /// The step that undoes **this** step, or `None` when it changed nothing.
    ///
    /// * [`CreateLeaf`](Self::CreateLeaf) → [`RemoveLeaf`](Self::RemoveLeaf);
    /// * [`MoveSelf`](Self::MoveSelf) → [`MoveSelfBack`](Self::MoveSelfBack);
    /// * [`EnableController`](Self::EnableController) →
    ///   [`DisableController`](Self::DisableController);
    /// * both verifies → `None` (a read has nothing to restore).
    ///
    /// The three undo variants themselves have no undo: running one is already
    /// the rollback, so asking for its inverse would loop. [`rollback_for`] is
    /// the only consumer, and it reverses the undos of the steps before the
    /// failure to get an executable order.
    ///
    /// Not `const`: producing an undo clones the step's [`PathBuf`], which a
    /// constant evaluation cannot do.
    #[must_use]
    pub fn undo(&self) -> Option<Self> {
        match self {
            Self::CreateLeaf { path } => Some(Self::RemoveLeaf { path: path.clone() }),
            Self::MoveSelf { pid, back_to, .. } => Some(Self::MoveSelfBack {
                pid: *pid,
                to: back_to.clone(),
            }),
            Self::EnableController { controller, path } => Some(Self::DisableController {
                controller,
                path: path.clone(),
            }),
            // A verify changed nothing, and the three undo steps have no undo.
            Self::VerifyParentEmpty { .. }
            | Self::VerifyDelegationInPlace { .. }
            | Self::MoveSelfBack { .. }
            | Self::RemoveLeaf { .. }
            | Self::DisableController { .. }
            | Self::IfMember { .. } => None,
        }
    }

    /// The undo to run when this step **failed** and the executor cannot tell
    /// whether it took effect; `None` when a failure is known to have changed
    /// nothing.
    ///
    /// Only [`MoveSelf`](Self::MoveSelf) qualifies: its kernel migration is
    /// atomic but a failed write may still have reached the kernel, so the undo
    /// is [`MoveSelfBack`](Self::MoveSelfBack) guarded by an
    /// [`IfMember`](Self::IfMember) check on the leaf.
    #[must_use]
    pub fn undo_if_applied(&self) -> Option<Self> {
        match self {
            Self::MoveSelf { pid, path, back_to } => Some(Self::IfMember {
                check: VerifyMembership {
                    pid: *pid,
                    path: path.parent().map_or_else(PathBuf::new, PathBuf::from),
                },
                then: Box::new(Self::MoveSelfBack {
                    pid: *pid,
                    to: back_to.clone(),
                }),
            }),
            _ => None,
        }
    }
}

/// The undo sequence that runs when the step at `index` of `plan` fails, in the
/// order it must run.
///
/// Walk every step **strictly before** `index` — a step that has not run yet has
/// nothing to undo — ask each for its [`undo`](LeafMoveStep::undo), drop the
/// verifies (which return `None`), and reverse the result. The result is exposed
/// as data rather than prose so the sequence is testable at **every** step, and
/// it is total: `index == plan.len()` is a plan that ran to the end, so it yields
/// the undo of the whole plan; an `index` **past** the end is not a position in
/// any plan and yields an empty vector; an empty plan yields an empty vector.
///
/// The order is load-bearing in both directions:
///
/// * Reversing puts the *latest* mutation's undo first, so a failure at
///   `enable_controller memory` disables `cpu` first, and only then moves the
///   process back and removes the leaf. Disabling an enabled domain controller
///   before the move restores the no-internal-process precondition the forward
///   plan depended on.
/// * [`MoveSelfBack`](LeafMoveStep::MoveSelfBack) therefore precedes
///   [`RemoveLeaf`](LeafMoveStep::RemoveLeaf): a cgroup that still holds a
///   process cannot be removed.
///
/// A failure *at* a [`MoveSelf`](LeafMoveStep::MoveSelf) additionally yields a
/// conditional [`IfMember`](LeafMoveStep::IfMember) move back, first: the
/// executor cannot know whether the write reached the kernel, so it checks
/// `/proc/<pid>/cgroup` and moves back only if the process is in the leaf.
///
/// A rollback never contains an
/// [`EnableController`](LeafMoveStep::EnableController): nothing in the undo
/// direction turns a controller back on.
///
/// ```ignore
/// // The executor loop, which runs nothing hidden:
/// for (index, step) in plan.iter().enumerate() {
///     if let Err(_) = run(step) {
///         for undo in rollback_for(&plan, index) {
///             let _ = run(&undo); // best effort; the sequence only aborts
///         }
///         return Err("not delegated");
///     }
/// }
/// ```
#[must_use]
pub fn rollback_for(plan: &[LeafMoveStep], index: usize) -> Vec<LeafMoveStep> {
    let mut undo: Vec<LeafMoveStep> = plan
        .get(..index)
        .unwrap_or_default()
        .iter()
        .filter_map(LeafMoveStep::undo)
        .collect();
    // The failing step itself may or may not have taken effect: its conditional
    // undo (if any) is the latest mutation's, so after the reversal it runs
    // first.
    undo.extend(plan.get(index).and_then(LeafMoveStep::undo_if_applied));
    undo.reverse();
    undo
}

/// The **forward** ordered steps of ADR-0099 decision 8's boot-time move into
/// `engine/`.
///
/// The order is the decision's, and it is load-bearing: create the leaf, move
/// this pid into it, confirm the service cgroup no longer holds a process,
/// confirm nothing is already enabled, then (and only then) enable
/// `cpu memory pids` in the parent's `cgroup.subtree_control`.
///
/// This is the whole plan. There are no rollback steps in it: when a step fails,
/// [`rollback_for`] derives the undo of every step that already ran, in reverse
/// order. `pid` is the caller's own pid: the later slice reads it in `main`,
/// **before any thread or child exists**, which is what removes the migration
/// race and what makes both the move and its rollback safe.
#[must_use]
pub fn plan_leaf_move(parent: &str, pid: u32) -> Vec<LeafMoveStep> {
    let leaf = merge_cgroup_path(parent, ENGINE_LEAF);
    let parent_procs = merge_cgroup_path(parent, CGROUP_PROCS);
    let subtree_control = merge_cgroup_path(parent, CGROUP_SUBTREE_CONTROL);
    let mut steps = vec![
        LeafMoveStep::CreateLeaf {
            path: PathBuf::from(&leaf),
        },
        LeafMoveStep::MoveSelf {
            pid,
            path: PathBuf::from(merge_cgroup_path(&leaf, CGROUP_PROCS)),
            back_to: PathBuf::from(&parent_procs),
        },
        LeafMoveStep::VerifyParentEmpty {
            path: PathBuf::from(&parent_procs),
        },
        LeafMoveStep::VerifyDelegationInPlace {
            path: PathBuf::from(&subtree_control),
        },
    ];
    for controller in REQUIRED_CONTROLLERS {
        steps.push(LeafMoveStep::EnableController {
            controller,
            path: PathBuf::from(&subtree_control),
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
                back_to: PathBuf::from("/sys/fs/cgroup/conspect.service/cgroup.procs"),
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
