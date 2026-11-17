//! The run's [`RailDriver`]: starts and stops a configured source's video and
//! audio rails when demand changes (`docs/architecture/demand-driven-sources.md`).
//!
//! Every method runs on demand-manager threads, never the output clock. A
//! start reuses the exact startup and live-add construction
//! ([`crate::pipeline`]'s plan builders and supervised producer spawn), so an
//! on-demand source decodes exactly as a boot-bound one did.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::{Source, SourceKind};
use multiview_engine::{RailDriver, RailFailure, Rails, RequestedSize};
use multiview_framestore::{NoSignalPolicy, SwapCell, TileStore, TileThresholds};

/// The catalogue entry for one configured source: its definition and frame
/// store. Allocated per configured source, so this is the whole idle cost.
struct Catalogued {
    source: Source,
    store: Arc<TileStore<Nv12Image>>,
    /// A `media_player` alias whose player has no channel in this run: why it
    /// can never show a picture (ADR-M012). `None` for every other entry.
    unavailable: Option<String>,
}

/// The live-updatable catalogue of configured sources.
pub type Catalogue = Arc<SwapCell<Arc<HashMap<String, Arc<CatalogEntry>>>>>;

/// One catalogue entry, shared with the rail driver.
pub struct CatalogEntry(Catalogued);

impl CatalogEntry {
    /// The source definition.
    #[must_use]
    pub fn source(&self) -> &Source {
        &self.0.source
    }

    /// The source's frame store. Empty until its video rail runs.
    #[must_use]
    pub fn store(&self) -> &Arc<TileStore<Nv12Image>> {
        &self.0.store
    }

    /// Whether this entry is a `media_player` alias (ADR-M012): its store IS
    /// the player channel's, fed by the player boot, so demand never starts,
    /// stops or clears a producer for it.
    #[must_use]
    pub fn is_alias(&self) -> bool {
        is_alias(&self.0.source.kind)
    }

    /// Why an alias can never show a picture (its player has no channel).
    #[must_use]
    pub fn unavailable(&self) -> Option<&str> {
        self.0.unavailable.as_deref()
    }
}

fn lock_aliases(
    aliases: &Mutex<std::collections::HashSet<String>>,
) -> MutexGuard<'_, std::collections::HashSet<String>> {
    aliases
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether `kind` is a `media_player` alias (ADR-M012).
#[must_use]
pub const fn is_alias(kind: &SourceKind) -> bool {
    matches!(kind, SourceKind::MediaPlayer { .. })
}

pub use crate::live_sources::no_channel_reason;

/// Catalogue a `media_player` alias with its player's `store` (ADR-M012), so
/// demand reports it and a consumer can lease it. `unavailable` is set when
/// the player has no channel. Off the output clock (hub worker or build).
pub fn catalogue_alias(
    catalogue: &Catalogue,
    source: &Source,
    store: &Arc<TileStore<Nv12Image>>,
    unavailable: Option<&str>,
) {
    let unavailable = unavailable.map(str::to_owned);
    catalogue.rcu(|current| {
        let mut next = current.as_ref().clone();
        next.insert(
            source.id.clone(),
            Arc::new(CatalogEntry(Catalogued {
                source: source.clone(),
                store: Arc::clone(store),
                unavailable: unavailable.clone(),
            })),
        );
        next
    });
}

/// A fresh frame store for source `id`. Allocating one costs a few atomics and
/// an empty ring; frames arrive only once a producer runs.
#[must_use]
pub fn empty_store(id: &str) -> Arc<TileStore<Nv12Image>> {
    Arc::new(TileStore::new(
        id.to_owned(),
        TileThresholds::default(),
        NoSignalPolicy::HoldForever,
    ))
}

/// Build the catalogue for `sources`, reusing `stores` where one exists.
#[must_use]
pub fn catalogue<'a, S: std::hash::BuildHasher>(
    sources: impl Iterator<Item = &'a Source>,
    stores: &HashMap<String, Arc<TileStore<Nv12Image>>, S>,
) -> Catalogue {
    let entries = sources
        .map(|source| {
            let store = stores
                .get(&source.id)
                .map_or_else(|| empty_store(&source.id), Arc::clone);
            (
                source.id.clone(),
                Arc::new(CatalogEntry(Catalogued {
                    source: source.clone(),
                    store,
                    unavailable: None,
                })),
            )
        })
        .collect();
    Arc::new(SwapCell::from_pointee(entries))
}

/// Insert or replace one catalogue entry, keeping its store when it has one.
pub fn catalogue_upsert(catalogue: &Catalogue, source: &Source) -> Arc<TileStore<Nv12Image>> {
    let mut store = None;
    catalogue.rcu(|current| {
        let mut next = current.as_ref().clone();
        let kept = next.get(&source.id).map_or_else(
            || empty_store(&source.id),
            |entry| Arc::clone(entry.store()),
        );
        store = Some(Arc::clone(&kept));
        next.insert(
            source.id.clone(),
            Arc::new(CatalogEntry(Catalogued {
                source: source.clone(),
                store: kept,
                unavailable: None,
            })),
        );
        next
    });
    store.unwrap_or_else(|| empty_store(&source.id))
}

/// Insert or replace one catalogue entry with the given store.
pub fn catalogue_upsert_with(
    catalogue: &Catalogue,
    source: &Source,
    store: &Arc<TileStore<Nv12Image>>,
) {
    catalogue.rcu(|current| {
        let mut next = current.as_ref().clone();
        next.insert(
            source.id.clone(),
            Arc::new(CatalogEntry(Catalogued {
                source: source.clone(),
                store: Arc::clone(store),
                unavailable: None,
            })),
        );
        next
    });
}

/// Remove one catalogue entry.
pub fn catalogue_remove(catalogue: &Catalogue, id: &str) {
    if catalogue.load().contains_key(id) {
        catalogue.rcu(|current| {
            let mut next = current.as_ref().clone();
            next.remove(id);
            next
        });
    }
}

/// Whether `kind` has an audio rail of its own.
#[must_use]
pub fn kind_has_audio(kind: &SourceKind) -> bool {
    matches!(
        kind,
        SourceKind::File { .. }
            | SourceKind::Rtsp { .. }
            | SourceKind::Hls { .. }
            | SourceKind::Ts { .. }
            | SourceKind::Srt { .. }
            | SourceKind::Rtmp { .. }
            | SourceKind::Rist { .. }
            | SourceKind::Bars
            | SourceKind::Aes67 { .. }
    )
}

/// Whether `kind` decodes or renders pictures.
#[must_use]
pub fn kind_has_video(kind: &SourceKind) -> bool {
    !matches!(kind, SourceKind::Aes67 { .. })
}

/// Whether `kind` carries its audio inside its video receive, so its audio needs
/// the video rail.
#[must_use]
pub fn kind_audio_rides_video(kind: &SourceKind) -> bool {
    matches!(kind, SourceKind::Webrtc { .. })
}

/// Starts one rail. Implemented by the pipeline, faked in tests.
pub trait RailSpawner: Send + Sync + 'static {
    /// Start `source`'s video rail into `store`. Returns the producer handles
    /// this rail owns, all registered in the stop registry under `id` or
    /// `{id}/...` (never `{id}/audio`).
    ///
    /// # Errors
    /// An operator-safe reason.
    fn start_video(
        &self,
        source: &Source,
        store: &Arc<TileStore<Nv12Image>>,
        size: Option<RequestedSize>,
        with_audio: bool,
    ) -> Result<Vec<JoinHandle<()>>, String>;

    /// Start `source`'s audio rail. `audio_only` is true when no video rail
    /// runs (the audio must not wait for a video anchor). Returns the producer
    /// handles, registered under `{id}/audio`.
    ///
    /// # Errors
    /// An operator-safe reason.
    fn start_audio(&self, source: &Source, audio_only: bool)
        -> Result<Vec<JoinHandle<()>>, String>;

    /// Detach `source`'s audio from the program bus (its rail stopped).
    fn audio_stopped(&self, id: &str);

    /// Whether a program bus exists, so an audio rail has somewhere to go.
    fn audio_available(&self) -> bool;
}

/// The run's rail driver.
pub struct SourceRails {
    catalogue: Catalogue,
    registry: crate::live_sources::StopRegistry,
    spawner: Arc<dyn RailSpawner>,
    owned: Mutex<HashMap<String, Owned>>,
    /// `media_player` aliases (ADR-M012) whose rails run or are changing: a
    /// rail change of one never touches the stop registry, even after its
    /// catalogue entry is gone (an alias may share its player's id, whose
    /// producer is the player channel). An id is recorded only while its
    /// rails run (a failed or stopped alias records nothing) and is dropped
    /// by the transition (or teardown) that stops them — which a removal
    /// always triggers, since demand stops every running rail of a removed
    /// source. Never dropped earlier: a stop of an unrecorded id would take
    /// the producer path, and for a player-identical id stop the player. So
    /// the set is bounded by the running aliases.
    aliases: Arc<Mutex<std::collections::HashSet<String>>>,
}

/// Producer handles a rail owns, for a bounded join on stop.
#[derive(Default)]
struct Owned {
    video: Vec<JoinHandle<()>>,
    audio: Vec<JoinHandle<()>>,
}

impl SourceRails {
    /// A driver over `catalogue`, stopping producers registered in `registry`.
    #[must_use]
    pub fn new(
        catalogue: Catalogue,
        registry: crate::live_sources::StopRegistry,
        spawner: Arc<dyn RailSpawner>,
    ) -> Self {
        Self {
            catalogue,
            registry,
            spawner,
            owned: Mutex::new(HashMap::new()),
            aliases: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// The number of aliases recorded as running (test seam).
    #[doc(hidden)]
    #[must_use]
    pub fn running_aliases(&self) -> usize {
        self.aliases().len()
    }

    fn aliases(&self) -> MutexGuard<'_, std::collections::HashSet<String>> {
        lock_aliases(&self.aliases)
    }

    /// Whether this rail change of `id` belongs to a `media_player` alias: its
    /// current entry is one, or its rails were last recorded running as one
    /// (an entry removed or replaced while they run).
    fn alias_rails(&self, id: &str, entry: Option<&Arc<CatalogEntry>>) -> bool {
        entry.is_some_and(|entry| entry.is_alias()) || self.aliases().contains(id)
    }

    /// Record (`running`) or drop the alias's running rails.
    fn note_alias(&self, id: &str, running: bool) {
        if running {
            self.aliases().insert(id.to_owned());
        } else {
            self.aliases().remove(id);
        }
    }

    /// A `media_player` alias's rail change (ADR-M012): the player boot owns
    /// the producer, so the video rail "runs" exactly while consumed and
    /// nothing is started, stopped or cleared. An alias whose player has no
    /// channel fails with that reason.
    fn alias_transition(
        entry: Option<&Arc<CatalogEntry>>,
        to: Rails,
    ) -> Result<Rails, RailFailure> {
        let running = Rails {
            video: to.video,
            audio: false,
        };
        match entry.and_then(|entry| entry.unavailable()) {
            Some(reason) if running.video => Err(RailFailure {
                running: Rails::NONE,
                reason: reason.to_owned(),
            }),
            _ => Ok(running),
        }
    }

    fn entry(&self, id: &str) -> Option<Arc<CatalogEntry>> {
        self.catalogue.load().get(id).cloned()
    }

    fn owned(&self) -> MutexGuard<'_, HashMap<String, Owned>> {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn stop(&self, id: &str, video: bool, audio: bool) {
        let handles = {
            let mut owned = self.owned();
            let entry = owned.entry(id.to_owned()).or_default();
            let mut handles = Vec::new();
            if video {
                handles.append(&mut entry.video);
            }
            if audio {
                handles.append(&mut entry.audio);
            }
            if entry.video.is_empty() && entry.audio.is_empty() {
                owned.remove(id);
            }
            handles
        };
        crate::live_sources::stop_rail(&self.registry, id, video, audio, handles);
        if audio {
            self.spawner.audio_stopped(id);
        }
    }
}

impl RailDriver<Nv12Image> for SourceRails {
    fn store(&self, id: &str) -> Option<Arc<TileStore<Nv12Image>>> {
        self.entry(id).map(|entry| Arc::clone(entry.store()))
    }

    fn resolve(&self, id: &str, asked: Rails) -> Rails {
        let Some(entry) = self.entry(id) else {
            return Rails::NONE;
        };
        let kind = &entry.source().kind;
        let audio = asked.audio && kind_has_audio(kind) && self.spawner.audio_available();
        let video =
            (asked.video || (audio && kind_audio_rides_video(kind))) && kind_has_video(kind);
        Rails {
            video,
            audio: audio && (video || !kind_audio_rides_video(kind)),
        }
    }

    fn transition(
        &self,
        id: &str,
        from: Rails,
        to: Rails,
        requested: Option<RequestedSize>,
    ) -> Result<Rails, RailFailure> {
        let entry = self.entry(id);
        if self.alias_rails(id, entry.as_ref()) {
            // A replaced entry (now a producer kind) leaves the alias's rails
            // to stop first; the next transition starts the producer.
            let alias_entry = entry.as_ref().filter(|entry| entry.is_alias());
            let outcome = if alias_entry.is_none() && !to.is_empty() {
                Err(RailFailure {
                    running: Rails::NONE,
                    reason: "the media player binding was replaced".to_owned(),
                })
            } else {
                Self::alias_transition(alias_entry, to)
            };
            let running = outcome
                .as_ref()
                .map_or_else(|failure| failure.running, |running| *running);
            self.note_alias(id, !running.is_empty());
            return outcome;
        }
        // The audio-only/anchored choice depends on video, so a video change
        // rebuilds a running audio rail too.
        let audio_mode_changes = from.audio && to.audio && from.video != to.video;
        let stop_video = from.video && !to.video;
        let stop_audio = from.audio && (!to.audio || audio_mode_changes);
        if stop_video || stop_audio {
            self.stop(id, stop_video, stop_audio);
        }
        let mut running = Rails {
            video: from.video && !stop_video,
            audio: from.audio && !stop_audio,
        };
        let Some(entry) = entry else {
            // Removed from the configuration: everything is stopped above.
            return if to.is_empty() {
                Ok(Rails::NONE)
            } else {
                Err(RailFailure {
                    running,
                    reason: "source is not configured".to_owned(),
                })
            };
        };
        if to.video && !running.video {
            let with_audio = to.audio && kind_audio_rides_video(&entry.source().kind);
            match self
                .spawner
                .start_video(entry.source(), entry.store(), requested, with_audio)
            {
                Ok(handles) => {
                    self.owned().entry(id.to_owned()).or_default().video = handles;
                    running.video = true;
                    if with_audio {
                        running.audio = true;
                    }
                }
                Err(reason) => return Err(RailFailure { running, reason }),
            }
        }
        if to.audio && !running.audio {
            match self.spawner.start_audio(entry.source(), !running.video) {
                Ok(handles) => {
                    self.owned().entry(id.to_owned()).or_default().audio = handles;
                    running.audio = true;
                }
                Err(reason) => return Err(RailFailure { running, reason }),
            }
        }
        Ok(running)
    }

    fn teardown(&self, id: &str, rails: Rails) {
        if self.alias_rails(id, self.entry(id).as_ref()) {
            self.note_alias(id, false);
            return;
        }
        let handles = self.owned().remove(id).map_or_else(Vec::new, |owned| {
            owned.video.into_iter().chain(owned.audio).collect()
        });
        crate::live_sources::stop_rail(&self.registry, id, rails.video, rails.audio, handles);
    }

    fn retire(&self, id: &str, rails: Rails) {
        if rails.video {
            // Never clear an alias: its store is the player channel's.
            if let Some(entry) = self.entry(id).filter(|entry| !entry.is_alias()) {
                entry.store().clear();
            }
        }
    }

    fn progress(&self, id: &str, rails: Rails) -> u64 {
        let video = if rails.video {
            self.entry(id).map_or(0, |entry| entry.store().sequence())
        } else {
            0
        };
        let audio = if rails.audio {
            crate::pipeline::audio_publishes(id)
        } else {
            0
        };
        video.wrapping_add(audio)
    }

    fn finite(&self, id: &str) -> bool {
        // A file ends by itself, and a static generator (bars, solid, a still
        // graphic) publishes once and holds: silence after the first frame is
        // not a stall for either.
        // A media player holds, ends or idles under transport control.
        self.entry(id).is_some_and(|entry| {
            entry.is_alias()
                || matches!(entry.source().kind, SourceKind::File { .. })
                || crate::synth::SyntheticKind::from_source_kind(&entry.source().kind)
                    .is_some_and(|kind| !kind.animated())
        })
    }
}

/// A stop flag registered for one rail producer, with its exit latch guard.
pub struct RegisteredStop {
    /// The flag the producer polls.
    pub stop: Arc<AtomicBool>,
    /// Flips the exit latch when dropped.
    pub exit: crate::live_sources::ExitGuard,
}

/// Register `key`'s stop flag and return it with its exit guard.
#[must_use]
pub fn register(registry: &crate::live_sources::StopRegistry, key: &str) -> RegisteredStop {
    let stop = Arc::new(AtomicBool::new(false));
    let exited = crate::live_sources::register_stop(registry, key, &stop);
    RegisteredStop {
        exit: crate::live_sources::ExitGuard::new(&exited),
        stop,
    }
}

impl RegisteredStop {
    /// Whether a stop was requested.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use multiview_engine::{ConsumerClass, ConsumerTag, DemandConfig, DemandManager, DemandPhase};
    use std::time::{Duration, Instant};

    /// A spawner that must never be asked for anything: an alias has no
    /// producer of its own.
    struct NoSpawn;
    impl RailSpawner for NoSpawn {
        fn start_video(
            &self,
            source: &Source,
            _: &Arc<TileStore<Nv12Image>>,
            _: Option<RequestedSize>,
            _: bool,
        ) -> Result<Vec<JoinHandle<()>>, String> {
            panic!("an alias never starts a producer: {}", source.id)
        }
        fn start_audio(&self, source: &Source, _: bool) -> Result<Vec<JoinHandle<()>>, String> {
            panic!("an alias never starts audio: {}", source.id)
        }
        fn audio_stopped(&self, _: &str) {}
        fn audio_available(&self) -> bool {
            false
        }
    }

    fn alias(id: &str, player: &str) -> Source {
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": "media_player", "player": player, "activation": "on_demand"
        }))
        .unwrap()
    }

    fn wait(mut done: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while !done() {
            if Instant::now() > end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Ship-1111 review #3: churn of aliases that fail (player has no
    /// channel) or are consumed, stopped and idle-retired before removal
    /// never leaves the running-alias record behind, and a removal never
    /// touches the stop registry (a player-identical id keeps its player).
    #[test]
    fn alias_churn_leaves_no_running_alias_record_and_never_stops_the_player() {
        let catalogue: Catalogue = Arc::new(SwapCell::from_pointee(HashMap::new()));
        let registry = crate::live_sources::stop_registry();
        let player_flag = Arc::new(AtomicBool::new(false));
        let _exited = crate::live_sources::register_stop(&registry, "vt-1", &player_flag);
        let rails = Arc::new(SourceRails::new(
            Arc::clone(&catalogue),
            Arc::clone(&registry),
            Arc::new(NoSpawn),
        ));
        let config = DemandConfig {
            close_after: Duration::from_millis(10),
            ..DemandConfig::default()
        };
        let driver: Arc<dyn RailDriver<Nv12Image>> = rails.clone();
        let manager = DemandManager::new(
            multiview_engine::SourceRegistry::<Nv12Image>::new(),
            driver,
            config,
        );
        manager.start();
        let player = empty_store("vt-1");
        for round in 0..20_usize {
            // An alias of a player with no channel: its start fails.
            let ghost = format!("ghost{round}");
            catalogue_alias(
                &catalogue,
                &alias(&ghost, "vt-9"),
                &empty_store(&ghost),
                Some("media player \"vt-9\" has no channel"),
            );
            let lease = manager.acquire(
                &ghost,
                Rails::VIDEO,
                ConsumerTag::new(ConsumerClass::Preview, "pvw"),
                None,
            );
            assert!(wait(|| manager.status(&ghost).phase == DemandPhase::Failed));
            drop(lease);
            catalogue_remove(&catalogue, &ghost);
            manager.remove_source(&ghost);
            // An identity alias consumed, stopped (idle-retired), then removed.
            catalogue_alias(&catalogue, &alias("vt-1", "vt-1"), &player, None);
            let lease = manager.acquire(
                "vt-1",
                Rails::VIDEO,
                ConsumerTag::new(ConsumerClass::Preview, "pvw"),
                None,
            );
            assert!(wait(|| manager.status("vt-1").running.video));
            drop(lease);
            assert!(wait(|| !manager.status("vt-1").running.video));
            catalogue_remove(&catalogue, "vt-1");
            manager.remove_source("vt-1");
            assert!(wait(|| rails.running_aliases() == 0), "round {round}");
        }
        assert!(
            !player_flag.load(Ordering::Acquire),
            "the player channel's producer was never stopped"
        );
        assert!(registry.lock().unwrap().contains_key("vt-1"));
        manager.shutdown();
    }
}
