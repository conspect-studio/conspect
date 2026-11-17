//! Ship-1111 review: a `media_player` alias (ADR-M012) under hub queue
//! pressure. Deterministic shed tests over the real drain and the real hub,
//! with a recording ingest seam standing in for the demand catalogue:
//!
//! * #1 producer -> alias is ONE ordered hub request; a shed teardown
//!   followed by an accepted bind never removes the new alias later;
//! * #2 a shed alias unbind is retried per tick, bounded by the source cap,
//!   and a player-identical id keeps the player's preview entry;
//! * #4 an alias of a player that did not boot keeps its refusal across a
//!   re-apply, even with a placeholder store under the player id.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_cli::control::CommandDrain;
use multiview_cli::live_sources::{
    shared_stores, stop_registry, IngestSpawner, LiveSourceHub, SharedStores, SourceSpawn,
    SpawnedProducer, StopRegistry, SubmitFault,
};
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::Event;
use multiview_framestore::TileStore;

type Store = Arc<TileStore<Nv12Image>>;

fn config() -> MultiviewConfig {
    MultiviewConfig::load_from_toml(
        r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "cam"
kind = "hls"
url = "http://[::1]:9/cam.m3u8"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam"
[[media_players]]
id = "vt-1"
[[media_players]]
id = "vt-9"
[[outputs]]
kind = "hls"
path = "/tmp/live-media-player-shed.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##,
    )
    .expect("parse")
}

/// What the hub told the demand catalogue, per id: the alias's store and its
/// "no picture" reason. A producer catalogue entry is `None` store.
#[derive(Default)]
struct Recorder {
    aliases: Mutex<BTreeMap<String, (Store, Option<String>)>>,
    removed_producers: Mutex<Vec<String>>,
}

struct Seam(Arc<Recorder>);

impl IngestSpawner for Seam {
    fn spawn(&self, _: SourceSpawn, _: &StopRegistry) -> Option<SpawnedProducer> {
        None
    }
    fn source_removed(&self, id: &str) {
        // A generic teardown removes whatever the id names, as the real
        // spawner's `catalogue_remove` + `remove_source` does.
        self.0.aliases.lock().unwrap().remove(id);
        self.0.removed_producers.lock().unwrap().push(id.to_owned());
    }
    fn catalogue_alias(
        &self,
        source: &multiview_config::Source,
        store: &Store,
        unavailable: Option<String>,
    ) -> bool {
        self.0
            .aliases
            .lock()
            .unwrap()
            .insert(source.id.clone(), (Arc::clone(store), unavailable));
        true
    }
    fn alias_removed(&self, id: &str, store: &Store) {
        let mut aliases = self.0.aliases.lock().unwrap();
        if aliases
            .get(id)
            .is_some_and(|(current, _)| Arc::ptr_eq(current, store))
        {
            aliases.remove(id);
        }
    }
}

struct Rig {
    sender: multiview_control::CommandSender,
    drain: CommandDrain,
    drive: CompositorDrive<Nv12Image>,
    hub: LiveSourceHub,
    fault: SubmitFault,
    recorder: Arc<Recorder>,
    preview: SharedStores,
    player: Store,
}

fn rig(booted: Option<&[&str]>, cap: Option<usize>) -> Rig {
    let config = config();
    let player: Store = Arc::new(TileStore::with_defaults("vt-1".to_owned()));
    let mut stores: HashMap<String, Store> = HashMap::from([
        (
            "cam".to_owned(),
            Arc::new(TileStore::with_defaults("cam".to_owned())),
        ),
        ("vt-1".to_owned(), Arc::clone(&player)),
    ]);
    // A player that failed boot can still leave a placeholder store under
    // its id (an identity alias binds a never-fed one): model it.
    stores.insert(
        "vt-9".to_owned(),
        Arc::new(TileStore::with_defaults("vt-9".to_owned())),
    );
    let preview = shared_stores(stores.clone());
    let recorder = Arc::new(Recorder::default());
    let hub = LiveSourceHub::start_with_ingest(
        stop_registry(),
        Arc::clone(&preview),
        Some(Arc::new(Seam(Arc::clone(&recorder)))),
    );
    let fault = SubmitFault::default();
    let (sender, rx) = command_bus(64);
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let mut drain = CommandDrain::new(rx, config.clone(), publisher)
        .with_live_sources(hub.handle().with_fault(fault.clone()));
    if let Some(booted) = booted {
        drain = drain.with_booted_players(Arc::new(
            booted
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<BTreeSet<_>>(),
        ));
    }
    if let Some(cap) = cap {
        drain = drain.with_source_cap(cap);
    }
    let canvas = CanvasColor::default();
    let drive = CompositorDrive::new(
        Arc::new(config.solve_layout().unwrap()),
        stores,
        Nv12Image::solid(64, 64, 16, 128, 128, canvas.output_tag()).unwrap(),
        canvas,
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .unwrap();
    Rig {
        sender,
        drain,
        drive,
        hub,
        fault,
        recorder,
        preview,
        player,
    }
}

impl Rig {
    fn submit(&mut self, command: Command) {
        self.sender.try_submit(command).unwrap();
        self.drain.apply(&mut self.drive);
    }
    fn upsert(&mut self, body: serde_json::Value) {
        self.submit(Command::UpsertSource {
            op: OperationId::new(),
            source: Box::new(serde_json::from_value(body).unwrap()),
        });
    }
    fn remove(&mut self, id: &str) {
        self.submit(Command::RemoveSource {
            op: OperationId::new(),
            id: id.to_owned(),
        });
    }
    fn tick(&mut self, n: usize) {
        for _ in 0..n {
            self.drain.apply(&mut self.drive);
        }
    }
    /// Wait until the hub drained (a no-op round-trip lands after it).
    fn settle(&self) {
        let probe: Store = Arc::new(TileStore::with_defaults("probe".to_owned()));
        assert_eq!(
            self.hub
                .handle()
                .request_unbind_alias("probe-none", &probe, true),
            multiview_cli::live_sources::HubSubmit::Accepted
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    fn alias(&self, id: &str) -> Option<(Store, Option<String>)> {
        self.recorder.aliases.lock().unwrap().get(id).cloned()
    }
}

fn wait(mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while !done() {
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// #1: producer -> alias with the teardown shed (Full) and the binding
/// accepted. The alias must stay catalogued and previewable after every
/// later tick: no stale generic teardown may remove it.
#[test]
fn producer_to_alias_with_a_shed_teardown_never_removes_the_new_alias() {
    let mut r = rig(Some(&["vt-1"]), None);
    // `cam` is a running producer. The queue sheds exactly one teardown.
    r.fault.full_next_teardown();
    r.upsert(serde_json::json!({"id": "cam", "kind": "media_player", "player": "vt-1"}));
    r.settle();
    let bound = r.alias("cam").is_some();
    // Later ticks retry anything pending (a shed generic teardown would be
    // resubmitted here and, landing after the bind, withdraw the alias).
    r.tick(4);
    r.settle();
    let kept = r.alias("cam").is_some();
    assert!(
        bound && kept,
        "bound after the upsert: {bound}; still bound after the retries: {kept}"
    );
    let (store, unavailable) = r.alias("cam").expect("the alias is still catalogued");
    assert!(Arc::ptr_eq(&store, &r.player), "bound to the player");
    assert!(unavailable.is_none());
    assert!(
        r.preview
            .load()
            .get("cam")
            .is_some_and(|s| Arc::ptr_eq(s, &r.player)),
        "the wall/preview entry is the player's store"
    );
    assert!(
        r.recorder
            .removed_producers
            .lock()
            .unwrap()
            .contains(&"cam".to_owned()),
        "the replaced producer was torn down (inside the bind)"
    );
    assert_eq!(r.drain.pending_teardowns(), 0);
    r.hub.shutdown();
}

/// #2: a removal whose unbind is shed is retried until the hub withdraws
/// the alias; churn under a full queue stays within the cap; a
/// player-identical alias keeps the player's preview entry.
#[test]
fn a_shed_alias_unbind_is_retried_and_churn_stays_within_the_cap() {
    const CAP: usize = 4;
    let mut r = rig(Some(&["vt-1"]), Some(CAP));
    r.upsert(serde_json::json!({"id": "vt-a", "kind": "media_player", "player": "vt-1"}));
    r.upsert(serde_json::json!({"id": "vt-1", "kind": "media_player", "player": "vt-1"}));
    r.settle();
    assert!(wait(
        || r.alias("vt-a").is_some() && r.alias("vt-1").is_some()
    ));

    r.fault.full();
    r.remove("vt-a");
    r.remove("vt-1");
    assert_eq!(r.drain.pending_unbinds(), 2, "both shed unbinds are kept");
    // Churn NEW alias ids under the full queue: bounded by the cap.
    for n in 0..40_usize {
        let id = format!("churn{n}");
        r.upsert(serde_json::json!({"id": id, "kind": "media_player", "player": "vt-1"}));
        r.remove(&id);
        assert!(
            r.drain.admitted_sources() <= CAP,
            "hub-known + pending stays within the cap: {} (unbinds {}, binds {})",
            r.drain.admitted_sources(),
            r.drain.pending_unbinds(),
            r.drain.pending_binds()
        );
    }
    r.fault.clear();
    r.tick(CAP * 4);
    r.settle();
    assert_eq!(r.drain.pending_unbinds(), 0, "every unbind reached the hub");
    assert_eq!(
        r.drain.pending_binds(),
        0,
        "no binding of a removed id is retried"
    );
    assert!(wait(|| r.alias("vt-a").is_none()), "vt-a withdrawn");
    assert!(wait(|| r.alias("vt-1").is_none()), "vt-1 alias withdrawn");
    assert!(!r.preview.load().contains_key("vt-a"));
    assert!(
        r.preview
            .load()
            .get("vt-1")
            .is_some_and(|s| Arc::ptr_eq(s, &r.player)),
        "the player channel's own preview entry stays"
    );
    assert!(
        r.recorder.removed_producers.lock().unwrap().is_empty(),
        "no generic teardown ever ran for an alias (it would stop vt-1)"
    );
    r.hub.shutdown();
}

/// #4: an alias of a player that did not boot is unavailable even though a
/// placeholder store sits under the player id — on add, re-apply, repoint
/// and for an identity alias.
#[test]
fn an_alias_of_a_player_without_a_channel_stays_refused_across_reapply_and_repoint() {
    let mut r = rig(Some(&["vt-1"]), None);
    for _ in 0..2 {
        r.upsert(serde_json::json!({"id": "vt-9", "kind": "media_player", "player": "vt-9"}));
        r.upsert(serde_json::json!({"id": "ghost", "kind": "media_player", "player": "vt-9"}));
    }
    r.settle();
    for id in ["vt-9", "ghost"] {
        assert!(wait(|| r.alias(id).is_some()), "{id} catalogued");
        let reason = r.alias(id).unwrap().1.unwrap_or_default();
        assert!(reason.contains("vt-9"), "{id}: {reason:?}");
    }
    // Repoint onto the booted player: available. Back again: refused.
    r.upsert(serde_json::json!({"id": "ghost", "kind": "media_player", "player": "vt-1"}));
    r.settle();
    assert!(wait(|| r.alias("ghost").is_some_and(|(_, u)| u.is_none())));
    r.upsert(serde_json::json!({"id": "ghost", "kind": "media_player", "player": "vt-9"}));
    r.settle();
    assert!(wait(|| r.alias("ghost").is_some_and(|(_, u)| u.is_some())));
    r.hub.shutdown();
}

/// What the hub holds for `id`: its catalogue entry's store and preview entry.
fn hub_state(r: &Rig, id: &str) -> (Option<Store>, Option<Store>) {
    (
        r.alias(id).map(|(store, _)| store),
        r.preview.load().get(id).cloned(),
    )
}

/// Round 3, sequence A: accepted x->vt-1; under a full queue repoint x->vt-9
/// (the hub never sees it) and remove x. Recovery must withdraw what the hub
/// actually holds (vt-1's binding), and the id must stop counting.
#[test]
fn removing_an_alias_whose_repoint_was_shed_withdraws_the_accepted_binding() {
    const CAP: usize = 3;
    let mut r = rig(Some(&["vt-1", "vt-9"]), Some(CAP));
    let base = r.drain.admitted_sources();
    r.upsert(serde_json::json!({"id": "x", "kind": "media_player", "player": "vt-1"}));
    r.settle();
    let (catalogued, preview) = hub_state(&r, "x");
    assert!(catalogued.is_some_and(|s| Arc::ptr_eq(&s, &r.player)));
    assert!(preview.is_some_and(|s| Arc::ptr_eq(&s, &r.player)));

    r.fault.full();
    r.upsert(serde_json::json!({"id": "x", "kind": "media_player", "player": "vt-9"}));
    r.remove("x");
    assert!(
        r.drain.admitted_sources() <= base + 1,
        "x counts once while pending"
    );
    r.fault.clear();
    r.tick(CAP * 4);
    r.settle();
    let (catalogued, preview) = hub_state(&r, "x");
    assert!(
        catalogued.is_none() && preview.is_none(),
        "the hub's catalogue entry, pins and preview of x are withdrawn: \
         catalogued {}, preview {}",
        catalogued.is_some(),
        preview.is_some()
    );
    assert_eq!(r.drain.admitted_sources(), base, "x no longer counts");
    r.hub.shutdown();
}

/// Round 3, sequence B: accepted x->vt-1; under a full queue remove x,
/// re-add x->vt-9 (never applied) and remove x again. The pending cleanup
/// must still target vt-1's accepted binding, never be overwritten by vt-9's.
#[test]
fn a_shed_readd_never_overwrites_the_pending_unbind_of_the_accepted_alias() {
    const CAP: usize = 3;
    let mut r = rig(Some(&["vt-1", "vt-9"]), Some(CAP));
    let base = r.drain.admitted_sources();
    r.upsert(serde_json::json!({"id": "x", "kind": "media_player", "player": "vt-1"}));
    r.settle();
    assert!(hub_state(&r, "x").0.is_some());

    r.fault.full();
    r.remove("x");
    r.upsert(serde_json::json!({"id": "x", "kind": "media_player", "player": "vt-9"}));
    r.remove("x");
    assert!(
        r.drain.admitted_sources() <= base + 1,
        "x counts once while pending"
    );
    r.fault.clear();
    r.tick(CAP * 4);
    r.settle();
    let (catalogued, preview) = hub_state(&r, "x");
    assert!(
        catalogued.is_none() && preview.is_none(),
        "the hub's catalogue entry, pins and preview of x are withdrawn: \
         catalogued {}, preview {}",
        catalogued.is_some(),
        preview.is_some()
    );
    assert_eq!(r.drain.admitted_sources(), base, "x no longer counts");
    r.hub.shutdown();
}

/// Round 4: aliases configured at BOOT are catalogued (and previewed) by the
/// pipeline before any command. A real `Pipeline`, its real hub + ingest
/// spawner and a drain wired with the demand seam, as the binary wires them.
#[cfg(feature = "ffmpeg")]
struct BootRig {
    _pipeline: multiview_cli::pipeline::Pipeline,
    demand: multiview_engine::DemandManager<Nv12Image>,
    catalogue: multiview_cli::source_rails::Catalogue,
    preview: SharedStores,
    hub: LiveSourceHub,
    fault: SubmitFault,
    sender: multiview_control::CommandSender,
    drain: CommandDrain,
    drive: CompositorDrive<Nv12Image>,
}

#[cfg(feature = "ffmpeg")]
impl BootRig {
    fn new() -> Self {
        let config = MultiviewConfig::load_from_toml(&toml_with_boot_aliases()).expect("parse");
        config.validate().expect("validates");
        let mut pipeline = multiview_cli::pipeline::Pipeline::build(&config).expect("builds");
        let demand = pipeline.demand();
        let catalogue = pipeline.catalogue();
        let preview = shared_stores(pipeline.preview_stores());
        for id in ["vt-boot", "vt-boot2"] {
            assert!(catalogue.load().get(id).is_some_and(|e| e.is_alias()));
            assert!(preview.load().contains_key(id), "{id} previewed at boot");
        }
        let hub = LiveSourceHub::start_with_ingest(
            pipeline.stop_registry(),
            Arc::clone(&preview),
            Some(pipeline.live_ingest_spawner()),
        );
        let fault = SubmitFault::default();
        let (sender, rx) = command_bus(16);
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let drain = CommandDrain::new(rx, config.clone(), publisher)
            .with_live_sources(hub.handle().with_fault(fault.clone()))
            .with_demand(pipeline.drain_demand());
        let canvas = CanvasColor::default();
        let drive = CompositorDrive::new(
            Arc::new(config.solve_layout().unwrap()),
            pipeline.preview_stores(),
            Nv12Image::solid(64, 64, 16, 128, 128, canvas.output_tag()).unwrap(),
            canvas,
            LinearRgba::opaque(0.0, 0.0, 0.0),
        )
        .unwrap();
        Self {
            _pipeline: pipeline,
            demand,
            catalogue,
            preview,
            hub,
            fault,
            sender,
            drain,
            drive,
        }
    }
    fn submit(&mut self, command: Option<Command>) {
        if let Some(command) = command {
            self.sender.try_submit(command).unwrap();
        }
        self.drain.apply(&mut self.drive);
    }
    fn remove(&mut self, id: &str) {
        self.submit(Some(Command::RemoveSource {
            op: OperationId::new(),
            id: id.to_owned(),
        }));
    }
    /// The hub withdrew `id`'s catalogue entry (with its pins) and preview.
    fn assert_withdrawn(&self, id: &str) {
        let gone = wait(|| {
            !self.catalogue.load().contains_key(id) && !self.preview.load().contains_key(id)
        });
        assert!(
            gone,
            "{id}: catalogue {} preview {}",
            self.catalogue.load().contains_key(id),
            self.preview.load().contains_key(id)
        );
        assert!(
            self.demand.status(id).consumers.is_empty(),
            "{id}: its standing pins were dropped"
        );
        assert!(self.preview.load().contains_key("vt-1"), "the player stays");
    }
    fn finish(self) {
        self.hub.shutdown();
        self.demand.shutdown();
    }
}

/// Round 4 (a): a boot alias removed before any live upsert is withdrawn
/// from the hub, and the cap is back to baseline.
#[cfg(feature = "ffmpeg")]
#[test]
fn removing_a_boot_alias_withdraws_its_boot_binding() {
    let mut r = BootRig::new();
    let base = r.drain.admitted_sources();
    r.remove("vt-boot");
    r.assert_withdrawn("vt-boot");
    assert_eq!(
        r.drain.admitted_sources(),
        base - 1,
        "vt-boot no longer counts"
    );
    r.finish();
}

/// Round 4 (b): a boot alias repointed while the queue is full (the hub
/// never sees it) and removed while full: on recovery the BOOT binding is
/// withdrawn (never the drive's unaccepted repoint), cap back to baseline.
#[cfg(feature = "ffmpeg")]
#[test]
fn a_shed_repoint_of_a_boot_alias_then_removal_withdraws_the_boot_binding() {
    let mut r = BootRig::new();
    let base = r.drain.admitted_sources();
    r.fault.full();
    r.submit(Some(Command::UpsertSource {
        op: OperationId::new(),
        source: Box::new(
            serde_json::from_value(
                serde_json::json!({"id": "vt-boot2", "kind": "media_player", "player": "vt-9"}),
            )
            .unwrap(),
        ),
    }));
    r.remove("vt-boot2");
    r.fault.clear();
    for _ in 0..8 {
        r.submit(None);
    }
    r.assert_withdrawn("vt-boot2");
    assert_eq!(
        r.drain.admitted_sources(),
        base - 1,
        "vt-boot2 no longer counts; nothing left pending"
    );
    r.finish();
}

#[cfg(feature = "ffmpeg")]
fn toml_with_boot_aliases() -> String {
    r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "cam"
kind = "bars"
[[sources]]
id = "vt-boot"
kind = "media_player"
player = "vt-1"
[[sources]]
id = "vt-boot2"
kind = "media_player"
player = "vt-1"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam"
[[media_players]]
id = "vt-1"
[[media_players]]
id = "vt-9"
[[outputs]]
kind = "hls"
path = "/tmp/live-media-player-boot-alias.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    .to_owned()
}
