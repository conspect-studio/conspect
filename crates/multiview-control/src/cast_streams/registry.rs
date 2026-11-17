//! The bounded registry state behind [`super::CastStreams`] (no clock, no
//! locking: the caller passes `now` and holds the one mutex).

use std::collections::HashMap;

use super::token::{self, Pepper, Seed, TokenDigest};
use super::{
    CastFetches, ReceiverFetch, ReceiverReport, IDLE_TTL, LINGER, MAX_LIFETIME, PENDING_TTL,
};
use crate::output_routing::RoutedFeed;

fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One stream. Holds the token's digest and seed, never the token.
#[derive(Debug)]
pub(super) struct Entry {
    pub id: String,
    pub owner: String,
    /// The cast output's API id and config id.
    pub output: String,
    pub config_id: String,
    pub feed: RoutedFeed,
    pub seed: Seed,
    pub digest: TokenDigest,
    pub created_ms: u64,
    /// The last successful playlist fetch (`None`: still pending).
    pub last_playlist_ms: Option<u64>,
    pub fetches: CastFetches,
    pub last_report: Option<ReceiverReport>,
    /// The owner as authenticated at creation (ABUSE-1): the fallback for an
    /// owner the API-key store does not hold (a JWT subject, the local
    /// admin); a store key is always re-resolved from the store.
    pub principal: Option<crate::auth::Principal>,
    /// How the owner authenticated at creation (ABUSE-3): decides how the
    /// re-authorization resolves it. `None` (never bound) fails closed.
    pub origin: Option<super::AuthOrigin>,
    /// The output authorization generation this token is authorized for
    /// (ABUSE-1); `None` until it is armed. Media is served only while it
    /// equals its output's [`Gate::generation`].
    pub generation: Option<u64>,
    /// The ladder call's state: rate, last probe, transport (ADR-0114).
    pub ladder: super::ladder::LadderState,
}

impl Entry {
    pub(super) fn expires_ms(&self) -> u64 {
        let hard = self.created_ms.saturating_add(ms(MAX_LIFETIME));
        let soft = match self.last_playlist_ms {
            None => self.created_ms.saturating_add(ms(PENDING_TTL)),
            Some(at) => at.saturating_add(ms(IDLE_TTL)),
        };
        hard.min(soft)
    }

    pub(super) fn expired(&self, now: u64) -> bool {
        now >= self.expires_ms()
    }
}

/// A cast output the registry has streams on (or lingers after its last).
#[derive(Debug, Clone)]
pub(super) struct Slot {
    pub api_id: String,
    pub feed: RoutedFeed,
    /// The cast API took this output to its feed (take back on end).
    pub moved: bool,
    /// When its last stream ended (lingering), `None` while it has streams.
    pub empty_since: Option<u64>,
}

/// A take the control plane committed on an output and the engine has not
/// settled yet (ABUSE-1): a feed the output may carry from now on.
#[derive(Debug, Clone)]
pub(super) struct Carry {
    /// The take's operation id (`None`: an unrequested move the control
    /// plane caused, e.g. a deleted layout falling back to Program).
    pub op: Option<String>,
    /// Its picture bus (`None`: the picture is unchanged, an audio take).
    pub bus: Option<String>,
    /// The grants it requires (`None`: undeterminable, fails closed).
    pub needs: Option<Vec<super::reauth::Need>>,
}

/// What the cast engine reported about one output's sink when a grant set
/// was superseded (#418 review ABUSE-HISTORY-R4): the baseline its
/// retirement is proven against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SinkMark {
    /// A cast stream is active (an inactive output serves no media).
    pub active: bool,
    /// The generation in the names of the files being served.
    pub generation: u64,
    /// Video segments published for that generation.
    pub video_segments: u64,
    /// The output is disabled for the run (#418 review r7): files it could
    /// not remove may be left, so nothing is proven replaced through it.
    pub disabled: bool,
}

impl SinkMark {
    /// The mark of an engine status.
    pub(crate) fn of(status: &crate::cast_engine::CastOutputStatus) -> Self {
        Self {
            active: status.active,
            generation: status.generation,
            video_segments: status.video_segments,
            disabled: status.disabled,
        }
    }

    /// Whether `now` (the output's current status) proves that nothing
    /// servable at this mark is servable any more: the output is inactive,
    /// or serves another generation (new file names; the old ones are
    /// purged and never served: `routes::cast_public`), or published
    /// [`super::RETIRE_SEGMENTS`] more video segments of this generation
    /// (every file of the marked era left the packager's retention). Never
    /// while the output is disabled (#418 review r7: a generation change or
    /// an inactive output proves the old files gone only when their purge
    /// succeeded; a failed purge disables the output).
    fn replaced_by(&self, now: &Self) -> bool {
        if now.disabled {
            return false;
        }
        !now.active
            || now.generation != self.generation
            || now.video_segments >= self.video_segments.saturating_add(super::RETIRE_SEGMENTS)
    }

    /// One mark at least as late as both (an overflow merge): replaced only
    /// when both are. Generations only ever increase, so the later
    /// generation's mark implies the earlier one's.
    fn later(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        let (a, b) = (a?, b?);
        Some(match a.generation.cmp(&b.generation) {
            std::cmp::Ordering::Less => b,
            std::cmp::Ordering::Greater => a,
            std::cmp::Ordering::Equal => Self {
                active: a.active || b.active,
                generation: a.generation,
                video_segments: a.video_segments.max(b.video_segments),
                disabled: a.disabled || b.disabled,
            },
        })
    }
}

/// The grants of a superseded feed whose media may still be servable
/// (#418 review ABUSE-HISTORY-R4).
#[derive(Debug, Clone)]
pub(super) struct Superseded {
    /// Unix ms before which it never expires (superseded +
    /// [`super::MEDIA_RETENTION_MS`]).
    pub not_before: u64,
    /// The output's sink when it was superseded (`None`: the engine status
    /// was unavailable; it then expires only on an inactive output).
    pub mark: Option<SinkMark>,
    /// The grants (`None`: undeterminable, fails closed).
    pub needs: Option<Vec<super::reauth::Need>>,
}

impl Superseded {
    /// Whether it may be forgotten at `now` with the output's sink at
    /// `sink` (`None`: the engine status is unavailable — fail closed:
    /// never). BOTH the retention must have elapsed AND the sink must have
    /// provably replaced everything servable from the time it was
    /// superseded: a stalled sink keeps it forever.
    fn expired(&self, now: u64, sink: Option<&SinkMark>) -> bool {
        let Some(sink) = sink else {
            return false;
        };
        let replaced = match &self.mark {
            Some(mark) => mark.replaced_by(sink),
            None => !sink.active && !sink.disabled,
        };
        now >= self.not_before && replaced
    }
}

/// The authorization gate of one output (ABUSE-1). Invariant: every token
/// armed at `generation` passed the grants of the `settled` feed, of every
/// carried take and of every `history` entry, so the media of any bus
/// [`Gate::clears`] (and any retained byte of a superseded feed) may be
/// served to it; anything else bumps the generation and re-authorizes every
/// token first, in the same critical section.
#[derive(Debug, Clone, Default)]
pub(super) struct Gate {
    pub generation: u64,
    /// Committed, unsettled changes, oldest first (at most
    /// [`super::MAX_CARRY`]).
    pub carry: std::collections::VecDeque<Carry>,
    /// The bus the output was last observed on and its grants.
    pub settled: Option<(String, Option<Vec<super::reauth::Need>>)>,
    /// #418 review ABUSE-HISTORY-R4: the grants of superseded feeds (a
    /// replaced `settled`, a settled or forgotten carry) whose media may
    /// still be servable; oldest first, at most [`super::MAX_HISTORY`] (an
    /// overflow merges into the oldest entry, keeping the later retention
    /// and sink mark: a need is never dropped early), holding at most
    /// [`super::MAX_HISTORY_NEEDS`] distinct grants (past it: one
    /// undeterminable entry, fail closed). Forgotten only by
    /// [`Gate::prune`], once [`Superseded::expired`].
    pub history: std::collections::VecDeque<Superseded>,
}

/// The union of two grant sets (`None` anywhere: undeterminable).
fn union(
    a: Option<Vec<super::reauth::Need>>,
    b: Option<Vec<super::reauth::Need>>,
) -> Option<Vec<super::reauth::Need>> {
    let (mut a, b) = (a?, b?);
    for need in b {
        if !a.contains(&need) {
            a.push(need);
        }
    }
    Some(a)
}

impl Gate {
    /// Whether the picture bus `bus` is cleared for every armed token.
    pub(super) fn clears(&self, bus: &str) -> bool {
        self.settled.as_ref().is_some_and(|(b, _)| b == bus)
            || self.carry.iter().any(|c| c.bus.as_deref() == Some(bus))
    }

    /// Every grant a new token must hold: the settled feed's, every
    /// carried take's and every superseded feed's still in the history
    /// (`None` anywhere: undeterminable). Call [`Gate::prune`] first.
    /// Forgetting a carried take (over [`super::MAX_CARRY`]) moves it to
    /// the history, so nothing is narrowed while its bytes stay servable.
    pub(super) fn cleared_needs(&self) -> Option<Vec<super::reauth::Need>> {
        let mut all = Vec::new();
        let settled = self.settled.as_ref().map(|(_, needs)| needs);
        for needs in settled
            .into_iter()
            .chain(self.carry.iter().map(|c| &c.needs))
            .chain(self.history.iter().map(|h| &h.needs))
        {
            for need in needs.as_ref()? {
                if !all.contains(need) {
                    all.push(need.clone());
                }
            }
        }
        Some(all)
    }

    /// Forget the history entries expired at `now` with the output's sink
    /// at `sink` (see [`Superseded::expired`]; lazy: wherever both are at
    /// hand). Only narrows what a new token must hold.
    pub(super) fn prune(&mut self, now: u64, sink: Option<&SinkMark>) {
        self.history.retain(|h| !h.expired(now, sink));
    }

    /// Whether any history entry is left.
    pub(super) fn remembers(&self) -> bool {
        !self.history.is_empty()
    }

    /// Record superseded `needs` whose media may stay servable, with the
    /// sink's current status `sink` as the baseline of their retirement.
    /// Bounded: past [`super::MAX_HISTORY`] they merge into the oldest
    /// entry, which keeps the later retention and mark, so no need ever
    /// expires early.
    pub(super) fn retire(
        &mut self,
        needs: Option<Vec<super::reauth::Need>>,
        now: u64,
        sink: Option<&SinkMark>,
    ) {
        self.prune(now, sink);
        let entry = Superseded {
            not_before: now.saturating_add(super::MEDIA_RETENTION_MS),
            mark: sink.copied(),
            needs,
        };
        if self.history.len() >= super::MAX_HISTORY {
            if let Some(old) = self.history.pop_front() {
                self.history.push_front(Superseded {
                    not_before: old.not_before.max(entry.not_before),
                    mark: SinkMark::later(old.mark, entry.mark),
                    needs: union(old.needs, entry.needs),
                });
            }
        } else {
            self.history.push_back(entry);
        }
        self.bound_history();
    }

    /// #418 review INV418-FINAL-1: past [`super::MAX_HISTORY_NEEDS`]
    /// distinct grants across the whole history, collapse it to ONE entry
    /// with undeterminable grants (`needs: None`, which fails closed:
    /// [`Gate::cleared_needs`] is `None`, so new tokens are refused and
    /// rechecks revoke) and the latest retention and sink mark of them all,
    /// so it expires only once every collapsed entry could have.
    fn bound_history(&mut self) {
        let mut distinct: Vec<&super::reauth::Need> = Vec::new();
        let over = self
            .history
            .iter()
            .filter_map(|h| h.needs.as_ref())
            .flatten()
            .any(|need| {
                if !distinct.contains(&need) {
                    distinct.push(need);
                }
                distinct.len() > super::MAX_HISTORY_NEEDS
            });
        if !over {
            return;
        }
        let collapsed = self.history.drain(..).reduce(|a, b| Superseded {
            not_before: a.not_before.max(b.not_before),
            mark: SinkMark::later(a.mark, b.mark),
            needs: None,
        });
        if let Some(mut entry) = collapsed {
            entry.needs = None;
            self.history.push_back(entry);
        }
    }

    /// Replace the settled feed with `next`; the grants it replaced move to
    /// the history (unless unchanged).
    pub(super) fn replace_settled(
        &mut self,
        next: (String, Option<Vec<super::reauth::Need>>),
        now: u64,
        sink: Option<&SinkMark>,
    ) {
        if let Some((_, old)) = self.settled.replace(next) {
            let unchanged = self.settled.as_ref().is_some_and(|(_, new)| *new == old);
            if !unchanged {
                self.retire(old, now, sink);
            }
        }
    }

    /// Settle through the take `op` (and every older one) and every
    /// control-plane move without an operation id (its bus is the one the
    /// outcome reports, or it no longer is on air). The settled carries
    /// move to the history: their media may still be servable.
    pub(super) fn settle(&mut self, op: Option<&str>, now: u64, sink: Option<&SinkMark>) {
        let mut gone = Vec::new();
        if let Some(at) =
            op.and_then(|op| self.carry.iter().position(|c| c.op.as_deref() == Some(op)))
        {
            gone.extend(self.carry.drain(..=at));
        }
        let (keep, unrequested): (Vec<Carry>, Vec<Carry>) =
            self.carry.drain(..).partition(|c| c.op.is_some());
        self.carry = keep.into();
        gone.extend(unrequested);
        for carry in gone {
            self.retire(carry.needs, now, sink);
        }
    }
}

/// A stream revoked by the server (not by its owner): kept, bounded, so its
/// owner's `GET` can say why the token stopped working.
#[derive(Debug, Clone)]
pub(super) struct Revoked {
    pub id: String,
    pub owner: String,
    pub reason: String,
    pub at_ms: u64,
}

/// An output whose last stream ended and whose linger is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// The output's API id.
    pub output: String,
    /// The output's config id (the engine id).
    pub config_id: String,
    /// The cast API moved its feed: take it back to Program.
    pub moved: bool,
}

/// A new stream.
#[derive(Debug)]
pub struct Claim {
    /// The stream id.
    pub id: String,
    /// The token (plaintext; for the owner only).
    pub token: String,
    /// No stream was on this output and it was not lingering: activate it.
    pub first: bool,
}

/// Why a claim failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// [`MAX_STREAMS`] reached.
    Capacity,
    /// [`MAX_STREAMS_PER_PRINCIPAL`] reached.
    PrincipalCapacity,
    /// The output was expected free and is not (a concurrent claim won).
    Taken,
}

/// Why a receiver report was not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportError {
    /// No live stream for this token.
    Unknown,
    /// Within [`super::REPORT_INTERVAL`] of the previous report.
    TooSoon,
}

/// The public view of one stream (no token).
#[derive(Debug, Clone)]
pub struct StreamView {
    /// Stream id.
    pub id: String,
    /// Owner key id.
    pub owner: String,
    /// The cast output's API id and config id.
    pub output: String,
    /// The cast output's config id.
    pub config_id: String,
    /// The feed it was created for.
    pub feed: RoutedFeed,
    /// Expiry (Unix ms) as of now.
    pub expires_unix_ms: u64,
    /// A playlist was fetched (`Live`).
    pub live: bool,
    /// Fetch counters.
    pub fetches: CastFetches,
    /// The last receiver report.
    pub last_report: Option<ReceiverReport>,
    /// The receiver's last capability probe (ADR-0114).
    pub probe: Option<super::ladder::ReceiverProbe>,
    /// The rung it plays and how it fares (ADR-0114).
    pub transport: Option<super::ladder::Transport>,
}

/// What the shared token check admits (ADR-0114 decision 5.1): a live token
/// armed at its output's CURRENT authorization generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    /// The stream id.
    pub stream_id: String,
    /// The cast output's config id.
    pub config_id: String,
    /// The authorization generation it is armed at.
    pub generation: u64,
}

/// Why a ladder call was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderError {
    /// No live stream for this token.
    Unknown,
    /// Over the per-token rate.
    TooSoon,
}

#[derive(Debug)]
pub(super) struct Registry {
    pub pepper: Pepper,
    pub entries: Vec<Entry>,
    pub slots: HashMap<String, Slot>,
    /// Principal -> (tokens, last refill ms).
    pub limiter: HashMap<String, (u32, u64)>,
    pub receiver_last_fetch: Option<ReceiverFetch>,
    /// Report rate per stream id: last accepted (ms).
    pub last_report_ms: HashMap<String, u64>,
    /// Server-side revocations, oldest first (at most
    /// [`super::MAX_REVOKED`]).
    pub revoked: std::collections::VecDeque<Revoked>,
    /// Per output config id: the authorization gate (bounded by the
    /// configured outputs; at most [`super::MAX_GATES`]).
    pub gates: HashMap<String, Gate>,
    /// The last generation handed out (one counter for every output, so a
    /// forgotten and re-created gate never repeats a generation).
    pub last_generation: u64,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            pepper: Pepper::random(),
            entries: Vec::new(),
            slots: HashMap::new(),
            limiter: HashMap::new(),
            receiver_last_fetch: None,
            last_report_ms: HashMap::new(),
            revoked: std::collections::VecDeque::new(),
            gates: HashMap::new(),
            last_generation: 0,
        }
    }
}

impl Registry {
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The index of the live entry `token` names. Every digest is compared
    /// (constant time, no early exit), so timing tells nothing.
    pub(super) fn find(&self, token: &str, now: u64) -> Option<usize> {
        let digest = TokenDigest::of(token);
        let mut found = None;
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.digest.ct_matches(&digest) {
                found = Some(i);
            }
        }
        let i = found?;
        (!self.entries.get(i)?.expired(now)).then_some(i)
    }

    /// Whether `entry` may be served now: armed at its output's current
    /// generation (no gate: nothing was ever armed there).
    pub(super) fn servable(&self, entry: &Entry) -> bool {
        entry.generation.is_some()
            && entry.generation == self.gates.get(&entry.config_id).map(|g| g.generation)
    }

    /// The gate of `config_id`, created (at a fresh generation) on first
    /// use. Gates exist only for outputs with streams or a history
    /// (the sweep drops the others); creation is
    /// additionally refused beyond [`super::MAX_GATES`] (`None`), which
    /// fails closed: an output without a gate never serves a token
    /// ([`Self::servable`]) and no token can be armed on it.
    pub(super) fn gate_mut(&mut self, config_id: &str) -> Option<&mut Gate> {
        if !self.gates.contains_key(config_id) {
            if self.gates.len() >= super::MAX_GATES {
                return None;
            }
            self.last_generation = self.last_generation.saturating_add(1);
            let generation = self.last_generation;
            self.gates.insert(
                config_id.to_owned(),
                Gate {
                    generation,
                    ..Gate::default()
                },
            );
        }
        self.gates.get_mut(config_id)
    }

    /// Move `config_id` to a fresh generation and return it (`None`: no
    /// gate could be created; see [`Self::gate_mut`]).
    pub(super) fn bump(&mut self, config_id: &str) -> Option<u64> {
        self.gate_mut(config_id)?;
        self.last_generation = self.last_generation.saturating_add(1);
        let generation = self.last_generation;
        self.gates.get_mut(config_id)?.generation = generation;
        Some(generation)
    }

    /// Revoke entry `at` on the server's behalf (see
    /// [`super::CastStreams::revoke`]).
    pub(super) fn revoke_at(&mut self, at: usize, reason: &str, now: u64) -> Entry {
        let entry = self.entries.remove(at);
        self.last_report_ms.remove(&entry.id);
        let horizon = now.saturating_sub(ms(MAX_LIFETIME));
        self.revoked.retain(|r| r.at_ms >= horizon);
        while self.revoked.len() >= super::MAX_REVOKED {
            self.revoked.pop_front();
        }
        self.revoked.push_back(Revoked {
            id: entry.id.clone(),
            owner: entry.owner.clone(),
            reason: reason.to_owned(),
            at_ms: now,
        });
        entry
    }

    pub(super) fn view(entry: &Entry) -> StreamView {
        StreamView {
            id: entry.id.clone(),
            owner: entry.owner.clone(),
            output: entry.output.clone(),
            config_id: entry.config_id.clone(),
            feed: entry.feed.clone(),
            expires_unix_ms: entry.expires_ms(),
            live: entry.last_playlist_ms.is_some(),
            fetches: entry.fetches.clone(),
            last_report: entry.last_report.clone(),
            probe: entry.ladder.probe.clone(),
            transport: entry.ladder.transport_view(),
        }
    }

    pub(super) fn token_of(&self, entry: &Entry) -> String {
        token::derive(&self.pepper, &entry.seed)
    }

    /// Streams on the output `config_id`.
    pub(super) fn streams_on(&self, config_id: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.config_id == config_id)
            .count()
    }

    /// Remove expired entries, start lingers and end the lingers that are
    /// over. O(streams + slots).
    /// `sinks`: the engine status of every cast output by config id
    /// (`None`: unavailable; an output missing from it is unavailable too).
    pub(super) fn sweep(
        &mut self,
        now: u64,
        sinks: Option<&HashMap<String, SinkMark>>,
    ) -> Vec<Ended> {
        let before = self.entries.len();
        self.entries.retain(|e| !e.expired(now));
        if self.entries.len() != before {
            let ids: std::collections::HashSet<&str> =
                self.entries.iter().map(|e| e.id.as_str()).collect();
            self.last_report_ms
                .retain(|id, _| ids.contains(id.as_str()));
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for entry in &self.entries {
            *counts.entry(entry.config_id.as_str()).or_insert(0) += 1;
        }
        let mut ended = Vec::new();
        let linger = ms(LINGER);
        for (config_id, slot) in &mut self.slots {
            if counts.get(config_id.as_str()).copied().unwrap_or(0) > 0 {
                slot.empty_since = None;
                continue;
            }
            let since = *slot.empty_since.get_or_insert(now);
            if now >= since.saturating_add(linger) {
                ended.push(Ended {
                    output: slot.api_id.clone(),
                    config_id: config_id.clone(),
                    moved: slot.moved,
                });
            }
        }
        for end in &ended {
            self.slots.remove(&end.config_id);
        }
        // A gate outlives its last stream while its history counts
        // (ABUSE-HISTORY-R4): a token created on the lingering output must
        // still hold the grants of retained superseded media.
        // A gate that lost its last stream retires its feeds to the history
        // first: the output lingers and its files stay servable.
        self.gates.retain(|config_id, gate| {
            let sink = sinks.and_then(|s| s.get(config_id));
            gate.prune(now, sink);
            if counts.contains_key(config_id.as_str()) {
                return true;
            }
            if let Some((_, needs)) = gate.settled.take() {
                gate.retire(needs, now, sink);
            }
            for carry in std::mem::take(&mut gate.carry) {
                gate.retire(carry.needs, now, sink);
            }
            gate.remembers()
        });
        ended
    }
}
