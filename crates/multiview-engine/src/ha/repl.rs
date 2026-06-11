//! The M9 **state-replication model**: the serializable engine-state snapshot the
//! active node replicates to a standby, the contiguous deltas applied between
//! snapshots, and the standby-side [`ReplicaApplier`] that reconstructs the
//! replicated state.
//!
//! The active periodically ships a full [`EngineSnapshot`] (the
//! failover-relevant program state — the active layout, the per-tile source
//! bindings, the current promotion [`epoch`](EngineSnapshot::epoch)) and, between
//! snapshots, a stream of small [`ReplicationDelta`]s. A standby keeps an up-to-
//! date replica so a promotion resumes from the right state with no warm-up gap
//! (make-before-break, model level).
//!
//! ## Contiguity is enforced — no silent divergence
//!
//! Every delta names the [`SnapshotVersion`] it applies *from* and advances *to*.
//! The [`ReplicaApplier`] applies a delta **only** when its `from` matches the
//! replica's current version and `to` is strictly greater; a gap (a lost delta)
//! or a non-monotonic version is **rejected** ([`ApplyError`]) and leaves the
//! replica untouched, so the standby never quietly diverges from the active — on
//! a gap it requests a fresh snapshot instead. The version is monotonic, mirroring
//! the snapshot/delta discipline that keeps replication consistent.
//!
//! ## Isolation
//!
//! This is a pure, serde-friendly value model. It owns no clock, performs no I/O,
//! never `.await`s. The wire transport that ships these snapshots/deltas between
//! nodes is behind the off-by-default `cluster` feature (the gated `transport`
//! submodule) and is compile-only here.
use serde::{Deserialize, Serialize};

/// A monotonic engine-state version: increments on every state change the active
/// replicates. Used to enforce contiguous delta application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SnapshotVersion(u64);

impl SnapshotVersion {
    /// Construct a version.
    #[must_use]
    pub const fn new(version: u64) -> Self {
        Self(version)
    }

    /// The raw version value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next version (saturating — never wraps).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// One tile's source binding within a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileBinding {
    /// Zero-based tile index.
    pub tile: u32,
    /// The source id bound to the tile (`None` = the tile is unbound / a slate).
    pub source: Option<String>,
}

/// The serializable engine-state snapshot replicated to a standby.
///
/// Captures the failover-relevant program state: the monotonic
/// [`version`](EngineSnapshot::version), the active layout name, the current HA
/// promotion [`epoch`](EngineSnapshot::epoch) (so a standby resumes the
/// split-brain fence at the right generation), and the per-tile source bindings.
/// Round-trips losslessly through serde (JSON for cross-node wire, also TOML for
/// snapshot dumps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSnapshot {
    /// The monotonic state version this snapshot represents.
    pub version: SnapshotVersion,
    /// The name of the active layout/template.
    pub active_layout: String,
    /// The HA promotion generation at the time of the snapshot.
    pub epoch: u64,
    /// The per-tile source bindings, in tile order.
    pub tiles: Vec<TileBinding>,
}

impl EngineSnapshot {
    /// Apply an *already-validated* [`ReplicationDelta`] to this snapshot in
    /// place, advancing its version to the delta's `to`. The caller
    /// ([`ReplicaApplier`]) is responsible for contiguity validation; this only
    /// performs the mutation. Consumes the delta (moving its owned fields in, so
    /// no clone is needed).
    fn apply_validated(&mut self, delta: ReplicationDelta) {
        match delta {
            ReplicationDelta::LayoutSwap { to, layout, .. } => {
                self.active_layout = layout;
                self.version = to;
            }
            ReplicationDelta::SourceRebound {
                to, tile, source, ..
            } => {
                if let Some(binding) = self.tiles.iter_mut().find(|b| b.tile == tile) {
                    binding.source = source;
                } else {
                    self.tiles.push(TileBinding { tile, source });
                }
                self.version = to;
            }
        }
    }
}

/// One incremental change replicated between snapshots.
///
/// Every delta names the [`SnapshotVersion`] it applies `from` and advances `to`,
/// so the [`ReplicaApplier`] can enforce contiguity. Serialized **tagged**
/// (`#[serde(tag = "kind")]`) per repo conventions; never `untagged`.
/// `#[non_exhaustive]` for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReplicationDelta {
    /// Swap the active layout.
    LayoutSwap {
        /// The version this delta applies from (must match the replica's current
        /// version).
        from: SnapshotVersion,
        /// The version the replica advances to (must be strictly greater).
        to: SnapshotVersion,
        /// The new active layout name.
        layout: String,
    },
    /// Rebind a tile's source.
    SourceRebound {
        /// The version this delta applies from.
        from: SnapshotVersion,
        /// The version the replica advances to.
        to: SnapshotVersion,
        /// Zero-based tile index.
        tile: u32,
        /// The new source id (`None` clears the binding).
        source: Option<String>,
    },
}

impl ReplicationDelta {
    /// The version this delta applies from.
    #[must_use]
    pub const fn from(&self) -> SnapshotVersion {
        match self {
            Self::LayoutSwap { from, .. } | Self::SourceRebound { from, .. } => *from,
        }
    }

    /// The version this delta advances to.
    #[must_use]
    pub const fn to(&self) -> SnapshotVersion {
        match self {
            Self::LayoutSwap { to, .. } | Self::SourceRebound { to, .. } => *to,
        }
    }
}

/// Why a snapshot install or delta apply was rejected.
///
/// `#[non_exhaustive]`: downstream `match` statements must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyError {
    /// A delta arrived before any baseline snapshot was installed.
    NoBaseline,
    /// A delta's `from` version does not match the replica's current version (a
    /// lost/reordered delta). The replica must request a fresh snapshot.
    VersionGap {
        /// The replica's current version (what the delta should have applied from).
        expected: SnapshotVersion,
        /// The version the delta claimed to apply from.
        got: SnapshotVersion,
    },
    /// A delta's `to` (or an installed snapshot's version) does not strictly
    /// advance the replica version — replication versions are monotonic.
    NonMonotonic {
        /// The replica's current version.
        current: SnapshotVersion,
        /// The non-advancing version that was rejected.
        rejected: SnapshotVersion,
    },
}

impl core::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoBaseline => {
                write!(f, "replication delta arrived before any baseline snapshot")
            }
            Self::VersionGap { expected, got } => write!(
                f,
                "replication delta version gap: expected from {}, got {}",
                expected.get(),
                got.get()
            ),
            Self::NonMonotonic { current, rejected } => write!(
                f,
                "non-monotonic replication version: current {}, rejected {}",
                current.get(),
                rejected.get()
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

/// The standby-side replica: installs baseline snapshots and applies contiguous
/// deltas to reconstruct the active's replicated state.
///
/// Pure and clock-free. [`install_snapshot`](ReplicaApplier::install_snapshot)
/// seeds or fast-forwards the replica (a newer snapshot replaces the current one;
/// an older one is rejected); [`apply_delta`](ReplicaApplier::apply_delta) applies
/// a single contiguous delta or rejects it without mutating the replica.
#[derive(Debug, Clone, Default)]
pub struct ReplicaApplier {
    current: Option<EngineSnapshot>,
}

impl ReplicaApplier {
    /// Construct an empty replica (no baseline yet).
    #[must_use]
    pub fn new() -> Self {
        Self { current: None }
    }

    /// The current replicated snapshot, if a baseline has been installed.
    #[must_use]
    pub const fn current(&self) -> Option<&EngineSnapshot> {
        self.current.as_ref()
    }

    /// The current replica version, if any.
    #[must_use]
    pub fn version(&self) -> Option<SnapshotVersion> {
        self.current.as_ref().map(|s| s.version)
    }

    /// Install a baseline `snapshot`.
    ///
    /// The first snapshot seeds the replica. A later snapshot whose version is
    /// strictly greater than the current one fast-forwards (e.g. after a gap, the
    /// active resends a full snapshot). A snapshot whose version does not advance
    /// the replica is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ApplyError::NonMonotonic`] if `snapshot.version` is not strictly
    /// greater than the current replica version.
    pub fn install_snapshot(&mut self, snapshot: EngineSnapshot) -> Result<(), ApplyError> {
        if let Some(ref current) = self.current {
            if snapshot.version <= current.version {
                return Err(ApplyError::NonMonotonic {
                    current: current.version,
                    rejected: snapshot.version,
                });
            }
        }
        self.current = Some(snapshot);
        Ok(())
    }

    /// Apply a single contiguous `delta`.
    ///
    /// The delta is applied only when there is a baseline, its `from` matches the
    /// replica's current version, and its `to` strictly advances it. Otherwise the
    /// replica is left **untouched** and the reason is returned — the standby then
    /// requests a fresh snapshot rather than diverging.
    ///
    /// # Errors
    ///
    /// * [`ApplyError::NoBaseline`] — no snapshot installed yet.
    /// * [`ApplyError::VersionGap`] — `delta.from` does not match the current
    ///   version (a lost/reordered delta).
    /// * [`ApplyError::NonMonotonic`] — `delta.to` does not strictly advance the
    ///   version.
    pub fn apply_delta(&mut self, delta: ReplicationDelta) -> Result<(), ApplyError> {
        let Some(current) = self.current.as_mut() else {
            return Err(ApplyError::NoBaseline);
        };
        if delta.from() != current.version {
            return Err(ApplyError::VersionGap {
                expected: current.version,
                got: delta.from(),
            });
        }
        if delta.to() <= current.version {
            return Err(ApplyError::NonMonotonic {
                current: current.version,
                rejected: delta.to(),
            });
        }
        current.apply_validated(delta);
        Ok(())
    }
}
