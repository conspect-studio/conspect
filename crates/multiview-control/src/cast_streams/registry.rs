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

/// The authorization gate of one output (ABUSE-1). Invariant: every token
/// armed at `generation` passed the grants of the `settled` feed and of
/// every carried take, so the media of any bus [`Gate::clears`] may be
/// served to it; anything else bumps the generation and re-authorizes
/// every token first, in the same critical section.
#[derive(Debug, Clone, Default)]
pub(super) struct Gate {
    pub generation: u64,
    /// Committed, unsettled changes, oldest first (at most
    /// [`super::MAX_CARRY`]).
    pub carry: std::collections::VecDeque<Carry>,
    /// The bus the output was last observed on and its grants.
    pub settled: Option<(String, Option<Vec<super::reauth::Need>>)>,
}

impl Gate {
    /// Whether the picture bus `bus` is cleared for every armed token.
    pub(super) fn clears(&self, bus: &str) -> bool {
        self.settled.as_ref().is_some_and(|(b, _)| b == bus)
            || self.carry.iter().any(|c| c.bus.as_deref() == Some(bus))
    }

    /// Every grant a new token must hold: the settled feed's and every
    /// carried take's (`None`: undeterminable). Forgetting a carried take
    /// (over [`super::MAX_CARRY`]) only narrows this: a bus no longer
    /// cleared is re-authorized when the media path sees it.
    pub(super) fn cleared_needs(&self) -> Option<Vec<super::reauth::Need>> {
        let mut all = Vec::new();
        let settled = self.settled.as_ref().map(|(_, needs)| needs);
        for needs in settled
            .into_iter()
            .chain(self.carry.iter().map(|c| &c.needs))
        {
            for need in needs.as_ref()? {
                if !all.contains(need) {
                    all.push(need.clone());
                }
            }
        }
        Some(all)
    }

    /// Settle through the take `op` (and every older one) and every
    /// control-plane move without an operation id (its bus is the one the
    /// outcome reports, or it no longer is on air).
    pub(super) fn settle(&mut self, op: Option<&str>) {
        if let Some(at) =
            op.and_then(|op| self.carry.iter().position(|c| c.op.as_deref() == Some(op)))
        {
            self.carry.drain(..=at);
        }
        self.carry.retain(|c| c.op.is_some());
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
    /// use. Gates exist only for outputs with streams (the sweep drops the
    /// others), so they are bounded by [`super::MAX_STREAMS`].
    pub(super) fn gate_mut(&mut self, config_id: &str) -> &mut Gate {
        if !self.gates.contains_key(config_id) {
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
        self.gates.entry(config_id.to_owned()).or_default()
    }

    /// Move `config_id` to a fresh generation and return it.
    pub(super) fn bump(&mut self, config_id: &str) -> u64 {
        self.last_generation = self.last_generation.saturating_add(1);
        let generation = self.last_generation;
        self.gate_mut(config_id).generation = generation;
        generation
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
    pub(super) fn sweep(&mut self, now: u64) -> Vec<Ended> {
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
        self.gates
            .retain(|config_id, _| counts.contains_key(config_id.as_str()));
        ended
    }
}
