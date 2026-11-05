//! Pure ADR-0183 safety model. This module never sends commands, HTTP, or shell.
//!
//! The trusted service adapter must authenticate its principal, check resource
//! existence and current readiness, and revalidate every plan at the existing
//! authorized apply boundary. A plan is NOT an execution capability. In particular,
//! a replacement requires atomic video/audio/caption compare-and-apply; enqueueing
//! independent routing commands is not sufficient. No such adapter exists here.
//!
//! The bounded ledger never evicts: exhaustion fails closed. It is in-memory only.
//! Restart requires durable ledger recovery or a new presentation high-water fence
//! that excludes all previously observed boundaries. Never reconstruct with an old
//! fence and claim exactly-once effects. Revisions do not change an event's identity.

use crate::auth::{
    authorize_object, authorize_output, authorize_scope, Action as AuthAction, Principal,
};
use multiview_events::AuthzScope;
use std::collections::BTreeMap;

/// Maximum bytes in each opaque identifier. No URLs or payloads are accepted.
pub const MAX_ID_BYTES: usize = 256;
/// Hard ceiling for both ledger and lease capacity.
pub const MAX_CAPACITY: usize = 4096;

/// Explicit execution posture; construction is always disarmed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Do not reserve new actions. Existing leases remain available for restore.
    #[default]
    Disarmed,
    /// Return proposals without reserving identities or leases.
    DryRun,
    /// Reserve eligible proposals; still no effects.
    Armed,
}
/// A destination namespace. Unlike raw ids these preserve authorization axes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// A source tile's monitor-only routing.
    Tile(String),
    /// A selected output, including its audio and captions.
    Output(String),
    /// A program bus, not an output with the same name.
    Program(String),
}
/// Typed video replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// Deterministic bars.
    Bars,
    /// A preconfigured slate resource.
    Slate(String),
    /// A ready, preconfigured VT player.
    Vt(String),
}
/// Explicit audio routing; tone is destination-local, never a global producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPolicy {
    /// Preserve original source audio.
    Original,
    /// Silence only the lease destination.
    Silent,
    /// Calibrated store-backed tone; -60 to -6 dBFS inclusive, nonempty mask.
    Tone {
        /// Destination channel mask, limited to 64 channels by type.
        channel_mask: u64,
        /// Integer thousandths of dBFS.
        level_millidbfs: i32,
    },
}
/// Narrow transport allowlist. Media loading is deliberately not implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtVerb {
    /// Play only after actual readiness, not mailbox acceptance.
    Play,
    /// Pause transport.
    Pause,
    /// Stop transport.
    Stop,
}
/// Allowlisted plans, not commands. Arbitrary scripts/URLs cannot be represented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Existing VT transport path.
    Vt {
        /// Existing player resource.
        player: String,
        /// Allowlisted operation.
        verb: VtVerb,
    },
    /// Route a source through the authorized routing path.
    Route {
        /// Destination.
        scope: Scope,
        /// Source resource.
        source: String,
    },
    /// Atomically acquire an owned video/audio/caption override.
    Replace {
        /// Destination.
        scope: Scope,
        /// Replacement video.
        video: Replacement,
        /// Destination-only audio.
        audio: AudioPolicy,
        /// Compare-and-apply routing generation from trusted adapter.
        expected_generation: u64,
        /// Maximum hold on the host monotonic watchdog, independent of media.
        max_hold_ns: u64,
    },
    /// Compare-and-restore the adapter's saved atomic snapshot for this lease.
    Restore {
        /// Destination.
        scope: Scope,
        /// Reservation owning the saved video/audio/caption state.
        lease: u64,
        /// Generation to compare atomically, never blindly overwrite.
        expected_generation: u64,
    },
    /// Reference to an administrator-approved destination policy, not a URL.
    Webhook {
        /// Existing policy resource. Delivery, limits and SSRF checks are adapter duties.
        destination: String,
    },
}
/// A typed resource existence request for the trusted adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// Source, player, slate, rule or webhook-policy object.
    Object(String),
    /// Namespaced routing destination.
    Destination(Scope),
}
/// Trust boundary, not a boolean authorization bypass.
///
/// Only the authenticated service may implement this adapter in runtime wiring.
/// The model applies role and per-object/output/program scopes itself. The adapter
/// must check existence, actual VT readiness and approved webhook policy. It must
/// recheck all permissions/generations before applying a plan. This trait does not
/// authenticate untrusted inputs and must never be exposed as a request body.
pub trait Authorization {
    /// Verified session/service principal (never a principal from a request body).
    fn principal(&self) -> &Principal;
    /// Verify a resource exists in the current authorized configuration.
    fn resource_exists(&self, resource: &Resource) -> bool;
    /// Actual frame/cue readiness; command acceptance alone must return false.
    fn vt_ready(&self, player: &str) -> bool;
}
/// Configuration for one source-linked rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Stable rule identity; edits must not bypass deduplication.
    pub id: String,
    /// Source to match and authorize.
    pub source: String,
    /// Lifecycle boundary selected by this rule.
    pub trigger: BoundaryKind,
    /// Single typed action.
    pub action: Action,
    /// Explicit opt-in for sources not currently presented.
    pub allow_unpresented: bool,
    /// Maximum permitted source-presentation lateness in nanoseconds.
    pub max_lateness_ns: u64,
}
/// Reliable lifecycle transition identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoundaryKind {
    /// Confirmed start boundary, not vendor vicinity.
    Start,
    /// Confirmed end boundary.
    End,
}
/// Adapter-supplied immutable evidence. Vendor vicinity must set reliable=false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    /// Stable origin-qualified event identity, not asset CAID or revision.
    pub event: String,
    /// Source resource.
    pub source: String,
    /// Presentation epoch.
    pub epoch: u64,
    /// Boundary identity.
    pub boundary: BoundaryKind,
    /// Reliable boundary evidence, not inferred marker presence/absence.
    pub reliable: bool,
    /// Current mapping and lifecycle (not cancelled, superseded or uncertain).
    pub current: bool,
    /// Whether the source is currently presented.
    pub presented: bool,
    /// Scheduled source-media nanoseconds; a future value never fires now.
    pub effective_ns: u64,
    /// Current presented source-media nanoseconds, not receipt/wall-clock time.
    pub position_ns: u64,
    /// Trusted host monotonic nanoseconds, independent of source progress.
    pub monotonic_ns: u64,
}
/// A proposal requiring authorization revalidation at the apply boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPlan {
    /// Authorized rule identity.
    pub rule: String,
    /// Original source; must remain ingested during replacement.
    pub source: String,
    /// Immutable evidence at reservation; executor must revalidate current tracker
    /// identity, epoch, lifecycle and due position immediately before apply.
    /// Restoration retains correlation but must not require the old epoch live.
    pub evidence: Boundary,
    /// Typed proposal, never executed by this module.
    pub action: Action,
}
/// Evaluation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Observational proposal only.
    DryRun(ActionPlan),
    /// Identity is reserved even if command submission later fails.
    Reserved {
        /// Local reservation identifier.
        id: u64,
        /// Proposal for the adapter.
        plan: ActionPlan,
    },
}
/// Distinguish mailbox acceptance from application success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Reserved but not submitted.
    Reserved,
    /// Mailbox accepted, not proof of readiness or execution.
    Accepted,
    /// Definitive failure; no automatic retry for the same boundary.
    Failed,
    /// Non-lease action definitively succeeded.
    Succeeded,
    /// Atomic replacement applied with an ownership generation.
    Applied {
        /// Installed routing generation.
        generation: u64,
    },
}
/// Fail-closed reason for diagnostics; no hidden retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Model is disarmed.
    Disarmed,
    /// Malformed or out-of-bound configuration.
    Invalid,
    /// Authentication proof has insufficient role/resource scopes.
    Unauthorized,
    /// Adapter could not find a configured resource.
    MissingResource,
    /// Vendor vicinity or otherwise unreliable boundary.
    Unreliable,
    /// Old epoch, invalid lifecycle or timing mapping.
    Stale,
    /// At or behind recovery high-water fence.
    Replay,
    /// Future source-media boundary.
    NotDue,
    /// Beyond explicit lateness limit.
    Late,
    /// Source not presented and no explicit opt-in.
    NotPresented,
    /// Player lacks actual readiness.
    NotReady,
    /// Same rule/event/epoch/boundary already reserved, including failures.
    Duplicate,
    /// Ledger or lease capacity reached; never evict replay protection.
    Capacity,
    /// Overlapping override ownership or routing conflict.
    ScopeConflict,
    /// Invalid acknowledgement transition.
    InvalidOutcome,
    /// No applied lease snapshot available.
    NotApplied,
    /// Manual routing change revoked automatic restoration.
    GenerationConflict,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key(String, String, u64, BoundaryKind);
struct Entry {
    id: u64,
    plan: ActionPlan,
    outcome: Outcome,
    revoked: bool,
    lease: Option<Lease>,
}
struct Lease {
    scope: Scope,
    expires_ns: u64,
    revoked: bool,
    restoring: bool,
}
/// Single-source, bounded control-plane state; no IO, threads, or hidden clocks.
/// Scope collisions are model-local: the executor MUST coordinate ownership across
/// all source models using one atomic destination lease/generation boundary.
/// On restart reconcile outstanding external overrides before rearming: a new
/// empty model cannot recover lease snapshots or monotonic deadlines. An external
/// watchdog must retain/recover these, or compare-and-restore them at recovery.
pub struct SafetyModel {
    source: String,
    mode: Mode,
    epoch: u64,
    replay_fence_ns: u64,
    capacity: usize,
    lease_capacity: usize,
    entries: BTreeMap<Key, Entry>,
    next_id: u64,
}
impl SafetyModel {
    /// Begin disarmed with a trusted recovery fence. Boundaries at/below the
    /// fence never execute. A restart must not reuse an earlier fence.
    ///
    /// # Errors
    /// Returns `Invalid` for zero or excessive capacities.
    pub fn new(
        source: String,
        capacity: usize,
        lease_capacity: usize,
        epoch: u64,
        replay_fence_ns: u64,
    ) -> Result<Self, Rejection> {
        if !valid_id(&source)
            || capacity == 0
            || capacity > MAX_CAPACITY
            || lease_capacity == 0
            || lease_capacity > capacity
        {
            return Err(Rejection::Invalid);
        }
        Ok(Self {
            source,
            mode: Mode::Disarmed,
            epoch,
            replay_fence_ns,
            capacity,
            lease_capacity,
            entries: BTreeMap::new(),
            next_id: 1,
        })
    }
    /// Trusted control-plane posture change. Disarm never silently restores.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
    /// Change to a strictly newer presentation epoch; retain replay ledger and
    /// leases for safe emergency restoration, and require explicit rearming.
    ///
    /// # Errors
    /// Returns `Stale` unless the epoch strictly increases.
    pub fn advance_epoch(&mut self, epoch: u64, replay_fence_ns: u64) -> Result<(), Rejection> {
        if epoch <= self.epoch {
            return Err(Rejection::Stale);
        }
        self.epoch = epoch;
        self.replay_fence_ns = replay_fence_ns;
        self.mode = Mode::Disarmed;
        Ok(())
    }
    /// Validate, authorize and reserve at most once. No action is submitted.
    ///
    /// # Errors
    /// Returns a typed rejection for invalid evidence, authorization, readiness,
    /// duplicate identity, scope conflict, or exhausted capacity.
    pub fn reserve(
        &mut self,
        rule: &Rule,
        event: &Boundary,
        auth: &impl Authorization,
    ) -> Result<Decision, Rejection> {
        if self.mode == Mode::Disarmed {
            return Err(Rejection::Disarmed);
        }
        if !valid_id(&rule.id)
            || !valid_id(&rule.source)
            || !valid_id(&event.event)
            || rule.source != self.source
            || event.source != rule.source
            || event.boundary != rule.trigger
        {
            return Err(Rejection::Invalid);
        }
        if !event.reliable {
            return Err(Rejection::Unreliable);
        }
        if !event.current || event.epoch != self.epoch {
            return Err(Rejection::Stale);
        }
        if event.effective_ns <= self.replay_fence_ns {
            return Err(Rejection::Replay);
        }
        if event.effective_ns > event.position_ns {
            return Err(Rejection::NotDue);
        }
        if event.position_ns - event.effective_ns > rule.max_lateness_ns {
            return Err(Rejection::Late);
        }
        if !event.presented && !rule.allow_unpresented {
            return Err(Rejection::NotPresented);
        }
        if matches!(rule.action, Action::Restore { .. }) {
            return Err(Rejection::Invalid);
        }
        let plan = ActionPlan {
            rule: rule.id.clone(),
            source: rule.source.clone(),
            evidence: event.clone(),
            action: rule.action.clone(),
        };
        authorize_plan(&plan, auth)?;
        let key = Key(
            rule.id.clone(),
            event.event.clone(),
            event.epoch,
            event.boundary,
        );
        if self.entries.contains_key(&key) {
            return Err(Rejection::Duplicate);
        }
        if let Some(scope) = action_scope(&plan.action) {
            if self
                .entries
                .values()
                .filter_map(|e| e.lease.as_ref())
                .any(|lease| scopes_overlap(scope, &lease.scope))
            {
                return Err(Rejection::ScopeConflict);
            }
        }
        let lease = if let Action::Replace {
            scope, max_hold_ns, ..
        } = &plan.action
        {
            if self.entries.values().filter(|e| e.lease.is_some()).count() >= self.lease_capacity {
                return Err(Rejection::Capacity);
            }
            Some(Lease {
                scope: scope.clone(),
                expires_ns: event
                    .monotonic_ns
                    .checked_add(*max_hold_ns)
                    .ok_or(Rejection::Invalid)?,
                revoked: false,
                restoring: false,
            })
        } else {
            None
        };
        if self.entries.len() >= self.capacity {
            return Err(Rejection::Capacity);
        }
        if self.mode == Mode::DryRun {
            return Ok(Decision::DryRun(plan));
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(Rejection::Capacity)?;
        self.entries.insert(
            key,
            Entry {
                id,
                plan: plan.clone(),
                outcome: Outcome::Reserved,
                revoked: false,
                lease,
            },
        );
        Ok(Decision::Reserved { id, plan })
    }
    /// Record a trusted correlated adapter result; acceptance is never success.
    /// Failed submissions keep their replay identity. Unknown outcomes must stay
    /// Reserved/Accepted rather than falsely claim failure and release ownership.
    ///
    /// # Errors
    /// Returns `InvalidOutcome` for unknown ids or illegal state transitions.
    pub fn acknowledge(&mut self, id: u64, outcome: Outcome) -> Result<(), Rejection> {
        let entry = self.entry_mut(id)?;
        if !matches!(entry.outcome, Outcome::Reserved | Outcome::Accepted)
            || matches!(outcome, Outcome::Reserved)
        {
            return Err(Rejection::InvalidOutcome);
        }
        match (&entry.plan.action, outcome) {
            (
                Action::Replace {
                    expected_generation,
                    ..
                },
                Outcome::Applied { generation },
            ) if generation > *expected_generation => {}
            (Action::Replace { .. }, Outcome::Applied { .. } | Outcome::Succeeded)
            | (_, Outcome::Applied { .. }) => return Err(Rejection::InvalidOutcome),
            _ => {}
        }
        entry.outcome = outcome;
        if outcome == Outcome::Failed {
            entry.lease = None;
        }
        Ok(())
    }
    /// Observe acceptance/success separately.
    #[must_use]
    pub fn outcome(&self, id: u64) -> Option<Outcome> {
        self.entries
            .values()
            .find(|e| e.id == id)
            .map(|e| e.outcome)
    }
    /// Return maximum-hold leases due on the HOST MONOTONIC clock, never media
    /// position. Poll even when source frames stop, epochs change, or disarmed.
    /// The adapter must also request restoration on matching reliable end evidence.
    /// Polling/restore effects are adapter duties, not performed by this model.
    #[must_use]
    pub fn expired_leases(&self, monotonic_ns: u64) -> Vec<u64> {
        self.entries
            .values()
            .filter(|e| {
                e.lease
                    .as_ref()
                    .is_some_and(|l| monotonic_ns >= l.expires_ns && !l.revoked && !l.restoring)
            })
            .map(|e| e.id)
            .collect()
    }
    /// Explicit emergency/end/maximum-hold restore, available even disarmed.
    /// A mismatch permanently revokes automatic restore. The returned generation
    /// MUST still be compared atomically at apply, closing the manual-change race.
    ///
    /// # Errors
    /// Rejects unknown, unapplied, revoked or already-restoring leases, generation
    /// mismatches and insufficient current resource permissions.
    pub fn restore(
        &mut self,
        id: u64,
        current_generation: u64,
        auth: &impl Authorization,
    ) -> Result<ActionPlan, Rejection> {
        let entry = self.entry_mut(id)?;
        let Outcome::Applied { generation } = entry.outcome else {
            return Err(Rejection::NotApplied);
        };
        if entry.revoked {
            return Err(Rejection::GenerationConflict);
        }
        if current_generation != generation {
            entry.revoked = true;
            entry.lease = None;
            return Err(Rejection::GenerationConflict);
        }
        let lease = entry.lease.as_mut().ok_or(Rejection::NotApplied)?;
        if lease.restoring {
            return Err(Rejection::Duplicate);
        }
        let plan = ActionPlan {
            rule: entry.plan.rule.clone(),
            source: entry.plan.source.clone(),
            evidence: entry.plan.evidence.clone(),
            action: Action::Restore {
                scope: lease.scope.clone(),
                lease: id,
                expected_generation: generation,
            },
        };
        authorize_plan(&plan, auth)?;
        lease.restoring = true;
        Ok(plan)
    }
    /// Correlated atomic restore result. A failed compare permanently revokes
    /// restoration; it must never be retried against a newer manual generation.
    ///
    /// # Errors
    /// Rejects unknown ids or leases with no outstanding restore proposal.
    pub fn finish_restore(&mut self, id: u64, restored: bool) -> Result<(), Rejection> {
        let entry = self.entry_mut(id)?;
        let lease = entry.lease.as_mut().ok_or(Rejection::NotApplied)?;
        if !lease.restoring {
            return Err(Rejection::NotApplied);
        }
        entry.revoked = !restored;
        entry.lease = None;
        Ok(())
    }
    fn entry_mut(&mut self, id: u64) -> Result<&mut Entry, Rejection> {
        self.entries
            .values_mut()
            .find(|e| e.id == id)
            .ok_or(Rejection::InvalidOutcome)
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}
fn action_scope(action: &Action) -> Option<&Scope> {
    match action {
        Action::Route { scope, .. }
        | Action::Replace { scope, .. }
        | Action::Restore { scope, .. } => Some(scope),
        _ => None,
    }
}
fn scopes_overlap(a: &Scope, b: &Scope) -> bool {
    match (a, b) {
        (Scope::Tile(a), Scope::Tile(b))
        | (Scope::Output(a), Scope::Output(b))
        | (Scope::Program(a), Scope::Program(b)) => a == b,
        // Different namespace relationships require adapter topology. Without it,
        // conservatively reject instead of assuming tile/program/output isolation.
        _ => true,
    }
}
fn object(id: &str, auth: &impl Authorization) -> Result<(), Rejection> {
    if !valid_id(id) {
        return Err(Rejection::Invalid);
    }
    authorize_object(auth.principal(), id).map_err(|_| Rejection::Unauthorized)?;
    if !auth.resource_exists(&Resource::Object(id.to_owned())) {
        return Err(Rejection::MissingResource);
    }
    Ok(())
}
/// Revalidate a plan with the authenticated adapter. The executor must also check
/// freshness, destination policy, actual readiness and generation at atomic apply.
///
/// # Errors
/// Rejects insufficient role/scopes, absent resources, unready VT players and
/// invalid identifiers, hold bounds or tone parameters.
pub fn authorize_plan(plan: &ActionPlan, auth: &impl Authorization) -> Result<(), Rejection> {
    auth.principal()
        .role
        .require(AuthAction::Write)
        .map_err(|_| Rejection::Unauthorized)?;
    object(&plan.rule, auth)?;
    object(&plan.source, auth)?;
    if let Some(scope) = action_scope(&plan.action) {
        let id = match scope {
            Scope::Tile(id) | Scope::Output(id) | Scope::Program(id) => id,
        };
        object(id, auth)?;
        match scope {
            Scope::Tile(_) => {}
            Scope::Output(id) => {
                authorize_output(auth.principal(), id).map_err(|_| Rejection::Unauthorized)?;
            }
            Scope::Program(id) => authorize_scope(auth.principal(), AuthzScope::Program(id))
                .map_err(|_| Rejection::Unauthorized)?,
        }
        if !auth.resource_exists(&Resource::Destination(scope.clone())) {
            return Err(Rejection::MissingResource);
        }
    }
    match &plan.action {
        Action::Route { source, .. } => object(source, auth)?,
        Action::Vt { player, verb } => {
            object(player, auth)?;
            if *verb == VtVerb::Play && !auth.vt_ready(player) {
                return Err(Rejection::NotReady);
            }
        }
        Action::Replace {
            video,
            audio,
            max_hold_ns,
            ..
        } => {
            if *max_hold_ns == 0 || *max_hold_ns > 86_400_000_000_000 {
                return Err(Rejection::Invalid);
            }
            if let AudioPolicy::Tone {
                channel_mask,
                level_millidbfs,
            } = audio
            {
                if *channel_mask == 0 || !(-60_000..=-6_000).contains(level_millidbfs) {
                    return Err(Rejection::Invalid);
                }
            }
            match video {
                Replacement::Bars => {}
                Replacement::Slate(id) => object(id, auth)?,
                Replacement::Vt(id) => {
                    object(id, auth)?;
                    if !auth.vt_ready(id) {
                        return Err(Rejection::NotReady);
                    }
                }
            }
        }
        Action::Webhook { destination } => object(destination, auth)?,
        Action::Restore { .. } => {}
    }
    Ok(())
}
