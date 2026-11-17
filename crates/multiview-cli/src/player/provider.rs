//! Bounded management-only admission and native ACK reconciliation.
#[cfg(target_os = "linux")]
use super::ClipSpec;
use super::{Generation, PlayerRuntime};
use multiview_control::{
    auth::Principal,
    error::{ControlError, ControlResult},
    player_load::{
        ArtifactRef, LoadFailureCode, LoadOperation, LoadThen, PlayerLoadProvider,
        MAX_LOAD_DETAIL_BYTES,
    },
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Default)]
pub struct RuntimeProvider {
    players: Mutex<HashMap<String, PlayerRuntime>>,
    operations: Mutex<Vec<Record>>,
    /// Keys whose terminal record was evicted: a replay is refused (409), never
    /// re-executed as a new capture.
    tombstones: Mutex<std::collections::VecDeque<Tombstone>>,
}
/// Per-player cap on retained operation records (pending + terminal).
pub(crate) const MAX_RECORDS_PER_PLAYER: usize = 64;
/// Per-player cap on expired-idempotency tombstones. Beyond this the oldest
/// tombstone is forgotten: replay protection is **bounded**, never unlimited.
pub(crate) const MAX_TOMBSTONES_PER_PLAYER: usize = 1024;
struct Tombstone {
    player: String,
    owner: String,
    key: String,
}
struct Record {
    player: String,
    owner: String,
    key: String,
    reference: ArtifactRef,
    then: LoadThen,
    generation: Generation,
    terminal: Option<LoadOperation>,
}
/// The process-wide native load provider handed to the control plane.
pub fn runtime_provider() -> Arc<RuntimeProvider> {
    static INSTANCE: OnceLock<Arc<RuntimeProvider>> = OnceLock::new();
    Arc::clone(INSTANCE.get_or_init(|| Arc::new(RuntimeProvider::default())))
}
/// Observed facts for one load generation.
#[allow(clippy::struct_excessive_bools)] // reason: independent observed facts feeding a pure rule
#[derive(Clone, Copy)]
struct Facts {
    acked: bool,
    ack_has_audio: bool,
    adopted: bool,
    audio_closed: bool,
    fault_refused: bool,
    opener_failed: bool,
    superseded: bool,
}
/// Pure terminal-state rule. A native ACK alone never completes an
/// audio-bearing load: the audio rail must have adopted that exact generation.
/// A closed rail with an audio-bearing, unadopted ACK is terminal `failed`
/// (the closed deck cannot be restored; never reported completed or pending).
fn classify(f: Facts) -> &'static str {
    let completed = f.acked && (f.adopted || (!f.ack_has_audio && f.audio_closed));
    let stranded = f.acked && f.ack_has_audio && f.audio_closed && !f.adopted;
    if completed {
        "completed"
    } else if f.fault_refused || f.opener_failed || stranded {
        "failed"
    } else if f.superseded && !f.acked {
        "superseded"
    } else {
        "pending"
    }
}
impl RuntimeProvider {
    pub fn register(&self, id: String, runtime: PlayerRuntime) {
        self.players
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, runtime);
    }
    /// Scoped teardown: remove `id` (only if it is still `runtime`, so a newer
    /// same-id boot is never evicted) and drop its operation history. Called
    /// after the player's native/audio owners have been handed to retirement,
    /// so the provider no longer pins the runtime or its journal flock.
    pub fn unregister(&self, id: &str, runtime: &PlayerRuntime) {
        let mut players = self
            .players
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if players
            .get(id)
            .is_some_and(|r| Arc::ptr_eq(&r.coordinator, &runtime.coordinator))
        {
            players.remove(id);
            drop(players);
            self.operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|o| o.player != id);
            self.tombstones
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|t| t.player != id);
        }
    }
    /// Whether a player id is currently registered (test/diagnostic).
    #[must_use]
    pub fn is_registered(&self, id: &str) -> bool {
        self.players
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(id)
    }
    /// Evict the oldest terminal record of `player` when at capacity,
    /// tombstoning its key. Pending records are never evicted.
    fn make_room(
        ops: &mut Vec<Record>,
        tombs: &mut std::collections::VecDeque<Tombstone>,
        player: &str,
    ) -> ControlResult<()> {
        if ops.iter().filter(|o| o.player == player).count() < MAX_RECORDS_PER_PLAYER {
            return Ok(());
        }
        let Some(idx) = ops
            .iter()
            .position(|o| o.player == player && o.terminal.is_some())
        else {
            return Err(ControlError::EngineBusy);
        };
        let gone = ops.remove(idx);
        tombs.push_back(Tombstone {
            player: gone.player,
            owner: gone.owner,
            key: gone.key,
        });
        while tombs.iter().filter(|t| t.player == player).count() > MAX_TOMBSTONES_PER_PLAYER {
            if let Some(i) = tombs.iter().position(|t| t.player == player) {
                tombs.remove(i);
            }
        }
        Ok(())
    }
    /// Management-plane only (never the tick path): latch the first terminal
    /// view and log a failed one exactly once, at that latch.
    fn reconcile(record: &mut Record, runtime: &PlayerRuntime) -> LoadOperation {
        if let Some(view) = &record.terminal {
            return view.clone();
        }
        let view = Self::view(runtime, record.generation);
        if view.state != "pending" {
            if view.state == "failed" {
                tracing::warn!(
                    player = %record.player,
                    operation = view.operation,
                    reason = ?view.reason,
                    detail = view.detail.as_deref().unwrap_or(""),
                    "media player load failed"
                );
            }
            record.terminal = Some(view.clone());
        }
        view
    }
    fn view(runtime: &PlayerRuntime, generation: Generation) -> LoadOperation {
        let native = runtime.native.load();
        let ack = native.as_ref().filter(|a| a.generation == generation.0);
        let fault = runtime
            .status
            .load()
            .fault
            .and_then(|fault| fault_code(fault, generation));
        let opener_failure = runtime
            .opener
            .status()
            .last_failure
            .filter(|f| f.generation == generation);
        let audio_closed = runtime.audio_commit.is_closed();
        let facts = Facts {
            acked: ack.is_some(),
            ack_has_audio: ack.is_some_and(|a| a.has_audio),
            adopted: runtime.audio_commit.has_adopted(generation),
            audio_closed,
            fault_refused: fault.is_some(),
            opener_failed: opener_failure.is_some(),
            superseded: runtime.coordinator.is_superseded(generation),
        };
        let state = classify(facts);
        let completed = state == "completed";
        let (reason, detail) = if state == "failed" {
            failure_reason(facts, fault, opener_failure.as_ref())
        } else {
            (None, None)
        };
        LoadOperation {
            operation: generation.0,
            state,
            native: if completed {
                ack.map(|a| (**a).clone())
            } else {
                None
            },
            reason,
            detail,
        }
    }
}

/// The engine fault recorded against exactly `generation`, as a wire code.
fn fault_code(fault: super::PlayerFault, generation: Generation) -> Option<LoadFailureCode> {
    match fault {
        super::PlayerFault::AudioUnavailable { generation: g } if g == generation.0 => {
            Some(LoadFailureCode::AudioUnavailable)
        }
        super::PlayerFault::LoadJournalFailed { generation: g } if g == generation.0 => {
            Some(LoadFailureCode::JournalFailed)
        }
        super::PlayerFault::LoadReferenceMissing { generation: g } if g == generation.0 => {
            Some(LoadFailureCode::ReferenceMissing)
        }
        _ => None,
    }
}

/// The opener's classified reason as its stable wire code.
fn opener_code(reason: super::LoadFailureReason) -> LoadFailureCode {
    use super::LoadFailureReason as R;
    match reason {
        R::NotFound => LoadFailureCode::NotFound,
        R::InvalidData => LoadFailureCode::InvalidData,
        R::Timeout => LoadFailureCode::Timeout,
        R::WrongCadence => LoadFailureCode::WrongCadence,
        R::Vfr => LoadFailureCode::Vfr,
        R::NoCadence => LoadFailureCode::NoCadence,
        R::Alpha => LoadFailureCode::Alpha,
        R::NoVideoStream => LoadFailureCode::NoVideoStream,
        R::UnknownLength => LoadFailureCode::UnknownLength,
        R::InvalidWindow => LoadFailureCode::InvalidWindow,
        R::DecoderOpen => LoadFailureCode::DecoderOpen,
        R::NoFrames => LoadFailureCode::NoFrames,
        R::Other => LoadFailureCode::Other,
    }
}

/// A fixed, path-free explanation per code.
const fn generic_detail(code: LoadFailureCode) -> &'static str {
    match code {
        LoadFailureCode::NotFound => "clip not found",
        LoadFailureCode::InvalidData => "clip container could not be parsed",
        LoadFailureCode::Timeout => "clip open or prime timed out",
        LoadFailureCode::WrongCadence => "clip frame rate does not match the canvas frame rate",
        LoadFailureCode::Vfr => "clip is variable frame rate",
        LoadFailureCode::NoCadence => "clip declares no frame rate",
        LoadFailureCode::Alpha => "clip carries alpha the player cannot decode",
        LoadFailureCode::NoVideoStream => "container has no video stream",
        LoadFailureCode::UnknownLength => "clip length is unknown",
        LoadFailureCode::InvalidWindow => "clip playout window is invalid",
        LoadFailureCode::DecoderOpen => "video decoder could not be opened",
        LoadFailureCode::NoFrames => "no decodable frame at the in-point",
        LoadFailureCode::AudioUnavailable => "clip carries audio but the audio rail is closed",
        LoadFailureCode::JournalFailed => "durable binding journal refused the load",
        LoadFailureCode::ReferenceMissing => "load has no resolvable durable identity",
        LoadFailureCode::Other => "load failed",
    }
}

/// Redacted operator detail. Only reasons produced by the pure probe/geometry
/// checks (whose messages name rates, frame counts and byte bounds, never a
/// path) carry the opener's own text; every libav-derived reason, whose error
/// text can embed the input path, gets a fixed message. Truncated on a char
/// boundary to [`MAX_LOAD_DETAIL_BYTES`].
fn redacted_detail(code: LoadFailureCode, detail: &str) -> String {
    let pure = matches!(
        code,
        LoadFailureCode::WrongCadence
            | LoadFailureCode::Vfr
            | LoadFailureCode::NoCadence
            | LoadFailureCode::Alpha
            | LoadFailureCode::NoVideoStream
            | LoadFailureCode::UnknownLength
            | LoadFailureCode::InvalidWindow
            | LoadFailureCode::NoFrames
    );
    let text = if pure && !detail.is_empty() {
        detail
    } else {
        generic_detail(code)
    };
    let mut end = text.len().min(MAX_LOAD_DETAIL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The typed reason for a `failed` view, in the same precedence as
/// [`classify`]: an engine refusal, then the opener's failure, then a stranded
/// audio-bearing ACK on a closed rail.
fn failure_reason(
    facts: Facts,
    fault: Option<LoadFailureCode>,
    opener: Option<&super::LoadFailure>,
) -> (Option<LoadFailureCode>, Option<String>) {
    let (code, detail) = if let Some(code) = fault {
        (code, String::new())
    } else if let Some(failure) = opener {
        (opener_code(failure.reason), failure.detail.clone())
    } else if facts.acked && facts.ack_has_audio && facts.audio_closed {
        (LoadFailureCode::AudioUnavailable, String::new())
    } else {
        (LoadFailureCode::Other, String::new())
    };
    (Some(code), Some(redacted_detail(code, &detail)))
}
impl PlayerLoadProvider for RuntimeProvider {
    fn load(
        &self,
        player: &str,
        principal: &Principal,
        key: &str,
        reference: ArtifactRef,
        then: LoadThen,
        lease: multiview_control::web_capture::ArtifactLease,
    ) -> ControlResult<LoadOperation> {
        if key.is_empty() || key.len() > 256 || principal.key_id.len() > 256 {
            return Err(ControlError::Validation(
                "load identity exceeds bound".into(),
            ));
        }
        let players = self.players.lock().map_err(|_| ControlError::EngineBusy)?;
        let runtime = players.get(player).ok_or(ControlError::EngineBusy)?;
        let mut ops = self
            .operations
            .lock()
            .map_err(|_| ControlError::EngineBusy)?;
        for old in ops.iter_mut().filter(|o| o.player == player) {
            Self::reconcile(old, runtime);
        }
        let expired = self
            .tombstones
            .lock()
            .map_err(|_| ControlError::EngineBusy)?
            .iter()
            .any(|t| t.player == player && t.owner == principal.key_id && t.key == key);
        let live = ops
            .iter()
            .any(|o| o.player == player && o.owner == principal.key_id && o.key == key);
        if expired && !live {
            return Err(ControlError::Conflict(
                "idempotency key expired; use a new key".into(),
            ));
        }
        if let Some(old) = ops
            .iter_mut()
            .find(|o| o.player == player && o.owner == principal.key_id && o.key == key)
        {
            if old.reference != reference || old.then != then {
                return Err(ControlError::Conflict(
                    "load key reused with different request".into(),
                ));
            }
            return Ok(Self::reconcile(old, runtime));
        }
        // Idempotency retention contract: identity is kept per player for at
        // most MAX_RECORDS_PER_PLAYER operations. When full, the oldest
        // *terminal* record is evicted (never a pending one) and its key is
        // tombstoned (bounded), so a later replay is refused with 409 rather
        // than executing a second capture. All-pending => EngineBusy.
        {
            let mut tombs = self
                .tombstones
                .lock()
                .map_err(|_| ControlError::EngineBusy)?;
            Self::make_room(&mut ops, &mut tombs, player)?;
        }
        #[cfg(target_os = "linux")]
        let spec = ClipSpec::from_artifact(
            lease,
            match then {
                LoadThen::Cue => super::LoadThen::Cue,
                LoadThen::Resume => super::LoadThen::Resume,
            },
        );
        #[cfg(not(target_os = "linux"))]
        {
            let _ = lease;
            Err(ControlError::EngineBusy)
        }
        #[cfg(target_os = "linux")]
        {
            let generation = runtime
                .opener
                .request(spec)
                .map_err(|_| ControlError::EngineBusy)?;
            ops.push(Record {
                player: player.into(),
                owner: principal.key_id.clone(),
                key: key.into(),
                reference,
                then,
                generation,
                terminal: None,
            });
            Ok(Self::view(runtime, generation))
        }
    }
    fn operation(
        &self,
        player: &str,
        principal: &Principal,
        operation: u64,
    ) -> ControlResult<LoadOperation> {
        let players = self.players.lock().map_err(|_| ControlError::EngineBusy)?;
        let runtime = players.get(player).ok_or(ControlError::EngineBusy)?;
        let mut ops = self
            .operations
            .lock()
            .map_err(|_| ControlError::EngineBusy)?;
        let record = ops
            .iter_mut()
            .find(|o| {
                o.player == player && o.owner == principal.key_id && o.generation.0 == operation
            })
            .ok_or(ControlError::NotFound {
                kind: "load-operation",
                id: operation.to_string(),
            })?;
        Ok(Self::reconcile(record, runtime))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: Facts = Facts {
        acked: false,
        ack_has_audio: false,
        adopted: false,
        audio_closed: false,
        fault_refused: false,
        opener_failed: false,
        superseded: false,
    };
    #[test]
    fn rail_closed_after_put_before_adopt_is_failed_not_pending_or_completed() {
        let slot = crate::player::AudioCommitSlot::<u8>::new();
        slot.put(Generation(7), 1).unwrap();
        assert!(slot.close().is_some());
        let f = Facts {
            acked: true,
            ack_has_audio: true,
            adopted: slot.has_adopted(Generation(7)),
            audio_closed: slot.is_closed(),
            ..BASE
        };
        assert_eq!(classify(f), "failed");
    }
    #[test]
    fn audio_ack_needs_adoption_and_video_only_completes_on_closed_rail() {
        let ack = Facts {
            acked: true,
            ack_has_audio: true,
            ..BASE
        };
        assert_eq!(classify(ack), "pending");
        assert_eq!(
            classify(Facts {
                adopted: true,
                ..ack
            }),
            "completed"
        );
        assert_eq!(
            classify(Facts {
                adopted: true,
                audio_closed: true,
                ..ack
            }),
            "completed"
        );
        let v = Facts {
            acked: true,
            audio_closed: true,
            ..BASE
        };
        assert_eq!(classify(v), "completed");
    }
    #[test]
    fn journal_and_reference_refusals_are_terminal_failed() {
        assert_eq!(
            classify(Facts {
                fault_refused: true,
                ..BASE
            }),
            "failed"
        );
        assert_eq!(
            classify(Facts {
                superseded: true,
                ..BASE
            }),
            "superseded"
        );
        assert_eq!(classify(BASE), "pending");
    }
    #[test]
    fn wrong_cadence_failure_carries_typed_reason_and_rates_detail() {
        let detail = crate::player::ProbeError::Cadence {
            want_num: 30000,
            want_den: 1001,
            got_num: 25,
            got_den: 1,
        }
        .to_string();
        let failure = super::super::LoadFailure {
            generation: Generation(3),
            reason: super::super::LoadFailureReason::WrongCadence,
            detail,
        };
        let facts = Facts {
            opener_failed: true,
            ..BASE
        };
        assert_eq!(classify(facts), "failed");
        let (reason, detail) = failure_reason(facts, None, Some(&failure));
        assert_eq!(reason, Some(LoadFailureCode::WrongCadence));
        assert_eq!(
            detail.as_deref(),
            Some("clip frame rate 25/1 does not match the canvas frame rate 30000/1001")
        );
        let wire = serde_json::to_value(LoadFailureCode::WrongCadence).unwrap();
        assert_eq!(wire, "wrong_cadence");
    }
    #[test]
    fn libav_derived_details_never_leak_paths() {
        for (reason, code) in [
            (
                super::super::LoadFailureReason::NotFound,
                LoadFailureCode::NotFound,
            ),
            (
                super::super::LoadFailureReason::InvalidData,
                LoadFailureCode::InvalidData,
            ),
            (
                super::super::LoadFailureReason::Timeout,
                LoadFailureCode::Timeout,
            ),
            (
                super::super::LoadFailureReason::DecoderOpen,
                LoadFailureCode::DecoderOpen,
            ),
            (
                super::super::LoadFailureReason::Other,
                LoadFailureCode::Other,
            ),
        ] {
            let failure = super::super::LoadFailure {
                generation: Generation(1),
                reason,
                detail: "failed to open input \"/srv/state/artifacts/secret.mov\": EIO".into(),
            };
            let (got, detail) = failure_reason(
                Facts {
                    opener_failed: true,
                    ..BASE
                },
                None,
                Some(&failure),
            );
            assert_eq!(got, Some(code));
            let detail = detail.unwrap();
            assert!(!detail.contains('/'), "{detail}");
            assert_eq!(detail, generic_detail(code));
        }
    }
    #[test]
    fn engine_faults_and_stranded_audio_map_to_their_codes() {
        let g = Generation(9);
        assert_eq!(
            fault_code(
                super::super::PlayerFault::LoadJournalFailed { generation: 9 },
                g
            ),
            Some(LoadFailureCode::JournalFailed)
        );
        assert_eq!(
            fault_code(
                super::super::PlayerFault::AudioUnavailable { generation: 9 },
                g
            ),
            Some(LoadFailureCode::AudioUnavailable)
        );
        assert_eq!(
            fault_code(
                super::super::PlayerFault::LoadReferenceMissing { generation: 9 },
                g
            ),
            Some(LoadFailureCode::ReferenceMissing)
        );
        assert_eq!(
            fault_code(
                super::super::PlayerFault::LoadJournalFailed { generation: 8 },
                g
            ),
            None,
            "a fault on another generation is not this operation's reason"
        );
        // The engine refusal wins over a concurrent opener record.
        let (reason, _) = failure_reason(
            Facts {
                fault_refused: true,
                ..BASE
            },
            Some(LoadFailureCode::JournalFailed),
            None,
        );
        assert_eq!(reason, Some(LoadFailureCode::JournalFailed));
        let stranded = Facts {
            acked: true,
            ack_has_audio: true,
            audio_closed: true,
            ..BASE
        };
        assert_eq!(classify(stranded), "failed");
        assert_eq!(
            failure_reason(stranded, None, None).0,
            Some(LoadFailureCode::AudioUnavailable)
        );
    }
    #[test]
    fn detail_is_bounded_on_a_char_boundary() {
        let long = "é".repeat(MAX_LOAD_DETAIL_BYTES);
        let out = redacted_detail(LoadFailureCode::InvalidWindow, &long);
        assert!(out.len() <= MAX_LOAD_DETAIL_BYTES);
        assert!(out.chars().all(|c| c == 'é'));
    }
    fn rec(player: &str, n: u64, terminal: bool) -> Record {
        Record {
            player: player.into(),
            owner: "o".into(),
            key: format!("k{n}"),
            reference: ArtifactRef {
                capture_job_id: n,
                artifact_id: "a".into(),
            },
            then: LoadThen::Cue,
            generation: Generation(n),
            terminal: terminal.then_some(LoadOperation {
                operation: n,
                state: "completed",
                native: None,
                reason: None,
                detail: None,
            }),
        }
    }
    #[test]
    fn sequential_completed_loads_evict_oldest_terminal_and_tombstone() {
        let mut ops = Vec::new();
        let mut tombs = std::collections::VecDeque::new();
        for n in 0..(u64::try_from(MAX_RECORDS_PER_PLAYER).unwrap() + 5) {
            RuntimeProvider::make_room(&mut ops, &mut tombs, "p").unwrap();
            ops.push(rec("p", n, true));
        }
        assert_eq!(ops.len(), MAX_RECORDS_PER_PLAYER);
        assert_eq!(tombs.len(), 5);
        assert_eq!(tombs.front().unwrap().key, "k0");
    }
    #[test]
    fn pending_records_are_never_evicted_and_other_players_are_independent() {
        let mut ops = Vec::new();
        let mut tombs = std::collections::VecDeque::new();
        for n in 0..u64::try_from(MAX_RECORDS_PER_PLAYER).unwrap() {
            ops.push(rec("p", n, false));
        }
        assert!(matches!(
            RuntimeProvider::make_room(&mut ops, &mut tombs, "p"),
            Err(ControlError::EngineBusy)
        ));
        assert_eq!(ops.len(), MAX_RECORDS_PER_PLAYER);
        assert!(RuntimeProvider::make_room(&mut ops, &mut tombs, "q").is_ok());
        assert!(tombs.is_empty());
    }
}
