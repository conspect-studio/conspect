//! #367: a boot-file edit must never replace the accepted state.
//!
//! 1. A file edit applies only the sections the FILE edit changed (new file
//!    against the previous file), never the whole file over the accepted
//!    state; untouched sections keep the accepted value.
//! 2. No watcher commit once shutdown began or the command bus is closed.
//! 3. A file edit that removes accepted sources/outputs/switcher/audio routes
//!    keeps a copy of the accepted state and raises an alarm naming them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_desired, BootModel};
use multiview_control::config_watch::{spawn, ConfigWatchHandle, ManualPoll, WatchOptions};
use multiview_control::{
    command_bus, AppState, CommandReceiver, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::{Event, WarningCode};

const HEAD: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "absolute"
[[sources]]
id = "in_a"
kind = "bars"
[[sources]]
id = "in_b"
kind = "bars"
[[sources]]
id = "in_c"
kind = "bars"
[[cells]]
id = "pgm"
rect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/config-watch-accepted.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// What the operator built through the API on top of the boot file: a second
/// output, the switcher and an audio route (mda001 shape, scaled down).
const ACCEPTED_EXTRA: &str = r#"[[outputs]]
kind = "hls"
id = "second"
path = "/tmp/config-watch-accepted-2.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[switcher]
[[switcher.mix_effects]]
id = "me1"
program = "main"
program_source = "in_a"
preview_source = "in_b"
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "in_b"
target_track = "clean"
include_in_program_bus = true
[audio.routes.channels]
kind = "stereo"
"#;

fn parse(text: &str) -> MultiviewConfig {
    let config = MultiviewConfig::load_from_toml(text).unwrap();
    config.validate().unwrap();
    config
}

struct Rig {
    _dir: tempfile::TempDir,
    path: PathBuf,
    state: AppState,
    accepted: MultiviewConfig,
    commands: Option<CommandReceiver>,
    poll: ManualPoll,
    watch: ConfigWatchHandle,
}

impl Rig {
    fn desired(&self) -> MultiviewConfig {
        load_desired(&self.path)
            .unwrap()
            .expect("accepted state")
            .config
    }
    fn desired_bytes(&self) -> Vec<u8> {
        std::fs::read(self.path.parent().unwrap().join(".multiview/desired.json")).unwrap()
    }
    fn replaced_copies(&self) -> Vec<PathBuf> {
        let dir = self.path.parent().unwrap().join(".multiview/rejected");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("replaced-"))
            })
            .collect()
    }
    async fn settle_edit(&mut self, text: &str) {
        std::fs::write(&self.path, text).unwrap();
        for _ in 0..3 {
            assert!(self.poll.poll_once().await);
        }
    }
}

fn state_for(
    path: &Path,
    boot: &MultiviewConfig,
    running: &MultiviewConfig,
) -> (AppState, CommandReceiver) {
    let seeded = multiview_control::seed_resources(running).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(256));
    let (sender, receiver) = command_bus(64);
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_running_canvas(multiview_config::LayoutCanvas::new(
        64,
        64,
        "25/1".parse().unwrap(),
    ))
    .with_base_document(serde_json::to_value(running).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        boot.clone(),
        running.clone(),
        StartMode::Boot,
        false,
        None,
    )));
    (state, receiver)
}

/// A run started from an accepted state that is larger than the boot file,
/// with the watcher settled on the unchanged boot file.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("conspect.toml");
    std::fs::write(&path, HEAD).unwrap();
    let boot = parse(HEAD);
    let accepted = parse(&format!("{HEAD}{ACCEPTED_EXTRA}"));
    let (state, commands) = state_for(&path, &boot, &accepted);
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    multiview_control::boot_model::commit_desired(&state, accepted.clone(), layouts)
        .await
        .unwrap();
    let (options, mut poll) = WatchOptions::default()
        .with_initial_observed(HEAD.to_owned())
        .with_manual_poll();
    let watch = spawn(path.clone(), accepted.clone(), state.clone(), options);
    assert!(poll.poll_once().await && poll.poll_once().await);
    assert_eq!(state.config_watch.snapshot().applied_count, 0);
    Rig {
        _dir: dir,
        path,
        state,
        accepted,
        commands: Some(commands),
        poll,
        watch,
    }
}

fn raised(
    events: &mut multiview_engine::EventSubscription<Event>,
    code: WarningCode,
) -> Vec<String> {
    let mut found = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::HealthWarningRaised(w) = event.event.as_ref() {
            if w.code == code {
                found.push(w.message.clone());
            }
        }
    }
    found
}

/// #367 (1): the incident. One key in a restart-only section changes; every
/// other accepted section (sources, outputs, switcher, audio) is kept.
#[tokio::test]
async fn a_one_key_boot_file_edit_keeps_every_other_accepted_section() {
    let mut rig = Box::pin(rig()).await;
    let edited = format!("{HEAD}[control]\nlisten = \"[::1]:0\"\nui_dir = \"/srv/conspect-ui\"\n");
    rig.settle_edit(&edited).await;

    let desired = rig.desired();
    assert_eq!(
        desired.control.as_ref().and_then(|c| c.ui_dir.clone()),
        Some(PathBuf::from("/srv/conspect-ui")),
        "the edited key lands"
    );
    let mut expected = rig.accepted.clone();
    expected.control.clone_from(&desired.control);
    assert_eq!(desired, expected, "nothing but [control] changed");
    assert_eq!(rig.state.config_watch.snapshot().applied_count, 1);
    assert_eq!(
        rig.replaced_copies(),
        Vec::<PathBuf>::new(),
        "nothing was removed"
    );
    rig.watch.stop();
}

/// #367 (1): a file edit of one source changes that source only.
#[tokio::test]
async fn a_source_edit_in_the_file_changes_only_that_source() {
    let mut rig = Box::pin(rig()).await;
    let edited = HEAD.replacen(
        "id = \"in_c\"\nkind = \"bars\"",
        "id = \"in_c\"\nkind = \"solid\"\ncolor = \"#202020\"",
        1,
    );
    assert_ne!(edited, HEAD);
    rig.settle_edit(&edited).await;
    let desired = rig.desired();
    assert_eq!(desired.sources[2], parse(&edited).sources[2]);
    assert_eq!(desired.outputs, rig.accepted.outputs);
    assert_eq!(desired.switcher, rig.accepted.switcher);
    assert_eq!(desired.audio, rig.accepted.audio);
    rig.watch.stop();
}

/// #367 (2): the engine is gone (bus closed, the incident's
/// `receiver is closed`): a file edit seen now is never committed.
#[tokio::test]
async fn no_commit_once_the_command_bus_is_closed() {
    let mut rig = Box::pin(rig()).await;
    let before = rig.desired_bytes();
    drop(rig.commands.take());
    let edited = format!("{HEAD}[control]\nlisten = \"[::1]:0\"\nui_dir = \"/srv/x\"\n");
    std::fs::write(&rig.path, &edited).unwrap();
    // The loop ends instead of committing (poll_once may then report false).
    for _ in 0..3 {
        if !rig.poll.poll_once().await {
            break;
        }
    }
    assert_eq!(rig.desired_bytes(), before, "desired.json untouched");
    assert_eq!(rig.state.config_watch.snapshot().applied_count, 0);
}

/// #367 (2): shutdown begins while the watcher waits for the config-mutation
/// lock with a settled edit in hand: it must not commit after the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_commit_when_shutdown_begins_while_waiting_for_the_lock() {
    let mut rig = Box::pin(rig()).await;
    let before = rig.desired_bytes();
    let edited = format!("{HEAD}[control]\nlisten = \"[::1]:0\"\nui_dir = \"/srv/y\"\n");
    std::fs::write(&rig.path, &edited).unwrap();
    assert!(rig.poll.poll_once().await, "candidate registered");
    let guard = rig.state.lock_config_mutation().await;
    let desired_path = rig.path.parent().unwrap().join(".multiview/desired.json");
    let state = rig.state.clone();
    let watch = rig.watch.clone();
    let mut poll = rig.poll;
    let settling = tokio::spawn(async move {
        let _ = poll.poll_once().await;
        poll
    });
    // Let the watcher reach the lock, then decide to stop, then release.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    watch.stop();
    drop(guard);
    let _poll = tokio::time::timeout(std::time::Duration::from_secs(10), settling)
        .await
        .expect("the watcher iteration finishes")
        .unwrap();
    assert_eq!(
        std::fs::read(desired_path).unwrap(),
        before,
        "desired.json untouched"
    );
    assert_eq!(state.config_watch.snapshot().applied_count, 0);
}

/// #367 (3): a file edit that removes an accepted source keeps a copy of the
/// accepted state and raises a critical alarm naming what was removed.
#[tokio::test]
async fn removing_an_accepted_item_keeps_a_copy_and_raises_an_alarm() {
    let mut rig = Box::pin(rig()).await;
    let before = rig.desired_bytes();
    let mut events = rig.state.engine.subscribe();
    let edited = HEAD.replacen("[[sources]]\nid = \"in_c\"\nkind = \"bars\"\n", "", 1);
    assert_ne!(edited, HEAD);
    rig.settle_edit(&edited).await;

    let desired = rig.desired();
    assert!(
        !desired.sources.iter().any(|s| s.id == "in_c"),
        "the edit applies"
    );
    assert_eq!(desired.outputs, rig.accepted.outputs);
    let copies = rig.replaced_copies();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(
        std::fs::read(&copies[0]).unwrap(),
        before,
        "a byte copy of the state before"
    );
    let alarms = raised(&mut events, WarningCode::ConfigFileRemovedAccepted);
    assert_eq!(alarms.len(), 1, "{alarms:?}");
    assert!(alarms[0].contains("source in_c"), "{}", alarms[0]);
    assert!(alarms[0].contains("replaced-"), "{}", alarms[0]);
    rig.watch.stop();
}

/// An edit that cannot combine with the accepted state (the file drops a
/// source the accepted switcher uses) is refused whole; nothing is written.
#[tokio::test]
async fn an_edit_that_conflicts_with_the_accepted_state_is_refused() {
    let mut rig = Box::pin(rig()).await;
    let before = rig.desired_bytes();
    let edited = HEAD.replacen("[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n", "", 1);
    rig.settle_edit(&edited).await;
    assert_eq!(rig.desired_bytes(), before);
    assert_eq!(rig.replaced_copies(), Vec::<PathBuf>::new());
    let snapshot = rig.state.config_watch.snapshot();
    assert_eq!(snapshot.applied_count, 0);
    assert!(
        snapshot
            .last_rejected
            .as_ref()
            .is_some_and(|r| r.detail.contains("does not combine")),
        "{snapshot:?}"
    );
    rig.watch.stop();
}

/// Shed/bound regression (#367), not an output-clock soak (this change is
/// control-plane only): the engine never drains its bus (a wedged engine)
/// while the file flips between removing and restoring an accepted source
/// many times. Every watcher poll completes (the timeout is only a hang net:
/// the watcher never waits on the engine), the kept copies stay bounded, and
/// sections the edits never touched are never changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_removing_edits_against_a_wedged_engine_stays_bounded() {
    let mut rig = Box::pin(rig()).await;
    // Fill the bus and never drain it: every live command is shed.
    while rig
        .state
        .commands
        .try_submit(multiview_control::Command::Start {
            op: multiview_control::OperationId::new(),
        })
        .is_ok()
    {}
    let without_c = HEAD.replacen("[[sources]]\nid = \"in_c\"\nkind = \"bars\"\n", "", 1);
    for round in 0..40_u32 {
        // Each round is a distinct document (so every write is a new edit).
        let text = if round % 2 == 0 {
            format!("{without_c}# round {round}\n")
        } else {
            format!("{HEAD}# round {round}\n")
        };
        std::fs::write(&rig.path, text).unwrap();
        for _ in 0..2 {
            let polled =
                tokio::time::timeout(std::time::Duration::from_secs(60), rig.poll.poll_once())
                    .await
                    .expect("a watcher poll never waits on the engine");
            assert!(polled);
        }
    }
    let copies = rig.replaced_copies();
    assert!(
        !copies.is_empty() && copies.len() <= multiview_control::boot_model::MAX_REPLACED,
        "kept copies are bounded: {}",
        copies.len()
    );
    // The accepted outputs/switcher/audio were never touched by the burst.
    let desired = rig.desired();
    assert_eq!(desired.outputs, rig.accepted.outputs);
    assert_eq!(desired.switcher, rig.accepted.switcher);
    assert_eq!(desired.audio, rig.accepted.audio);
    rig.watch.stop();
}

/// Review (L3-1/L3-2): the exit decision freezes the accepted state. No path
/// writes `desired.json` after it: not the watcher (whose stop flag is not set
/// here, and whose bus is still open, as on a fatal software exit), and not a
/// direct commit (the REST mutations and the revert route funnel through the
/// same durable write).
#[tokio::test]
async fn no_commit_from_any_path_once_the_accepted_state_is_frozen() {
    let mut rig = Box::pin(rig()).await;
    let before = rig.desired_bytes();
    rig.state.freeze_accepted_state();
    let edited = format!("{HEAD}[control]\nlisten = \"[::1]:0\"\nui_dir = \"/srv/z\"\n");
    std::fs::write(&rig.path, &edited).unwrap();
    for _ in 0..3 {
        if !rig.poll.poll_once().await {
            break;
        }
    }
    assert_eq!(rig.desired_bytes(), before, "the watcher did not commit");
    let layouts = multiview_control::boot_model::layout_library(&rig.state).unwrap();
    let mut changed = rig.accepted.clone();
    changed.sources.pop();
    let refused = multiview_control::boot_model::commit_desired(&rig.state, changed, layouts).await;
    assert!(refused.is_err(), "a direct commit is refused once frozen");
    assert_eq!(rig.desired_bytes(), before, "desired.json untouched");
    assert_eq!(rig.state.config_watch.snapshot().applied_count, 0);
}
