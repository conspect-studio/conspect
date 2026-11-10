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
        let handles = self.owned().remove(id).map_or_else(Vec::new, |owned| {
            owned.video.into_iter().chain(owned.audio).collect()
        });
        crate::live_sources::stop_rail(&self.registry, id, rails.video, rails.audio, handles);
    }

    fn retire(&self, id: &str, rails: Rails) {
        if rails.video {
            if let Some(entry) = self.entry(id) {
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
        // Main's audio stores do not expose a publication sequence. Keep the
        // existing audio implementation; this integration observes video only.
        video
    }

    fn finite(&self, id: &str) -> bool {
        // A file ends by itself, and a static generator (bars, solid, a still
        // graphic) publishes once and holds: silence after the first frame is
        // not a stall for either.
        self.entry(id).is_some_and(|entry| {
            matches!(entry.source().kind, SourceKind::File { .. })
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
