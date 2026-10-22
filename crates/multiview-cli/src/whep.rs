//! The engine-backed **live WHEP preview egress** provider (ADR-P006), gated
//! behind the cli `webrtc-native` feature.
//!
//! This wires the native `multiview_webrtc::whep_egress::WhepEgress` transport
//! (the single str0m owner, ADR-0048) into the control plane's
//! [`multiview_control::WhepProvider`] seam so a browser can WHEP-play a live
//! preview tap over real DTLS/SRTP, with audio, on all scopes:
//!
//! * **program** — sampled from the wait-free [`ProgramSlot`] the engine loop
//!   publishes the composited NV12 canvas into (a *pre-encode canvas approx*,
//!   ADR-P005), encoded with a real low-latency H.264 (preferred) or VP8 encoder;
//! * **input** — sampled from the per-source last-good [`TileStore`] the JPEG
//!   preview already reads; and
//! * **output** — the same sampling seam (a canvas-approx tap of the rendition's
//!   source frame); the real-encoded-bitstream fan-out tap is the ADR-P006 PRV-5b
//!   upgrade, not required for the live egress to function.
//!
//! Optional **Opus audio** rides the same seam when the offer negotiates it: a
//! per-session [`OpusEncoder`] fed the program audio bus.
//!
//! ## Isolation (invariant #10 — the cardinal preview rule)
//!
//! Every media source here **samples** a wait-free latest-frame slot / lock-free
//! store; it never blocks, paces, or back-pressures the engine. The encode +
//! egress run on a dedicated driver task that owns the shared UDP socket and only
//! ever pushes into bounded **drop-oldest** `SampleFeed`s — a stalled or absent
//! browser merely loses the oldest samples and can never stall the program path
//! or other sessions. The driver never `.await`s a client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::pipeline::Nv12Image;
use multiview_control::{WhepAnswer, WhepProvider, WhepReject, WhepScope};
use multiview_core::time::MediaTime;
use multiview_engine::{MonotonicTimeSource, TimeSource};
use multiview_ffmpeg::codec::VideoCodec;
use multiview_ffmpeg::encode::{VideoEncodeTarget, VideoEncoder};
use multiview_ffmpeg::encode_options::{
    preview_gop_frames, preview_h264_options, preview_vp8_options,
};
use multiview_preview::whep::transport::{
    sample_feed, EncodedSample, PreviewMediaSource, SampleFeed, SampleKind, SampleSink, SessionId,
    WhepTransport,
};
use multiview_preview::whep::PreviewCodec;
use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::transport::WebRtcEndpoint;
use multiview_webrtc::turn::TurnRelayDriver;
use multiview_webrtc::whep_egress::WhepEgress;

use crate::live_sources::SharedStores;
use crate::preview::ProgramSlot;

/// The preview egress frame rate cap (ADR-P006: ≤ 15 fps).
const PREVIEW_FPS: u32 = 15;
/// The longest-edge cap for the preview encode (ADR-P006: ≤ 1280).
const PREVIEW_LONGEST_EDGE: u32 = 1280;
/// The preview video bitrate budget (bits/sec).
const PREVIEW_BITRATE: usize = 1_500_000;
/// The 90 kHz video RTP clock tick per preview frame (90000 / 15).
const VIDEO_TS_STEP: u32 = 90_000 / PREVIEW_FPS;
/// The driver tick: ≤ preview cadence, so a session pumps at least per frame and
/// ICE/DTLS timers advance promptly.
const DRIVER_TICK: Duration = Duration::from_millis(20);

/// A real preview video encoder (H.264 preferred, VP8 fallback) behind a mutex so
/// the `&self` `PreviewMediaSource` pump can drive its `&mut`-only libav context.
///
/// Lazily opened on the first frame (when the frame geometry is known), scaled to
/// the preview longest-edge cap and re-stamped on the 90 kHz RTP clock.
struct PreviewVideoEncoder {
    codec: PreviewCodec,
    inner: Mutex<Option<VideoEncoder>>,
    /// The next 90 kHz RTP timestamp to stamp (monotonic; one step per frame).
    rtp_ts: AtomicU32,
    /// Whether the first emitted access unit (the keyframe) has been produced.
    first_done: AtomicBool,
}

impl PreviewVideoEncoder {
    fn new(codec: PreviewCodec) -> Self {
        Self {
            codec,
            inner: Mutex::new(None),
            rtp_ts: AtomicU32::new(0),
            first_done: AtomicBool::new(false),
        }
    }

    /// The scaled (longest-edge-capped, even) preview dimensions for `image`.
    ///
    /// Integer-only scaling (the workspace bans silent `as` conversions): scale
    /// each dimension by `PREVIEW_LONGEST_EDGE / longest` using `u64` math, then
    /// floor to an even value (NV12 chroma) with a floor of 2.
    fn target_dims(image: &Nv12Image) -> (u32, u32) {
        let (w, h) = (image.width().max(2), image.height().max(2));
        let longest = w.max(h);
        if longest <= PREVIEW_LONGEST_EDGE {
            return (w & !1, h & !1);
        }
        let cap = u64::from(PREVIEW_LONGEST_EDGE);
        let long64 = u64::from(longest).max(1);
        let scale = |v: u32| -> u32 {
            let scaled = u64::from(v).saturating_mul(cap) / long64;
            let clamped = u32::try_from(scaled).unwrap_or(PREVIEW_LONGEST_EDGE);
            (clamped.max(2)) & !1
        };
        (scale(w), scale(h))
    }

    /// Encode one NV12 frame into an [`EncodedSample`] (or `None` if the encoder
    /// buffered it). The frame is fed at its native size; the encoder's configured
    /// output size handles the scale. Best-effort: an encode fault yields `None`
    /// (preview never propagates a failure).
    fn encode(&self, image: &Nv12Image) -> Option<EncodedSample> {
        let Ok(mut guard) = self.inner.lock() else {
            return None;
        };
        if guard.is_none() {
            *guard = self.open(image);
        }
        let encoder = guard.as_mut()?;
        let video = nv12_to_video(image).ok()?;
        encoder.send_frame(&video).ok()?;
        let packet = encoder.receive_packet().ok()??;
        let data = packet.data()?;
        let first = !self.first_done.swap(true, Ordering::AcqRel);
        let ts = self.rtp_ts.fetch_add(VIDEO_TS_STEP, Ordering::AcqRel);
        Some(EncodedSample {
            data: Arc::from(data),
            rtp_timestamp: ts,
            keyframe: first || packet.is_key(),
            kind: SampleKind::Video,
        })
    }

    /// Open the libav encoder for `image`'s (scaled) geometry with the fixed
    /// low-latency preview profile (zerolatency, B-frames off, repeat-headers,
    /// 2 s GOP — ADR-P006), choosing a runtime-resolved encoder.
    fn open(&self, image: &Nv12Image) -> Option<VideoEncoder> {
        use ffmpeg_next::format::Pixel;
        use multiview_core::time::Rational;
        let (w, h) = Self::target_dims(image);
        // The preview cadence as an exact rational (invariant #3: never float fps).
        let fps = Rational::new(i64::from(PREVIEW_FPS), 1);
        let (encoder_name, options) = match self.codec {
            PreviewCodec::H264 => {
                let name = multiview_ffmpeg::codec::select_encoder(VideoCodec::H264)?;
                (name, preview_h264_options(name, fps))
            }
            PreviewCodec::Vp8 => {
                let name = multiview_ffmpeg::codec::select_encoder(VideoCodec::Vp8)?;
                (name, preview_vp8_options(fps))
            }
            // A future preview codec with no encoder wired here yields no encoder
            // (the session was already rejected at negotiate if unsupported).
            _ => return None,
        };
        let target = VideoEncodeTarget {
            codec_name: encoder_name.to_owned(),
            width: w,
            height: h,
            format: Pixel::NV12,
            // The encoder time-base is the reciprocal of the cadence (1/fps).
            time_base: Rational::new(1, i64::from(PREVIEW_FPS)),
            bit_rate: PREVIEW_BITRATE,
            gop: preview_gop_frames(fps),
            cuda_device: None,
        };
        VideoEncoder::new_with_options(&target, &options).ok()
    }
}

/// The lazily-opened program-audio Opus encoder for a WHEP session, behind a
/// mutex so the `&self` `pump_once` can drive its `&mut`-only libav context.
///
/// 48 kHz / 20 ms / stereo `libopus` (ADR-0049/ADR-P006). Fed the interleaved
/// `f32` program PCM the bake consumer taps post-loudnorm; it packetizes into
/// 20 ms frames internally and stamps each on the 48 kHz RTP clock from a sample
/// counter (invariant #3: a counter, never input time).
struct PreviewOpusEncoder {
    inner: Mutex<Option<multiview_ffmpeg::OpusEncoder>>,
    /// Whether opening the encoder has been attempted (a failed open is not
    /// retried every block — audio then stays absent for the session).
    tried_open: AtomicBool,
}

impl PreviewOpusEncoder {
    fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            tried_open: AtomicBool::new(false),
        }
    }

    /// Encode one interleaved-stereo `f32` program block into zero or more Opus
    /// [`EncodedSample`]s (one per 20 ms frame the block completes). Best-effort:
    /// an open/encode fault yields no samples (preview never propagates a fault).
    fn encode(&self, interleaved: &[f32]) -> Vec<EncodedSample> {
        let Ok(mut guard) = self.inner.lock() else {
            return Vec::new();
        };
        if guard.is_none() {
            if self.tried_open.swap(true, Ordering::AcqRel) {
                return Vec::new(); // a prior open failed; don't retry per block.
            }
            // The program Opus rendition bit rate (ADR-0049 streaming profile).
            *guard = multiview_ffmpeg::OpusEncoder::new(96_000).ok();
        }
        let Some(encoder) = guard.as_mut() else {
            return Vec::new();
        };
        if encoder.push_interleaved_f32(interleaved).is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Ok(Some(packet)) = encoder.receive_packet() {
            // The packet PTS is the running 48 kHz sample counter (RFC 7587 fixes
            // the Opus RTP clock at 48 kHz); narrow it to the wire u32 RTP stamp.
            let rtp_timestamp = packet
                .pts()
                .and_then(|p| u32::try_from(p & i64::from(u32::MAX)).ok())
                .unwrap_or(0);
            let owned = packet.to_owned_packet();
            let Some(data) = owned.data() else { continue };
            if data.is_empty() {
                continue;
            }
            out.push(EncodedSample {
                data: Arc::from(data),
                rtp_timestamp,
                keyframe: false, // audio is independently decodable (no gate).
                kind: SampleKind::Audio,
            });
        }
        out
    }
}

/// A [`PreviewMediaSource`] that samples a wait-free NV12 latest-frame slot,
/// encodes each sampled frame with a real preview encoder, and pushes the encoded
/// sample into a bounded **drop-oldest** feed the transport drains.
///
/// The slot is the engine's program/preview slot (program scope) or a per-input
/// last-good store snapshot (input/output scope). Sampling is non-blocking and
/// drop-oldest at both ends — invariant #10.
struct SlotMediaSource {
    encoder: PreviewVideoEncoder,
    slot: SlotReader,
    sink: SampleSink,
    feed: Mutex<Option<SampleFeed>>,
    codec: PreviewCodec,
    /// The consumer end of the Opus audio feed, handed to the transport exactly
    /// once. Present only when the session negotiated Opus AND this scope has a
    /// program-audio source wired (ADR-P006: a feed that can never be fed is never
    /// held). `None` ⇒ the session is video-only.
    audio: Mutex<Option<SampleFeed>>,
    /// The program-PCM preview tap + Opus encoder driving the audio feed. Present
    /// only when this scope has program audio AND the offer negotiated Opus; the
    /// driver calls [`Self::pump_once`] which drains the tap, Opus-encodes, and
    /// pushes into the audio sink (drop-oldest). `None` ⇒ no audio is produced.
    audio_pump: Option<AudioPump>,
}

/// The producer half of a WHEP session's program audio: the bounded drop-oldest
/// program-PCM preview tap, the Opus encoder, and the sink feeding the transport.
struct AudioPump {
    tap: crate::preview::ProgramAudioSlot,
    encoder: PreviewOpusEncoder,
    sink: SampleSink,
}

/// How a [`SlotMediaSource`] reads its latest NV12 frame — wait-free for every
/// scope (a slot load or a lock-free store read).
enum SlotReader {
    /// The program/preview slot the engine loop publishes the composited canvas
    /// into (program scope).
    Program(ProgramSlot),
    /// A per-input last-good store, read at the current wall-clock instant
    /// (input/output scope).
    Input {
        stores: SharedStores,
        id: String,
        clock: MonotonicTimeSource,
    },
}

impl SlotReader {
    /// Load the latest NV12 frame, or `None` if none is available yet.
    fn load(&self) -> Option<Arc<Nv12Image>> {
        match self {
            Self::Program(slot) => slot.load_full(),
            Self::Input { stores, id, clock } => {
                let stores = stores.load();
                let store = stores.get(id)?;
                let now = MediaTime::from_nanos(clock.now_nanos());
                let read = store.read_at(now);
                read.frame().map(Arc::clone)
            }
        }
    }
}

impl SlotMediaSource {
    /// Build a video source over `slot`. `audio_tap` is the program-PCM preview
    /// slot the driver pumps Opus audio from when the session negotiated Opus —
    /// `None` when the scope has no program audio (the ADR-P006 contract: a
    /// session whose scope has no audio source leaves audio absent, never a dead
    /// feed). When a tap is given AND `opus` is true, an Opus feed + encoder is
    /// wired; otherwise audio stays absent. The transport carries Opus end-to-end
    /// (proven in `multiview-webrtc`'s offline egress test).
    fn new(
        codec: PreviewCodec,
        slot: SlotReader,
        audio_tap: Option<crate::preview::ProgramAudioSlot>,
        opus: bool,
    ) -> Self {
        // Shallow drop-oldest preview rings (ADR-P001: depth 1–3).
        let (sink, feed) = sample_feed(3);
        // Wire the audio producer only when the offer negotiated Opus AND this
        // scope has a program-audio tap. A deeper audio ring (depth 8) tolerates
        // the ~2:1 fan-out of one 40 ms program block into two 20 ms Opus frames.
        let (audio, audio_pump) = match (opus, audio_tap) {
            (true, Some(tap)) => {
                let (audio_sink, audio_feed) = sample_feed(8);
                (
                    Some(audio_feed),
                    Some(AudioPump {
                        tap,
                        encoder: PreviewOpusEncoder::new(),
                        sink: audio_sink,
                    }),
                )
            }
            _ => (None, None),
        };
        Self {
            encoder: PreviewVideoEncoder::new(codec),
            slot,
            sink,
            feed: Mutex::new(Some(feed)),
            codec,
            audio: Mutex::new(audio),
            audio_pump,
        }
    }

    /// Sample the latest frame + drain the program-PCM tap, encode + push (both
    /// drop-oldest). Called by the driver at preview cadence. Non-blocking; never
    /// paces the engine (it samples wait-free slots).
    fn pump_once(&self) {
        if let Some(image) = self.slot.load() {
            if let Some(sample) = self.encoder.encode(&image) {
                let _ = self.sink.push(sample);
            }
        }
        // Drain every queued program-audio block, Opus-encode it, and push the
        // resulting 20 ms frames (drop-oldest). Draining the bounded tap fully
        // each tick keeps audio paced by the program clock without growing memory;
        // a stalled WHEP consumer only loses the oldest Opus frames (inv #10).
        if let Some(pump) = &self.audio_pump {
            while let Some(block) = pump.tap.pop() {
                for sample in pump.encoder.encode(block.interleaved()) {
                    let _ = pump.sink.push(sample);
                }
            }
        }
    }
}

impl PreviewMediaSource for SlotMediaSource {
    fn codec(&self) -> PreviewCodec {
        self.codec
    }
    fn feed(&self) -> SampleFeed {
        self.feed
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .unwrap_or_else(|| sample_feed(1).1)
    }
    fn audio_feed(&self) -> Option<SampleFeed> {
        self.audio.lock().ok().and_then(|mut g| g.take())
    }
}

/// A live WHEP egress session's media sources, kept alive by the driver so its
/// `pump_once` runs each tick and its feeds are drained by the transport.
struct LiveSession {
    media: Arc<SlotMediaSource>,
    scope_label: String,
    /// The session's **own** program-PCM ring (when it negotiated Opus AND this
    /// run has program audio): the driver fans each shared-tap block into here,
    /// and `media.pump_once` drains it to Opus-encode. Per-session so multiple
    /// WHEP sessions don't steal each other's blocks. `None` ⇒ video-only session.
    audio_tap: Option<crate::preview::ProgramAudioSlot>,
}

/// The shared driver state: the native egress transport + the live media sources.
#[derive(Default)]
struct DriverState {
    sessions: HashMap<String, LiveSession>,
}

/// The engine-backed live WHEP egress provider.
///
/// Owns the native [`WhepEgress`] transport and the [`WebRtcEndpoint`] (the single
/// dual-stack UDP socket), drives them on a dedicated task, and builds a real
/// per-scope [`SlotMediaSource`] on each `negotiate`.
pub struct CliWhepProvider {
    egress: Arc<WhepEgress>,
    state: Arc<Mutex<DriverState>>,
    program: ProgramSlot,
    stores: SharedStores,
    /// The shared **program-audio preview tap** (ADR-P006 audio): the single drop-
    /// oldest ring the bake consumer pushes post-loudnorm program PCM into. The
    /// driver drains it once per tick and fans each block to every live audio
    /// session's own per-session ring (so multiple WHEP sessions don't steal each
    /// other's blocks). `None` when this run carries no program audio — then the
    /// "program" / "output" scopes negotiate video-only (audio absent, ADR-P006).
    program_audio: Option<crate::preview::ProgramAudioSlot>,
    /// Whether the live native transport actually came up (the socket bound). When
    /// `false` the provider sheds every focus to the JPEG fallback and the
    /// capabilities endpoint advertises `webrtc: false`.
    available: bool,
}

impl std::fmt::Debug for CliWhepProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliWhepProvider")
            .field("available", &self.available)
            .finish_non_exhaustive()
    }
}

impl CliWhepProvider {
    /// Build the provider, bind the shared UDP socket from `config`, and spawn the
    /// driver task. If the socket cannot bind, the provider stays *unavailable*
    /// (the SPA sheds to JPEG honestly) rather than failing the run.
    ///
    /// `program_audio` is the shared program-PCM preview tap (ADR-P006 audio): the
    /// SAME [`ProgramAudioSlot`](crate::preview::ProgramAudioSlot) the cli also
    /// hands the pipeline (`Pipeline::set_program_audio_preview`), so a WHEP peer
    /// hears program audio when (and only when) program audio is configured.
    /// `None` ⇒ no program audio this run (WHEP negotiates video-only).
    #[must_use]
    pub fn spawn(
        config: EndpointConfig,
        program: ProgramSlot,
        stores: SharedStores,
        program_audio: Option<crate::preview::ProgramAudioSlot>,
    ) -> Self {
        let state = Arc::new(Mutex::new(DriverState::default()));
        // Build the TURN relay driver from the configured ICE servers BEFORE the
        // endpoint consumes `config` (the operator's NAT-traversal path, ADR-0048
        // §5.1): one in-crate TURN client per configured TURN server, driven sans-
        // IO inside the egress driver over the SAME UDP socket as the media. Empty
        // when no TURN server is configured (host + advertised candidates only).
        let turn = TurnRelayDriver::from_config(&config, Instant::now());
        let endpoint = match WebRtcEndpoint::bind(config) {
            Ok(ep) => ep,
            Err(err) => {
                tracing::warn!(%err, "WHEP egress endpoint bind failed; preview sheds to JPEG");
                return Self {
                    egress: Arc::new(WhepEgress::new()),
                    state,
                    program,
                    stores,
                    program_audio,
                    available: false,
                };
            }
        };
        // Register the bound host candidate so the answer carries reachability.
        // TURN relay candidates are learned at runtime by the driver's TURN
        // client(s) and published into the egress via `learn_relay` (ADR-0048
        // §5.1), so a browser behind NAT can WHEP-play via the operator's relay.
        let host = endpoint
            .host_candidates()
            .ok()
            .and_then(|c| c.into_iter().next());
        let egress = Arc::new(match host {
            Some(addr) => WhepEgress::with_host_candidate(addr),
            None => WhepEgress::new(),
        });
        // The local socket address relayed traffic egresses from (for the relay
        // candidate's `raddr`); the bound host candidate, or the unspecified bind
        // address as a fallback.
        let local = host.unwrap_or_else(|| {
            endpoint.local_addr().unwrap_or_else(|_| {
                std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
            })
        });
        spawn_driver(
            Arc::clone(&egress),
            Arc::clone(&state),
            endpoint,
            turn,
            local,
            program_audio.clone(),
        );
        Self {
            egress,
            state,
            program,
            stores,
            program_audio,
            available: true,
        }
    }

    /// Build the preview provider for the **single-socket** unified path (defect B,
    /// ADR-0048 §4): the caller (the cli's unified-webrtc wiring) supplies the
    /// `egress` transport that the shared [`UnifiedEndpoint`] drives on the ONE
    /// socket, and a `host` for the egress's host candidate. This spawns ONLY the
    /// encode-pump thread (it fills the `SampleFeed`s); it binds NO socket and runs
    /// NO socket I/O — that is the unified endpoint's job. `available` is `true`
    /// because the shared socket is already bound by the unified endpoint.
    #[must_use]
    pub fn for_unified(
        egress: Arc<WhepEgress>,
        program: ProgramSlot,
        stores: SharedStores,
        program_audio: Option<crate::preview::ProgramAudioSlot>,
    ) -> Self {
        let state = Arc::new(Mutex::new(DriverState::default()));
        spawn_encode_pump(Arc::clone(&state), program_audio.clone());
        Self {
            egress,
            state,
            program,
            stores,
            program_audio,
            available: true,
        }
    }

    /// The native egress transport this provider negotiates against — handed to the
    /// shared [`UnifiedEndpoint`] so the ONE socket drives its inbound demux +
    /// egress drain (the single-socket model, ADR-0048 §4).
    #[must_use]
    pub fn egress(&self) -> Arc<WhepEgress> {
        Arc::clone(&self.egress)
    }

    /// Build a [`SlotMediaSource`] for `scope`, choosing the wait-free reader, and
    /// (when `opus` AND this run has program audio) a per-session program-PCM ring
    /// the driver fans the shared tap into. Returns the media plus that per-session
    /// tap (`None` ⇒ video-only). Audio rides the program scopes only; an input
    /// scope previews that input's video (its per-source audio fan-out is the
    /// ADR-P006 PRV-5b follow-on), so it stays video-only here.
    fn media_for(
        &self,
        scope: &WhepScope,
        codec: PreviewCodec,
        opus: bool,
    ) -> (
        Arc<SlotMediaSource>,
        Option<crate::preview::ProgramAudioSlot>,
    ) {
        let (reader, audio_scope) = match scope {
            WhepScope::Input(id) => (
                SlotReader::Input {
                    stores: self.stores.clone(),
                    id: id.clone(),
                    clock: MonotonicTimeSource::new(),
                },
                false,
            ),
            // Program, Output, and any future scope sample the program/preview
            // slot (the canvas-approx tap; the real-rendition fan-out is the
            // ADR-P006 PRV-5b upgrade) and carry the program audio.
            WhepScope::Program | WhepScope::Output(_) | _ => {
                (SlotReader::Program(Arc::clone(&self.program)), true)
            }
        };
        // A per-session program-PCM ring, only when the offer negotiated Opus AND
        // this run has program audio AND the scope carries audio.
        let session_tap = (opus && audio_scope && self.program_audio.is_some())
            .then(crate::preview::ProgramAudioSlot::new);
        let media = Arc::new(SlotMediaSource::new(
            codec,
            reader,
            session_tap.clone(),
            opus,
        ));
        (media, session_tap)
    }
}

impl WhepProvider for CliWhepProvider {
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject> {
        if !self.available {
            return Err(WhepReject::CapacityExceeded {
                fallback: "jpeg".to_owned(),
            });
        }
        // Select the preview codec from the offer (H.264 preferred, then VP8).
        let codec = select_offer_codec(offer)?;
        // The offer may negotiate Opus; program audio reaches the peer when this
        // run has a program-audio tap and the scope carries audio (ADR-P006).
        let opus = offer_has_opus(offer);
        let (media, audio_tap) = self.media_for(scope, codec, opus);
        let answer = self
            .egress
            .accept_session(offer, codec, media.as_ref())
            .map_err(|e| map_whep_error(&e))?;
        let session_id = answer.transport.session_id.as_str().to_owned();
        // Keep the media source alive + pumped by the driver for the session's life.
        if let Ok(mut state) = self.state.lock() {
            state.sessions.insert(
                session_id.clone(),
                LiveSession {
                    media,
                    scope_label: scope.label(),
                    audio_tap,
                },
            );
        }
        Ok(WhepAnswer {
            session_id,
            sdp: answer.sdp_answer,
        })
    }

    fn release(&self, _scope: &WhepScope, session_id: &str) -> bool {
        let id = SessionId::new(session_id);
        let known = self.egress.session_state(&id).is_some();
        let _ = self.egress.close(&id);
        if let Ok(mut state) = self.state.lock() {
            state.sessions.remove(session_id);
        }
        known
    }

    fn active_sessions(&self) -> usize {
        self.state.lock().map_or(0, |s| s.sessions.len())
    }

    fn webrtc_available(&self) -> bool {
        self.available
    }
}

/// Spawn ONLY the preview **encode-pump** thread (no socket I/O): drain the shared
/// program-audio tap, fan it to each audio session's ring, and pump each session's
/// sample→encode→`SampleFeed` at preview cadence. The socket I/O (inbound demux,
/// TURN, egress drain→send) is owned by the single shared
/// [`UnifiedEndpoint`](multiview_webrtc::transport::UnifiedEndpoint) (defect B,
/// ADR-0048 §4), which drains the `SampleFeed`s this thread fills. The two are
/// cleanly isolated: this thread touches only bounded drop-oldest feeds, never the
/// socket (invariant #10).
fn spawn_encode_pump(
    state: Arc<Mutex<DriverState>>,
    program_audio: Option<crate::preview::ProgramAudioSlot>,
) {
    std::thread::Builder::new()
        .name("whep-encode-pump".to_owned())
        .spawn(move || loop {
            if let Ok(state) = state.lock() {
                fan_program_audio(program_audio.as_ref(), state.sessions.values());
                for session in state.sessions.values() {
                    session.media.pump_once();
                    let _ = &session.scope_label;
                }
            }
            // Preview cadence; never busy-spin (best-effort, inv #10).
            std::thread::sleep(DRIVER_TICK);
        })
        .ok();
}

/// Spawn the egress driver: own the shared UDP socket, run the configured TURN
/// clients over it, pump every session's encode + egress at preview cadence, and
/// fan inbound datagrams to the sessions. Never `.await`s a client; a stalled
/// session loses only its own media (invariant #10).
///
/// This is the **standalone** preview driver (preview alone on its own socket),
/// retained for direct use/tests; the cli runs every WebRTC role on ONE socket via
/// [`UnifiedEndpoint`](multiview_webrtc::transport::UnifiedEndpoint) using
/// [`CliWhepProvider::for_unified`] + [`spawn_encode_pump`] instead (ADR-0048 §4).
#[allow(
    dead_code,
    reason = "standalone preview-only driver retained alongside the unified path"
)]
fn spawn_driver(
    egress: Arc<WhepEgress>,
    state: Arc<Mutex<DriverState>>,
    endpoint: WebRtcEndpoint,
    mut turn: TurnRelayDriver,
    local: std::net::SocketAddr,
    program_audio: Option<crate::preview::ProgramAudioSlot>,
) {
    std::thread::Builder::new()
        .name("whep-egress-driver".to_owned())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let now = Instant::now();
                // 1. Drain inbound datagrams (non-blocking). A datagram from a
                //    configured TURN server feeds its client (allocation/refresh);
                //    any other datagram is media broadcast to the sessions.
                loop {
                    match endpoint.recv_from(&mut buf) {
                        Ok((len, source)) => {
                            let payload = buf.get(..len).unwrap_or(&[]);
                            // A relayed Data indication decapsulates to media on the
                            // relay addr (defect C); a TURN control reply feeds the
                            // driver; anything else is media on the local addr.
                            if let Some(relayed) = turn.try_unwrap_relayed(source, payload, now) {
                                let _ = egress.handle_datagram_broadcast(
                                    relayed.peer,
                                    relayed.relay,
                                    &relayed.payload,
                                    now,
                                );
                                continue;
                            }
                            if turn.feed(source, payload, now) {
                                continue;
                            }
                            let dst = endpoint.local_addr().unwrap_or(source);
                            let _ = egress.handle_datagram_broadcast(source, dst, payload, now);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
                // 2. Drive the TURN client(s): send their queued datagrams and
                //    publish any newly-allocated relay into the egress so the next
                //    negotiated session offers it as a relay candidate (the
                //    operator's NAT-traversal path, ADR-0048 §5.1).
                pump_turn_relays(&mut turn, egress.as_ref(), local, now, |payload, dst| {
                    let _ = endpoint.send_to(payload, dst);
                });
                // 3. Pump each live session's encode (sample → encode → feed).
                //    First drain the SHARED program-audio tap once and fan each
                //    block to every audio session's per-session ring (so sessions
                //    don't steal each other's blocks), then encode per session.
                if let Ok(state) = state.lock() {
                    fan_program_audio(program_audio.as_ref(), state.sessions.values());
                    for session in state.sessions.values() {
                        session.media.pump_once();
                        let _ = &session.scope_label;
                    }
                }
                // 4. Drive every session's egress and send the outbound datagrams,
                //    routing through the TURN relay when str0m chose the relay
                //    candidate (source == an allocated relay addr → TURN Send
                //    indication to the relay's server; defect C).
                if let Ok(out) = egress.drive_all(now) {
                    for (source, dst, payload) in out {
                        if turn.is_relay(source) {
                            if let Some((server, wire)) =
                                turn.frame_for_relay(source, dst, &payload, now)
                            {
                                let _ = endpoint.send_to(&wire, server);
                            }
                        } else {
                            let _ = endpoint.send_to(&payload, dst);
                        }
                    }
                }
                // 5. Park until the next session timer or the driver tick, whichever
                //    is sooner — never busy-spin (invariant #10: best-effort).
                let wake = egress.next_wake(now);
                let until = wake
                    .saturating_duration_since(Instant::now())
                    .min(DRIVER_TICK);
                std::thread::sleep(until.max(Duration::from_millis(1)));
            }
        })
        .ok();
}

/// Drain the SHARED program-audio tap once and fan each post-loudnorm block to
/// every live audio session's per-session ring (ADR-P006 audio). A single drain
/// of the shared tap (so the bake consumer's bounded ring never grows) cloned to
/// each audio session, so concurrent WHEP sessions never steal each other's
/// blocks — each session then Opus-encodes from its own ring in `pump_once`.
///
/// A no-op when this run has no program-audio tap, or when no live session
/// negotiated audio. Non-blocking; every push is bounded drop-oldest (inv #10).
fn fan_program_audio<'a, I>(tap: Option<&crate::preview::ProgramAudioSlot>, sessions: I)
where
    I: Iterator<Item = &'a LiveSession>,
{
    let Some(tap) = tap else { return };
    // The per-session audio rings present this tick (a video-only session has none).
    let session_taps: Vec<&crate::preview::ProgramAudioSlot> =
        sessions.filter_map(|s| s.audio_tap.as_ref()).collect();
    if session_taps.is_empty() {
        // No audio consumer: still drain the shared tap so it never grows while a
        // video-only (or no) session is connected (bounded drop-oldest anyway).
        while tap.pop().is_some() {}
        return;
    }
    while let Some(block) = tap.pop() {
        for session_tap in &session_taps {
            // Drop-oldest into each session's ring; the eviction flag is the
            // "consumer behind" signal we intentionally ignore here (inv #10).
            let _ = session_tap.push(block.clone());
        }
    }
}

/// Drive the shared [`TurnRelayDriver`] one pass: send each queued TURN datagram
/// via `send` and publish any newly-allocated relay into `egress` (with `local`
/// as the address the relayed traffic egresses from), so every subsequently-
/// negotiated WHEP session offers it as a relay candidate (ADR-0048 §5.1).
///
/// Pure over the `send` closure (the socket is the caller's) so the driver's TURN
/// pump is offline-testable without binding a socket — mirroring the WHIP
/// endpoint's `pump_turn`. A no-op when no TURN server is configured.
fn pump_turn_relays<S: FnMut(&[u8], std::net::SocketAddr)>(
    turn: &mut TurnRelayDriver,
    egress: &WhepEgress,
    local: std::net::SocketAddr,
    now: Instant,
    mut send: S,
) {
    while let Some((dst, payload)) = turn.poll_transmit(now) {
        send(&payload, dst);
    }
    for relay in turn.take_new_relays() {
        egress.learn_relay(relay, local);
    }
}

/// Select the preview codec from a WHEP offer (H.264 preferred, then VP8),
/// mapping the absence of a supported codec to the route's `415`.
fn select_offer_codec(offer: &str) -> Result<PreviewCodec, WhepReject> {
    let mut in_video = false;
    let mut has_h264 = false;
    let mut has_vp8 = false;
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_video = rest.starts_with("video");
            continue;
        }
        if !in_video {
            continue;
        }
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(name) = rtpmap
                .split_whitespace()
                .nth(1)
                .and_then(|m| m.split('/').next())
            {
                if name.eq_ignore_ascii_case("H264") {
                    has_h264 = true;
                } else if name.eq_ignore_ascii_case("VP8") {
                    has_vp8 = true;
                }
            }
        }
    }
    if has_h264 && multiview_ffmpeg::codec::can_encode(VideoCodec::H264) {
        Ok(PreviewCodec::H264)
    } else if has_vp8 && multiview_ffmpeg::codec::can_encode(VideoCodec::Vp8) {
        Ok(PreviewCodec::Vp8)
    } else {
        Err(WhepReject::UnsupportedCodec)
    }
}

/// Whether the offer carries an Opus audio m-line at 48 kHz (ADR-P006).
fn offer_has_opus(offer: &str) -> bool {
    let mut in_audio = false;
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_audio = rest.starts_with("audio");
            continue;
        }
        if !in_audio {
            continue;
        }
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(m) = rtpmap.split_whitespace().nth(1) {
                let mut f = m.split('/');
                if f.next().is_some_and(|n| n.eq_ignore_ascii_case("opus"))
                    && f.next() == Some("48000")
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Map a transport [`WhepError`] onto the control-plane [`WhepReject`].
fn map_whep_error(err: &multiview_preview::whep::WhepError) -> WhepReject {
    use multiview_preview::whep::WhepError;
    match err {
        WhepError::NoSupportedCodec => WhepReject::UnsupportedCodec,
        WhepError::MalformedOffer { reason } => WhepReject::Malformed((*reason).to_owned()),
        // Access/lifecycle faults (and any future variant) shed honestly to JPEG —
        // a preview-only refusal that never reflects or affects the engine.
        WhepError::AccessDenied { .. } | WhepError::IllegalTransition { .. } | _ => {
            WhepReject::CapacityExceeded {
                fallback: "jpeg".to_owned(),
            }
        }
    }
}

/// Convert an [`Nv12Image`] into a libav NV12 [`Video`] frame for the encoder
/// (safe `ffmpeg-next` value API only — no FFI in this crate).
fn nv12_to_video(image: &Nv12Image) -> Result<ffmpeg_next::util::frame::Video, ()> {
    use ffmpeg_next::format::Pixel;
    use ffmpeg_next::util::frame::Video;
    let w = image.width();
    let h = image.height();
    let mut frame = Video::new(Pixel::NV12, w, h);
    let wu = usize::try_from(w).map_err(|_| ())?;
    let hu = usize::try_from(h).map_err(|_| ())?;
    let y_stride = frame.stride(0);
    let uv_stride = frame.stride(1);
    copy_plane(frame.data_mut(0), y_stride, image.y_plane(), wu, hu)?;
    copy_plane(frame.data_mut(1), uv_stride, image.uv_plane(), wu, hu / 2)?;
    Ok(frame)
}

/// Copy `rows` rows of `row_bytes` from a tightly-packed `src` into a
/// (possibly stride-padded) `dst` plane.
fn copy_plane(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    row_bytes: usize,
    rows: usize,
) -> Result<(), ()> {
    if dst_stride < row_bytes {
        return Err(());
    }
    for row in 0..rows {
        let s = src
            .get(row * row_bytes..(row * row_bytes + row_bytes))
            .ok_or(())?;
        let d = dst
            .get_mut(row * dst_stride..(row * dst_stride + row_bytes))
            .ok_or(())?;
        d.copy_from_slice(s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use std::net::SocketAddr;
    use std::time::Instant;

    use multiview_webrtc::config::{EndpointConfig, IceServer, TurnCredentials};
    use multiview_webrtc::turn::message::{long_term_key, Attribute, Class, Method, StunMessage};
    use multiview_webrtc::turn::TurnRelayDriver;
    use multiview_webrtc::whep_egress::WhepEgress;

    use super::pump_turn_relays;

    /// A WHEP offer carrying a recvonly H.264 video m-line (str0m needs real ICE
    /// credentials + a DTLS fingerprint + `setup:actpass` to answer).
    const VIDEO_OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP6 ::1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP6 ::\r\n\
a=ice-ufrag:tEsT\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 \
6F:8E:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7:\
F8:09:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

    /// A minimal, allocate-only fake TURN server (the long-term-credential 401
    /// challenge + an authenticated Allocate success carrying XOR-RELAYED-ADDRESS).
    /// Purpose-built for this wiring test; the full RFC 5766 surface (Refresh /
    /// `CreatePermission` / `ChannelBind` / Data) is exercised by the
    /// `multiview-webrtc` `fake_turn` fixture — here we only need the relay learned.
    struct MiniTurnServer {
        relay: SocketAddr,
        username: String,
        password: String,
        realm: String,
        nonce: String,
        saw_auth: bool,
    }

    impl MiniTurnServer {
        fn new(relay: SocketAddr, username: &str, password: &str, realm: &str) -> Self {
            Self {
                relay,
                username: username.to_owned(),
                password: password.to_owned(),
                realm: realm.to_owned(),
                nonce: "nonce-xyz".to_owned(),
                saw_auth: false,
            }
        }

        fn key(&self) -> Vec<u8> {
            long_term_key(&self.username, &self.realm, &self.password)
        }

        fn handle(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
            let msg = StunMessage::parse(datagram).expect("client sends valid STUN");
            if msg.class() != Class::Request || msg.method() != Method::Allocate {
                return None;
            }
            let authed = msg
                .attributes()
                .iter()
                .any(|a| matches!(a, Attribute::Username(_)));
            if !authed || !msg.verify_integrity(&self.key()) {
                let mut reply = StunMessage::with_transaction(
                    Class::Error,
                    Method::Allocate,
                    msg.transaction_id(),
                );
                reply.push(Attribute::ErrorCode {
                    code: 401,
                    reason: "Unauthorized".to_owned(),
                });
                reply.push(Attribute::Realm(self.realm.clone()));
                reply.push(Attribute::Nonce(self.nonce.clone()));
                return Some(reply.to_bytes(None));
            }
            self.saw_auth = true;
            let mut reply = StunMessage::with_transaction(
                Class::Success,
                Method::Allocate,
                msg.transaction_id(),
            );
            reply.push(Attribute::XorRelayedAddress(self.relay));
            reply.push(Attribute::Lifetime(600));
            reply.push(Attribute::Username(self.username.clone()));
            reply.push(Attribute::Realm(self.realm.clone()));
            reply.push(Attribute::Nonce(self.nonce.clone()));
            Some(reply.to_bytes(Some(&self.key())))
        }
    }

    fn relay_candidate_count(candidates: &[String]) -> usize {
        candidates
            .iter()
            .filter(|c| c.contains("typ relay"))
            .count()
    }

    #[test]
    fn whep_driver_pump_runs_turn_and_publishes_the_relay_to_the_egress() {
        // ITEM-1 of PR #141: the cli WHEP egress driver runs its configured TURN
        // client over the SAME socket via `pump_turn_relays`, and a relay it
        // allocates is published into the WhepEgress so the next negotiated
        // session offers a `typ relay` candidate (browser-behind-NAT WHEP-play).
        let now = Instant::now();
        let server_addr: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
        let relay_addr: SocketAddr = "[2001:db8::1]:49152".parse().unwrap();
        let host: SocketAddr = "[2001:db8::abc]:8189".parse().unwrap();
        let mut server = MiniTurnServer::new(relay_addr, "alice", "s3cret", "example.org");

        let config = EndpointConfig {
            ice_servers: vec![IceServer::turn(
                server_addr,
                TurnCredentials::long_term("alice", "s3cret"),
            )],
            ..EndpointConfig::default()
        };
        let mut turn = TurnRelayDriver::from_config(&config, now);
        assert_eq!(
            turn.client_count(),
            1,
            "the WHEP driver built a TURN client"
        );
        let egress = WhepEgress::with_host_candidate(host);

        // Drive the pump in a shuttle: the closure captures the TURN datagrams and
        // feeds them to the fake server, whose replies we feed back into the
        // driver before the next pump (mirroring the live recv→feed loop).
        let mut pending_replies: Vec<Vec<u8>> = Vec::new();
        for _ in 0..16 {
            for reply in pending_replies.drain(..) {
                assert!(
                    turn.feed(server_addr, &reply, now),
                    "the TURN-server reply is consumed by the driver"
                );
            }
            pump_turn_relays(&mut turn, &egress, host, now, |payload, dst| {
                assert_eq!(dst, server_addr);
                if let Some(reply) = server.handle(payload) {
                    pending_replies.push(reply);
                }
            });
            // A relay learned ⇒ a subsequent session offers it; check early-exit.
            let media = crate::whep::tests::dummy_media();
            if let Ok(answer) = egress.accept_session(
                VIDEO_OFFER,
                multiview_preview::whep::PreviewCodec::H264,
                media.as_ref(),
            ) {
                if relay_candidate_count(&answer.transport.candidates) >= 1 {
                    assert!(server.saw_auth, "the Allocate was authenticated");
                    return; // success
                }
            }
        }
        panic!("the WHEP driver never published the TURN relay to the egress");
    }

    #[test]
    fn whep_driver_pump_is_a_noop_without_turn_and_never_blocks() {
        // INVARIANT #10: with no TURN configured the pump sends nothing and
        // publishes nothing — and it completes immediately (it never blocks the
        // egress driver thread, so a slow/absent TURN server can't stall preview).
        let now = Instant::now();
        let host: SocketAddr = "[2001:db8::abc]:8189".parse().unwrap();
        let mut turn = TurnRelayDriver::from_config(&EndpointConfig::default(), now);
        assert!(turn.is_empty());
        let egress = WhepEgress::with_host_candidate(host);

        let started = Instant::now();
        let mut sent = 0usize;
        pump_turn_relays(&mut turn, &egress, host, now, |_p, _d| sent += 1);
        assert_eq!(sent, 0, "no TURN server ⇒ no TURN datagrams");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "the pump never blocks"
        );

        let media = dummy_media();
        let answer = egress
            .accept_session(
                VIDEO_OFFER,
                multiview_preview::whep::PreviewCodec::H264,
                media.as_ref(),
            )
            .expect("accept");
        assert_eq!(
            relay_candidate_count(&answer.transport.candidates),
            0,
            "no relay candidate without TURN"
        );
    }

    use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
    use multiview_preview::whep::transport::{PreviewMediaSource, SampleKind};
    use multiview_preview::whep::PreviewCodec;

    use super::{fan_program_audio, LiveSession, SlotMediaSource, SlotReader};
    use crate::preview::ProgramAudioSlot;

    /// A 20 ms stereo program block at a steady value (one 20 ms Opus frame's
    /// worth of PCM at 48 kHz = 960 frames).
    fn pcm_block(value: f32) -> AudioBlock {
        let fmt = AudioFormat::new(48_000, ChannelLayout::Stereo);
        AudioBlock::from_interleaved(fmt, vec![value; 960 * 2]).unwrap()
    }

    fn program_media(opus: bool, tap: Option<ProgramAudioSlot>) -> SlotMediaSource {
        SlotMediaSource::new(
            PreviewCodec::H264,
            SlotReader::Program(crate::preview::program_slot()),
            tap,
            opus,
        )
    }

    #[test]
    fn program_audio_opus_encodes_to_the_audio_feed_when_opus_negotiated() {
        // ITEM-2 of PR #141: with Opus negotiated AND a program-audio tap, the
        // WHEP media source drains the tap, Opus-encodes the post-loudnorm PCM,
        // and the resulting frames reach the audio feed the transport drains.
        let tap = ProgramAudioSlot::new();
        let media = program_media(true, Some(tap.clone()));
        let audio_feed = media
            .audio_feed()
            .expect("an Opus offer wires an audio feed");

        // Feed several 20 ms blocks (>1 so libopus surely emits at least one
        // packet) and pump.
        for _ in 0..10 {
            let _ = tap.push(pcm_block(0.1));
        }
        media.pump_once();

        let mut opus_frames = 0usize;
        while let Some(sample) = audio_feed.pop() {
            assert_eq!(sample.kind, SampleKind::Audio, "audio-tagged");
            assert!(!sample.data.is_empty(), "a real Opus payload");
            opus_frames += 1;
        }
        assert!(
            opus_frames >= 1,
            "the program PCM was Opus-encoded onto the audio feed"
        );
        assert!(tap.is_empty(), "pump_once drains the per-session tap");
    }

    #[test]
    fn no_audio_feed_without_opus_or_without_a_tap() {
        // ADR-P006 "no audio source ⇒ audio absent": a feed that can never be fed
        // is never held.
        let no_opus = program_media(false, Some(ProgramAudioSlot::new()));
        assert!(
            no_opus.audio_feed().is_none(),
            "a video-only (no-Opus) offer leaves audio absent"
        );
        let no_tap = program_media(true, None);
        assert!(
            no_tap.audio_feed().is_none(),
            "an Opus offer with no program-audio tap leaves audio absent"
        );
    }

    fn audio_session(tap: ProgramAudioSlot) -> LiveSession {
        LiveSession {
            media: std::sync::Arc::new(program_media(true, Some(tap.clone()))),
            scope_label: "program".to_owned(),
            audio_tap: Some(tap),
        }
    }

    fn video_only_session() -> LiveSession {
        LiveSession {
            media: std::sync::Arc::new(program_media(false, None)),
            scope_label: "program".to_owned(),
            audio_tap: None,
        }
    }

    #[test]
    fn fan_program_audio_clones_to_every_audio_session_and_drains_the_shared_tap() {
        // The driver drains the SHARED tap once and fans each block to every audio
        // session's own ring, so concurrent sessions never steal each other's
        // blocks; a video-only session gets nothing.
        let shared = ProgramAudioSlot::new();
        let a = ProgramAudioSlot::new();
        let b = ProgramAudioSlot::new();
        let sessions = [
            audio_session(a.clone()),
            audio_session(b.clone()),
            video_only_session(),
        ];
        for _ in 0..3 {
            let _ = shared.push(pcm_block(0.1));
        }
        fan_program_audio(Some(&shared), sessions.iter());
        assert!(shared.is_empty(), "the shared tap is drained once per pass");
        assert_eq!(a.len(), 3, "session A received every block");
        assert_eq!(b.len(), 3, "session B received every block (no stealing)");
    }

    #[test]
    fn fan_program_audio_drains_the_tap_even_with_only_video_sessions() {
        // INVARIANT #10: with no audio consumer the shared tap must still be
        // drained so it never grows (it is drop-oldest anyway, but draining keeps
        // it empty while a video-only session is connected).
        let shared = ProgramAudioSlot::new();
        for _ in 0..5 {
            let _ = shared.push(pcm_block(0.2));
        }
        let sessions = [video_only_session()];
        fan_program_audio(Some(&shared), sessions.iter());
        assert!(
            shared.is_empty(),
            "the tap is drained even with no audio sink"
        );
    }

    #[test]
    fn a_stalled_audio_consumer_never_blocks_the_pump_or_grows_memory() {
        // INVARIANT #10: a WHEP audio consumer that never drains its feed (a
        // stalled/absent browser) must NEVER back-pressure the pump or grow
        // memory. Push far more program blocks than any ring depth and pump
        // repeatedly WITHOUT draining the audio feed; this stays bounded + fast.
        let tap = ProgramAudioSlot::new();
        let media = program_media(true, Some(tap.clone()));
        // Hold the feed but never pop it (the stalled consumer).
        let _held_feed = media.audio_feed();
        let started = std::time::Instant::now();
        for _ in 0..2_000 {
            let _ = tap.push(pcm_block(0.1));
            media.pump_once();
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "pumping into a drop-oldest feed never blocks on a stalled consumer"
        );
        assert!(
            tap.is_empty(),
            "each pump drains the bounded tap (never grows)"
        );
    }

    /// A no-op preview media source for the egress `accept_session` in these
    /// wiring tests (the relay candidate is what we assert, not media flow).
    pub(super) fn dummy_media() -> std::sync::Arc<DummyMedia> {
        std::sync::Arc::new(DummyMedia)
    }

    pub(super) struct DummyMedia;

    impl multiview_preview::whep::transport::PreviewMediaSource for DummyMedia {
        fn codec(&self) -> multiview_preview::whep::PreviewCodec {
            multiview_preview::whep::PreviewCodec::H264
        }
        fn feed(&self) -> multiview_preview::whep::transport::SampleFeed {
            multiview_preview::whep::transport::sample_feed(1).1
        }
        fn audio_feed(&self) -> Option<multiview_preview::whep::transport::SampleFeed> {
            None
        }
    }
}
