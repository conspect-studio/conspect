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
        ended
    }
}
