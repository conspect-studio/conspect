//! Single-socket WebRTC orchestration (ADR-0048 §4, box-validation defect B).
//!
//! Binds **one** shared dual-stack UDP socket via
//! [`UnifiedEndpoint`](multiview_webrtc::transport::UnifiedEndpoint) and adopts
//! **every** WebRTC role onto it — preview WHEP egress, WHIP ingest, WHEP-serve
//! outputs, and `whip_push` outputs — then spawns the **one** driver task that
//! demultiplexes all of them. Previously the cli bound `webrtc.udp_port` once per
//! role (preview + WHIP + WHEP-serve + each whip_push), so the 2nd/3rd `bind` hit
//! `EADDRINUSE` and silently degraded those roles to "unavailable"; with preview +
//! WHIP + WHEP-serve in one config, WHIP ingest and WHEP-serve were dead. This is
//! the wiring that makes the single-socket model the cli's actual path.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The one driver task owns the socket and never `.await`s a peer; per-role media
//! crosses only bounded drop-oldest rings/feeds. The preview encode runs on its
//! own pump thread that touches only `SampleFeed`s (filled there, drained by the
//! driver), never the socket — a clean producer/consumer split.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use multiview_config::MultiviewConfig;
use multiview_webrtc::transport::UnifiedEndpoint;
use multiview_webrtc::whep_egress::WhepEgress;

use crate::webrtc_endpoint::endpoint_config_from;
use crate::webrtc_ingest::WhipRegistry;
use crate::webrtc_outputs::EgressRegistry;

/// The control-plane providers wired to the one shared socket, returned to the
/// control-plane bring-up. Any role absent from the config is `None` (its routes
/// keep the default `503`/JPEG-shed behaviour).
pub struct UnifiedWebrtc {
    /// The WHEP **preview** provider (program/input/output scope previews).
    pub whep: Option<multiview_control::SharedWhep>,
    /// The WHIP **ingest** provider (`POST /api/v1/whip/{source}`).
    pub whip: Option<multiview_control::SharedWhip>,
    /// The WHEP-serve **output** provider (`POST /api/v1/whep/{output}`).
    pub whep_output: Option<multiview_control::SharedWhepOutput>,
}

/// Bind the one shared socket, adopt every configured WebRTC role onto it, spawn
/// the single driver task, and return the control-plane providers (defect B).
///
/// On a bind failure the whole subsystem sheds honestly: preview falls back to
/// JPEG (`whep: None` ⇒ the gated provider reports unavailable) and the WHIP /
/// WHEP-serve routes keep their default `503` — exactly as the per-role binds did,
/// but now from a single point of failure (the one socket) instead of three.
#[must_use]
pub fn spawn_unified_webrtc(
    config: &MultiviewConfig,
    program_slot: crate::preview::ProgramSlot,
    shared_stores: crate::live_sources::SharedStores,
    program_audio: Option<crate::preview::ProgramAudioSlot>,
    webrtc_registry: WhipRegistry,
    egress_registry: &EgressRegistry,
) -> UnifiedWebrtc {
    let endpoint_config = endpoint_config_from(config);
    let builder = match UnifiedEndpoint::bind(endpoint_config) {
        Ok(b) => b,
        Err(err) => {
            tracing::warn!(%err, "the shared WebRTC socket failed to bind; all WebRTC roles unavailable this run (preview sheds to JPEG)");
            return UnifiedWebrtc {
                whep: None,
                whip: None,
                whep_output: None,
            };
        }
    };

    // The concrete host candidate (the unspecified bind addr is not a valid str0m
    // candidate) for the preview egress; `None` ⇒ a socket-free egress (host comes
    // from advertised addresses on each session anyway).
    let host = builder
        .host_candidates()
        .iter()
        .find(|a| !a.ip().is_unspecified())
        .copied();

    // 1. WHIP ingest lane (only when there are `webrtc` sources).
    let want_ingest = crate::webrtc_ingest::has_webrtc_sources(config);
    let (builder, whip) = if want_ingest {
        let (builder, handle) = builder.with_ingest();
        (
            builder,
            crate::webrtc_ingest::provider_for_handle(config, handle, webrtc_registry),
        )
    } else {
        (builder, None)
    };

    // 2. WHEP-serve output lane (only when there are `webrtc` outputs).
    let want_serve = crate::webrtc_outputs::has_whep_outputs(egress_registry);
    let (builder, whep_output) = if want_serve {
        let (builder, handle) = builder.with_serve();
        (
            builder,
            crate::webrtc_outputs::provider_for_handle(egress_registry, handle),
        )
    } else {
        (builder, None)
    };

    // 3. The native preview egress (always wired when the socket bound — preview is
    //    the always-available WebRTC surface).
    let egress = Arc::new(match host {
        Some(addr) => WhepEgress::with_host_candidate(addr),
        None => WhepEgress::new(),
    });
    let preview_provider = crate::whep::CliWhepProvider::for_unified(
        Arc::clone(&egress),
        program_slot,
        shared_stores,
        program_audio,
    );
    let builder = builder.with_preview(Arc::clone(&egress));

    // 4. `whip_push` outputs (one lane per configured output).
    let mut builder = builder;
    for spec in crate::webrtc_outputs::whip_push_specs(egress_registry) {
        let Some(signaller) =
            crate::webrtc_outputs::build_push_signaller(spec.url.clone(), spec.token.clone())
        else {
            continue;
        };
        builder = builder.with_push(spec.feed, spec.audio, signaller);
        tracing::info!(output = %spec.id, url = %spec.url, "whip_push lane on the shared socket");
    }

    // Spawn the ONE driver task that owns the socket for the run's lifetime.
    let endpoint = builder.build();
    let stop = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        if let Err(err) = endpoint.run(stop).await {
            tracing::warn!(%err, "unified WebRTC endpoint driver exited");
        }
    });
    tracing::info!("unified WebRTC endpoint bound (one shared socket for preview + WHIP + WHEP-serve + whip_push)");

    let gated = multiview_control::GatedWhep::with_defaults(Arc::new(preview_provider));
    UnifiedWebrtc {
        whep: Some(Arc::new(gated)),
        whip,
        whep_output,
    }
}
