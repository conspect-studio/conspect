//! Standing pins under a real `Pipeline` (`docs/architecture/demand-driven-sources.md`):
//! boot and runtime key a source's standing leases by the owning source, so
//! arming automation keeps an `always_on` pin, two rules can share a
//! replacement, and disarming or deleting a source releases what it held.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt::Write as _;

use multiview_cli::live_sources::SourceSpawn;
use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;
use multiview_engine::{ConsumerClass, ConsumerTag, Rails};

fn hls(id: &str, extra: &str) -> String {
    format!(
        "[[sources]]\nid = \"{id}\"\nkind = \"hls\"\nurl = \"http://[::1]:9/{id}/index.m3u8\"\n{extra}"
    )
}

fn rule(armed: bool) -> String {
    format!(
        "[sources.events.automation]\narmed = {armed}\nrestore_on_end = true\nmax_duration_s = 60\n\
         [sources.events.automation.on_break_start]\naction = \"swap_cell\"\ntarget = \"slate\"\n"
    )
}

fn config(armed: bool) -> MultiviewConfig {
    let mut doc = String::from(
        r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
"##,
    );
    doc.push_str(&hls("slate", "activation = \"on_demand\"\n"));
    // `watched` keeps the default always_on activation.
    doc.push_str(&hls("watched", ""));
    doc.push_str(&rule(armed));
    doc.push_str(&hls("other", "activation = \"on_demand\"\n"));
    doc.push_str(&rule(true));
    let dir = tempfile::tempdir().unwrap();
    let playlist = dir.path().join("p.m3u8");
    let _ = write!(
        doc,
        "[[outputs]]\nkind = \"hls\"\npath = \"{}\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n",
        playlist.display()
    );
    MultiviewConfig::load_from_toml(&doc).expect("fixture parses")
}

fn holders(tags: &[ConsumerTag]) -> Vec<(ConsumerClass, String)> {
    tags.iter()
        .map(|tag| (tag.class, tag.label.to_string()))
        .collect()
}

/// Review DMD-01: an armed `always_on` source keeps its ALL pin, two rules
/// sharing a replacement both hold it, and disarming one releases exactly
/// its own boot pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_pins_are_owned_by_their_source_and_merge() {
    let mut pipeline = Pipeline::build(&config(true)).expect("builds");
    let demand = pipeline.demand();
    let watched = demand.status("watched");
    assert_eq!(
        watched.wanted,
        Rails::ALL,
        "the always_on pin survives arming"
    );
    assert!(holders(&watched.consumers).contains(&(ConsumerClass::AlwaysOn, "always_on".into())));
    assert!(
        holders(&watched.consumers).contains(&(ConsumerClass::EventAutomation, "watched".into()))
    );
    let slate = holders(&demand.status("slate").consumers);
    assert_eq!(
        slate,
        vec![
            (ConsumerClass::EventAutomation, "other".into()),
            (ConsumerClass::EventAutomation, "watched".into()),
        ],
        "both rules hold their shared replacement"
    );
    // Each replacement holder names its owning source as an object, so the
    // control plane can redact it per principal (review DMD-04).
    assert!(demand
        .status("slate")
        .consumers
        .iter()
        .all(|tag| tag.object.as_deref() == Some(&*tag.label)));
    let view = multiview_cli::source_demand::view_of(&demand.status("slate"));
    assert!(view
        .consumers
        .iter()
        .all(|c| c.object_id.as_deref() == Some(c.label.as_str())));

    // Disarm `watched` at runtime: its replacement pin goes, `other`'s stays.
    let spawner = pipeline.live_ingest_spawner();
    let disarmed = config(false)
        .sources
        .into_iter()
        .find(|s| s.id == "watched")
        .unwrap();
    let store = multiview_cli::source_rails::empty_store("watched");
    assert!(spawner.catalogue(&SourceSpawn {
        source: disarmed,
        store
    }));
    let slate = holders(&demand.status("slate").consumers);
    assert_eq!(
        slate,
        vec![(ConsumerClass::EventAutomation, "other".into())]
    );
    let watched = holders(&demand.status("watched").consumers);
    assert_eq!(watched, vec![(ConsumerClass::AlwaysOn, "always_on".into())]);
    demand.stop_all();
}

fn bars_doc(armed: bool) -> MultiviewConfig {
    let mut doc = config(true);
    let mut source = doc
        .sources
        .iter()
        .find(|s| s.id == "watched")
        .unwrap()
        .clone();
    source.id = "cam".into();
    source.kind = multiview_config::SourceKind::Bars;
    if let Some(rule) = source
        .events
        .as_mut()
        .and_then(|events| events.automation.as_mut())
    {
        rule.armed = armed;
    }
    doc.sources.push(source);
    doc
}

/// Review DMD-09: arming or disarming automation through the live-source hub
/// changes pins only; the running producer is not restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arming_or_disarming_automation_never_restarts_the_producer() {
    let doc = bars_doc(false);
    let mut pipeline = Pipeline::build(&doc).expect("builds");
    let demand = pipeline.demand();
    demand.start();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !demand.status("cam").running.video {
        assert!(
            std::time::Instant::now() < end,
            "{:?}",
            demand.status("cam")
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let generation = demand.status("cam").generation;
    let transitions = demand.transitions();
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
        Some(pipeline.live_ingest_spawner()),
    );
    let handle = hub.handle();
    for armed in [true, false, true] {
        let source = bars_doc(armed)
            .sources
            .into_iter()
            .find(|s| s.id == "cam")
            .unwrap();
        let store = pipeline.catalogue().load()["cam"].store().clone();
        assert_eq!(
            handle.request_spawn_source(SourceSpawn { source, store }),
            multiview_cli::live_sources::HubSubmit::Accepted
        );
        let want = usize::from(armed);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while demand
            .status("slate")
            .consumers
            .iter()
            .filter(|tag| &*tag.label == "cam")
            .count()
            != want
        {
            assert!(
                std::time::Instant::now() < end,
                "pins never followed armed={armed}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        demand.status("cam").generation,
        generation,
        "producer restarted"
    );
    // Only the on-demand replacement (`slate`) may have started or stopped.
    let moved = demand.transitions() - transitions;
    assert!(moved <= 3, "unexpected transitions: {moved}");
    assert!(demand.status("cam").running.video, "still running");
    hub.shutdown();
    demand.stop_all();
}

/// Review R2-02: a running source with NO events block gains its first
/// automation rule (armed, then disarmed, then removed): pins follow, the
/// producer never restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_rule_on_a_source_without_events_never_restarts_the_producer() {
    let mut doc = config(false);
    let mut bare = doc
        .sources
        .iter()
        .find(|s| s.id == "watched")
        .unwrap()
        .clone();
    bare.id = "cam".into();
    bare.kind = multiview_config::SourceKind::Bars;
    let rule = bare
        .events
        .take()
        .and_then(|events| events.automation)
        .unwrap();
    let ruled = |armed: bool| {
        let mut source = bare.clone();
        let mut rule = rule.clone();
        rule.armed = armed;
        source.events = Some(multiview_config::schema::SourceEventsConfig {
            observe: true,
            automation: Some(rule),
        });
        source
    };
    doc.sources.push(bare.clone());
    let mut pipeline = Pipeline::build(&doc).expect("builds");
    let demand = pipeline.demand();
    demand.start();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !demand.status("cam").running.video {
        assert!(
            std::time::Instant::now() < end,
            "{:?}",
            demand.status("cam")
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let generation = demand.status("cam").generation;
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
        Some(pipeline.live_ingest_spawner()),
    );
    let handle = hub.handle();
    for (armed, want) in [
        (Some(true), 1_usize),
        (Some(false), 0),
        (Some(true), 1),
        (None, 0),
    ] {
        let source = match armed {
            Some(armed) => ruled(armed),
            None => bare.clone(),
        };
        let store = pipeline.catalogue().load()["cam"].store().clone();
        assert_eq!(
            handle.request_spawn_source(SourceSpawn { source, store }),
            multiview_cli::live_sources::HubSubmit::Accepted
        );
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while demand
            .status("slate")
            .consumers
            .iter()
            .filter(|tag| &*tag.label == "cam")
            .count()
            != want
        {
            assert!(
                std::time::Instant::now() < end,
                "pins never followed armed={armed:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        demand.status("cam").generation,
        generation,
        "adding the first rule restarted the producer"
    );
    assert!(demand.status("cam").running.video, "still running");
    hub.shutdown();
    demand.stop_all();
}

/// Review DMD-05: deleting a started source through the hub releases its
/// operator Start, and re-adding the id does not inherit it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_started_source_releases_its_start() {
    let mut pipeline = Pipeline::build(&config(false)).expect("builds");
    let demand = pipeline.demand();
    let spawner = pipeline.live_ingest_spawner();
    let slate = config(false)
        .sources
        .into_iter()
        .find(|s| s.id == "slate")
        .unwrap();
    for _ in 0..3 {
        let store = multiview_cli::source_rails::empty_store("slate");
        assert!(spawner.catalogue(&SourceSpawn {
            source: slate.clone(),
            store
        }));
        demand.start_source("slate");
        assert!(holders(&demand.status("slate").consumers)
            .contains(&(ConsumerClass::Manual, "operator".into())));
        spawner.source_removed("slate");
        assert!(
            !holders(&demand.status("slate").consumers)
                .iter()
                .any(|(class, _)| *class == ConsumerClass::Manual),
            "deleted source keeps its Start"
        );
    }
    demand.stop_all();
}

/// One raw HTTP/1.0 exchange against the bound control server.
async fn http(addr: std::net::SocketAddr, request: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
}

fn http_body(response: &str) -> serde_json::Value {
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

/// Which owners hold `id` through automation.
fn automation_holders(
    demand: &multiview_engine::DemandManager<multiview_compositor::pipeline::Nv12Image>,
    id: &str,
) -> Vec<String> {
    demand
        .status(id)
        .consumers
        .iter()
        .filter(|tag| tag.class == ConsumerClass::EventAutomation)
        .map(|tag| tag.label.to_string())
        .collect()
}

/// A compositor drive over `doc`'s layout whose stores are the catalogue's,
/// exactly as the demand run wires them (the drain reuses a registered store).
fn catalogue_drive(
    doc: &MultiviewConfig,
    catalogue: &multiview_cli::source_rails::Catalogue,
) -> multiview_engine::CompositorDrive<multiview_compositor::pipeline::Nv12Image> {
    use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
    let canvas = CanvasColor::default();
    let nosignal = Nv12Image::solid(
        doc.canvas.width,
        doc.canvas.height,
        16,
        128,
        128,
        canvas.output_tag(),
    )
    .unwrap();
    let stores = catalogue
        .load()
        .iter()
        .map(|(id, entry)| (id.clone(), entry.store().clone()))
        .collect();
    multiview_engine::CompositorDrive::new(
        std::sync::Arc::new(doc.solve_layout().unwrap()),
        stores,
        nosignal,
        canvas,
        multiview_compositor::blend::LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .unwrap()
}

/// One generic PUT: (automation, playlist metadata, the [slate, alt] pins cam holds).
type Step = (
    Option<serde_json::Value>,
    Option<serde_json::Value>,
    [usize; 2],
);

/// Reviews R2-01, R3-02: automation and presentation edits through the
/// GENERIC source PUT reach the hub through the REAL engine command drain
/// (demand-managed run). Pins follow every policy edit; the producer never
/// restarts, whether the edit also changes playlist metadata or follows an
/// earlier metadata-only edit; a real media change still restarts it.
#[allow(clippy::too_many_lines)] // one REST walk through the policy edits
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generic_source_put_of_automation_updates_the_pins() {
    std::env::set_var("MULTIVIEW_CONTROL_TOKEN", "test-secret");
    let mut doc = config(false);
    let mut alt = doc
        .sources
        .iter()
        .find(|s| s.id == "slate")
        .unwrap()
        .clone();
    alt.id = "alt".into();
    doc.sources.push(alt);
    let mut cam = doc
        .sources
        .iter()
        .find(|s| s.id == "watched")
        .unwrap()
        .clone();
    cam.id = "cam".into();
    cam.kind = multiview_config::SourceKind::Bars;
    cam.events = None;
    doc.sources.push(cam);
    let mut pipeline = Pipeline::build(&doc).expect("builds");
    let demand = pipeline.demand();
    demand.start();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !demand.status("cam").running.video {
        assert!(
            std::time::Instant::now() < end,
            "{:?}",
            demand.status("cam")
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let generation = demand.status("cam").generation;
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        multiview_cli::live_sources::shared_stores(std::collections::HashMap::new()),
        Some(pipeline.live_ingest_spawner()),
    );
    let publisher = std::sync::Arc::new(multiview_engine::EnginePublisher::<
        multiview_control::EngineStateSnapshot,
        multiview_events::Event,
    >::new(8));
    let (commands, command_rx) = multiview_control::command_bus(8);
    let mut drive = catalogue_drive(&doc, &pipeline.catalogue());
    // The REAL drain the demand run builds (no hand-forwarding of commands).
    let mut drain = multiview_cli::control::command_drain_with_live_sources_and_demand(
        command_rx,
        doc.clone(),
        std::sync::Arc::clone(&publisher),
        hub.handle(),
        pipeline.drain_demand(),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr, server, _state) = multiview_cli::control::bind_and_serve(
        "[::1]:0",
        &doc,
        publisher,
        commands,
        multiview_control::no_preview(),
        None, // switcher absent (legacy path)
        None, // switcher lease owner absent
        None,
        None,
        None,
        None,
        None,
        multiview_control::LiveApplyCaps::default(),
        multiview_control::LiveSourceCapability::synthetic_and_network(),
        None,
        None,
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .expect("control server binds");
    let auth = "Authorization: Bearer admin.test-secret";
    let rule = |target: &str| {
        serde_json::json!({"armed": true, "restore_on_end": true, "max_duration_s": 60,
            "on_break_start": {"action": "swap_cell", "target": target}})
    };
    let disarmed = {
        let mut off = rule("alt");
        off["armed"] = serde_json::json!(false);
        off
    };
    let named = |name: &str| Some(serde_json::json!({ "name": name }));
    // (automation, playlist metadata, expected [slate, alt] pins held by cam)
    let steps: [Step; 7] = [
        (Some(rule("slate")), None, [1, 0]),
        (Some(rule("alt")), None, [0, 1]),
        (Some(disarmed.clone()), None, [0, 0]),
        (None, None, [0, 0]),
        // R3-02: metadata and automation in ONE edit.
        (Some(rule("slate")), named("Cam A"), [1, 0]),
        // R3-02: a metadata-only edit, then an automation-only edit.
        (Some(rule("slate")), named("Cam B"), [1, 0]),
        (Some(disarmed), named("Cam B"), [0, 0]),
    ];
    for (automation, metadata, [on_slate, on_alt]) in steps {
        let got = http(
            addr,
            format!("GET /api/v1/sources/cam HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        let etag = header(&got, "etag").unwrap().to_owned();
        let mut resource = http_body(&got);
        let body = resource["body"].as_object_mut().unwrap();
        match &automation {
            Some(rule) => {
                body.insert("events".into(), serde_json::json!({"automation": rule}));
            }
            None => {
                body.remove("events");
            }
        }
        match &metadata {
            Some(metadata) => {
                body.insert("playlist_metadata".into(), metadata.clone());
            }
            None => {
                body.remove("playlist_metadata");
            }
        }
        let body =
            serde_json::json!({"name": resource["name"], "body": resource["body"]}).to_string();
        let put = http(
            addr,
            format!(
                "PUT /api/v1/sources/cam HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\nIf-Match: {etag}\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(
            put.starts_with("HTTP/1.0 200") || put.starts_with("HTTP/1.1 200"),
            "{put}"
        );
        assert_eq!(header(&put, "x-multiview-apply"), Some("live"));
        // One frame boundary: the real drain applies whatever the PUT sent.
        drain(&mut drive);
        let watched = usize::from(
            automation
                .as_ref()
                .is_some_and(|rule| rule["armed"] == true),
        );
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let slate = automation_holders(&demand, "slate")
                .iter()
                .filter(|o| *o == "cam")
                .count();
            let alt = automation_holders(&demand, "alt")
                .iter()
                .filter(|o| *o == "cam")
                .count();
            let own = automation_holders(&demand, "cam").len();
            if [slate, alt, own] == [on_slate, on_alt, watched] {
                break;
            }
            assert!(
                std::time::Instant::now() < end,
                "pins stale after {automation:?}/{metadata:?}: slate={slate} alt={alt} watched={own}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            demand.status("cam").generation,
            generation,
            "producer restarted after {automation:?}/{metadata:?}"
        );
    }
    assert!(demand.status("cam").running.video, "still running");
    // A real media change (the kind) still restarts the producer.
    let got = http(
        addr,
        format!("GET /api/v1/sources/cam HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    let etag = header(&got, "etag").unwrap().to_owned();
    let mut resource = http_body(&got);
    resource["body"]["kind"] = serde_json::json!("solid");
    resource["body"]["color"] = serde_json::json!("#22aa44");
    let body = serde_json::json!({"name": resource["name"], "body": resource["body"]}).to_string();
    let put = http(
        addr,
        format!(
            "PUT /api/v1/sources/cam HTTP/1.0\r\nHost: {addr}\r\n{auth}\r\nIf-Match: {etag}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(
        put.starts_with("HTTP/1.0 200") || put.starts_with("HTTP/1.1 200"),
        "{put}"
    );
    drain(&mut drive);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while demand.status("cam").generation == generation {
        assert!(
            std::time::Instant::now() < end,
            "a media change must restart the producer"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let _ = shutdown_tx.send(());
    let _ = server.await;
    hub.shutdown();
    demand.stop_all();
}
