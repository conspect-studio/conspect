//! Failing-first tests for the single-socket [`UnifiedEndpoint`] (ADR-0048 §4,
//! box-validation defect B). The cli previously bound `webrtc.udp_port` once per
//! role (preview WHEP + WHIP ingest + WHEP-serve + each `whip_push`), so the
//! 2nd/3rd `bind` hit `EADDRINUSE` and silently degraded those roles to
//! "unavailable" — with preview + WHIP + WHEP-serve in one config, ingest +
//! output-serve were dead.
//!
//! The fix: ONE bound dual-stack socket adopted by ALL roles. These tests prove a
//! single [`UnifiedEndpoint`] hosts preview + WHIP + WHEP-serve at once, each role
//! reachable (negotiates an answer) — no second bind, no degrade.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::similar_names,
    clippy::default_constructed_unit_structs
)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig, UnifiedEndpoint};

/// Build a publisher/viewer offer (a browser/OBS-shaped offer with a host
/// candidate) so each role has a real SDP to negotiate against.
fn offer(direction_recv: bool) -> String {
    let now = Instant::now();
    let mut s = Session::new(&SessionConfig::default(), now);
    s.add_host_candidate("[::1]:55000".parse::<SocketAddr>().unwrap())
        .unwrap();
    if direction_recv {
        s.create_recv_offer(&[MediaKind::Video, MediaKind::Audio])
            .unwrap()
    } else {
        s.create_offer(&[MediaKind::Video, MediaKind::Audio])
            .unwrap()
    }
}

#[test]
fn one_socket_hosts_preview_whip_and_whep_serve_without_eaddrinuse() {
    // The whole point of defect B: preview + WHIP ingest + WHEP-serve coexist on
    // ONE bound socket. Build a single endpoint, register all three roles, and
    // confirm each negotiates an answer — i.e. none silently degraded because a
    // second bind clashed.
    let cfg = EndpointConfig {
        // Ephemeral port (bind is local); a concrete advertised addr so the
        // gathered candidate is valid (the unspecified bind addr is not).
        udp_port: 0,
        advertised_addresses: vec!["::1".parse().unwrap()],
        ..EndpointConfig::default()
    };

    // ONE bind for the whole endpoint.
    let builder = UnifiedEndpoint::bind(cfg).expect("the single shared socket binds once");

    // Register WHIP ingest + WHEP-serve, and the native preview egress — all on the
    // one socket. The preview egress wants a CONCRETE host candidate (the
    // unspecified bind addr is not a valid str0m candidate), matching how the cli
    // wires it.
    let host = *builder
        .host_candidates()
        .iter()
        .find(|a| !a.ip().is_unspecified())
        .expect("a concrete advertised host candidate");
    let (builder, whip) = builder.with_ingest();
    let (builder, whep_serve) = builder.with_serve();
    let preview =
        std::sync::Arc::new(multiview_webrtc::whep_egress::WhepEgress::with_host_candidate(host));
    let _endpoint = builder
        .with_preview(std::sync::Arc::clone(&preview))
        .build();

    // 1. WHIP ingest negotiates a publisher answer.
    let whip_answer = whip
        .negotiate("cam-1", &offer(false), true)
        .expect("WHIP ingest is reachable on the shared socket (not EADDRINUSE-dead)");
    assert!(
        whip_answer.answer_sdp.contains("a=group:BUNDLE"),
        "WHIP answered str0m's own SDP"
    );

    // 2. WHEP-serve negotiates a viewer answer for a registered output.
    whep_serve.register_output("prog-out", 8, test_feed());
    let whep_answer = whep_serve
        .negotiate("prog-out", &offer(true), true)
        .expect("WHEP-serve is reachable on the shared socket (not EADDRINUSE-dead)");
    assert!(
        whep_answer.answer_sdp.contains("a=group:BUNDLE"),
        "WHEP-serve answered str0m's own SDP"
    );

    // 3. Preview WHEP egress negotiates a viewer answer.
    let preview_answer = preview
        .accept_session(
            &offer(true),
            multiview_preview::whep::PreviewCodec::H264,
            &FakeMedia::default(),
        )
        .expect("preview WHEP is reachable on the shared socket (not EADDRINUSE-dead)");
    assert!(
        preview_answer.sdp_answer.contains("a=group:BUNDLE"),
        "preview answered str0m's own SDP"
    );

    // All three roles negotiated — they share the single socket, none degraded.
    assert_eq!(whip.live_publisher_count(), 1);
    assert_eq!(whep_serve.live_viewer_count("prog-out"), 1);
}

/// Invariant #10 / round-2 BLOCKER: a **permanently-readable** (flooding) UDP
/// source must NOT starve the driver's timer/command/GC servicing. The driver
/// caps each receive wake at a budget, but the round-1 code then fell straight
/// back into `select!` with the readable arm permanently ready and *unbiased* —
/// so the tick arm (which drains the register/release command queue, advances
/// ICE timers, and GCs) was serviced only by probabilistic `select!` fairness,
/// not a deterministic handoff. Under a saturating flood the command queue was
/// not drained on any bounded schedule.
///
/// This drives the REAL public [`UnifiedEndpoint::run`] against a real socket a
/// dedicated OS thread floods without pause, registers a WHIP ingest session,
/// then issues a `release`. The release enqueues a `Release` command that ONLY
/// the driver's servicing path applies — closing the session's RTP ring
/// ([`RtpRing::is_ended`]). If servicing is starved by the flood the ring never
/// ends; the `tokio::time::timeout` then makes the regression a deterministic
/// **failure** (a hang), not a flaky pass. After the fix the driver performs an
/// explicit bounded handoff (`yield_now` + a single deterministic servicing
/// pump) after every exhausted receive budget, so the command is applied within
/// a couple of milliseconds regardless of the flood.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_inbound_flood_cannot_starve_timer_and_command_servicing() {
    let cfg = EndpointConfig {
        udp_port: 0,
        advertised_addresses: vec!["::1".parse().unwrap()],
        ..EndpointConfig::default()
    };
    let builder = UnifiedEndpoint::bind(cfg).expect("the single shared socket binds once");
    // The concrete bound address the flooder targets (ephemeral loopback port).
    let bound = builder.local_addr().expect("a bound local addr");
    let (builder, whip) = builder.with_ingest();
    let endpoint = builder.build();

    // Register a real ingest session so there is a live session whose RTP ring
    // the driver's servicing path can close on release.
    let negotiated = whip
        .negotiate("cam-flood", &offer(false), true)
        .expect("WHIP ingest negotiates a publisher on the shared socket");
    let ring = negotiated.ring.clone();
    assert!(
        !ring.is_ended(),
        "freshly-negotiated ring is open before any release"
    );

    // Spawn the real driver.
    let stop = Arc::new(AtomicBool::new(false));
    let driver = tokio::spawn(endpoint.run(Arc::clone(&stop)));

    // A dedicated OS thread floods the bound socket without pause, so the
    // driver's `socket.readable()` is effectively always ready — the permanent,
    // saturating, hostile flood the BLOCKER is about. It sends from a normal
    // blocking std socket so it does NOT share the tokio runtime's workers.
    let flood_stop = Arc::new(AtomicBool::new(false));
    let flooder = {
        let flood_stop = Arc::clone(&flood_stop);
        std::thread::spawn(move || {
            let s = std::net::UdpSocket::bind("[::1]:0").expect("flooder binds a v6 loopback");
            let target = SocketAddr::new("::1".parse().unwrap(), bound.port());
            // A small junk datagram str0m will demux-miss and drop — its only job
            // is to keep the receive path permanently busy.
            let junk = [0u8; 64];
            while !flood_stop.load(Ordering::Relaxed) {
                // Burst then re-check the stop flag; ignore send errors (a full
                // socket buffer just means the kernel queue is already saturated).
                for _ in 0..512 {
                    let _ = s.send_to(&junk, target);
                }
            }
        })
    };

    // Issue the release: this enqueues a `Release` command the driver's tick /
    // servicing path must apply (closing the ring) even while flooded.
    assert!(
        whip.release("cam-flood", negotiated.session_id.as_str()),
        "release dispatches a teardown command for the live session"
    );

    // The ring must end (the Release command was serviced) within a bounded
    // wall-clock window despite the permanent flood. A regression starves the
    // servicing path, the ring never ends, and this times out (a hard failure).
    let serviced = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if ring.is_ended() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;

    // Stop the driver and flooder before asserting, so a failure still tears down.
    stop.store(true, Ordering::Release);
    flood_stop.store(true, Ordering::Relaxed);

    // (b) The driver honours the stop signal within a bounded window even under
    // the still-draining flood — it cannot wedge.
    let stopped = tokio::time::timeout(Duration::from_secs(5), driver).await;
    let _ = flooder.join();

    assert!(
        serviced.is_ok() && serviced.unwrap(),
        "under a permanent inbound flood the driver must still service the \
         release command (close the ring) within the bound — it was starved"
    );
    assert!(
        matches!(stopped, Ok(Ok(Ok(())))),
        "the driver must honour the stop signal within the bound under flood, \
         got {stopped:?}"
    );
}

/// A bounded egress feed for the WHEP-serve output registration. The sink is
/// leaked alive (`Box::leak`) for the test's duration so the feed stays open.
fn test_feed() -> multiview_webrtc::egress::EgressFeed {
    let (sink, feed) = multiview_webrtc::egress::egress_feed();
    let _: &'static _ = Box::leak(Box::new(sink));
    feed
}

/// A minimal `PreviewMediaSource` for the preview negotiation (no real media; the
/// test only exercises negotiation reachability on the shared socket).
#[derive(Default)]
struct FakeMedia;

impl multiview_preview::whep::transport::PreviewMediaSource for FakeMedia {
    fn codec(&self) -> multiview_preview::whep::PreviewCodec {
        multiview_preview::whep::PreviewCodec::H264
    }
    fn feed(&self) -> multiview_preview::whep::transport::SampleFeed {
        let (sink, feed) = multiview_preview::whep::transport::sample_feed(8);
        let _: &'static _ = Box::leak(Box::new(sink));
        feed
    }
    fn audio_feed(&self) -> Option<multiview_preview::whep::transport::SampleFeed> {
        None
    }
}
