//! The **LOCAL** support model (Conspect, ADR-0053 §1/§3, brief §10, spec §7/§11):
//! the ticket store, the inbound data-request store, and the tier-derived
//! entitlement routing.
//!
//! Everything here is the **complete local lifecycle** the spec demands now. The
//! portal sync — ticket-thread mirroring and `CS-xxxx` correlation with the
//! account — rides the later server transport (O1-blocked); this local store is
//! the **source** that sync will mirror, not a stub: an operator can raise, read,
//! reply to, and close a ticket entirely locally, and a `CS-xxxx` id is minted
//! here so the correlation is ready the moment the transport lands.
//!
//! # Three local surfaces
//!
//! * [`TicketStore`] — `CS-xxxx`-identified tickets with an append-only update
//!   thread (the opening body + every reply), an auto-attached machine
//!   [`TicketContext`] (identity/version/entitlement/ladder-state/fingerprint
//!   score, §7.1), and a `state` (`open` → `closed`). A reply to a `closed`
//!   ticket is refused ([`ReplyOutcome::Closed`] → `409 ticket_closed`).
//! * [`DataRequestStore`] — **inbound** egress requests awaiting **local
//!   approval**. No data leaves the machine without an explicit local yes
//!   (ADR-0053 §3). Approve/deny are one-way; an elapsed request is `Expired`
//!   (`410 request_expired`). The portal-fed producer is the later transport;
//!   the store + the approve/deny/expire lifecycle are complete now.
//! * [`SupportRoute`] / [`support_route`] — the tier-derived routing the
//!   entitlement endpoint renders (eligible tiers route to a real queue; the
//!   free tier routes to `community` with the spec's one quiet line).
//!
//! # Isolation (invariant #10)
//!
//! Every store holds control-plane state only, behind a short-held `Mutex`. None
//! holds an engine handle, spawns a task on the data plane, or is `.await`ed by
//! the engine — a wedged client of these surfaces cannot back-pressure the
//! engine, and no support action takes a running program off air.
use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use multiview_core::time::MediaTime;

/// The default cap on retained tickets (oldest evicted past this). Bounded so a
/// long-running deployment cannot grow control-plane memory without bound
/// (invariant #10 / brief §16).
pub const DEFAULT_TICKET_CAPACITY: usize = 1_000;

/// The default cap on retained data requests (oldest evicted past this).
pub const DEFAULT_DATA_REQUEST_CAPACITY: usize = 256;

/// Mint a fresh `CS-xxxx`-shaped ticket id (an uppercased short hex of a v4
/// UUID). The shape matches the portal's `CS-` correlation key so the later sync
/// can mirror the local ticket to its portal twin (brief §11 #26–#28).
#[must_use]
fn mint_ticket_id() -> String {
    let id = Uuid::new_v4().simple().to_string();
    let short: String = id.chars().take(8).collect();
    format!("CS-{}", short.to_uppercase())
}

// ── Tickets ───────────────────────────────────────────────────────────────

/// The operator-declared severity of a ticket (spec §11): `question`,
/// `degraded`, `blocking`. Serialised `lowercase` so the slug is stable across
/// the machine and the portal.
///
/// This is a **closed** set the spec pins to exactly these three levels (§11) —
/// deliberately **not** `#[non_exhaustive]` so a consumer (and the contract test)
/// can match it exhaustively. A new severity would be a contract change, not an
/// additive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum TicketSeverity {
    /// A question — no service impact.
    Question,
    /// Service is degraded but program output continues.
    Degraded,
    /// A blocking incident.
    Blocking,
}

/// The lifecycle state of a ticket. Local lifecycle is fully functional: an
/// operator can `close` a ticket they opened; a closed ticket refuses replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TicketState {
    /// Open — replies are accepted.
    Open,
    /// Closed — replies are refused (`409 ticket_closed`).
    Closed,
}

/// One immutable update on a ticket's append-only thread (the opening body or a
/// reply): who wrote it, when, and the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TicketUpdate {
    /// The authenticated principal that wrote this update (its key id), or a
    /// well-known support actor when the portal-fed sync lands later.
    pub author: String,
    /// The media-timeline timestamp (nanoseconds) the update was recorded.
    pub at_nanos: i64,
    /// The update text.
    pub body: String,
}

/// The auto-attached machine context every ticket carries (§7.1): identity,
/// version, entitlement, ladder state, and the fingerprint score. **Reported,
/// never raw** — it carries the opaque tier + the salted fingerprint **score**
/// (a number), never raw serials/MACs (brief §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TicketContext {
    /// The application version (the build identity), always reported.
    pub app_version: String,
    /// The entitlement summary (the opaque tier + whether a lease is licensed).
    pub entitlement: TicketEntitlement,
    /// The enforcement-ladder summary (the computed level), reported as data.
    pub enforcement: TicketEnforcement,
    /// The salted hardware-fingerprint **score** (0–100, brief §2.3/§8) of the
    /// active lease, or `None` when no lease is installed. A number, never raw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint_score: Option<u8>,
}

/// The entitlement summary auto-attached to a ticket — the opaque tier + the
/// licensed flag (rendered, never computed; brief §1, O7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TicketEntitlement {
    /// Whether a verified lease is installed.
    pub licensed: bool,
    /// The opaque commercial tier (rendered), or `none` when unlicensed.
    pub tier: String,
}

/// The enforcement summary auto-attached to a ticket — the computed ladder level
/// (data, never control flow; brief §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TicketEnforcement {
    /// The canonical enforcement level slug (e.g. `active`, `unlicensed`).
    pub level: String,
}

/// A ticket: a `CS-xxxx` id, the subject + severity, the auto-attached machine
/// context, an append-only update thread (opening body first), and the route +
/// state. The portal sync mirrors this local resource over the later transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Ticket {
    /// The `CS-xxxx` ticket id (the portal correlation key).
    pub ticket_id: String,
    /// The operator-supplied subject line.
    pub subject: String,
    /// The declared severity.
    pub severity: TicketSeverity,
    /// The lifecycle state (`open` → `closed`).
    pub state: TicketState,
    /// The tier-derived support route this ticket was raised against.
    pub route: SupportRoute,
    /// The auto-attached machine context (§7.1).
    pub context: TicketContext,
    /// The append-only thread: the opening body, then every reply.
    pub updates: Vec<TicketUpdate>,
}

/// A ticket summary for the list surface (the thread is omitted to keep the
/// listing light; fetch a single ticket for the full thread).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TicketSummary {
    /// The `CS-xxxx` ticket id.
    pub ticket_id: String,
    /// The subject line.
    pub subject: String,
    /// The declared severity.
    pub severity: TicketSeverity,
    /// The lifecycle state.
    pub state: TicketState,
    /// The number of updates on the thread.
    pub updates: usize,
}

impl From<&Ticket> for TicketSummary {
    fn from(t: &Ticket) -> Self {
        Self {
            ticket_id: t.ticket_id.clone(),
            subject: t.subject.clone(),
            severity: t.severity,
            state: t.state,
            updates: t.updates.len(),
        }
    }
}

/// The outcome of a [`TicketStore::reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyOutcome {
    /// The reply appended; the updated ticket is returned.
    Appended(Box<Ticket>),
    /// The ticket is closed — the reply is refused (`409 ticket_closed`).
    Closed,
    /// No ticket has that id.
    NotFound,
}

/// The outcome of a [`TicketStore::close`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The ticket was open and is now closed.
    Closed,
    /// The ticket was already closed (idempotent).
    AlreadyClosed,
    /// No ticket has that id.
    NotFound,
}

/// The data a [`TicketStore::raise`] needs to mint a ticket — grouped so the
/// raise seam stays a two-argument call (the new ticket + the instant) rather
/// than a long positional parameter list. The store mints the `CS-xxxx` id and
/// stamps `state = Open` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTicket {
    /// The authenticated principal raising the ticket (its key id), recorded as
    /// the opening update's author.
    pub author: String,
    /// The operator-supplied subject line.
    pub subject: String,
    /// The opening body — seeds the append-only thread.
    pub body: String,
    /// The declared severity.
    pub severity: TicketSeverity,
    /// The auto-attached machine context (§7.1).
    pub context: TicketContext,
    /// The tier-derived support route this ticket is raised against.
    pub route: SupportRoute,
}

/// Append + query + transition access to the local ticket store. Append-only
/// thread: there is no edit/delete of an update, only `raise`, `reply`, `close`,
/// and reads.
pub trait TicketStore: Send + Sync + 'static {
    /// Raise a new ticket from `new`, recorded at `at`. The store mints the
    /// `CS-xxxx` id, seeds the thread with the opening body, and stamps the
    /// auto-attached context + route. Returns the created ticket.
    fn raise(&self, new: NewTicket, at: MediaTime) -> Ticket;

    /// List ticket summaries, newest-first.
    fn list(&self) -> Vec<TicketSummary>;

    /// Fetch one ticket (full thread) by id, or `None` if unknown.
    fn get(&self, ticket_id: &str) -> Option<Ticket>;

    /// Append a reply to a ticket's thread. See [`ReplyOutcome`].
    fn reply(&self, ticket_id: &str, author: &str, body: String, at: MediaTime) -> ReplyOutcome;

    /// Close a ticket locally. See [`CloseOutcome`].
    fn close(&self, ticket_id: &str) -> CloseOutcome;
}

/// Alias matching the crate's `*Repository` naming for trait objects in state.
pub use TicketStore as TicketRepository;

/// An in-memory, bounded ticket store.
#[derive(Debug)]
pub struct InMemoryTickets {
    capacity: usize,
    tickets: Mutex<VecDeque<Ticket>>,
}

impl Default for InMemoryTickets {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_TICKET_CAPACITY)
    }
}

impl InMemoryTickets {
    /// A fresh, empty store at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty store retaining at most `capacity` newest tickets.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            tickets: Mutex::new(VecDeque::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Ticket>> {
        self.tickets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TicketStore for InMemoryTickets {
    fn raise(&self, new: NewTicket, at: MediaTime) -> Ticket {
        let NewTicket {
            author,
            subject,
            body,
            severity,
            context,
            route,
        } = new;
        let ticket = Ticket {
            ticket_id: mint_ticket_id(),
            subject,
            severity,
            state: TicketState::Open,
            route,
            context,
            updates: vec![TicketUpdate {
                author,
                at_nanos: at.as_nanos(),
                body,
            }],
        };
        let mut guard = self.lock();
        guard.push_back(ticket.clone());
        while guard.len() > self.capacity {
            guard.pop_front();
        }
        ticket
    }

    fn list(&self) -> Vec<TicketSummary> {
        self.lock().iter().rev().map(TicketSummary::from).collect()
    }

    fn get(&self, ticket_id: &str) -> Option<Ticket> {
        self.lock()
            .iter()
            .find(|t| t.ticket_id == ticket_id)
            .cloned()
    }

    fn reply(&self, ticket_id: &str, author: &str, body: String, at: MediaTime) -> ReplyOutcome {
        let mut guard = self.lock();
        let Some(ticket) = guard.iter_mut().find(|t| t.ticket_id == ticket_id) else {
            return ReplyOutcome::NotFound;
        };
        if ticket.state == TicketState::Closed {
            return ReplyOutcome::Closed;
        }
        ticket.updates.push(TicketUpdate {
            author: author.to_owned(),
            at_nanos: at.as_nanos(),
            body,
        });
        ReplyOutcome::Appended(Box::new(ticket.clone()))
    }

    fn close(&self, ticket_id: &str) -> CloseOutcome {
        let mut guard = self.lock();
        let Some(ticket) = guard.iter_mut().find(|t| t.ticket_id == ticket_id) else {
            return CloseOutcome::NotFound;
        };
        match ticket.state {
            TicketState::Open => {
                ticket.state = TicketState::Closed;
                CloseOutcome::Closed
            }
            TicketState::Closed => CloseOutcome::AlreadyClosed,
        }
    }
}

// ── Entitlement routing ─────────────────────────────────────────────────────

/// The first-line support route a tier maps to (spec §11): a real support queue
/// for eligible tiers, or the community channel for the free tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FirstLine {
    /// A partner/reseller handles first-line support (partner-attributed tiers).
    Partner,
    /// Aperim's Conspect support queue handles first-line support.
    Conspect,
    /// The community channel (the free tier; not an entitled support queue).
    Community,
}

impl FirstLine {
    /// Whether this first-line route is an **entitled** support queue (i.e. a
    /// ticket may be raised against it). The community channel is not entitled.
    #[must_use]
    pub const fn is_entitled(self) -> bool {
        matches!(self, FirstLine::Partner | FirstLine::Conspect)
    }
}

/// The tier-derived support route the entitlement endpoint renders + a ticket is
/// raised against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SupportRoute {
    /// The destination queue label (a stable slug the portal shares).
    pub to: String,
    /// The first-line owner.
    pub first_line: FirstLine,
}

/// The full entitlement-routing answer the `GET /support/entitlement` endpoint
/// returns: whether the machine is eligible for entitled support, the route, and
/// the SLA token (a quiet line for the free tier).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SupportEntitlement {
    /// Whether the machine is eligible for an entitled support queue (false →
    /// community only).
    pub eligible: bool,
    /// The tier-derived route.
    pub route: SupportRoute,
    /// The SLA token (e.g. `standard`, or the free tier's `community-best-effort`
    /// one-line note). Always present so the surface is uniform.
    pub sla: String,
}

/// Derive the support entitlement from the opaque tier (O7: tier is opaque, we
/// map it, we never compute its commercial semantics). The eligible tiers are
/// `studio`, `broadcast`, and `evaluation`; everything else (including the
/// absence of a lease) is the free tier → community, not eligible, with the
/// spec's one quiet line.
///
/// `tier` is `None` when no lease is installed (the unlicensed / free machine).
#[must_use]
pub fn support_entitlement(tier: Option<&str>) -> SupportEntitlement {
    let route = support_route(tier);
    let eligible = route.first_line.is_entitled();
    let sla = if eligible {
        // Eligible tiers carry a standard response SLA token (the portal owns the
        // concrete hours; the machine renders an opaque token, like the tier).
        "standard".to_owned()
    } else {
        // The spec's one quiet line for the free tier (community, best-effort).
        "community-best-effort".to_owned()
    };
    SupportEntitlement {
        eligible,
        route,
        sla,
    }
}

/// Map the opaque tier to its first-line route (the routing half of
/// [`support_entitlement`]). Eligible tiers route to a real queue; everything
/// else routes to `community`.
#[must_use]
pub fn support_route(tier: Option<&str>) -> SupportRoute {
    match tier.map(str::to_ascii_lowercase).as_deref() {
        // Partner-attributed tiers route to the partner first-line. (Partner
        // attribution is opaque, O4 — a `partner-`-prefixed tier slug is the
        // machine-visible signal the portal sets; we route on it, never compute
        // the commercial partner model.)
        Some(t) if t.starts_with("partner") => SupportRoute {
            to: "partner".to_owned(),
            first_line: FirstLine::Partner,
        },
        // The directly-entitled tiers route to the Conspect queue.
        Some("studio" | "broadcast" | "evaluation") => SupportRoute {
            to: "conspect".to_owned(),
            first_line: FirstLine::Conspect,
        },
        // The free tier (or any unrecognised/absent tier) routes to community.
        _ => SupportRoute {
            to: "community".to_owned(),
            first_line: FirstLine::Community,
        },
    }
}

// ── Data requests ───────────────────────────────────────────────────────────

/// The lifecycle state of an inbound data request. Approve/deny are one-way; an
/// elapsed (un-actioned) request is `Expired` (`410 request_expired`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DataRequestState {
    /// Awaiting local approval/denial — nothing has left.
    Pending,
    /// Approved locally — egress of the requested data is now permitted.
    Approved,
    /// Denied locally — nothing leaves.
    Denied,
    /// The request window elapsed before the operator actioned it.
    Expired,
}

/// One inbound data request awaiting **local approval**: support (or the licence
/// server, over the later transport) asks for additional data; nothing leaves
/// until the operator approves it here (ADR-0053 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DataRequest {
    /// The request id (the `DR-xxxx` / portal correlation key when synced).
    pub request_id: String,
    /// A human-readable description of what is being requested (never the data
    /// itself — this is the *ask*, not the payload).
    pub what: String,
    /// The media-timeline timestamp (nanoseconds) the request was recorded.
    pub requested_at_nanos: i64,
    /// The lifecycle state.
    pub state: DataRequestState,
    /// An optional structured detail (e.g. the ticket the request relates to).
    /// Never the requested data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl DataRequest {
    /// A fresh `Pending` data request.
    #[must_use]
    pub fn new(
        request_id: String,
        what: String,
        at: MediaTime,
        detail: Option<serde_json::Value>,
    ) -> Self {
        Self {
            request_id,
            what,
            requested_at_nanos: at.as_nanos(),
            state: DataRequestState::Pending,
            detail,
        }
    }
}

/// The outcome of a local approve/deny on a data request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRequestOutcome {
    /// The request was `Pending` and is now the requested terminal state.
    Actioned(DataRequestState),
    /// The request had already expired — too late (`410 request_expired`).
    Expired,
    /// The request was already approved/denied (idempotent re-action returns the
    /// existing terminal state).
    AlreadyActioned(DataRequestState),
    /// No request has that id.
    NotFound,
}

/// Append + query + transition access to the inbound data-request store. Approve
/// and deny are one-way into a terminal state; there is no edit/delete.
pub trait DataRequestStore: Send + Sync + 'static {
    /// Enqueue an inbound data request (seeded locally now; portal-fed later).
    fn enqueue(&self, request: DataRequest);

    /// List the **pending** data requests, oldest-first (the approval surface).
    fn list_pending(&self) -> Vec<DataRequest>;

    /// Fetch one request (any state) by id, or `None` if unknown.
    fn get(&self, request_id: &str) -> Option<DataRequest>;

    /// Approve a pending request **locally** — egress is gated on this yes.
    fn approve(&self, request_id: &str) -> DataRequestOutcome;

    /// Deny a pending request locally — nothing leaves.
    fn deny(&self, request_id: &str) -> DataRequestOutcome;
}

/// Alias matching the crate's `*Repository` naming for trait objects in state.
pub use DataRequestStore as DataRequestRepository;

/// An in-memory, bounded data-request store.
#[derive(Debug)]
pub struct InMemoryDataRequests {
    capacity: usize,
    requests: Mutex<VecDeque<DataRequest>>,
}

impl Default for InMemoryDataRequests {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_DATA_REQUEST_CAPACITY)
    }
}

impl InMemoryDataRequests {
    /// A fresh, empty store at the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh, empty store retaining at most `capacity` newest requests.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            requests: Mutex::new(VecDeque::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<DataRequest>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The shared transition logic for approve/deny: a `Pending` request moves to
    /// `target`; an `Expired` request is too late; an already-terminal request is
    /// idempotently its existing state.
    fn transition(&self, request_id: &str, target: DataRequestState) -> DataRequestOutcome {
        let mut guard = self.lock();
        let Some(request) = guard.iter_mut().find(|r| r.request_id == request_id) else {
            return DataRequestOutcome::NotFound;
        };
        match request.state {
            DataRequestState::Pending => {
                request.state = target;
                DataRequestOutcome::Actioned(target)
            }
            DataRequestState::Expired => DataRequestOutcome::Expired,
            terminal @ (DataRequestState::Approved | DataRequestState::Denied) => {
                DataRequestOutcome::AlreadyActioned(terminal)
            }
        }
    }
}

impl DataRequestStore for InMemoryDataRequests {
    fn enqueue(&self, request: DataRequest) {
        let mut guard = self.lock();
        guard.push_back(request);
        while guard.len() > self.capacity {
            guard.pop_front();
        }
    }

    fn list_pending(&self) -> Vec<DataRequest> {
        self.lock()
            .iter()
            .filter(|r| r.state == DataRequestState::Pending)
            .cloned()
            .collect()
    }

    fn get(&self, request_id: &str) -> Option<DataRequest> {
        self.lock()
            .iter()
            .find(|r| r.request_id == request_id)
            .cloned()
    }

    fn approve(&self, request_id: &str) -> DataRequestOutcome {
        self.transition(request_id, DataRequestState::Approved)
    }

    fn deny(&self, request_id: &str) -> DataRequestOutcome {
        self.transition(request_id, DataRequestState::Denied)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        support_entitlement, support_route, CloseOutcome, DataRequest, DataRequestOutcome,
        DataRequestState, DataRequestStore, FirstLine, InMemoryDataRequests, InMemoryTickets,
        NewTicket, ReplyOutcome, TicketContext, TicketEnforcement, TicketEntitlement,
        TicketSeverity, TicketState, TicketStore,
    };
    use multiview_core::time::MediaTime;

    fn ctx() -> TicketContext {
        TicketContext {
            app_version: "test".to_owned(),
            entitlement: TicketEntitlement {
                licensed: true,
                tier: "studio".to_owned(),
            },
            enforcement: TicketEnforcement {
                level: "active".to_owned(),
            },
            fingerprint_score: Some(100),
        }
    }

    fn new_ticket(severity: TicketSeverity) -> NewTicket {
        NewTicket {
            author: "op".to_owned(),
            subject: "s".to_owned(),
            body: "b".to_owned(),
            severity,
            context: ctx(),
            route: support_route(Some("studio")),
        }
    }

    #[test]
    fn ticket_ids_use_the_cs_shape() {
        let store = InMemoryTickets::new();
        let t = store.raise(
            new_ticket(TicketSeverity::Question),
            MediaTime::from_nanos(1),
        );
        assert!(t.ticket_id.starts_with("CS-"), "got {}", t.ticket_id);
        assert_eq!(t.updates.len(), 1, "the opening body seeds the thread");
        assert_eq!(t.state, TicketState::Open);
    }

    #[test]
    fn reply_appends_and_reply_on_closed_is_refused() {
        let store = InMemoryTickets::new();
        let t = store.raise(
            new_ticket(TicketSeverity::Blocking),
            MediaTime::from_nanos(1),
        );
        let id = t.ticket_id;
        match store.reply(&id, "op", "more".to_owned(), MediaTime::from_nanos(2)) {
            ReplyOutcome::Appended(updated) => assert_eq!(updated.updates.len(), 2),
            other => panic!("expected Appended, got {other:?}"),
        }
        assert_eq!(store.close(&id), CloseOutcome::Closed);
        assert_eq!(store.close(&id), CloseOutcome::AlreadyClosed);
        assert_eq!(
            store.reply(&id, "op", "again".to_owned(), MediaTime::from_nanos(3)),
            ReplyOutcome::Closed,
            "a closed ticket refuses replies"
        );
        assert_eq!(
            store.reply("CS-NOPE", "op", "x".to_owned(), MediaTime::from_nanos(4)),
            ReplyOutcome::NotFound
        );
    }

    #[test]
    fn entitlement_routing_maps_tiers_exactly() {
        // Eligible tiers route to a real queue and are eligible.
        for tier in ["studio", "broadcast", "evaluation", "STUDIO"] {
            let e = support_entitlement(Some(tier));
            assert!(e.eligible, "{tier} eligible");
            assert_eq!(e.route.first_line, FirstLine::Conspect);
            assert_eq!(e.sla, "standard");
        }
        // A partner-attributed tier routes to the partner first-line.
        let p = support_entitlement(Some("partner-acme"));
        assert!(p.eligible);
        assert_eq!(p.route.first_line, FirstLine::Partner);
        // The free tier (or none) routes to community, not eligible, quiet line.
        for tier in [Some("free"), Some("hobby"), None] {
            let e = support_entitlement(tier);
            assert!(!e.eligible, "{tier:?} not eligible");
            assert_eq!(e.route.first_line, FirstLine::Community);
            assert_eq!(e.sla, "community-best-effort");
        }
    }

    #[test]
    fn data_request_approve_and_deny_are_one_way_and_expiry_is_too_late() {
        let store = InMemoryDataRequests::new();
        store.enqueue(DataRequest::new(
            "DR-1".to_owned(),
            "logs".to_owned(),
            MediaTime::from_nanos(0),
            None,
        ));
        assert_eq!(
            store.approve("DR-1"),
            DataRequestOutcome::Actioned(DataRequestState::Approved)
        );
        // Re-approve / deny after a terminal state is idempotent, never a new yes.
        assert_eq!(
            store.deny("DR-1"),
            DataRequestOutcome::AlreadyActioned(DataRequestState::Approved)
        );
        assert_eq!(store.get("DR-1").unwrap().state, DataRequestState::Approved);

        // An expired request is too late — nothing leaves.
        let mut expired = DataRequest::new(
            "DR-2".to_owned(),
            "x".to_owned(),
            MediaTime::from_nanos(0),
            None,
        );
        expired.state = DataRequestState::Expired;
        store.enqueue(expired);
        assert_eq!(store.approve("DR-2"), DataRequestOutcome::Expired);
        assert_eq!(store.get("DR-2").unwrap().state, DataRequestState::Expired);

        assert_eq!(store.approve("DR-missing"), DataRequestOutcome::NotFound);
    }

    #[test]
    fn list_pending_shows_only_pending() {
        let store = InMemoryDataRequests::new();
        store.enqueue(DataRequest::new(
            "DR-1".to_owned(),
            "a".to_owned(),
            MediaTime::from_nanos(0),
            None,
        ));
        store.enqueue(DataRequest::new(
            "DR-2".to_owned(),
            "b".to_owned(),
            MediaTime::from_nanos(0),
            None,
        ));
        store.approve("DR-1");
        let pending = store.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, "DR-2");
    }
}
