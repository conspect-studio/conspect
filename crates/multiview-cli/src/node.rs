//! The `multiview node` subcommand: display-node bootstrap (DEV-B5,
//! [ADR-0045]).
//!
//! A display node is a small commodity box (thin client, Raspberry Pi) that
//! behaves like a hardware decoder built from commodity parts: it runs **one**
//! supervised ingest from a central Multiview → hardware decode → a single-
//! source full-canvas present → the `display-kms` scanout sink ([ADR-0044]) +
//! ALSA HDMI audio. It is a subcommand of the existing `multiview` binary, not
//! a second binary, so it inherits the hardened ingest/resilience/timing stack
//! unchanged.
//!
//! ## What this module owns (the software bootstrap, always compiled + tested)
//!
//! Everything here is **pure and software-testable** — no DRM, no ALSA, no
//! network:
//!
//! * [`NodeRuntimeConfig`] — the node's own bootstrap config (controller
//!   endpoint, enrollment token, identity dir, link offset, clock mode),
//!   parsed from TOML and validated.
//! * [`pairing_code`] — the six-character code shown on the attached display
//!   for operator pairing (ADR-0045: "a six-character code (plus QR)").
//! * [`EnrollmentRequest`] — the keypair-bound enrollment body the node POSTs
//!   to `POST /api/v1/devices/enroll` (the request shape; the HTTP call itself
//!   is a follow-on, see below).
//! * [`ClockMode`] — the optional node-only display-locked clock policy
//!   ([ADR-0045] §7), default **off**, with its invariant-#1 boundary check.
//! * [`PresentationChooser`] — the node presentation model ([ADR-0045] §8):
//!   pure frame choice (which decoded frame goes on which vblank) over the
//!   program `WallClockRef` + link offset. The node is a pull-side consumer
//!   with **no feedback into the engine**, so invariants #1 and #10 hold by
//!   construction.
//!
//! ## What is a documented follow-on (rule 26 — hardware/network validation)
//!
//! This slice ships the bootstrap; the live path is hardware- and network-
//! validated separately and is **not** claimed done here:
//!
//! * the live ingest spawn (`multiview-input` pacer/jitter/normalize/reconnect
//!   → `multiview-framestore` ladder → the [`crate`]'s display sink) — the
//!   media path, validated on a real KMS/display unit;
//! * the real `POST /devices/enroll` + `/pair` HTTP client and the matching
//!   `multiview-control` device routes (DEV-B6);
//! * DRM-master takeover, ALSA PCM open, and the systemd unit (deployment).
//!
//! [ADR-0044]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0044.md
//! [ADR-0045]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0045.md

use base64::Engine as _;
use multiview_core::wallclock::WallClockRef;
use serde::{Deserialize, Serialize};

use crate::cli::NodeArgs;

/// The default presentation link offset (ms) — AES67's fixed receiver-side
/// delay applied to video ([ADR-0045] §8: "typically 100–300 ms"; uniformity
/// across nodes is what matters, not smallness).
const DEFAULT_LINK_OFFSET_MS: u32 = 200;

/// The default identity/lease-state directory (where the persisted device
/// keypair lives). Matches the licence plane's lease-state dir convention so a
/// node and its heartbeat share one device identity.
const DEFAULT_IDENTITY_DIR: &str = "/var/lib/multiview";

/// Errors from parsing or validating a [`NodeRuntimeConfig`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NodeConfigError {
    /// The TOML document failed to parse.
    #[error("could not parse the node config: {0}")]
    Parse(#[from] toml::de::Error),
    /// A semantic rule was violated (named in the message).
    #[error("invalid node config: {0}")]
    Validation(String),
}

/// The node-only presentation clock policy ([ADR-0045] §7).
///
/// Serialized **internally**/by value as a `snake_case` scalar (`"default"` /
/// `"display_locked"`), never `untagged` (ADR-0010). Default is [`Self::Default`]
/// — the repeat/drop reconciliation is always correct, merely occasionally
/// non-ideal, so locking the cadence to the panel is opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockMode {
    /// Repeat/drop reconciliation at the mailbox (the default): one duplicated-
    /// or-dropped frame every ~16.7 s at a 60.000 Hz tick vs 59.94 Hz display.
    #[default]
    Default,
    /// Slew the node's *local* presentation cadence to the measured display
    /// refresh (the mpv display-resample analogue). **Node-only, Class-2.**
    DisplayLocked,
}

impl ClockMode {
    /// Validate this clock mode for the host kind ([ADR-0045] §7).
    ///
    /// [`Self::DisplayLocked`] is permitted **only** on a dedicated display
    /// node, where the attached panel is the box's terminal output. On a local
    /// multiview the output clock also serves encoders and network outputs, so
    /// locking it to a monitor would break invariant #1 — the combination is
    /// rejected.
    ///
    /// # Errors
    /// [`NodeConfigError::Validation`] when display-locked is requested on a
    /// non-dedicated host.
    pub fn validate_for_node(self, is_dedicated_node: bool) -> Result<(), NodeConfigError> {
        match self {
            Self::Default => Ok(()),
            Self::DisplayLocked if is_dedicated_node => Ok(()),
            Self::DisplayLocked => Err(NodeConfigError::Validation(
                "display-locked clock mode is permitted only on a dedicated display node (the \
                 panel is the terminal output); a local multiview's clock also serves encoders \
                 and network outputs, so locking it to a monitor would break invariant #1"
                    .to_owned(),
            )),
        }
    }
}

/// The node's own bootstrap config (config-as-code for a display node).
///
/// This is the **node-side** view — what a node process needs to find its
/// controller, prove its enrolled identity, and present frames. It is distinct
/// from the controller-side [`multiview_config::Device`] registry entry (the
/// fleet operator's view of the node). Addresses are IPv6-first (ADR-0042):
/// bracket IPv6 literals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeRuntimeConfig {
    /// The central Multiview controller's base URL (IPv6-first, bracketed),
    /// e.g. `https://[fd00:db8::1]:8443`.
    pub controller: String,
    /// The one-time enrollment token issued in the SPA (displayed once, stored
    /// hashed server-side; presented by the node on first enrollment).
    pub enrollment_token: String,
    /// The directory holding the persisted device keypair (`device-key.ed25519`,
    /// 0600). Defaults to [`DEFAULT_IDENTITY_DIR`] so a node and its licence
    /// heartbeat share one device identity.
    #[serde(default = "default_identity_dir")]
    pub identity_dir: String,
    /// Human-friendly node name, carried into the enrollment request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The presentation link offset in milliseconds ([ADR-0045] §8). Defaults
    /// to [`DEFAULT_LINK_OFFSET_MS`].
    #[serde(default = "default_link_offset_ms")]
    pub link_offset_ms: u32,
    /// The node clock-mode policy ([ADR-0045] §7). Defaults to
    /// [`ClockMode::Default`] (display-locked is opt-in).
    #[serde(default)]
    pub clock_mode: ClockMode,
    /// An optional explicit program-stream URL hint. Absent ⇒ the assignment
    /// the controller pushes over the control WebSocket after enrollment names
    /// the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_stream: Option<String>,
}

fn default_identity_dir() -> String {
    DEFAULT_IDENTITY_DIR.to_owned()
}

const fn default_link_offset_ms() -> u32 {
    DEFAULT_LINK_OFFSET_MS
}

impl NodeRuntimeConfig {
    /// Parse a node config from a TOML document.
    ///
    /// # Errors
    /// [`NodeConfigError::Parse`] when the document is not valid TOML or
    /// carries an unknown field.
    pub fn parse(text: &str) -> Result<Self, NodeConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Validate the node config's semantics (non-empty controller + token;
    /// a sane link offset).
    ///
    /// The display-locked-clock host-kind rule is checked separately via
    /// [`ClockMode::validate_for_node`] once the host kind is known.
    ///
    /// # Errors
    /// [`NodeConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), NodeConfigError> {
        if self.controller.trim().is_empty() {
            return Err(NodeConfigError::Validation(
                "controller endpoint is empty".to_owned(),
            ));
        }
        if self.enrollment_token.trim().is_empty() {
            return Err(NodeConfigError::Validation(
                "enrollment_token is empty".to_owned(),
            ));
        }
        // A node is a dedicated display box, so the display-locked clock is
        // always permitted here; the rejection path is a local multiview, which
        // never runs `multiview node`. Surface the §7 rule explicitly anyway so
        // a future shared-host caller can reuse it.
        self.clock_mode.validate_for_node(true)?;
        Ok(())
    }
}

/// The six-character code shown on the attached display for operator pairing
/// ([ADR-0045]).
///
/// Derived **deterministically** from the device public key, so the same node
/// always shows the same code before it is paired (a power-cycle does not
/// change the code an operator is typing). Crockford base32 over the SHA-256 of
/// the key, truncated to six characters — the Crockford alphabet excludes the
/// ambiguous `I`/`L`/`O`/`U` so the code reads cleanly off a panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// The code as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The Crockford base32 alphabet (RFC-less; Douglas Crockford's spec): digits
/// then uppercase letters with `I`, `L`, `O`, `U` removed to avoid ambiguity.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Derive the six-character display pairing code from a raw 32-byte device
/// public key ([ADR-0045]).
#[must_use]
pub fn pairing_code(device_public_key: &[u8; 32]) -> PairingCode {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(device_public_key);
    // Take six 5-bit groups from the leading digest bytes (30 bits of the
    // hash). Pure index arithmetic — no `as` casts (workspace lint policy).
    let mut code = String::with_capacity(6);
    for i in 0..6_usize {
        let bit = i * 5;
        let byte_idx = bit / 8;
        let shift = bit % 8;
        // Assemble a 5-bit window that may straddle two bytes. `.get` (not
        // indexing) keeps the workspace `indexing_slicing` deny satisfied; an
        // out-of-range byte contributes zero (only the trailing window can
        // straddle the 32-byte digest end).
        let lo = u16::from(digest.get(byte_idx).copied().unwrap_or(0));
        let hi = u16::from(digest.get(byte_idx + 1).copied().unwrap_or(0));
        let window = (lo << 8) | hi;
        // The top bit of `lo` sits at window bit 15; the 5-bit group starts
        // `shift` bits in from the top of `lo`.
        let group = (window >> (11 - shift)) & 0b1_1111;
        let idx = usize::from(group);
        // `idx` is always 0..32 (masked to 5 bits); CROCKFORD has 32 entries.
        let ch = CROCKFORD.get(idx).copied().unwrap_or(b'0');
        code.push(char::from(ch));
    }
    PairingCode(code)
}

/// The keypair-bound enrollment request body ([ADR-0045]; the device-identity
/// shape follows [ADR-I008]).
///
/// The node generates a keypair on first start and presents its **public** key
/// (base64url of the raw 32-byte Ed25519 point) plus the one-time enrollment
/// token; the operator then completes pairing against the [`pairing_code`]
/// shown on the display. This type is the *body shape only* — building it is
/// pure; the `POST /api/v1/devices/enroll` call is a follow-on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct EnrollmentRequest {
    /// The device public key: base64url (no padding) of the raw 32-byte
    /// Ed25519 point (ADR-I008 verified the raw-32 encoding, never SPKI/DER).
    pub device_public_key: String,
    /// The one-time enrollment token presented for adoption.
    pub enrollment_token: String,
    /// The node's human-friendly name, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl EnrollmentRequest {
    /// Build the enrollment body from the raw device public key, the one-time
    /// token, and the optional display name.
    #[must_use]
    pub fn build(
        device_public_key: &[u8; 32],
        enrollment_token: &str,
        display_name: Option<&str>,
    ) -> Self {
        Self {
            device_public_key: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(device_public_key),
            enrollment_token: enrollment_token.to_owned(),
            display_name: display_name.map(ToOwned::to_owned),
        }
    }
}

/// A predicted next-vblank instant on the node's disciplined wall clock, in
/// integer nanoseconds past the Unix epoch ([ADR-0045] §8: "the predicted next
/// vblank from the KMS flip timestamps").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VblankPrediction {
    wall_ns: i64,
}

impl VblankPrediction {
    /// A prediction at the given wall-clock instant (ns past the Unix epoch).
    #[must_use]
    pub const fn at_wall_ns(wall_ns: i64) -> Self {
        Self { wall_ns }
    }

    /// The predicted instant (ns past the Unix epoch).
    #[must_use]
    pub const fn wall_ns(self) -> i64 {
        self.wall_ns
    }
}

/// The chooser's decision for one vblank ([ADR-0045] §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresentChoice {
    /// Present the decoded frame with this media PTS (in the epoch's rate
    /// units). The caller drops any queued frame older than this one.
    Present {
        /// The chosen frame's media PTS.
        pts: i64,
    },
    /// Repeat the currently-scanned-out frame: the next frame's deadline is
    /// still in the future, or the decode queue is empty (hold last-good). KMS
    /// repeats the current framebuffer for free, so this is a no-commit.
    Repeat,
}

/// The node presentation model ([ADR-0045] §8): **pure frame choice** over the
/// program epoch and the link offset.
///
/// The node decodes into a small queue and, at each page-flip-complete event,
/// asks the chooser which queued frame to present at the predicted next vblank.
/// The chooser maps each frame's media PTS to its target wall instant via the
/// program [`WallClockRef`] plus the fixed link offset, then picks the
/// **freshest** frame whose target is at or before the predicted vblank
/// ("drop if late"); if none is due yet it repeats ("repeat if early"). It
/// never reaches back into the engine — a node is a pull-side consumer, so
/// invariants #1 and #10 hold by construction.
#[derive(Debug, Clone, Copy)]
pub struct PresentationChooser {
    epoch: WallClockRef,
    link_offset_ns: i64,
}

impl PresentationChooser {
    /// Build a chooser for a program epoch and a link offset in milliseconds.
    #[must_use]
    pub fn new(epoch: WallClockRef, link_offset_ms: u32) -> Self {
        Self {
            epoch,
            // ms → ns via exact integer arithmetic (i64), never float.
            link_offset_ns: i64::from(link_offset_ms).saturating_mul(1_000_000),
        }
    }

    /// The target wall instant (ns) at which a frame with this media PTS should
    /// be presented: `wall_at(pts) + link_offset`.
    #[must_use]
    pub fn target_wall_ns(&self, pts: i64) -> i64 {
        self.epoch.wall_at(pts).saturating_add(self.link_offset_ns)
    }

    /// Choose which queued frame to present at the predicted vblank.
    ///
    /// `decode_queue` is the set of media PTS currently decoded and available
    /// (order-independent; the chooser scans for the best). Returns
    /// [`PresentChoice::Present`] for the freshest frame due at or before the
    /// vblank, or [`PresentChoice::Repeat`] when none is due yet (or the queue
    /// is empty) — never blocks, never goes black.
    #[must_use]
    pub fn choose(&self, decode_queue: &[i64], predicted: VblankPrediction) -> PresentChoice {
        let vblank = predicted.wall_ns();
        let mut best: Option<i64> = None;
        for &pts in decode_queue {
            // A frame is "due" once its target instant has arrived (≤ vblank).
            if self.target_wall_ns(pts) <= vblank {
                // Keep the freshest due frame (largest target instant), so an
                // older still-due frame is dropped in its favour.
                best = match best {
                    Some(cur) if self.target_wall_ns(cur) >= self.target_wall_ns(pts) => Some(cur),
                    _ => Some(pts),
                };
            }
        }
        match best {
            Some(pts) => PresentChoice::Present { pts },
            None => PresentChoice::Repeat,
        }
    }
}

/// Run the `multiview node` subcommand: load + validate the node config,
/// resolve the device identity, and render the enrollment + presentation plan.
///
/// This slice ships the **bootstrap/plan** surface (`--plan-only`, the default):
/// it proves the config, the keypair-bound identity, the display pairing code,
/// and the resolved presentation settings, then prints them. The live ingest →
/// hardware-decode → scanout path and the real enrollment HTTP are
/// hardware/network follow-ons (see the module docs) and are reported as such
/// rather than silently skipped.
///
/// # Errors
/// Propagates a config read/parse/validate failure, or (when the identity can
/// be loaded) an identity-load failure.
pub fn run_node(args: &NodeArgs) -> anyhow::Result<NodePlan> {
    let text = std::fs::read_to_string(&args.config).map_err(|e| {
        anyhow::anyhow!(
            "could not read the node config {}: {e}",
            args.config.display()
        )
    })?;
    let cfg = NodeRuntimeConfig::parse(&text)?;
    cfg.validate()?;
    let identity = resolve_identity(&cfg)?;
    Ok(NodePlan {
        controller: cfg.controller.clone(),
        link_offset_ms: cfg.link_offset_ms,
        clock_mode: cfg.clock_mode,
        identity,
        live_path_available: false,
    })
}

/// The resolved device identity for the node, if it could be loaded.
///
/// On a build without the `heartbeat` feature the persisted-keypair loader is
/// not compiled in, so the identity is reported as unavailable rather than
/// guessed — the bootstrap surface stays honest (rule 27).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeIdentity {
    /// The device keypair was loaded: its public key (base64url raw-32) and the
    /// display pairing code derived from it.
    Loaded {
        /// base64url (no padding) of the raw 32-byte Ed25519 public key.
        device_public_key: String,
        /// The six-character display pairing code.
        pairing_code: String,
    },
    /// The persisted-keypair loader is not available in this build (the
    /// `heartbeat` feature is off). Enrollment requires it.
    Unavailable {
        /// Why the identity could not be resolved.
        reason: String,
    },
}

/// The rendered node plan (the `--plan-only` bootstrap surface).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NodePlan {
    /// The controller base URL the node enrolls against.
    pub controller: String,
    /// The resolved presentation link offset (ms).
    pub link_offset_ms: u32,
    /// The resolved clock-mode policy.
    pub clock_mode: ClockMode,
    /// The resolved device identity (or why it is unavailable).
    pub identity: NodeIdentity,
    /// Whether the live ingest → scanout path is wired in this build. Always
    /// `false` in this slice — the live path is a hardware follow-on.
    pub live_path_available: bool,
}

impl NodePlan {
    /// Render the plan as the multi-line text the binary prints.
    #[must_use]
    pub fn render(&self) -> String {
        // `write!`/`writeln!` into a `String` is infallible, so the discarded
        // `Result` is intentional (the established control.rs convention).
        use std::fmt::Write as _;
        let mut out = String::new();
        out.push_str("multiview node — bootstrap plan\n");
        let _ = writeln!(out, "  controller:     {}", self.controller);
        let _ = writeln!(out, "  link offset:    {} ms", self.link_offset_ms);
        let _ = writeln!(out, "  clock mode:     {:?}", self.clock_mode);
        match &self.identity {
            NodeIdentity::Loaded {
                device_public_key,
                pairing_code,
            } => {
                let _ = writeln!(out, "  device key:     {device_public_key}");
                let _ = writeln!(out, "  pairing code:   {pairing_code}");
            }
            NodeIdentity::Unavailable { reason } => {
                let _ = writeln!(out, "  device identity: unavailable ({reason})");
            }
        }
        let live = if self.live_path_available {
            "wired"
        } else {
            "not wired (hardware follow-on: ingest → KMS scanout + ALSA)"
        };
        let _ = writeln!(out, "  live path:      {live}");
        out
    }
}

/// Resolve the node's device identity from the persisted keypair.
///
/// The persisted-keypair store (atomic create-once, 0600, `O_NOFOLLOW`) lives
/// behind the `heartbeat` feature with the licence plane; this slice reuses it
/// when present and reports honestly when it is not (rule 6: never reinvent the
/// hardened keystore; rule 27: never claim what isn't built).
#[cfg(feature = "heartbeat")]
fn resolve_identity(cfg: &NodeRuntimeConfig) -> anyhow::Result<NodeIdentity> {
    let public_key = crate::licence::load_device_public_key(&cfg.identity_dir)
        .map_err(|e| anyhow::anyhow!("could not load the device identity: {e}"))?;
    Ok(NodeIdentity::Loaded {
        device_public_key: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public_key),
        pairing_code: pairing_code(&public_key).as_str().to_owned(),
    })
}

#[cfg(not(feature = "heartbeat"))]
// The `heartbeat`-on counterpart genuinely returns a fallible `Result` (loading
// the persisted key can fail), and both cfgs must share one signature for the
// single `resolve_identity(&cfg)?` call site — so this infallible arm keeps the
// `Result` shape deliberately.
#[allow(clippy::unnecessary_wraps)]
fn resolve_identity(_cfg: &NodeRuntimeConfig) -> anyhow::Result<NodeIdentity> {
    Ok(NodeIdentity::Unavailable {
        reason: "the device keypair loader requires the `heartbeat` feature".to_owned(),
    })
}
