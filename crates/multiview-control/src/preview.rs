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

    /// The actual PVW bus canvas as JPEG; the default is unavailable, NEVER
    /// program fallback. Encoding belongs off the output clock.
    fn switcher_preview_jpeg(&self, _quality: u8) -> Option<Vec<u8>> {
        None
    }

    /// The current shared monitor-wall geometry (ADR-0103), or `None` when no
    /// wall is configured or it has not produced a picture yet. Wait-free: the
    /// implementation loads a published snapshot and never touches the engine.
    fn wall(&self) -> Option<WallGeometry> {
        None
    }
}

/// One source's rectangle inside the shared wall picture, in coded pixels.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WallCell {
    /// Source (input) id whose newest frame fills this rectangle.
    pub source: String,
    /// Left edge in coded pixels.
    pub x: u32,
    /// Top edge in coded pixels.
    pub y: u32,
    /// Width in coded pixels.
    pub w: u32,
    /// Height in coded pixels.
    pub h: u32,
    /// Whether this cell currently carries a picture. A cell without one is
    /// black in the wall; its surface shows the source's own still instead.
    /// Excluded from layout identity: it changes without a new generation.
    pub live: bool,
}

/// Exact wall cadence; never a float frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WallCadence {
    /// Frames numerator.
    pub num: i64,
    /// Seconds denominator.
    pub den: i64,
}

/// `GET /api/v1/preview/wall`: the one shared monitor-wall picture's layout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WallGeometry {
    /// Changes whenever the cell assignment changes.
    pub generation: u64,
    /// Coded picture width.
    pub width: u32,
    /// Coded picture height.
    pub height: u32,
    /// Wall frame cadence.
    pub cadence: WallCadence,
    /// Source rectangles, sorted by source id.
    pub cells: Vec<WallCell>,
    /// The live Preview-bus source, when a switcher is attached. Filled by the
    /// route from the switcher state, never by the engine worker.
    pub preview_source: Option<String>,
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
    /// The one shared monitor wall (ADR-0103): every source composed once and
    /// encoded once. Whole-system scope: it embeds every source's pixels.
    Wall,
}

impl WhepScope {
    /// A short, stable label for logs/audit (never carries a secret).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Program => "program".to_owned(),
            Self::Input(id) => format!("input:{id}"),
            Self::Output(id) => format!("output:{id}"),
            Self::Wall => "wall".to_owned(),
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

    /// Liveness sweep at the injected instant `now`: close every session whose
    /// peer has vanished (ICE failed, DTLS closed, silent) and return the ids it
    /// ended. The default is a no-op (a transport with no liveness signal).
    fn reap_dead(&self, _now: std::time::Instant) -> Vec<String> {
        Vec::new()
    }

    /// The configured focus caps and live count, if this provider enforces any.
    fn focus_limits(&self) -> Option<FocusLimits> {
        None
    }

    /// Like [`Self::negotiate`], naming the `owner` (the authenticated key id)
    /// making the offer. A provider that caps sessions may use it to replace the
    /// same owner's stale session on a re-offer. Defaults to ignoring the owner.
    ///
    /// # Errors
    ///
    /// See [`Self::negotiate`].
    fn negotiate_for(
        &self,
        scope: &WhepScope,
        offer: &str,
        _owner: &str,
    ) -> Result<WhepAnswer, WhepReject> {
        self.negotiate(scope, offer)
    }
}

/// Focus caps and occupancy, surfaced by `GET /api/v1/preview/capabilities` so the
/// UI can tell the truth about how many live viewers the server admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FocusLimits {
    /// Maximum concurrent focus (WHEP) sessions server-wide.
    pub max: u32,
    /// Maximum concurrent focus sessions per scope.
    pub max_per_scope: u32,
    /// Focus sessions live right now.
    pub active: u32,
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
    /// True only when an authorized `MoQ` transport provider is ready.
    pub moq: bool,
    /// Per-scope WHEP availability + the program fidelity label.
    pub scopes: ScopeCapabilities,
    /// WHEP focus caps and occupancy (absent when the provider enforces none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<FocusLimits>,
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
            moq: false,
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
            focus: provider.focus_limits(),
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
use multiview_preview::focus::{FocusDenied, FocusGate, FocusLease};

/// One held focus slot plus who/what/when, so a death sweep, a same-owner
/// re-offer and the scope caps can all find it.
struct Held {
    _lease: FocusLease<String>,
    scope: WhepScope,
    owner: String,
    /// Monotonic admission order (oldest = smallest).
    seq: u64,
}

#[derive(Default)]
struct Leases {
    by_session: HashMap<String, Held>,
    next_seq: u64,
}

/// A [`WhepProvider`] decorator that enforces the **concurrent-focus caps** in
/// front of any inner transport (PRV-3) and releases a slot when its session
/// ends **for any reason**.
///
/// * **Admission on `POST`.** [`Self::negotiate`] acquires a [`FocusLease`]
///   *before* delegating to the inner transport. When a cap is full the focus is
///   **rejected** with [`WhepReject::CapacityExceeded`] (the `503 fallback`
///   shape) — never queued, never able to stall the engine (invariant #10). If
///   the inner transport refuses the offer the lease is dropped immediately.
/// * **Release on `DELETE`.** [`Self::release`] drops the lease.
/// * **Release on death.** [`Self::reap_dead`] (driven by a periodic sweep)
///   asks the transport which sessions ended — a viewer that vanishes never
///   sends `DELETE` — and drops exactly those leases.
/// * **Re-offer.** When the caps are full and the *same owner* (key id) already
///   holds a session **on the same scope**, that owner's oldest such session is
///   released to make room: a reload or remount replaces its own stale session
///   instead of being locked out. An owner can never evict another owner's, or a
///   different scope's, session; with cap headroom nothing is evicted.
///
/// ## Isolation (invariants #1 / #10)
///
/// The gate and the lease map are the decorator's own counters, behind
/// short-lived `Mutex`es the engine never touches. No lock is held across the
/// inner transport call, and `GatedWhep` holds no engine handle or command bus.
pub struct GatedWhep {
    inner: SharedWhep,
    gate: FocusGate<String>,
    /// The transport the client should fall back to when a cap is full.
    fallback: String,
    leases: Mutex<Leases>,
}

impl std::fmt::Debug for GatedWhep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
            leases: Mutex::new(Leases::default()),
        }
    }

    /// Wrap `inner` with the default caps ([`FocusCaps::default`]) and the `jpeg`
    /// fallback hint.
    #[must_use]
    pub fn with_defaults(inner: SharedWhep) -> Self {
        Self::new(inner, FocusCaps::default(), "jpeg")
    }

    /// Drop the leases of `ids` (sessions the transport has ended).
    fn drop_leases(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        // Collect under the lock, drop (which re-locks the gate) after releasing it.
        let dropped: Vec<Held> = self.leases.lock().map_or_else(
            |_| Vec::new(),
            |mut l| {
                ids.iter()
                    .filter_map(|id| l.by_session.remove(id))
                    .collect()
            },
        );
        drop(dropped);
    }

    /// Release the owner's oldest session **in the same scope** to make room for
    /// a re-offer. Returns whether one was released.
    fn evict_own(&self, owner: &str, scope: &WhepScope) -> bool {
        let victim = self.leases.lock().ok().and_then(|l| {
            l.by_session
                .iter()
                .filter(|(_, h)| h.owner == owner && &h.scope == scope)
                .min_by_key(|(_, h)| h.seq)
                .map(|(id, _)| id.clone())
        });
        let Some(id) = victim else {
            return false;
        };
        // Tear the transport session down first (no lock held), then free the slot.
        let _ = self.inner.release(scope, &id);
        self.drop_leases(&[id]);
        true
    }

    fn admit(
        &self,
        scope: &WhepScope,
        owner: Option<&str>,
    ) -> Result<FocusLease<String>, WhepReject> {
        let shed = || WhepReject::CapacityExceeded {
            fallback: self.fallback.clone(),
        };
        match self.gate.try_acquire(scope.label()) {
            Ok(lease) => Ok(lease),
            Err(FocusDenied::Suspended) => Err(shed()),
            Err(_full) => {
                let replaced = owner.is_some_and(|o| self.evict_own(o, scope));
                if !replaced {
                    return Err(shed());
                }
                self.gate.try_acquire(scope.label()).map_err(|_| shed())
            }
        }
    }

    fn negotiate_inner(
        &self,
        scope: &WhepScope,
        offer: &str,
        owner: Option<&str>,
    ) -> Result<WhepAnswer, WhepReject> {
        // Admit FIRST: the lease reserves the slot before the expensive inner
        // negotiation. If the inner transport refuses, dropping `lease` frees it.
        let lease = self.admit(scope, owner)?;
        let answer = match owner {
            Some(o) => self.inner.negotiate_for(scope, offer, o)?,
            None => self.inner.negotiate(scope, offer)?,
        };
        if let Ok(mut l) = self.leases.lock() {
            l.next_seq = l.next_seq.wrapping_add(1);
            let seq = l.next_seq;
            l.by_session.insert(
                answer.session_id.clone(),
                Held {
                    _lease: lease,
                    scope: scope.clone(),
                    owner: owner.unwrap_or_default().to_owned(),
                    seq,
                },
            );
        }
        // A poisoned map drops `lease` here instead, freeing the slot at once:
        // the session is admitted but no longer cap-tracked (preview-only).
        Ok(answer)
    }
}

impl WhepProvider for GatedWhep {
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject> {
        self.negotiate_inner(scope, offer, None)
    }

    fn negotiate_for(
        &self,
        scope: &WhepScope,
        offer: &str,
        owner: &str,
    ) -> Result<WhepAnswer, WhepReject> {
        self.negotiate_inner(scope, offer, Some(owner))
    }

    fn release(&self, scope: &WhepScope, session_id: &str) -> bool {
        // A session id is only releasable through the scope it was opened on (a
        // key authorised for one scope must not close another scope's session).
        let mismatch = self
            .leases
            .lock()
            .ok()
            .and_then(|l| l.by_session.get(session_id).map(|h| &h.scope != scope))
            .unwrap_or(false);
        if mismatch {
            return false;
        }
        let freed = self.inner.release(scope, session_id);
        // Drop the lease even when the transport already forgot the session (it
        // may have been reaped): the slot must never outlive the session.
        self.drop_leases(&[session_id.to_owned()]);
        freed
    }

    fn active_sessions(&self) -> usize {
        self.gate.active()
    }

    fn webrtc_available(&self) -> bool {
        self.inner.webrtc_available()
    }

    fn program_fidelity(&self) -> ProgramFidelity {
        self.inner.program_fidelity()
    }

    fn reap_dead(&self, now: std::time::Instant) -> Vec<String> {
        let ended = self.inner.reap_dead(now);
        self.drop_leases(&ended);
        // Also drop leases whose session the transport no longer tracks at all.
        ended
    }

    fn focus_limits(&self) -> Option<FocusLimits> {
        let caps = self.gate.caps();
        let to = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Some(FocusLimits {
            max: to(caps.global),
            max_per_scope: to(caps.per_scope),
            active: to(self.gate.active()),
        })
    }
}
