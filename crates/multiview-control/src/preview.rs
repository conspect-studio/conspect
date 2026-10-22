//! The live-preview provider seam.
//!
//! The control plane serves low-rate JPEG snapshots of the composited **program**
//! and of each **input** so the web UI can show live previews. The pixels live in
//! the engine/compositor (which this crate does not depend on), so the provider
//! is a trait the binary implements: it hands back already-encoded JPEG bytes,
//! keeping `multiview-control` free of any pixel-format or codec dependency.
//!
//! Like every other engine→control read path, a provider implementation must be
//! **isolation-safe** (invariant #10): it samples a wait-free latest-frame slot
//! and the lock-free per-input stores; it never blocks the engine. Encoding runs
//! on the request task (off the output-clock loop), not on the hot path.

use std::sync::Arc;

/// Supplies JPEG snapshots of the live program and inputs for the preview API.
///
/// `quality` is the JPEG quality (1–100). Each method returns `None` when no
/// frame is available yet (the route answers `503`), so a freshly-started engine
/// or an unknown input id degrades gracefully rather than erroring.
pub trait PreviewProvider: Send + Sync {
    /// The latest composited **program** frame as JPEG, or `None` if none yet.
    fn program_jpeg(&self, quality: u8) -> Option<Vec<u8>>;

    /// The latest frame of the input `id` as JPEG, or `None` if the input is
    /// unknown or has produced no frame.
    fn input_jpeg(&self, id: &str, quality: u8) -> Option<Vec<u8>>;

    /// The ids of the inputs that can be previewed (for the UI to enumerate
    /// thumbnails). May be empty.
    fn input_ids(&self) -> Vec<String>;
}

/// The default provider used when the binary wires no live preview (e.g. the
/// in-memory test harness): every snapshot is absent.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPreview;

impl PreviewProvider for NoPreview {
    fn program_jpeg(&self, _quality: u8) -> Option<Vec<u8>> {
        None
    }
    fn input_jpeg(&self, _id: &str, _quality: u8) -> Option<Vec<u8>> {
        None
    }
    fn input_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A shared [`PreviewProvider`] handle, as stored in [`crate::AppState`].
pub type SharedPreview = Arc<dyn PreviewProvider>;

/// The default shared provider ([`NoPreview`]).
#[must_use]
pub fn no_preview() -> SharedPreview {
    Arc::new(NoPreview)
}

/// The entity a WHEP focus session is opened against (preview brief §5).
///
/// Click-to-focus promotes exactly one entity from the cheap JPEG grid to a
/// single low-latency WebRTC preview encode. The scope is the routing key the
/// transport uses to find the right engine tap; control carries it opaquely so
/// it stays codec- and pixel-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhepScope {
    /// The composited program canvas (the multiview output).
    Program,
    /// One ingest input, by its source id.
    Input(String),
    /// One program output/rendition, by its output id.
    Output(String),
}

impl WhepScope {
    /// A short, stable label for logs/audit (never carries a secret).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Program => "program".to_owned(),
            Self::Input(id) => format!("input:{id}"),
            Self::Output(id) => format!("output:{id}"),
        }
    }
}

/// A negotiated WHEP focus answer: the transport-minted session id and the SDP
/// answer body the `201 Created` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhepAnswer {
    /// The transport's session id; also the trailing segment of the WHEP
    /// resource URL the client `DELETE`s to release the session.
    pub session_id: String,
    /// The SDP **answer** body (`application/sdp`).
    pub sdp: String,
}

/// Why a WHEP focus negotiation was refused.
///
/// The route layer maps each variant onto an RFC 9457 `application/problem+json`
/// response. A refusal is *operator-visible preview back-pressure*; it never
/// reflects or affects the protected engine output (invariant #10).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhepReject {
    /// The SDP offer was not well-formed enough to negotiate (`400`).
    Malformed(String),
    /// The offer advertised no video codec this build can preview-encode
    /// (`415 Unsupported Media Type`).
    UnsupportedCodec,
    /// The addressed entity (input/output id) is not one this deployment can
    /// focus — unknown id or a non-allowlisted scheme (`404`).
    UnknownEntity,
    /// The concurrent-focus cap is hit or no preview-encode budget is available,
    /// so the focus is shed with a `fallback` transport hint (`503`).
    CapacityExceeded {
        /// The transport the client should fall back to (e.g. `"jpeg"`,
        /// `"llhls"`), surfaced in the problem body so the UI degrades honestly.
        fallback: String,
    },
}

/// The WHEP focus transport seam — the codec-free boundary the control plane
/// delegates focus negotiation/teardown to.
///
/// The control plane stays free of any WebRTC/native dependency: it carries the
/// SDP offer string and the [`WhepScope`] in, and hands back a [`WhepAnswer`] or
/// a [`WhepReject`]. The binary (`multiview-cli`) implements this over the
/// `multiview-preview` `WhepTransport` seam (str0m or a `MediaMTX` sidecar behind a
/// further gate); the in-memory fake used in tests proves the route wiring.
///
/// ## Isolation (invariant #10)
///
/// A focus session is strictly a *preview* consumer. An implementation must read
/// engine taps lossily (drop-oldest) and must never hold a handle the engine
/// awaits, never publish onto the protected output path, and never block the
/// engine or the encoder feeding it. Both methods are synchronous and must not
/// block on the engine: control never `.await`s a focus negotiation against the
/// data plane.
pub trait WhepProvider: Send + Sync {
    /// Negotiate a focus session for `scope` from the browser's SDP `offer`.
    ///
    /// # Errors
    ///
    /// Returns a [`WhepReject`] when the offer is malformed, advertises no
    /// supported codec, addresses an unknown entity, or the focus cap / encode
    /// budget is exhausted. Every refusal is preview-only; it never affects the
    /// engine.
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject>;

    /// Release the focus session `session_id` previously opened for `scope`.
    ///
    /// Returns `true` if a matching live session was found and torn down,
    /// `false` if it was unknown or already released (so the route answers `204`
    /// for the idempotent teardown and `404` for an unknown session). Idempotent:
    /// releasing twice is not an error.
    fn release(&self, scope: &WhepScope, session_id: &str) -> bool;

    /// The number of live focus sessions (for the descriptor / isolation tests).
    ///
    /// Best-effort and preview-only; never reflects engine state.
    fn active_sessions(&self) -> usize;

    /// Whether this build can actually serve WHEP (a native ICE/DTLS/SRTP
    /// transport is wired). The `GET /api/v1/preview/capabilities` endpoint
    /// surfaces this so the SPA picks its transport (WHEP → JPEG ladder, ADR-W020)
    /// **before** issuing an offer (ADR-P006 move 6). The default is `false`: a
    /// negotiation-only / pure build advertises no WebRTC and the SPA stays on the
    /// always-available JPEG ladder.
    fn webrtc_available(&self) -> bool {
        false
    }

    /// The fidelity label the **program** WHEP scope would carry (ADR-P005/P006).
    ///
    /// `RealEncodedOutput` when the program rendition itself is WebRTC-compatible
    /// (H.264, B-frame-free) and the focus is fed the real encoded bitstream via
    /// the fan-out tap; otherwise the canvas-approx path
    /// ([`ProgramFidelity::PreEncodeCanvasApprox`]). The default is the
    /// canvas-approx path — the always-available program focus shape.
    fn program_fidelity(&self) -> ProgramFidelity {
        ProgramFidelity::PreEncodeCanvasApprox
    }
}

/// The fidelity a **program** WHEP focus would carry (ADR-P005/P006). Serialized
/// in the capabilities response so the SPA can label the program preview before
/// it opens one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProgramFidelity {
    /// The program focus is fed the real encoded program bitstream (a fan-out tap
    /// of a WebRTC-compatible rendition).
    RealEncodedOutput,
    /// The program focus is the pre-encode canvas downscale (the default,
    /// always-available program focus shape).
    PreEncodeCanvasApprox,
}

/// Per-scope WHEP availability (plus the program scope's fidelity label).
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ScopeCapability {
    /// Whether a WHEP focus can be opened on this scope.
    pub whep: bool,
    /// The program scope's fidelity label (only present for the program scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fidelity: Option<ProgramFidelity>,
}

/// The per-scope capability map.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ScopeCapabilities {
    /// The composited program canvas scope.
    pub program: ScopeCapability,
    /// The per-input scope.
    pub inputs: ScopeCapability,
    /// The per-output rendition scope.
    pub outputs: ScopeCapability,
}

/// The `GET /api/v1/preview/capabilities` response (ADR-P006 move 6): what preview
/// transports this build can serve, so the SPA picks WHEP vs the JPEG ladder
/// (ADR-W020) **before** issuing an offer.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PreviewCapabilities {
    /// Whether this build can serve WHEP/WebRTC at all (a native transport is
    /// wired). `false` on a pure / negotiation-only build.
    pub webrtc: bool,
    /// Per-scope WHEP availability + the program fidelity label.
    pub scopes: ScopeCapabilities,
    /// The always-available fallback transport literal: `"jpeg"` (the one
    /// fallback literal everywhere — ADR-P006 move 6).
    pub fallback: String,
}

impl PreviewCapabilities {
    /// Build the capabilities view from a [`WhepProvider`]. When WebRTC is
    /// unavailable every scope advertises `whep: false`, so the SPA stays on the
    /// JPEG ladder; `fallback` is always `"jpeg"`.
    #[must_use]
    pub fn from_provider(provider: &dyn WhepProvider) -> Self {
        let webrtc = provider.webrtc_available();
        Self {
            webrtc,
            scopes: ScopeCapabilities {
                program: ScopeCapability {
                    whep: webrtc,
                    fidelity: Some(provider.program_fidelity()),
                },
                inputs: ScopeCapability {
                    whep: webrtc,
                    fidelity: None,
                },
                outputs: ScopeCapability {
                    whep: webrtc,
                    fidelity: None,
                },
            },
            fallback: "jpeg".to_owned(),
        }
    }
}

/// The default WHEP provider used when the binary wires no focus transport (the
/// negotiation-only / pure build): every focus negotiation is refused as
/// unsupported and there are never any live sessions.
///
/// This keeps the routes present and authz-enforced even on a build without a
/// WebRTC transport — a `View` token still gets `403` and a valid `Focus` offer
/// gets an honest `503 fallback: jpeg`, never a `404`/panic.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWhep;

impl WhepProvider for NoWhep {
    fn negotiate(&self, _scope: &WhepScope, _offer: &str) -> Result<WhepAnswer, WhepReject> {
        // No transport is wired: the focus cannot be served, so shed honestly to
        // the always-available JPEG transport rather than pretending success.
        Err(WhepReject::CapacityExceeded {
            fallback: "jpeg".to_owned(),
        })
    }

    fn release(&self, _scope: &WhepScope, _session_id: &str) -> bool {
        false
    }

    fn active_sessions(&self) -> usize {
        0
    }
}

/// A shared [`WhepProvider`] handle, as stored in [`crate::AppState`].
pub type SharedWhep = Arc<dyn WhepProvider>;

/// The default shared WHEP provider ([`NoWhep`]).
#[must_use]
pub fn no_whep() -> SharedWhep {
    Arc::new(NoWhep)
}

use std::collections::HashMap;
use std::sync::Mutex;

pub use multiview_preview::focus::FocusCaps;
use multiview_preview::focus::{FocusGate, FocusLease};

/// A [`WhepProvider`] decorator that enforces the **concurrent-focus caps** in
/// front of any inner transport (PRV-3).
///
/// Click-to-focus is the one expensive preview transport (a real preview-encode
/// session), so the number of concurrent focus sessions must be bounded
/// deterministically — a server-wide cap plus an independent per-scope cap
/// (brief §3 "CAP CONCURRENCY", ADR-P002). `GatedWhep` wraps the inner
/// [`WhepProvider`] with a [`FocusGate`]:
///
/// * **Admission on `POST`.** [`Self::negotiate`] acquires a [`FocusLease`]
///   *before* delegating to the inner transport. When a cap is full the focus is
///   **rejected** with [`WhepReject::CapacityExceeded`] carrying the configured
///   `fallback` transport hint (the existing `503 fallback: jpeg` shape) — it
///   is never queued and never able to starve or stall the engine (invariant
///   #10). If the inner transport then refuses the (admitted) offer, the lease is
///   dropped so the slot is freed immediately.
/// * **Release on `DELETE`/expiry.** [`Self::release`] drops the lease keyed by
///   the freed `session_id`, returning the slot to the gate.
///
/// ## Isolation (invariant #10)
///
/// The gate and the session→lease map are **the decorator's own counters only**,
/// behind short-lived `Mutex`es the engine never touches. `GatedWhep` holds no
/// engine handle and no command bus; an unadmitted or released focus changes
/// nothing on the protected output path.
pub struct GatedWhep {
    inner: SharedWhep,
    gate: FocusGate<String>,
    /// The transport the client should fall back to when a cap is full (the
    /// honest `503` hint; e.g. `"jpeg"`).
    fallback: String,
    /// Live focus leases keyed by the inner transport's `session_id`, so a
    /// `DELETE`/expiry frees exactly the right slot. The decorator's own state.
    leases: Mutex<HashMap<String, FocusLease<String>>>,
}

impl std::fmt::Debug for GatedWhep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The inner transport object and the live-lease map are omitted (the
        // former has no useful Debug, the latter could be large); the live
        // `active` count and the fallback hint are the diagnostic fields.
        f.debug_struct("GatedWhep")
            .field("active", &self.gate.active())
            .field("fallback", &self.fallback)
            .finish_non_exhaustive()
    }
}

impl GatedWhep {
    /// Wrap `inner` with the concurrent-focus `caps`, shedding to `fallback` when
    /// a cap is full.
    #[must_use]
    pub fn new(inner: SharedWhep, caps: FocusCaps, fallback: impl Into<String>) -> Self {
        Self {
            inner,
            gate: FocusGate::new(caps),
            fallback: fallback.into(),
            leases: Mutex::new(HashMap::new()),
        }
    }

    /// Wrap `inner` with conservative default caps ([`FocusCaps::default`] — one
    /// focus server-wide and per scope) and the `jpeg` fallback hint.
    #[must_use]
    pub fn with_defaults(inner: SharedWhep) -> Self {
        Self::new(inner, FocusCaps::default(), "jpeg")
    }
}

impl WhepProvider for GatedWhep {
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject> {
        // Admit against the cap FIRST: acquiring the lease reserves the slot
        // before the (expensive) inner negotiation runs. A full cap sheds
        // honestly to the JPEG fallback (the existing `503 fallback` shape) —
        // never queued, never able to stall the engine (invariant #10).
        let lease = self.gate.try_acquire(scope.label()).map_err(|_denied| {
            WhepReject::CapacityExceeded {
                fallback: self.fallback.clone(),
            }
        })?;
        // The slot is reserved; delegate the actual SDP/ICE negotiation. If the
        // inner transport refuses the offer, dropping `lease` here frees the slot
        // immediately so a refused focus never leaks capacity.
        let answer = self.inner.negotiate(scope, offer)?;
        // Success: keep the lease alive, keyed by the transport's session id, so
        // the matching DELETE/expiry releases exactly this slot.
        if let Ok(mut leases) = self.leases.lock() {
            leases.insert(answer.session_id.clone(), lease);
        }
        // If the lease-map lock were poisoned (only reachable after a panic in
        // another thread while it was held), `lease` is dropped here instead and
        // its slot is freed immediately — the session is admitted but stops being
        // cap-tracked. This is preview-only bookkeeping (never the engine); the
        // worst case is one extra concurrent focus, not a stalled output path.
        Ok(answer)
    }

    fn release(&self, scope: &WhepScope, session_id: &str) -> bool {
        let freed = self.inner.release(scope, session_id);
        if freed {
            if let Ok(mut leases) = self.leases.lock() {
                // Dropping the lease returns the slot to the gate.
                leases.remove(session_id);
            }
        }
        freed
    }

    fn active_sessions(&self) -> usize {
        self.gate.active()
    }

    fn webrtc_available(&self) -> bool {
        // The gate is a thin cap decorator; whether WebRTC can be served is the
        // inner transport's truth.
        self.inner.webrtc_available()
    }
}
