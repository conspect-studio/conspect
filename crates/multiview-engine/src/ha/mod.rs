//! The M9 **high-availability** model: an active/standby + N+1 instance model
//! with a heartbeat health-check state machine, an automatic output-failover
//! policy, and a state-replication model (FEATURES.md M9 "System redundancy:
//! hot-standby / N+1 / failover"; resilience-and-av §2).
//!
//! Three pure layers:
//!
//! * **Heartbeat / health-check** — peers exchange [`Heartbeat`]s; each node runs
//!   a pure [`HaStateMachine`] over an injected
//!   [`MediaTime`]. When the active's heartbeats
//!   miss a configured threshold, the standby **promotes** (begins driving
//!   output) — make-before-break, so output never falters at the model level.
//! * **Failover policy** — [`FailoverPolicy`] + [`Cluster`] decide *which* node
//!   drives output. The election is a pure, deterministic function of cluster
//!   health (liveness, [`Priority`], [`NodeId`]) so every survivor computes the
//!   **same** winner and cannot disagree — the root of the no-split-brain
//!   guarantee. An [`Epoch`] (a monotonic promotion generation) fences zombie
//!   actives: a higher epoch is authoritative, a lower epoch is ignored.
//! * **State replication** — [`repl`] is the serializable engine-state snapshot
//!   plus the contiguous deltas the active replicates to a standby so a promotion
//!   resumes from the right layout/source bindings.
//!
//! ## Isolation (invariants #1 + #10) — HA never compromises the output clock
//!
//! This is load-bearing. On the **active** node the HA machinery is a pure value
//! machine over an injected [`MediaTime`]: it
//! samples peer heartbeats and *returns* a [`FailoverDecision`] for the control
//! plane to act on. It owns no clock, performs no I/O, never `.await`s and never
//! sends on a channel a peer can fill — exactly like the rest of the engine's
//! control surfaces. The active node's `out_pts = f(tick)` is therefore untouched
//! by anything HA does: a flapping peer, a partitioned network, or a thrashing
//! election can neither stall nor speed up the tick loop (invariant #1), and the
//! cluster path cannot back-pressure the engine (invariant #10).
//!
//! **Failover preserves output continuity (model level).** The promotion is
//! make-before-break: the moment a standby's [`HaStateMachine`] decides the
//! active is dead it *immediately* begins driving output ([`drives_output`] flips
//! to `true` on the same tick it promotes), rather than first tearing down and
//! then rebuilding. The election guarantees **at most one** node promotes and —
//! whenever any node is alive — **exactly one** node drives output, so there is no
//! window in which nobody is the program source.
//!
//! All HA arithmetic is in **integer nanoseconds** (deadlines) and integer
//! generations (epochs) — never float, consistent with invariant #3.
//!
//! [`drives_output`]: HaStateMachine::drives_output
//!
//! ## Transport is gated and compile-only
//!
//! The actual peer-to-peer heartbeat sockets and the replication wire I/O live in
//! the `transport` submodule behind the off-by-default **`cluster`** feature,
//! compiled only when that feature is enabled and compile-verified only here
//! (this environment has no live multi-node cluster). Their correctness rests on
//! the pure, fully-tested model in this module and in [`repl`].
use crate::error::{Error, Result};
use multiview_core::time::MediaTime;

pub mod repl;

#[cfg(feature = "cluster")]
pub mod transport;

/// A stable cluster instance identifier.
///
/// Used both to address peers and, as the final tie-break in the failover
/// election, to make the elected promoter a deterministic function of cluster
/// health (lower id wins on an exact priority tie).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// Construct a node id.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The raw id value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A node's failover priority — **higher promotes first**.
///
/// The primary key of the election: among live standbys, the highest-priority one
/// is elected. Equal priorities are broken by lowest [`NodeId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(u32);

impl Priority {
    /// Construct a priority (higher = promotes first).
    #[must_use]
    pub const fn new(priority: u32) -> Self {
        Self(priority)
    }

    /// The raw priority value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A promotion generation counter — the **anti-split-brain fence**.
///
/// Every promotion bumps the epoch. When two nodes both believe they are active
/// (e.g. a healed partition), the one carrying the **higher** epoch is
/// authoritative; the lower-epoch node is a zombie and yields. Equal epochs are
/// broken by the same priority/id rule as the election, so the outcome is total
/// and deterministic.
pub type Epoch = u64;

/// Whether a node is the program source ([`Active`](NodeRole::Active)) or a hot
/// spare ([`Standby`](NodeRole::Standby)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeRole {
    /// The node currently driving program output.
    Active,
    /// A hot spare watching the active, ready to promote on its loss.
    Standby,
}

/// A cluster member's identity: its [`NodeId`] and failover [`Priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HaNode {
    id: NodeId,
    priority: Priority,
}

impl HaNode {
    /// Construct a member with the given id and failover priority.
    #[must_use]
    pub const fn new(id: NodeId, priority: Priority) -> Self {
        Self { id, priority }
    }

    /// This member's id.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// This member's failover priority.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        self.priority
    }
}

/// One heartbeat message a node broadcasts to its peers.
///
/// Carries the sender's self-declared [`NodeRole`], its current [`Epoch`], whether
/// it believes it is driving output, and the [`MediaTime`]
/// the heartbeat was stamped at (used to age it against the miss deadline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    /// The node that sent this heartbeat.
    pub from: NodeId,
    /// The sender's failover priority (carried so an equal-epoch tie can be broken
    /// by the same priority/id rule as the election — see [`HaStateMachine::observe_heartbeat`]).
    pub priority: Priority,
    /// The role the sender claims.
    pub role: NodeRole,
    /// The sender's promotion generation (the split-brain fence).
    pub epoch: Epoch,
    /// Whether the sender believes it is driving program output.
    pub drives_output: bool,
    /// The instant (on the shared media timeline) the heartbeat was stamped.
    pub sent_at: MediaTime,
}

/// Tuning for the heartbeat health check.
///
/// A node is considered dead once `miss_threshold` whole heartbeat intervals have
/// elapsed since its last heartbeat. The deadline is computed exactly from the
/// integer `interval_ns` and `miss_threshold` (never float), consistent with
/// invariant #3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatConfig {
    interval_ns: i64,
    miss_threshold: u32,
}

impl HeartbeatConfig {
    /// Construct a config from the heartbeat `interval_ns` (nanoseconds, must be
    /// positive) and the `miss_threshold` (consecutive missed intervals before a
    /// peer is declared dead, must be at least 1).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidControlLoop`] if `interval_ns <= 0` or
    /// `miss_threshold == 0` — a non-positive interval or zero threshold would
    /// make liveness undecidable.
    pub fn new(interval_ns: i64, miss_threshold: u32) -> Result<Self> {
        if interval_ns <= 0 {
            return Err(Error::InvalidControlLoop(format!(
                "heartbeat interval must be positive, got {interval_ns} ns"
            )));
        }
        if miss_threshold == 0 {
            return Err(Error::InvalidControlLoop(
                "heartbeat miss threshold must be at least 1".to_owned(),
            ));
        }
        Ok(Self {
            interval_ns,
            miss_threshold,
        })
    }

    /// The heartbeat interval in nanoseconds.
    #[must_use]
    pub const fn interval_ns(&self) -> i64 {
        self.interval_ns
    }

    /// The consecutive-miss threshold before a peer is declared dead.
    #[must_use]
    pub const fn miss_threshold(&self) -> u32 {
        self.miss_threshold
    }

    /// The dead-deadline window: `interval_ns * miss_threshold`, in nanoseconds
    /// (saturating — never overflows or panics).
    #[must_use]
    pub fn dead_after_ns(&self) -> i64 {
        self.interval_ns
            .saturating_mul(i64::from(self.miss_threshold))
    }
}

/// Tracks the liveness of one peer against the heartbeat deadline.
///
/// A pure clock-free record: [`PeerHealth::record`] stamps the last heartbeat,
/// [`PeerHealth::is_alive`] answers liveness for an injected `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerHealth {
    id: NodeId,
    config: HeartbeatConfig,
    last_seen: Option<MediaTime>,
}

impl PeerHealth {
    /// Construct a tracker for `id` with no heartbeat recorded yet.
    #[must_use]
    pub const fn new(id: NodeId, config: HeartbeatConfig) -> Self {
        Self {
            id,
            config,
            last_seen: None,
        }
    }

    /// The peer this tracks.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// The last instant a heartbeat was recorded, if any.
    #[must_use]
    pub const fn last_seen(&self) -> Option<MediaTime> {
        self.last_seen
    }

    /// Record a heartbeat received/stamped at `at`. Monotonic: a stale heartbeat
    /// (older than one already recorded) does not move the deadline backwards.
    pub fn record(&mut self, at: MediaTime) {
        match self.last_seen {
            Some(prev) if prev.as_nanos() >= at.as_nanos() => {}
            _ => self.last_seen = Some(at),
        }
    }

    /// Whether the peer is alive at `now`: a heartbeat was seen and `now` is
    /// within `dead_after_ns` of it. A peer never heard from is **not** alive.
    #[must_use]
    pub fn is_alive(&self, now: MediaTime) -> bool {
        match self.last_seen {
            None => false,
            Some(last) => {
                let age = now.as_nanos().saturating_sub(last.as_nanos());
                age < self.config.dead_after_ns()
            }
        }
    }
}

/// The per-node high-availability state machine.
///
/// Each node runs one of these. It tracks the node's own [`NodeRole`] and
/// [`Epoch`], the liveness of the active it watches, and decides — purely, over an
/// injected [`MediaTime`] — when to **promote**
/// (standby → active, begin driving output) and when to **yield** (active →
/// standby, on hearing a higher-authority active). It owns no clock and performs
/// no I/O.
///
/// A standby promotes when the active it watches has been silent past the
/// heartbeat deadline (or, on cold start, once the node's own start deadline
/// elapses with no active ever heard). Promotion is **make-before-break**:
/// [`drives_output`](HaStateMachine::drives_output) flips to `true` on the same
/// [`tick`](HaStateMachine::tick) that promotes, with no intervening gap.
#[derive(Debug, Clone)]
pub struct HaStateMachine {
    node: HaNode,
    role: NodeRole,
    epoch: Epoch,
    config: HeartbeatConfig,
    /// Liveness of the active peer this node watches (if it has heard one).
    active_health: Option<PeerHealth>,
    /// The reference instant from which the cold-start promotion deadline runs.
    ///
    /// Measured from the shared cluster timeline's origin ([`MediaTime::ZERO`]) so
    /// that a node which never hears *any* active promotes once one dead-window of
    /// cluster time has passed — the N+1 cold-start case where the active was
    /// already gone before this node observed. (Once a real active *is* heard,
    /// that peer's liveness drives promotion instead and this anchor is unused.)
    start_anchor: MediaTime,
}

impl HaStateMachine {
    /// Construct a node in `role` with the given heartbeat `config`.
    #[must_use]
    pub fn new(node: HaNode, role: NodeRole, config: HeartbeatConfig) -> Self {
        Self {
            node,
            role,
            epoch: 0,
            config,
            active_health: None,
            start_anchor: MediaTime::ZERO,
        }
    }

    /// This node's identity.
    #[must_use]
    pub const fn node(&self) -> HaNode {
        self.node
    }

    /// This node's current role.
    #[must_use]
    pub const fn role(&self) -> NodeRole {
        self.role
    }

    /// This node's current promotion generation.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// Whether this node currently drives program output. True exactly when the
    /// node is [`Active`](NodeRole::Active).
    #[must_use]
    pub const fn drives_output(&self) -> bool {
        matches!(self.role, NodeRole::Active)
    }

    /// Whether this node currently has at least one promotion (epoch > 0). Useful
    /// for diagnostics / tests.
    #[must_use]
    pub const fn has_promoted(&self) -> bool {
        self.epoch > 0
    }

    /// Observe a peer [`Heartbeat`].
    ///
    /// * From an **active** peer: refresh its liveness, and — if this node is
    ///   itself active but the peer carries higher authority (a higher [`Epoch`],
    ///   or an equal epoch the peer wins on the priority/id tie-break) — **yield**
    ///   (active → standby). This is the anti-split-brain fence: a higher-epoch
    ///   active always wins; a stale lower-epoch "zombie" active is ignored.
    /// * Heartbeats from this node itself are ignored.
    ///
    /// Returns `true` if observing this heartbeat changed this node's role.
    pub fn observe_heartbeat(&mut self, hb: Heartbeat) -> bool {
        if hb.from == self.node.id() {
            return false;
        }

        if !matches!(hb.role, NodeRole::Active) {
            return false;
        }

        // Refresh the active peer's liveness.
        let health = self
            .active_health
            .get_or_insert_with(|| PeerHealth::new(hb.from, self.config));
        // If we are now tracking a different active id, retarget the tracker.
        if health.id() != hb.from {
            *health = PeerHealth::new(hb.from, self.config);
        }
        health.record(hb.sent_at);

        // Anti-split-brain: if we are active and this peer is a higher authority,
        // yield to it.
        if matches!(self.role, NodeRole::Active)
            && self.peer_outranks_us(hb.epoch, hb.priority, hb.from)
        {
            self.role = NodeRole::Standby;
            return true;
        }
        false
    }

    /// Whether a peer active carrying `peer_epoch` from `(peer_priority,
    /// peer_id)` outranks this node: a higher epoch always wins; an equal epoch is
    /// broken by **higher [`Priority`] then lower [`NodeId`]** (the same total
    /// order the election uses, so the two nodes agree on who yields). A lower
    /// epoch never outranks us (a stale "zombie" active is ignored).
    fn peer_outranks_us(
        &self,
        peer_epoch: Epoch,
        peer_priority: Priority,
        peer_id: NodeId,
    ) -> bool {
        match peer_epoch.cmp(&self.epoch) {
            core::cmp::Ordering::Greater => true,
            core::cmp::Ordering::Less => false,
            // Equal epoch: the peer wins the tie iff it would win the election —
            // higher priority, then lower id. Compare `(priority, Reverse(id))`.
            core::cmp::Ordering::Equal => match peer_priority.cmp(&self.node.priority()) {
                core::cmp::Ordering::Greater => true,
                core::cmp::Ordering::Less => false,
                core::cmp::Ordering::Equal => peer_id < self.node.id(),
            },
        }
    }

    /// Advance the machine to `now`, returning `true` if this tick **promoted**
    /// the node (standby → active).
    ///
    /// A standby promotes when the active it watches is dead at `now`, or — on
    /// cold start with no active ever heard — once its own start deadline (one
    /// dead-window after the first observed instant) elapses. Promotion bumps the
    /// [`Epoch`] exactly once and flips
    /// [`drives_output`](HaStateMachine::drives_output) to `true` on the same tick
    /// (make-before-break). An already-active node never re-promotes here.
    pub fn tick(&mut self, now: MediaTime) -> bool {
        if matches!(self.role, NodeRole::Active) {
            return false;
        }

        let should_promote = match self.active_health {
            Some(ref health) => !health.is_alive(now),
            None => self.cold_start_deadline_elapsed(now),
        };

        if should_promote {
            self.role = NodeRole::Active;
            self.epoch = self.epoch.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// Whether the cold-start promotion deadline has elapsed (no active ever
    /// heard, one dead-window past the cluster-timeline origin).
    fn cold_start_deadline_elapsed(&self, now: MediaTime) -> bool {
        let age = now.as_nanos().saturating_sub(self.start_anchor.as_nanos());
        age >= self.config.dead_after_ns()
    }
}

/// The decision the failover policy returns for a node, given cluster health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailoverDecision {
    /// Do nothing: a healthy active is driving output, or this node is not the
    /// elected promoter.
    Hold,
    /// This node is the elected promoter: it should promote and begin driving
    /// output (make-before-break).
    Promote,
}

/// The failover **policy**: the deterministic rule mapping cluster health to the
/// single node that should drive output.
///
/// The default policy: while the active is alive, hold (the active keeps driving).
/// When the active is dead, elect the **highest-[`Priority`] live standby**,
/// breaking ties by **lowest [`NodeId`]**. Because the rule is a total order over
/// `(alive, priority, id)`, every survivor computes the *same* winner — the basis
/// of the no-split-brain guarantee (no two nodes ever elect different promoters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct FailoverPolicy;

impl FailoverPolicy {
    /// Construct the default policy.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// A model of the whole cluster from one observer's vantage: the original active,
/// the standbys, their health, and the [`FailoverPolicy`].
///
/// Pure and clock-free: feed it heartbeats / health stamps and ask
/// [`Cluster::evaluate`] (or [`Cluster::elected_promoter`]) at an injected `now`.
/// The election is deterministic, so this same computation on every node yields
/// the same promoter — the structural reason there is no split brain.
#[derive(Debug, Clone)]
pub struct Cluster {
    active: HaNode,
    /// Health trackers for every member (active + standbys), keyed by node.
    members: Vec<(HaNode, PeerHealth)>,
    policy: FailoverPolicy,
    config: HeartbeatConfig,
}

impl Cluster {
    /// Construct a cluster of one `active` plus `standbys`, using `policy` and
    /// heartbeat `config`. No heartbeats are recorded yet (every member starts
    /// not-alive until a heartbeat is recorded).
    #[must_use]
    pub fn new(
        active: HaNode,
        standbys: Vec<HaNode>,
        policy: FailoverPolicy,
        config: HeartbeatConfig,
    ) -> Self {
        let mut members = Vec::with_capacity(standbys.len().saturating_add(1));
        members.push((active, PeerHealth::new(active.id(), config)));
        for s in standbys {
            members.push((s, PeerHealth::new(s.id(), config)));
        }
        Self {
            active,
            members,
            policy,
            config,
        }
    }

    /// The configured (original) active member.
    #[must_use]
    pub const fn active(&self) -> HaNode {
        self.active
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> FailoverPolicy {
        self.policy
    }

    /// Record a heartbeat from `id` stamped at `at`. Unknown ids are ignored.
    pub fn record_heartbeat(&mut self, id: NodeId, at: MediaTime) {
        if let Some((_, health)) = self.members.iter_mut().find(|(n, _)| n.id() == id) {
            health.record(at);
        }
    }

    /// Observe a full [`Heartbeat`]: records the sender's liveness. (The role/
    /// epoch carried are consumed by each node's own [`HaStateMachine`]; the
    /// cluster view only needs liveness for the election.)
    pub fn observe(&mut self, hb: Heartbeat) {
        self.record_heartbeat(hb.from, hb.sent_at);
    }

    /// Whether the configured active is alive at `now`.
    #[must_use]
    pub fn active_alive(&self, now: MediaTime) -> bool {
        self.members
            .iter()
            .find(|(n, _)| n.id() == self.active.id())
            .is_some_and(|(_, h)| h.is_alive(now))
    }

    /// The single node the policy elects to drive output at `now`, or `None` if
    /// no node is alive.
    ///
    /// If the active is alive it is the driver (its own id). Otherwise the
    /// election picks the highest-priority live standby, tie-broken by lowest id.
    /// The result is the same on every node, so survivors never disagree.
    #[must_use]
    pub fn elected_promoter(&self, now: MediaTime) -> Option<NodeId> {
        if self.active_alive(now) {
            return Some(self.active.id());
        }
        self.live_standbys(now)
            .max_by(|a, b| {
                a.priority()
                    .cmp(&b.priority())
                    // Higher priority first; on a tie, *lower* id wins, so invert
                    // the id comparison (a smaller id should compare "greater").
                    .then_with(|| b.id().cmp(&a.id()))
            })
            .map(|n| n.id())
    }

    /// The failover decision for `node_id` at `now`.
    ///
    /// [`FailoverDecision::Promote`] iff `node_id` is the single elected promoter
    /// **and** is not the already-active node; otherwise [`FailoverDecision::Hold`].
    #[must_use]
    pub fn evaluate(&self, node_id: NodeId, now: MediaTime) -> FailoverDecision {
        match self.elected_promoter(now) {
            Some(elected) if elected == node_id && elected != self.active.id() => {
                FailoverDecision::Promote
            }
            _ => FailoverDecision::Hold,
        }
    }

    /// The live standby members at `now` (excludes the configured active).
    fn live_standbys(&self, now: MediaTime) -> impl Iterator<Item = HaNode> + '_ {
        let active_id = self.active.id();
        self.members
            .iter()
            .filter(move |(n, h)| n.id() != active_id && h.is_alive(now))
            .map(|(n, _)| *n)
    }

    /// The heartbeat configuration in force.
    #[must_use]
    pub const fn config(&self) -> HeartbeatConfig {
        self.config
    }
}
