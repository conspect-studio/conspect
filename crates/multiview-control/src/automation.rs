//! ADR-0183 ad-break automation executor (management plane only).
//!
//! Subscribes (by polling, ~4 Hz, on a tokio task — never the output clock) to
//! the source-event service through [`SourceBreakProvider`], and for each
//! source with an ARMED `events.automation` rule:
//!
//! * on a confirmed break start, records the pre-break routing of the cell(s)
//!   showing the source, reserves a [`SafetyModel`] lease, and issues existing
//!   `Command::SwapSource` (and, for program/tone actions, `Command::RouteAudio`)
//!   commands through the bounded, non-blocking command bus;
//! * on the confirmed end (or the monotonic watchdog after `max_duration_s`,
//!   or disarm, or rule removal, or the operator's "Restore now"), it
//!   compare-and-restores the recorded state;
//! * a manual operator change to any replaced destination during the break
//!   (detected through the [`RoutingLedger`] generation) revokes auto-restore.
//!
//! Every action appends to a bounded per-source log (shown in the Events
//! dialog) and to the audit trail. Optional webhooks are POSTed off-task with
//! timeouts through the SSRF guard so external VT/automation can react.

use crate::auth::{Principal, Role};
use crate::command::{Command, OperationId, RoutingLedger, SubmitError};
use crate::event_automation::{
    Action as PlanAction, AudioPolicy, Authorization, Boundary, BoundaryKind, Decision, Mode,
    Outcome, Reconciled, Rejection, Replacement, Resource, RestoreObservation, Rule as PlanRule,
    SafetyModel, Scope,
};
use crate::source_events::{ActiveBreak, SharedSourceBreakProvider};
use multiview_config::routing::StreamRef;
use multiview_config::schema::{BreakActionKind, EventAutomationRule};
use multiview_core::stream::StreamKind;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Bounded action-log entries kept per source.
pub const MAX_LOG_ENTRIES: usize = 64;
/// Bounded number of sources with live automation runtime state.
pub const MAX_AUTOMATED_SOURCES: usize = 256;
/// Executor poll cadence.
pub const TICK_MS: u64 = 250;

/// Reserved replacement target: the first configured `bars` source.
pub const TARGET_BARS: &str = "bars";
/// Reserved replacement target: bars plus its 1 kHz line-up tone on the
/// source's program-bus audio.
pub const TARGET_BARS_TONE: &str = "bars_tone";

/// What the executor needs to issue commands; the real bus or a test fake.
pub trait CommandSink: Send + Sync {
    /// Non-blocking submit.
    ///
    /// # Errors
    /// The bus is full or closed.
    fn submit(&self, command: Command) -> Result<(), SubmitError>;
    /// Routing ledger shared with every other command producer.
    fn ledger(&self) -> &RoutingLedger;
}
impl CommandSink for crate::command::CommandSender {
    fn submit(&self, command: Command) -> Result<(), SubmitError> {
        self.try_submit(command).map(|_| ())
    }
    fn ledger(&self) -> &RoutingLedger {
        crate::command::CommandSender::ledger(self)
    }
}

/// Configuration snapshot the executor evaluates each tick.
#[derive(Debug, Clone, Default)]
pub struct ConfigView {
    /// Every configured source id with its kind tag and optional rule.
    pub sources: Vec<SourceView>,
    /// Working-layout cells with their configured binding.
    pub cells: Vec<(String, Option<String>)>,
}
/// One configured source.
#[derive(Debug, Clone)]
pub struct SourceView {
    /// Source id.
    pub id: String,
    /// Whether it is a `bars` synthetic source.
    pub bars: bool,
    /// Optional automation rule.
    pub rule: Option<EventAutomationRule>,
}
/// Supplies the configuration view (the durable source store in production).
pub trait ConfigProvider: Send + Sync {
    /// Read the current view. Called on the management plane only.
    fn view(&self) -> ConfigView;
    /// Drop any cached view (called after a rule/arm/disarm write).
    fn invalidate(&self) {}
}
/// Audit sink: `(source id, detail)`.
pub type AuditSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;
/// Webhook sink: `(url, payload)`; must not block.
pub type WebhookSink = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// Live automation phase of one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Phase {
    /// No rule, or rule disarmed and nothing owned.
    Disarmed,
    /// Armed and waiting for a break.
    Idle,
    /// A replacement is on air.
    InBreak,
    /// A restore is in flight.
    Restoring,
    /// An operator changed a replaced destination; auto-restore is cancelled
    /// until the break ends.
    ManualOverride,
}
/// One replaced cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReplacedCell {
    /// Cell id.
    pub cell: String,
    /// Source the cell showed before the break (restored at the end).
    pub original: String,
    /// Source taken into the cell.
    pub replacement: String,
}
/// Public status of one source's automation.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AutomationStatus {
    /// Current phase.
    pub phase: Phase,
    /// Whether the rule is armed.
    pub armed: bool,
    /// Break occurrence being handled.
    pub event_id: Option<String>,
    /// Whether that occurrence is operator test evidence.
    pub test: bool,
    /// Unix milliseconds the replacement was taken.
    pub since_unix_ms: Option<i64>,
    /// Cells currently replaced.
    pub replaced: Vec<ReplacedCell>,
    /// Whether the source's program-bus audio was re-pointed.
    pub audio_replaced: Option<String>,
    /// Last refusal/failure, if any.
    pub last_error: Option<String>,
}
/// One action-log row.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AutomationLogEntry {
    /// Unix milliseconds.
    pub at_unix_ms: i64,
    /// Short machine action (`take`, `restore`, `watchdog`, ...).
    pub action: String,
    /// Human-readable detail.
    pub detail: String,
}

/// Why a restore runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreReason {
    /// Confirmed break end.
    BreakEnd,
    /// Max-duration watchdog.
    Watchdog,
    /// Rule disarmed during the break.
    Disarmed,
    /// Rule removed (or source deleted) during the break.
    RuleRemoved,
    /// Operator "Restore now".
    Manual,
}
impl RestoreReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::BreakEnd => "break_end",
            Self::Watchdog => "watchdog",
            Self::Disarmed => "disarmed",
            Self::RuleRemoved => "rule_removed",
            Self::Manual => "manual",
        }
    }
}
/// Manual-restore refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualRestoreError {
    /// Nothing owned for this source.
    NothingToRestore,
    /// An operator change already revoked the owned replacement.
    ManualOverride,
    /// The command bus shed the restore; retry.
    Busy,
}

#[derive(Debug, Clone)]
struct Owned {
    lease: u64,
    replacement: String,
    event: ActiveBreak,
    epoch: u64,
    since_unix_ms: i64,
    cells: Vec<ReplacedCell>,
    /// Program-bus audio channel (== source id) re-pointed to this source.
    audio: Option<String>,
    keys: Vec<String>,
    /// Ledger generation sum right after our own take/restore commands.
    owned_generation: u64,
    applied_generation: u64,
    rule_restore_on_end: bool,
    webhook: Option<String>,
    /// Cells/audio still to restore after a partially shed restore.
    restoring: bool,
}

struct Run {
    model: SafetyModel,
    owned: Option<Owned>,
    manual_override: Option<ActiveBreak>,
    /// Occurrences already handled (taken, skipped or overridden) this epoch.
    handled: VecDeque<(u64, String)>,
    log: VecDeque<AutomationLogEntry>,
    last_error: Option<String>,
    armed: bool,
}

/// The executor. Cheap to share; all state is behind one short-held mutex
/// that is never held across an `.await`.
pub struct AutomationExecutor {
    provider: SharedSourceBreakProvider,
    sink: Arc<dyn CommandSink>,
    config: Arc<dyn ConfigProvider>,
    audit: AuditSink,
    webhook: Option<WebhookSink>,
    runs: Mutex<BTreeMap<String, Run>>,
    started: std::time::Instant,
    principal: Principal,
}

struct Adapter<'a> {
    principal: &'a Principal,
    view: &'a ConfigView,
}
impl Authorization for Adapter<'_> {
    fn principal(&self) -> &Principal {
        self.principal
    }
    fn resource_exists(&self, resource: &Resource) -> bool {
        match resource {
            Resource::Object(id) => {
                id.ends_with("#automation") || self.view.sources.iter().any(|s| &s.id == id)
            }
            // The bundle destination is owned by the executor; per-cell
            // existence is enforced by the engine (an unknown cell is a held no-op).
            Resource::Destination(_) => true,
        }
    }
    fn vt_ready(&self, _: &str) -> bool {
        false
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn rule_id(source: &str) -> String {
    format!("{source}#automation")
}

impl AutomationExecutor {
    /// Build an executor. It does nothing until [`Self::tick`] runs.
    #[must_use]
    pub fn new(
        provider: SharedSourceBreakProvider,
        sink: Arc<dyn CommandSink>,
        config: Arc<dyn ConfigProvider>,
        audit: AuditSink,
        webhook: Option<WebhookSink>,
    ) -> Self {
        Self {
            provider,
            sink,
            config,
            audit,
            webhook,
            runs: Mutex::new(BTreeMap::new()),
            started: std::time::Instant::now(),
            // Rules are authorized per object when they are written/armed
            // (PUT/arm routes); the executor acts as this service principal.
            principal: Principal {
                key_id: "automation".into(),
                role: Role::Operator,
                scoped_object_ids: None,
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
        }
    }

    fn monotonic_ns(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Run>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Spawn the polling task on the current tokio runtime (management plane).
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let now = this.monotonic_ns();
                this.tick(now);
            }
        })
    }

    /// Evaluate every source once at host-monotonic `now_ns`.
    pub fn tick(&self, now_ns: u64) {
        let view = self.config.view();
        let mut runs = self.lock();
        let ids: Vec<String> = view
            .sources
            .iter()
            .filter(|s| s.rule.is_some())
            .map(|s| s.id.clone())
            .chain(runs.keys().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for id in ids {
            let rule = view
                .sources
                .iter()
                .find(|s| s.id == id)
                .and_then(|s| s.rule.clone());
            if !runs.contains_key(&id) {
                if rule.is_none() || runs.len() >= MAX_AUTOMATED_SOURCES {
                    continue;
                }
                let Ok(model) = SafetyModel::new(id.clone(), 1024, 1, 0, 0) else {
                    continue;
                };
                runs.insert(
                    id.clone(),
                    Run {
                        model,
                        owned: None,
                        manual_override: None,
                        handled: VecDeque::new(),
                        log: VecDeque::new(),
                        last_error: None,
                        armed: false,
                    },
                );
            }
            let Some(run) = runs.get_mut(&id) else {
                continue;
            };
            self.step(&id, rule.as_ref(), run, &view, now_ns);
            if rule.is_none() && run.owned.is_none() && run.manual_override.is_none() {
                runs.remove(&id);
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn step(
        &self,
        id: &str,
        rule: Option<&EventAutomationRule>,
        run: &mut Run,
        view: &ConfigView,
        now_ns: u64,
    ) {
        let armed = rule.is_some_and(|r| r.armed);
        if armed != run.armed {
            run.armed = armed;
            self.log(
                id,
                run,
                if armed { "armed" } else { "disarmed" },
                if armed {
                    "Automation armed".into()
                } else {
                    "Automation disarmed".into()
                },
            );
        }
        let status = self.provider.break_status(id);
        if let Some(status) = &status {
            if status.epoch > 0 && run.model_epoch() < status.epoch {
                // New presentation epoch (reconnect): retain leases for
                // restore; replay ledger identities are per epoch.
                let _ = run.model.advance_epoch(status.epoch, 0);
            }
        }
        run.model
            .set_mode(if armed { Mode::Armed } else { Mode::Disarmed });
        let active = status
            .as_ref()
            .filter(|s| !s.degraded)
            .and_then(|s| s.active.clone());

        // Owned replacement: watch for end, watchdog, disarm, override.
        if let Some(owned) = run.owned.clone() {
            if owned.restoring {
                self.continue_restore(id, run, view, now_ns);
                return;
            }
            let current = self.generation(&owned.keys);
            if current != owned.owned_generation {
                let _ = run
                    .model
                    .restore_at(owned.lease, current, now_ns, &self.adapter(view));
                run.manual_override = Some(owned.event.clone());
                run.owned = None;
                self.log(
                    id,
                    run,
                    "manual_override",
                    "Operator changed a replaced destination; automatic restore cancelled".into(),
                );
                return;
            }
            let reason = if rule.is_none() {
                Some(RestoreReason::RuleRemoved)
            } else if !armed {
                Some(RestoreReason::Disarmed)
            } else if run.model.expired_leases(now_ns).contains(&owned.lease) {
                Some(RestoreReason::Watchdog)
            } else if owned.rule_restore_on_end
                && status.as_ref().is_some_and(|s| {
                    s.epoch != owned.epoch
                        || s.active.as_ref().is_none_or(|a| a.event_id != owned.event.event_id)
                })
            {
                Some(RestoreReason::BreakEnd)
            } else {
                None
            };
            if let Some(reason) = reason {
                let _ = self.restore(id, run, view, now_ns, reason);
            }
            return;
        }
        if let Some(event) = run.manual_override.clone() {
            if active.as_ref().is_none_or(|a| a.event_id != event.event_id) {
                run.manual_override = None;
                self.log(
                    id,
                    run,
                    "break_end",
                    "Break ended while under manual control; nothing restored".into(),
                );
            }
            return;
        }
        let (Some(rule), Some(event), Some(status)) = (rule, active, status) else {
            return;
        };
        if !armed {
            return;
        }
        if run
            .handled
            .iter()
            .any(|(e, ev)| *e == status.epoch && *ev == event.event_id)
        {
            return;
        }
        if run.handled.len() >= MAX_LOG_ENTRIES {
            run.handled.pop_front();
        }
        run.handled.push_back((status.epoch, event.event_id.clone()));
        self.take(id, rule, run, view, &event, status.epoch, now_ns);
    }

    fn adapter<'a>(&'a self, view: &'a ConfigView) -> Adapter<'a> {
        Adapter {
            principal: &self.principal,
            view,
        }
    }

    fn generation(&self, keys: &[String]) -> u64 {
        keys.iter()
            .map(|k| self.sink.ledger().generation(k))
            .fold(0u64, u64::saturating_add)
    }

    fn resolve_target(target: &str, view: &ConfigView) -> Option<String> {
        if target == TARGET_BARS || target == TARGET_BARS_TONE {
            view.sources.iter().find(|s| s.bars).map(|s| s.id.clone())
        } else {
            view.sources
                .iter()
                .any(|s| s.id == target)
                .then(|| target.to_owned())
        }
    }

    fn binding(&self, cell: &str, view: &ConfigView) -> Option<String> {
        self.sink.ledger().bound_source(cell).or_else(|| {
            if self.sink.ledger().layout_changed() || self.sink.ledger().layout_replaced() {
                None
            } else {
                view.cells
                    .iter()
                    .find(|(c, _)| c == cell)
                    .and_then(|(_, b)| b.clone())
            }
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn take(
        &self,
        id: &str,
        rule: &EventAutomationRule,
        run: &mut Run,
        view: &ConfigView,
        event: &ActiveBreak,
        epoch: u64,
        now_ns: u64,
    ) {
        let label = if event.test { " (test)" } else { "" };
        self.log(
            id,
            run,
            "break_start",
            format!("Break start{label}: {}", event.event_id),
        );
        let target_name = rule.on_break_start.target.as_str();
        let Some(replacement) = Self::resolve_target(target_name, view) else {
            let detail = if target_name == TARGET_BARS || target_name == TARGET_BARS_TONE {
                "No bars source is configured; add a Bars source to use bars/bars+tone".to_owned()
            } else {
                format!("Replacement source {target_name:?} does not exist")
            };
            run.last_error = Some(detail.clone());
            self.log(id, run, "refused", detail);
            return;
        };
        let cells: Vec<String> = match &rule.on_break_start.cells {
            Some(cells) => cells.clone(),
            None => view
                .cells
                .iter()
                .map(|(c, _)| c.clone())
                .filter(|c| self.binding(c, view).as_deref() == Some(id))
                .collect(),
        };
        let replaced: Vec<ReplacedCell> = cells
            .iter()
            .filter_map(|cell| {
                let original = self.binding(cell, view)?;
                (original != replacement).then(|| ReplacedCell {
                    cell: cell.clone(),
                    original,
                    replacement: replacement.clone(),
                })
            })
            .collect();
        let audio = (rule.on_break_start.action == BreakActionKind::RouteProgram
            || target_name == TARGET_BARS_TONE)
            .then(|| id.to_owned());
        if replaced.is_empty() && audio.is_none() {
            let detail = "No cell currently shows this source; nothing to replace".to_owned();
            run.last_error = Some(detail.clone());
            self.log(id, run, "skipped", detail);
            return;
        }
        let mut keys: Vec<String> = replaced
            .iter()
            .map(|c| RoutingLedger::cell_key(&c.cell))
            .collect();
        if let Some(channel) = &audio {
            keys.push(RoutingLedger::audio_key(channel));
        }
        let expected = self.generation(&keys);
        let plan_rule = PlanRule {
            id: rule_id(id),
            source: id.to_owned(),
            trigger: BoundaryKind::Start,
            action: PlanAction::Replace {
                scope: Scope::Tile(id.to_owned()),
                video: if target_name == TARGET_BARS || target_name == TARGET_BARS_TONE {
                    Replacement::Bars
                } else {
                    Replacement::Slate(replacement.clone())
                },
                audio: AudioPolicy::Original,
                expected_generation: expected,
                max_hold_ns: u64::from(rule.max_duration_s).saturating_mul(1_000_000_000),
            },
            allow_unpresented: true,
            max_lateness_ns: u64::MAX,
        };
        // S1: revalidate against a FRESH acquisition immediately before the
        // effect — a cancellation/revision published since `step` read the
        // status must stop the take.
        let fresh = self.provider.break_status(id);
        let still_current = fresh.as_ref().is_some_and(|s| {
            s.epoch == epoch
                && !s.degraded
                && s.active.as_ref().is_some_and(|a| a.event_id == event.event_id)
        });
        let boundary = Boundary {
            event: event.event_id.clone(),
            source: id.to_owned(),
            epoch,
            boundary: BoundaryKind::Start,
            reliable: true,
            current: still_current,
            presented: true,
            effective_ns: now_ns.max(1),
            position_ns: now_ns.max(1),
            monotonic_ns: now_ns,
        };
        let adapter = self.adapter(view);
        let lease = match run.model.reserve(&plan_rule, &boundary, &adapter) {
            Ok(Decision::Reserved { id: lease, .. }) => lease,
            Ok(Decision::DryRun(_)) => return,
            Err(rejection) => {
                let detail = format!("Take refused by safety model: {rejection:?}");
                run.last_error = Some(detail.clone());
                self.log(id, run, "refused", detail);
                return;
            }
        };
        let mut landed = 0usize;
        let mut failed = false;
        for cell in &replaced {
            if self
                .sink
                .submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: cell.cell.clone(),
                    source: cell.replacement.clone(),
                })
                .is_ok()
            {
                landed += 1;
            } else {
                failed = true;
            }
        }
        if let Some(channel) = &audio {
            if self
                .sink
                .submit(Command::RouteAudio {
                    op: OperationId::new(),
                    target: channel.clone(),
                    source: StreamRef::best(replacement.clone(), StreamKind::Audio),
                    gain_db: 0.0,
                    mute: false,
                })
                .is_ok()
            {
                landed += 1;
            } else {
                failed = true;
            }
        }
        let applied = self.generation(&keys);
        if landed == 0 {
            let _ = run.model.acknowledge(lease, Outcome::Failed);
            let detail = "Command bus busy; take was not applied".to_owned();
            run.last_error = Some(detail.clone());
            self.log(id, run, "failed", detail);
            return;
        }
        if run
            .model
            .acknowledge(lease, Outcome::Applied { generation: applied })
            .is_err()
        {
            run.last_error = Some("take acknowledgement rejected".into());
        }
        let summary = replaced
            .iter()
            .map(|c| format!("{}: {} → {}", c.cell, c.original, c.replacement))
            .chain(audio.iter().map(|_| format!("program audio → {replacement}")))
            .collect::<Vec<_>>()
            .join(", ");
        run.last_error = failed.then(|| "Some take commands were shed by a busy bus".into());
        let since = now_unix_ms();
        run.owned = Some(Owned {
            lease,
            replacement: replacement.clone(),
            event: event.clone(),
            epoch,
            since_unix_ms: since,
            cells: replaced,
            audio,
            keys,
            owned_generation: applied,
            applied_generation: applied,
            rule_restore_on_end: rule.restore_on_end,
            webhook: rule.webhook_url.clone(),
            restoring: false,
        });
        self.log(id, run, "take", format!("Replaced {summary}"));
        self.fire_webhook(rule.webhook_url.as_deref(), id, "break_start", event, run);
    }

    fn restore(
        &self,
        id: &str,
        run: &mut Run,
        view: &ConfigView,
        now_ns: u64,
        reason: RestoreReason,
    ) -> Result<(), ManualRestoreError> {
        let Some(owned) = run.owned.clone() else {
            return Err(if run.manual_override.is_some() {
                ManualRestoreError::ManualOverride
            } else {
                ManualRestoreError::NothingToRestore
            });
        };
        let current = self.generation(&owned.keys);
        // Our own partial restore moved the generation; compare against what
        // WE last wrote and present the applied generation to the model.
        let presented = if current == owned.owned_generation {
            owned.applied_generation
        } else {
            current
        };
        match run
            .model
            .restore_at(owned.lease, presented, now_ns, &self.adapter(view))
        {
            Ok(_) => {}
            Err(Rejection::GenerationConflict) => {
                run.manual_override = Some(owned.event.clone());
                run.owned = None;
                self.log(
                    id,
                    run,
                    "manual_override",
                    "Operator changed a replaced destination; automatic restore cancelled".into(),
                );
                return Err(ManualRestoreError::ManualOverride);
            }
            Err(rejection) => {
                let detail = format!("Restore refused by safety model: {rejection:?}");
                run.last_error = Some(detail.clone());
                self.log(id, run, "refused", detail);
                return Err(ManualRestoreError::Busy);
            }
        }
        self.log(
            id,
            run,
            "restoring",
            format!("Restoring ({})", reason.as_str()),
        );
        if let Some(o) = run.owned.as_mut() {
            o.restoring = true;
        }
        self.fire_webhook(owned.webhook.as_deref(), id, "break_end", &owned.event, run);
        self.dispatch_restore(id, run, view, now_ns);
        if run.owned.is_some() {
            Err(ManualRestoreError::Busy)
        } else {
            Ok(())
        }
    }

    /// Submit every outstanding restore command; finish when all land.
    fn dispatch_restore(&self, id: &str, run: &mut Run, _view: &ConfigView, _now_ns: u64) {
        let Some(owned) = run.owned.as_mut() else {
            return;
        };
        let mut remaining = Vec::new();
        for cell in owned.cells.drain(..) {
            let ok = self
                .sink
                .submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: cell.cell.clone(),
                    source: cell.original.clone(),
                })
                .is_ok();
            if !ok {
                remaining.push(cell);
            }
        }
        owned.cells = remaining;
        if let Some(channel) = owned.audio.clone() {
            let ok = self
                .sink
                .submit(Command::RouteAudio {
                    op: OperationId::new(),
                    target: channel.clone(),
                    source: StreamRef::best(channel, StreamKind::Audio),
                    gain_db: 0.0,
                    mute: false,
                })
                .is_ok();
            if ok {
                owned.audio = None;
            }
        }
        let keys = owned.keys.clone();
        let lease = owned.lease;
        let done = owned.cells.is_empty() && owned.audio.is_none();
        let generation = self.generation(&keys);
        if let Some(owned) = run.owned.as_mut() {
            owned.owned_generation = generation;
        }
        if done {
            let _ = run.model.finish_restore(lease, true);
            run.owned = None;
            run.last_error = None;
            self.log(id, run, "restore", "Restored pre-break routing".into());
        } else {
            // F2: the dispatch was (partly) lost; reconcile with bounded retry.
            match run.model.reconcile_restore(lease, RestoreObservation::Unchanged) {
                Ok(Reconciled::Retry) => {
                    run.last_error = Some("Command bus busy; restore will retry".into());
                }
                _ => {
                    run.owned = None;
                    let detail =
                        "Restore could not be dispatched after bounded retries; operator action required"
                            .to_owned();
                    run.last_error = Some(detail.clone());
                    self.log(id, run, "restore_failed", detail);
                }
            }
        }
    }

    fn continue_restore(&self, id: &str, run: &mut Run, view: &ConfigView, now_ns: u64) {
        let Some(owned) = run.owned.clone() else {
            return;
        };
        let current = self.generation(&owned.keys);
        if current != owned.owned_generation {
            // Someone else routed a destination mid-restore: never overwrite it.
            let _ = run
                .model
                .reconcile_restore(owned.lease, RestoreObservation::Changed);
            run.owned = None;
            run.manual_override = None;
            self.log(
                id,
                run,
                "manual_override",
                "Operator changed a destination during restore; remaining restore cancelled".into(),
            );
            return;
        }
        if run
            .model
            .restore_at(owned.lease, owned.applied_generation, now_ns, &self.adapter(view))
            .is_ok()
        {
            self.dispatch_restore(id, run, view, now_ns);
        }
    }

    fn fire_webhook(
        &self,
        url: Option<&str>,
        id: &str,
        phase: &str,
        event: &ActiveBreak,
        run: &mut Run,
    ) {
        let (Some(url), Some(sink)) = (url, &self.webhook) else {
            return;
        };
        sink(
            url,
            serde_json::json!({
                "type": phase,
                "source_id": id,
                "event_id": event.event_id,
                "test": event.test,
                "at_unix_ms": now_unix_ms(),
            }),
        );
        self.log(id, run, "webhook", format!("Webhook {phase} queued"));
    }

    fn log(&self, id: &str, run: &mut Run, action: &str, detail: String) {
        if run.log.len() >= MAX_LOG_ENTRIES {
            run.log.pop_front();
        }
        run.log.push_back(AutomationLogEntry {
            at_unix_ms: now_unix_ms(),
            action: action.to_owned(),
            detail: detail.clone(),
        });
        (self.audit)(
            id,
            serde_json::json!({ "command": "event_automation", "action": action, "detail": detail }),
        );
    }

    /// Operator "Restore now".
    ///
    /// # Errors
    /// Nothing owned, a manual override already revoked it, or the bus shed.
    pub fn manual_restore(&self, id: &str) -> Result<(), ManualRestoreError> {
        let view = self.config.view();
        let now = self.monotonic_ns();
        let mut runs = self.lock();
        let run = runs
            .get_mut(id)
            .ok_or(ManualRestoreError::NothingToRestore)?;
        if run.owned.as_ref().is_some_and(|o| o.restoring) {
            self.continue_restore(id, run, &view, now);
            return if run.owned.is_some() {
                Err(ManualRestoreError::Busy)
            } else {
                Ok(())
            };
        }
        self.restore(id, run, &view, now, RestoreReason::Manual)
    }

    /// Status reported when no executor runs.
    #[must_use]
    pub fn idle_status() -> (AutomationStatus, Vec<AutomationLogEntry>) {
        (
            AutomationStatus {
                phase: Phase::Disarmed,
                armed: false,
                event_id: None,
                test: false,
                since_unix_ms: None,
                replaced: Vec::new(),
                audio_replaced: None,
                last_error: None,
            },
            Vec::new(),
        )
    }

    /// Current status and bounded log (empty/disarmed when unknown).
    #[must_use]
    pub fn status(&self, id: &str) -> (AutomationStatus, Vec<AutomationLogEntry>) {
        let runs = self.lock();
        let Some(run) = runs.get(id) else {
            return Self::idle_status();
        };
        let phase = match (&run.owned, &run.manual_override) {
            (Some(o), _) if o.restoring => Phase::Restoring,
            (Some(_), _) => Phase::InBreak,
            (None, Some(_)) => Phase::ManualOverride,
            (None, None) if run.armed => Phase::Idle,
            (None, None) => Phase::Disarmed,
        };
        let event = run
            .owned
            .as_ref()
            .map(|o| &o.event)
            .or(run.manual_override.as_ref());
        (
            AutomationStatus {
                phase,
                armed: run.armed,
                event_id: event.map(|e| e.event_id.clone()),
                test: event.is_some_and(|e| e.test),
                since_unix_ms: run.owned.as_ref().map(|o| o.since_unix_ms),
                replaced: run.owned.as_ref().map(|o| o.cells.clone()).unwrap_or_default(),
                audio_replaced: run
                    .owned
                    .as_ref()
                    .and_then(|o| o.audio.as_ref().map(|_| o.replacement.clone())),
                last_error: run.last_error.clone(),
            },
            run.log.iter().rev().cloned().collect(),
        )
    }
}

impl Run {
    fn model_epoch(&self) -> u64 {
        self.model.epoch()
    }
}

/// Production [`ConfigProvider`]: the accepted durable source documents plus
/// the working layout's cell bindings. Cached for one second (it parses the
/// accepted record), invalidated after every rule/arm/disarm write.
pub struct StateConfigProvider {
    state: Mutex<Option<crate::state::AppState>>,
    cache: Mutex<Option<(std::time::Instant, ConfigView)>>,
}
impl StateConfigProvider {
    /// Build an unbound provider; call [`Self::bind`] with the final state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            cache: Mutex::new(None),
        }
    }
    /// Bind the application state once it is fully assembled.
    pub fn bind(&self, state: crate::state::AppState) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
        self.invalidate();
    }
    fn compose(state: &crate::state::AppState) -> ConfigView {
        let Ok(document) = crate::boot_model::current_desired_config(state) else {
            return ConfigView::default();
        };
        let sources = document
            .get("sources")
            .and_then(serde_json::Value::as_array)
            .map(|sources| {
                sources
                    .iter()
                    .filter_map(|body| {
                        let source: multiview_config::Source =
                            serde_json::from_value(body.clone()).ok()?;
                        Some(SourceView {
                            bars: matches!(source.kind, multiview_config::SourceKind::Bars),
                            rule: source.events.and_then(|e| e.automation),
                            id: source.id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cells = document
            .get("cells")
            .and_then(serde_json::Value::as_array)
            .map(|cells| {
                cells
                    .iter()
                    .filter_map(|cell| {
                        let id = cell.get("id")?.as_str()?.to_owned();
                        let bound = cell
                            .get("source")
                            .and_then(|s| s.get("input_id"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                        Some((id, bound))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ConfigView { sources, cells }
    }
}
impl Default for StateConfigProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl ConfigProvider for StateConfigProvider {
    fn view(&self) -> ConfigView {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, view)) = cache.as_ref() {
            if at.elapsed() < std::time::Duration::from_secs(1) {
                return view.clone();
            }
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let view = state.as_ref().map(Self::compose).unwrap_or_default();
        *cache = Some((std::time::Instant::now(), view.clone()));
        view
    }
    fn invalidate(&self) {
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl AutomationExecutor {
    /// Drop the cached configuration view (after a rule write).
    pub fn invalidate_config(&self) {
        self.config.invalidate();
    }
}

/// Webhook connect/total timeout.
pub const WEBHOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// Maximum webhooks in flight at once; excess deliveries are dropped (logged).
pub const MAX_WEBHOOKS_IN_FLIGHT: usize = 8;

/// Parsed `http://host[:port]/path` webhook target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookTarget {
    /// Host (name or IP literal, IPv6 without brackets).
    pub host: String,
    /// TCP port (default 80).
    pub port: u16,
    /// Absolute path + query.
    pub path: String,
}
/// Parse a webhook URL. Only plain `http://` is supported (no userinfo).
///
/// # Errors
/// A message for unsupported or malformed URLs.
pub fn parse_webhook_url(url: &str) -> Result<WebhookTarget, String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))
        .ok_or("only http:// webhooks are supported")?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    if authority.is_empty() || authority.contains('@') {
        return Err("webhook authority must be host[:port] without credentials".into());
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, tail) = v6.split_once(']').ok_or("unterminated IPv6 literal")?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| "bad port")?,
            None if tail.is_empty() => 80,
            None => return Err("bad authority".into()),
        };
        (host.to_owned(), port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host.to_owned(), port.parse().map_err(|_| "bad port")?)
    } else {
        (authority.to_owned(), 80)
    };
    if host.is_empty() || path.chars().any(|c| c.is_control() || c == ' ') {
        return Err("bad webhook URL".into());
    }
    Ok(WebhookTarget {
        host,
        port,
        path: path.to_owned(),
    })
}

/// Build a non-blocking [`WebhookSink`] that POSTs JSON on the current tokio
/// runtime: resolves the host, screens EVERY resolved address with the SSRF
/// guard (`net_guard::screen_resolved`) under `policy`, dials only vetted
/// addresses, and bounds the whole exchange by [`WEBHOOK_TIMEOUT`]. At most
/// [`MAX_WEBHOOKS_IN_FLIGHT`] run concurrently; excess is dropped.
#[must_use]
pub fn http_webhook_sink(
    policy: Arc<multiview_config::device::net_guard::DialPolicy>,
) -> WebhookSink {
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_WEBHOOKS_IN_FLIGHT));
    Arc::new(move |url: &str, payload: serde_json::Value| {
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!("event automation webhook dropped: too many in flight");
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = url.to_owned();
        let policy = Arc::clone(&policy);
        handle.spawn(async move {
            let _permit = permit;
            let result =
                tokio::time::timeout(WEBHOOK_TIMEOUT, post_webhook(&url, &payload, &policy)).await;
            match result {
                Ok(Ok(status)) => tracing::info!(%url, status, "event automation webhook delivered"),
                Ok(Err(error)) => tracing::warn!(%url, %error, "event automation webhook failed"),
                Err(_) => tracing::warn!(%url, "event automation webhook timed out"),
            }
        });
    })
}

async fn post_webhook(
    url: &str,
    payload: &serde_json::Value,
    policy: &multiview_config::device::net_guard::DialPolicy,
) -> Result<u16, String> {
    use multiview_config::device::net_guard;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = parse_webhook_url(url)?;
    let addrs: Vec<std::net::SocketAddr> = if let Ok(ip) = target.host.parse::<std::net::IpAddr>()
    {
        vec![std::net::SocketAddr::new(ip, target.port)]
    } else {
        tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|e| e.to_string())?
            .collect()
    };
    net_guard::screen_resolved(addrs.iter().map(std::net::SocketAddr::ip), policy)
        .map_err(|e| e.to_string())?;
    let mut stream = tokio::net::TcpStream::connect(addrs.as_slice())
        .await
        .map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let host_header = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {host_header}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nUser-Agent: conspect-event-automation\r\nConnection: close\r\n\r\n",
        target.path,
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).await.map_err(|e| e.to_string())?;
    let mut status = [0u8; 12];
    stream
        .read_exact(&mut status)
        .await
        .map_err(|e| e.to_string())?;
    std::str::from_utf8(status.get(9..12).unwrap_or_default())
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "malformed HTTP status".to_owned())
}
