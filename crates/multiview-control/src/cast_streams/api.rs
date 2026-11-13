//! [`CastStreams`] operations. Every method takes the one lock briefly and
//! never across an `.await`.

use super::registry::{Claim, ClaimError, Ended, Entry, Registry, ReportError, Slot, StreamView};
use super::{
    bounded_user_agent, token, CastFetches, CastStreams, ReceiverFetch, ReceiverReport,
    CREATE_BURST, CREATE_REFILL, MAX_LIMITED_PRINCIPALS, MAX_STREAMS, MAX_STREAMS_PER_PRINCIPAL,
    REPORT_INTERVAL,
};
use crate::output_routing::RoutedFeed;

fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A cast output the registry holds: what it shows and how many streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotView {
    /// The output's config id.
    pub config_id: String,
    /// The feed its streams asked for.
    pub feed: RoutedFeed,
    /// Live streams on it (0 while lingering).
    pub streams: usize,
}

impl CastStreams {
    /// Expire streams and end lingers: the outputs to deactivate.
    pub fn sweep(&self) -> Vec<Ended> {
        let now = self.now_ms();
        self.lock().sweep(now)
    }

    /// Admit one stream creation by `principal` (burst [`CREATE_BURST`],
    /// one more per [`CREATE_REFILL`]).
    ///
    /// # Errors
    /// The delay until a creation is admitted.
    pub fn admit_create(&self, principal: &str) -> Result<(), std::time::Duration> {
        let now = self.now_ms();
        let refill = ms(CREATE_REFILL).max(1);
        let mut inner = self.lock();
        if !inner.limiter.contains_key(principal) && inner.limiter.len() >= MAX_LIMITED_PRINCIPALS {
            if let Some(oldest) = inner
                .limiter
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(k, _)| k.clone())
            {
                inner.limiter.remove(&oldest);
            }
        }
        let (tokens, at) = inner
            .limiter
            .entry(principal.to_owned())
            .or_insert((CREATE_BURST, now));
        let gained = now.saturating_sub(*at) / refill;
        let refilled = u64::from(*tokens).saturating_add(gained);
        *tokens = u32::try_from(refilled.min(u64::from(CREATE_BURST))).unwrap_or(CREATE_BURST);
        *at = if *tokens == CREATE_BURST {
            now
        } else {
            at.saturating_add(gained.saturating_mul(refill))
        };
        if *tokens == 0 {
            let wait = at.saturating_add(refill).saturating_sub(now);
            return Err(std::time::Duration::from_millis(wait.max(1)));
        }
        *tokens -= 1;
        Ok(())
    }

    /// The outputs streams are on (or lingering), by config id.
    #[must_use]
    pub fn slots(&self) -> Vec<SlotView> {
        let inner = self.lock();
        inner
            .slots
            .iter()
            .map(|(config_id, slot)| SlotView {
                config_id: config_id.clone(),
                feed: slot.feed.clone(),
                streams: inner.streams_on(config_id),
            })
            .collect()
    }

    /// Add a stream of `owner` on the cast output `config_id` (API id
    /// `output`) showing `feed`. With `expect_free`, the output must have no
    /// stream of another feed (a concurrent claim that took it first wins).
    ///
    /// # Errors
    /// [`ClaimError`].
    pub fn claim(
        &self,
        owner: &str,
        output: &str,
        config_id: &str,
        feed: &RoutedFeed,
        expect_free: bool,
    ) -> Result<Claim, ClaimError> {
        let now = self.now_ms();
        let mut inner = self.lock();
        inner.sweep(now);
        if inner.entries.len() >= MAX_STREAMS {
            return Err(ClaimError::Capacity);
        }
        let held = inner.entries.iter().filter(|e| e.owner == owner).count();
        if held >= MAX_STREAMS_PER_PRINCIPAL {
            return Err(ClaimError::PrincipalCapacity);
        }
        let streams = inner.streams_on(config_id);
        let first = !inner.slots.contains_key(config_id);
        match inner.slots.get_mut(config_id) {
            Some(slot) if expect_free && streams > 0 && slot.feed != *feed => {
                return Err(ClaimError::Taken);
            }
            Some(slot) => {
                slot.feed = feed.clone();
                slot.empty_since = None;
            }
            None => {
                inner.slots.insert(
                    config_id.to_owned(),
                    Slot {
                        api_id: output.to_owned(),
                        feed: feed.clone(),
                        moved: false,
                        empty_since: None,
                    },
                );
            }
        }
        let (seed, token, digest) = token::mint(&inner.pepper);
        let id = format!("cast-stream-{}", uuid::Uuid::new_v4());
        inner.entries.push(Entry {
            id: id.clone(),
            owner: owner.to_owned(),
            output: output.to_owned(),
            config_id: config_id.to_owned(),
            feed: feed.clone(),
            seed,
            digest,
            created_ms: now,
            last_playlist_ms: None,
            fetches: CastFetches::default(),
            last_report: None,
        });
        Ok(Claim { id, token, first })
    }

    /// Record that the cast API took `config_id` to its feed (so its end
    /// takes it back to Program).
    pub fn mark_moved(&self, config_id: &str) {
        if let Some(slot) = self.lock().slots.get_mut(config_id) {
            slot.moved = true;
        }
    }

    /// Remove stream `id` (revoked at once). `None` when unknown.
    pub fn remove(&self, id: &str) -> Option<StreamView> {
        let mut inner = self.lock();
        let at = inner.entries.iter().position(|e| e.id == id)?;
        let entry = inner.entries.remove(at);
        inner.last_report_ms.remove(id);
        Some(Registry::view(&entry))
    }

    /// Stream `id` and its token. `None` when unknown or expired.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<(StreamView, String)> {
        let now = self.now_ms();
        let inner = self.lock();
        let entry = inner
            .entries
            .iter()
            .find(|e| e.id == id && !e.expired(now))?;
        Some((Registry::view(entry), inner.token_of(entry)))
    }

    /// Every live stream (filter by owner in the caller).
    #[must_use]
    pub fn list(&self) -> Vec<StreamView> {
        let now = self.now_ms();
        let inner = self.lock();
        inner
            .entries
            .iter()
            .filter(|e| !e.expired(now))
            .map(Registry::view)
            .collect()
    }

    /// The stream id and output config id a token opens.
    #[must_use]
    pub fn lookup(&self, token: &str) -> Option<(String, String)> {
        if !token::well_formed(token) {
            return None;
        }
        let now = self.now_ms();
        let inner = self.lock();
        let at = inner.find(token, now)?;
        let entry = inner.entries.get(at)?;
        Some((entry.id.clone(), entry.config_id.clone()))
    }

    /// Count a successful fetch on stream `id`; a playlist moves it to
    /// `Live` and refreshes its idle expiry.
    pub fn record_fetch(&self, id: &str, playlist: bool, user_agent: Option<&str>) {
        let now = self.now_ms();
        let mut inner = self.lock();
        let Some(entry) = inner.entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if entry.expired(now) {
            return;
        }
        if playlist {
            entry.last_playlist_ms = Some(now);
            entry.fetches.playlists = entry.fetches.playlists.saturating_add(1);
        } else {
            entry.fetches.segments = entry.fetches.segments.saturating_add(1);
        }
        entry.fetches.last_unix_ms = Some(now);
        entry.fetches.cast_device = user_agent.is_some_and(|ua| ua.contains("CrKey"));
        entry.fetches.user_agent = bounded_user_agent(user_agent);
    }

    /// Store a receiver report on the stream `token` opens.
    ///
    /// # Errors
    /// [`ReportError`].
    pub fn report(&self, token: &str, report: serde_json::Value) -> Result<(), ReportError> {
        if !token::well_formed(token) {
            return Err(ReportError::Unknown);
        }
        let now = self.now_ms();
        let mut inner = self.lock();
        let at = inner.find(token, now).ok_or(ReportError::Unknown)?;
        let id = inner
            .entries
            .get(at)
            .map(|e| e.id.clone())
            .ok_or(ReportError::Unknown)?;
        if inner
            .last_report_ms
            .get(&id)
            .is_some_and(|last| now < last.saturating_add(ms(REPORT_INTERVAL)))
        {
            return Err(ReportError::TooSoon);
        }
        inner.last_report_ms.insert(id, now);
        if let Some(entry) = inner.entries.get_mut(at) {
            entry.last_report = Some(ReceiverReport {
                received_unix_ms: now,
                report,
            });
        }
        Ok(())
    }

    /// Record a fetch of the receiver app.
    pub fn record_receiver_fetch(&self, user_agent: Option<&str>) {
        let now = self.now_ms();
        self.lock().receiver_last_fetch = Some(ReceiverFetch {
            unix_ms: now,
            user_agent: bounded_user_agent(user_agent),
            cast_device: user_agent.is_some_and(|ua| ua.contains("CrKey")),
        });
    }

    /// The last fetch of the receiver app.
    #[must_use]
    pub fn receiver_last_fetch(&self) -> Option<ReceiverFetch> {
        self.lock().receiver_last_fetch.clone()
    }

    /// Bookkeeping sizes `(streams, slots, limited principals, report
    /// rates)`: the bounds tests read this.
    #[must_use]
    pub fn sizes(&self) -> (usize, usize, usize, usize) {
        let inner = self.lock();
        (
            inner.entries.len(),
            inner.slots.len(),
            inner.limiter.len(),
            inner.last_report_ms.len(),
        )
    }
}
