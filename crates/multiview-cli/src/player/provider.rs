//! Bounded management-only admission and native ACK reconciliation.
#[cfg(target_os = "linux")]
use super::ClipSpec;
use super::{Generation, PlayerRuntime};
use multiview_control::{
    auth::Principal,
    error::{ControlError, ControlResult},
    player_load::{ArtifactRef, LoadOperation, LoadThen, PlayerLoadProvider},
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
    fn reconcile(record: &mut Record, runtime: &PlayerRuntime) -> LoadOperation {
        if let Some(view) = &record.terminal {
            return view.clone();
        }
        let view = Self::view(runtime, record.generation);
        if view.state != "pending" {
            record.terminal = Some(view.clone());
        }
        view
    }
    fn view(runtime: &PlayerRuntime, generation: Generation) -> LoadOperation {
        let native = runtime.native.load();
        let ack = native.as_ref().filter(|a| a.generation == generation.0);
        let fault_refused = matches!(
            runtime.status.load().fault,
            Some(
                super::PlayerFault::AudioUnavailable { generation: g }
                | super::PlayerFault::LoadJournalFailed { generation: g }
                | super::PlayerFault::LoadReferenceMissing { generation: g }
            ) if g == generation.0
        );
        let state = classify(Facts {
            acked: ack.is_some(),
            ack_has_audio: ack.is_some_and(|a| a.has_audio),
            adopted: runtime.audio_commit.has_adopted(generation),
            audio_closed: runtime.audio_commit.is_closed(),
            fault_refused,
            opener_failed: runtime
                .opener
                .status()
                .last_failure
                .is_some_and(|f| f.generation == generation),
            superseded: runtime.coordinator.is_superseded(generation),
        });
        let completed = state == "completed";
        LoadOperation {
            operation: generation.0,
            state,
            native: if completed {
                ack.map(|a| (**a).clone())
            } else {
                None
            },
        }
    }
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
