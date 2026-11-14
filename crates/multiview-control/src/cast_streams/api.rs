//! [`CastStreams`] operations. Every method takes the one lock briefly and
//! never across an `.await`.

use super::reauth::Need;
use super::registry::{Claim, ClaimError, Ended, Entry, Registry, ReportError, Slot, StreamView};
use super::{
    bounded_user_agent, token, CastFetches, CastStreams, ReceiverFetch, ReceiverReport,
    CREATE_BURST, CREATE_REFILL, MAX_LIFETIME, MAX_LIMITED_PRINCIPALS, MAX_STREAMS,
    MAX_STREAMS_PER_PRINCIPAL, REPORT_INTERVAL,
};
use crate::output_routing::RoutedFeed;

fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A live stream on one output, as the re-authorization reads it (no token).
#[derive(Debug, Clone)]
pub struct OwnedStream {
    /// The stream id.
    pub id: String,
    /// The owner key id.
    pub owner: String,
    /// The cast output's API id.
    pub output: String,
    /// The owner as authenticated at creation, when recorded.
    pub principal: Option<crate::auth::Principal>,
    /// How the owner authenticated at creation (ABUSE-3; `None`: never
    /// bound, fails closed).
    pub origin: Option<super::AuthOrigin>,
}

impl OwnedStream {
    fn of(e: &Entry) -> Self {
        Self {
            id: e.id.clone(),
            owner: e.owner.clone(),
            output: e.output.clone(),
            principal: e.principal.clone(),
            origin: e.origin,
        }
    }
}

/// The owner check a gate operation runs on every armed token, under the
/// registry lock (no `.await`, no engine): `Err(reason)` revokes it. The
/// grants are those of the output's feed (`None`: undeterminable).
pub(crate) type Check<'a> = &'a dyn Fn(&OwnedStream, Option<&[Need]>) -> Result<(), &'static str>;

/// A feed change the control plane commits on an output (ABUSE-1).
#[derive(Debug, Clone)]
pub(crate) struct Change {
    /// The take that commits it (`None`: a move without an outcome).
    pub op: Option<String>,
    /// Its picture bus (`None`: unchanged).
    pub bus: Option<String>,
    /// What it requires (`None`: undeterminable).
    pub needs: Option<Vec<Need>>,
}

/// What the output was seen to carry: by an outcome (`op` set when it
/// settles a take), the media path's readback, or a periodic re-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeMode {
    /// An engine outcome: settles its take (and older ones).
    Outcome,
    /// The readback shows a bus the gate does not clear (a fallback the
    /// relay has not delivered yet): re-authorize when not covered.
    Readback,
    /// Re-check every token against the current grants (a re-scoped key, a
    /// re-pointed track): no generation move unless the needs grew.
    Recheck,
}

/// A gate snapshot for arming a new token: its generation and what a new
/// token must be authorized for besides the output's current feed.
#[derive(Debug, Clone)]
pub(crate) struct Arm {
    /// The generation read.
    pub generation: u64,
    /// The cleared grants (`None`: undeterminable: refuse).
    pub needs: Option<Vec<Need>>,
}

/// Why a token could not be armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArmError {
    /// The output's generation moved since the snapshot: re-check.
    Stale,
    /// The stream is gone (revoked or expired).
    Gone,
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
            principal: None,
            origin: None,
            generation: None,
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

    /// Record the owner's principal and how it authenticated at creation
    /// (ABUSE-3: the re-authorization resolves it by that provenance).
    pub fn bind_principal(
        &self,
        id: &str,
        principal: crate::auth::Principal,
        origin: super::AuthOrigin,
    ) {
        if let Some(entry) = self.lock().entries.iter_mut().find(|e| e.id == id) {
            entry.principal = Some(principal);
            entry.origin = Some(origin);
        }
    }

    /// The gate of output `config_id` for arming a token (created at a
    /// fresh generation on first use).
    pub(crate) fn arm_snapshot(&self, config_id: &str) -> Arm {
        let mut inner = self.lock();
        // No gate (over [`super::MAX_GATES`]): nothing is cleared and
        // arming finds no gate either, so the creation is refused.
        inner.gate_mut(config_id).map_or(
            Arm {
                generation: 0,
                needs: None,
            },
            |gate| Arm {
                generation: gate.generation,
                needs: gate.cleared_needs(),
            },
        )
    }

    /// Arm stream `id` at `generation` (its owner was checked against that
    /// snapshot and against `current`, the output's feed it read: `(bus,
    /// grants)`). A generation that moved meanwhile is [`ArmError::Stale`]:
    /// no token is ever served against a stale feed.
    ///
    /// # Errors
    /// [`ArmError`].
    pub(crate) fn arm(
        &self,
        id: &str,
        generation: u64,
        current: Option<(String, Option<Vec<Need>>)>,
    ) -> Result<(), ArmError> {
        let now = self.now_ms();
        let mut inner = self.lock();
        let at = inner
            .entries
            .iter()
            .position(|e| e.id == id && !e.expired(now))
            .ok_or(ArmError::Gone)?;
        let config_id = inner
            .entries
            .get(at)
            .map(|e| e.config_id.clone())
            .ok_or(ArmError::Gone)?;
        let gate = inner.gates.get_mut(&config_id).ok_or(ArmError::Stale)?;
        if gate.generation != generation {
            return Err(ArmError::Stale);
        }
        if gate.settled.is_none() {
            gate.settled = current;
        }
        if let Some(entry) = inner.entries.get_mut(at) {
            entry.generation = Some(generation);
        }
        Ok(())
    }

    /// Re-check every armed token on `config_id` against `needs`, at a new
    /// generation (`bump`) or the current one; revoke the failing ones
    /// (under the lock the caller holds).
    fn recheck(
        inner: &mut Registry,
        config_id: &str,
        needs: Option<&[Need]>,
        bump: bool,
        check: Check<'_>,
        reasons: &mut Vec<(OwnedStream, &'static str)>,
        now: u64,
    ) {
        let generation = if bump {
            inner.bump(config_id)
        } else {
            inner.gate_mut(config_id).map(|g| g.generation)
        };
        // No gate: no token on this output is servable; revoke every armed
        // one (fail closed) rather than leave it waiting for a gate.
        let generation = generation.unwrap_or(0);
        let mut at = 0;
        while let Some(entry) = inner.entries.get(at) {
            if entry.config_id != config_id || entry.generation.is_none() || entry.expired(now) {
                at += 1;
                continue;
            }
            let stream = OwnedStream::of(entry);
            let verdict = if generation == 0 {
                Err(super::reauth::REASON_UNKNOWN)
            } else {
                check(&stream, needs)
            };
            match verdict {
                Ok(()) => {
                    if let Some(entry) = inner.entries.get_mut(at) {
                        entry.generation = Some(generation);
                    }
                    at += 1;
                }
                Err(reason) => {
                    let _ = inner.revoke_at(at, reason, now);
                    reasons.push((stream, reason));
                }
            }
        }
    }

    /// Commit `change` on output `config_id` (ABUSE-1), in ONE critical
    /// section: a new generation, the change carried (what the output may
    /// now carry, until an outcome settles it), every armed token
    /// re-authorized for it and moved to the new generation or revoked.
    /// Nothing to do on an output without streams. Returns the revoked.
    pub(crate) fn commit(
        &self,
        config_id: &str,
        change: Change,
        check: Check<'_>,
    ) -> Vec<(OwnedStream, &'static str)> {
        let now = self.now_ms();
        let mut inner = self.lock();
        let mut revoked = Vec::new();
        if !inner.slots.contains_key(config_id)
            && !inner.entries.iter().any(|e| e.config_id == config_id)
        {
            return revoked;
        }
        let Some(gate) = inner.gate_mut(config_id) else {
            // Over [`super::MAX_GATES`]: fail closed on this output.
            Self::recheck(&mut inner, config_id, None, false, check, &mut revoked, now);
            return revoked;
        };
        while gate.carry.len() >= super::MAX_CARRY {
            gate.carry.pop_front();
        }
        gate.carry.push_back(super::registry::Carry {
            op: change.op,
            bus: change.bus,
            needs: change.needs,
        });
        // Every token at the new generation is authorized for EVERYTHING
        // the output may carry now (the invariant of [`super::registry::Gate`]).
        let needs = gate.cleared_needs();
        Self::recheck(
            &mut inner,
            config_id,
            needs.as_deref(),
            true,
            check,
            &mut revoked,
            now,
        );
        revoked
    }

    /// The output `config_id` was seen on `bus` requiring `needs` (see
    /// [`ChangeMode`]). When the armed tokens were not already authorized
    /// for all of it (or on [`ChangeMode::Recheck`]), every one is
    /// re-authorized in the same critical section — at a new generation
    /// when the needs are new. Returns the revoked.
    pub(crate) fn observe(
        &self,
        config_id: &str,
        op: Option<&str>,
        bus: &str,
        needs: Option<Vec<Need>>,
        mode: ChangeMode,
        check: Check<'_>,
    ) -> Vec<(OwnedStream, &'static str)> {
        let now = self.now_ms();
        let mut inner = self.lock();
        let mut revoked = Vec::new();
        if !inner.slots.contains_key(config_id)
            && !inner.entries.iter().any(|e| e.config_id == config_id)
        {
            return revoked;
        }
        let Some(gate) = inner.gate_mut(config_id) else {
            // Over [`super::MAX_GATES`]: fail closed on this output.
            Self::recheck(&mut inner, config_id, None, false, check, &mut revoked, now);
            return revoked;
        };
        let covered = match (&needs, gate.cleared_needs()) {
            (Some(needs), Some(cleared)) => needs.iter().all(|n| cleared.contains(n)),
            _ => false,
        };
        if mode == ChangeMode::Outcome {
            gate.settle(op);
        }
        gate.settled = Some((bus.to_owned(), needs));
        if !covered || mode == ChangeMode::Recheck {
            // What the output may carry now, under the current grants.
            let all = gate.cleared_needs();
            Self::recheck(
                &mut inner,
                config_id,
                all.as_deref(),
                !covered,
                check,
                &mut revoked,
                now,
            );
        }
        revoked
    }

    /// Whether every armed token on `config_id` is authorized for the
    /// picture bus `bus` (the media path's check of the readback).
    #[must_use]
    pub(crate) fn clears(&self, config_id: &str, bus: &str) -> bool {
        self.lock()
            .gates
            .get(config_id)
            .is_some_and(|g| g.clears(bus))
    }

    /// Stream `id` as the re-authorization reads it (armed or not).
    #[must_use]
    pub(crate) fn owned(&self, id: &str) -> Option<OwnedStream> {
        self.lock()
            .entries
            .iter()
            .find(|e| e.id == id)
            .map(OwnedStream::of)
    }

    /// TEST SEAM: the tokens on `config_id` a public request would be
    /// served for now.
    #[cfg(any(test, feature = "_test-seams"))]
    #[must_use]
    pub fn servable_on(&self, config_id: &str) -> usize {
        let now = self.now_ms();
        let inner = self.lock();
        inner
            .entries
            .iter()
            .filter(|e| e.config_id == config_id && !e.expired(now) && inner.servable(e))
            .count()
    }

    /// The generation of output `config_id` (`None`: no gate; tests).
    #[must_use]
    pub fn generation(&self, config_id: &str) -> Option<u64> {
        self.lock().gates.get(config_id).map(|g| g.generation)
    }

    /// The live streams on the output `config_id`.
    #[must_use]
    pub fn live_on(&self, config_id: &str) -> Vec<OwnedStream> {
        let now = self.now_ms();
        self.lock()
            .entries
            .iter()
            .filter(|e| e.config_id == config_id && !e.expired(now))
            .map(OwnedStream::of)
            .collect()
    }

    /// Revoke stream `id` on the server's behalf (not its owner's): the
    /// token is dead at once and `reason` stays readable by its owner
    /// ([`Self::revoked`]). `None` when unknown.
    pub fn revoke(&self, id: &str, reason: &str) -> Option<StreamView> {
        let now = self.now_ms();
        let mut inner = self.lock();
        let at = inner.entries.iter().position(|e| e.id == id)?;
        let entry = inner.revoke_at(at, reason, now);
        Some(Registry::view(&entry))
    }

    /// Why the server revoked stream `id`: `(owner, reason)`.
    #[must_use]
    pub fn revoked(&self, id: &str) -> Option<(String, String)> {
        let horizon = self.now_ms().saturating_sub(ms(MAX_LIFETIME));
        self.lock()
            .revoked
            .iter()
            .rev()
            .find(|r| r.id == id && r.at_ms >= horizon)
            .map(|r| (r.owner.clone(), r.reason.clone()))
    }

    /// Undo a claim whose output could not be prepared: the stream goes and,
    /// when it was the output's only one, the output is forgotten without a
    /// linger (`was_first`: the claim created the slot).
    pub fn release_claim(&self, id: &str, was_first: bool) {
        let mut inner = self.lock();
        let Some(at) = inner.entries.iter().position(|e| e.id == id) else {
            return;
        };
        let entry = inner.entries.remove(at);
        inner.last_report_ms.remove(id);
        if was_first && inner.streams_on(&entry.config_id) == 0 {
            inner.slots.remove(&entry.config_id);
        }
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

    /// What a public request with `token` may be served (ABUSE-1): the
    /// stream id and output config id of a live token armed at its output's
    /// CURRENT generation, else `None` (the same 404 as an unknown token).
    #[must_use]
    pub fn serve(&self, token: &str) -> Option<(String, String)> {
        if !token::well_formed(token) {
            return None;
        }
        let now = self.now_ms();
        let inner = self.lock();
        let at = inner.find(token, now)?;
        let entry = inner.entries.get(at)?;
        inner
            .servable(entry)
            .then(|| (entry.id.clone(), entry.config_id.clone()))
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
