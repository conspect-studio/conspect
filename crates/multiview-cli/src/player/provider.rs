//! Bounded management-only admission and native ACK reconciliation.
use super::{ClipSpec, Generation, PlayerRuntime};
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
impl RuntimeProvider {
    pub fn register(&self, id: String, runtime: PlayerRuntime) {
        self.players
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, runtime);
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
        let completed = ack.is_some_and(|a| {
            runtime.audio_commit.has_adopted(generation)
                || (!a.has_audio && runtime.audio_commit.is_closed())
        });
        let failed = runtime
            .opener
            .status()
            .last_failure
            .is_some_and(|f| f.generation == generation);
        let state = if completed {
            "completed"
        } else if failed {
            "failed"
        } else if runtime.coordinator.is_superseded(generation) && ack.is_none() {
            "superseded"
        } else {
            "pending"
        };
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
        lease: multiview_webengine::artifact::Lease,
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
        // Never evict operation identity silently. Admission remains bounded.
        if ops.len() >= 64 {
            return Err(ControlError::EngineBusy);
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
            return Err(ControlError::EngineBusy);
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
            .ok_or(ControlError::EngineBusy)?;
        Ok(Self::reconcile(record, runtime))
    }
}
