//! Declarative VT clip fills — the config serde mirror of the fill/job
//! subsystem (ADR-0100 Decisions 1/2/3/7/10/12, ADR-0101 Decisions 1/3/7).
//!
//! A VT slot is a **media player** ([`MediaPlayer`](crate::MediaPlayer)) playing
//! a clip. A [`MediaFill`] is the durable **desired state** that says how that
//! clip is produced: `web_capture` (a headless browser runs a site template and
//! captures a loop) or `url_fetch` (a URL is fetched by an out-of-process tool).
//! The job runner, the clip manifest, the API and the UI are not modelled here —
//! this crate carries only the operator's **authored** fill definition, exactly
//! as it carries the authored media library today.
//!
//! Template-specific parameters (`overlay`, `lat`, `lon`, `zoom`, …) live in
//! [`WebCaptureView::params`], never as named fields: a template is data plus a
//! small script (ADR-0101 Decision 1), so the engine cannot enumerate a site's
//! parameters at compile time. The typed [`ParamValue`] keeps the map to the
//! scalar shapes a template parameter can take (string | number | bool).
//!
//! ## Wire shape
//!
//! ```toml
//! [[media_fill]]
//! id = "syd-radar"
//! player = "vt-1"
//! kind = "web_capture"
//! template = "windy"
//! refresh = { mode = "interval", minutes = 5 }
//! during_refresh = "play_last"          # default
//! storage = "disk"                      # default
//! [media_fill.view]
//! width = 1920
//! height = 1080
//! capture = "steps"                     # default
//! timing = "equal"                      # default
//! frame_ms = 600                        # default
//! end_hold_ms = 1000                    # default
//! crossfade_ms = 0                      # default
//! background = "opaque"                 # default; "transparent" is refused
//! [media_fill.view.params]
//! overlay = "radar"
//! lat = -33.946
//! lon = 150.370
//! zoom = 10
//! [media_fill.terms]
//! user = "operator@example.com"
//! accepted_at = "2026-11-07T00:00:00Z"
//! template = "windy"
//! terms_version = "2023-09-01"
//! ```
//!
//! `MediaFill` keeps `kind` as an internally-tagged [`MediaFillKind`] flattened
//! to the top level. serde cannot combine `deny_unknown_fields` with a
//! `#[serde(flatten)]` field, so deserialization goes through a private
//! known-keys-only wire struct (see [`MediaFillWire`]) which preserves the
//! unknown-field rejection the rest of this crate's schema has.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::device::net_guard::{screen_ip, DialPolicy};
use crate::error::ConfigError;

/// Default `frame_ms` for a captured step (ADR-0100 Decision 1).
pub const DEFAULT_FRAME_MS: u32 = 600;

/// Default `end_hold_ms` (hold the newest frame before the loop wraps).
pub const DEFAULT_END_HOLD_MS: u32 = 1000;

/// Lower bound on `frame_ms`: 25 fps, the slowest cadence a step may imply.
const MIN_FRAME_MS: u32 = 40;

/// Upper bound on `frame_ms` and on the hold/crossfade durations (60 s).
const MAX_DURATION_MS: u32 = 60_000;

/// Smallest square edge a captured view may declare.
const MIN_VIEW_EDGE: u32 = 64;

/// Largest captured width (8K UHD).
const MAX_VIEW_WIDTH: u32 = 7680;

/// Largest captured height (8K UHD).
const MAX_VIEW_HEIGHT: u32 = 4320;

/// The smallest width at which the Windy desktop layout applies (ADR-0101
/// Decision 3): below it the site switches to a mobile layout with different
/// selectors, so the template cannot be captured reliably.
const WINDY_MIN_WIDTH: u32 = 1024;

/// Longest `interval` refresh period, in minutes (24 h).
const MAX_REFRESH_MINUTES: u32 = 1440;

/// One declarative VT clip fill: the durable desired state bound to exactly one
/// [`MediaPlayer`](crate::MediaPlayer) (ADR-0100 Decision 1).
///
/// A fill is **job intent**, not a live source: it carries no `activation`,
/// `idle` or `ready` state. Editing it is a job-intent change; changing the set
/// of media players stays restart-class.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct MediaFill {
    /// Stable fill id (unique within the document; the API authorizes per id).
    pub id: String,
    /// The media player this fill targets ([`MediaPlayer::id`](crate::MediaPlayer::id)).
    /// At most one fill may target a player.
    pub player: String,
    /// What the fill produces, flattened to the top level (`kind = "…"`).
    #[serde(flatten)]
    pub kind: MediaFillKind,
    /// When a new clip is produced (ADR-0100 Decision 3).
    pub refresh: FillRefresh,
    /// What the slot shows while a refresh runs (ADR-0100 Decision 10).
    #[serde(skip_serializing_if = "is_default_during_refresh")]
    pub during_refresh: DuringRefresh,
    /// Where the produced clip is stored (ADR-0100 Decision 12).
    #[serde(skip_serializing_if = "is_default_slot_storage")]
    pub storage: SlotStorage,
    /// The operator's site-terms acceptance (ADR-0101 Decision 7). Required for
    /// a `web_capture` fill; absent for a `url_fetch` fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms: Option<TermsAcceptance>,
}

/// What a fill produces — internally tagged on `kind` and flattened into
/// [`MediaFill`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MediaFillKind {
    /// A headless-browser capture of a site template (ADR-0100 Decision 5).
    WebCapture {
        /// The built-in template id (ADR-0101 Decision 2), e.g. `windy`.
        template: String,
        /// The capture view: geometry, cadence and template parameters.
        view: WebCaptureView,
    },
    /// A URL fetched out of process (ADR-0100 Decision 8).
    UrlFetch {
        /// The source URL. Must be `https`, carry no userinfo, and name a
        /// globally-routable host.
        url: String,
    },
}

/// One template-specific parameter value (ADR-0101 Decision 1): a template is
/// data-driven, so its parameters are a typed scalar map, not named fields.
///
/// Untagged, so a parameter round-trips as the scalar it was authored as
/// (`overlay = "radar"`, `zoom = 10`, `lat = -33.946`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ParamValue {
    /// A boolean parameter.
    Bool(bool),
    /// An integer parameter.
    Integer(i64),
    /// A floating-point parameter.
    Float(f64),
    /// A string parameter.
    String(String),
}

/// The declarative view of a `web_capture` fill (ADR-0100 Decisions 1/5/6/7,
/// ADR-0101 Decision 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WebCaptureView {
    /// Template-specific parameters (`overlay`, `lat`, `lon`, `zoom`, …).
    /// Empty ⇒ omitted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
    /// Captured width in pixels (even, `64..=7680`; Windy requires `>= 1024`).
    pub width: u32,
    /// Captured height in pixels (even, `64..=4320`).
    pub height: u32,
    /// How the animation is captured (default `steps`).
    #[serde(default, skip_serializing_if = "is_default_capture_mode")]
    pub capture: CaptureMode,
    /// How per-frame durations are derived from the source timeline (default
    /// `equal`).
    #[serde(default, skip_serializing_if = "is_default_frame_timing")]
    pub timing: FrameTiming,
    /// Per-frame duration for `timing = "equal"`, in milliseconds (default 600).
    #[serde(
        default = "default_frame_ms",
        skip_serializing_if = "is_default_frame_ms"
    )]
    pub frame_ms: u32,
    /// How long the newest frame is held before the loop wraps, in
    /// milliseconds (default 1000).
    #[serde(
        default = "default_end_hold_ms",
        skip_serializing_if = "is_default_end_hold_ms"
    )]
    pub end_hold_ms: u32,
    /// Loop-point crossfade duration in milliseconds (default 0).
    #[serde(default, skip_serializing_if = "is_default_crossfade_ms")]
    pub crossfade_ms: u32,
    /// Background of the captured view (default `opaque`).
    #[serde(default, skip_serializing_if = "is_default_view_background")]
    pub background: ViewBackground,
}

/// How a `web_capture` animation is captured (ADR-0100 Decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureMode {
    /// Pause, step the timeline and capture one frame per distinct data frame
    /// (the default; used for radar).
    #[default]
    Steps,
    /// Screencast one animation cycle (used for particle layers).
    Continuous,
}

/// How per-frame durations are derived from the source timeline (ADR-0100
/// Decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FrameTiming {
    /// Every frame gets `frame_ms` (the default).
    #[default]
    Equal,
    /// Every frame gets a share of the same total in proportion to its gap to
    /// the next.
    Real,
}

/// The captured view's background (ADR-0100 Decisions 6/7 and step 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ViewBackground {
    /// Fully opaque capture (the default).
    #[default]
    Opaque,
    /// Transparent capture. **Refused today**: FFV1 is not in ADR-0058's
    /// verified alpha list, so an alpha clip cannot round-trip yet.
    Transparent,
}

/// When a fill produces a new clip (ADR-0100 Decision 3), internally tagged on
/// `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FillRefresh {
    /// Runs only on an explicit trigger (button, API, automation) — never at
    /// boot.
    OneShot,
    /// Runs on a schedule, staggered by a stable per-fill offset.
    Interval {
        /// Period in minutes (`1..=1440`).
        minutes: u32,
    },
}

/// What the slot shows while a refresh runs (ADR-0100 Decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DuringRefresh {
    /// The old clip plays until the new one is ready, then swaps at the next
    /// loop boundary (the default).
    #[default]
    PlayLast,
    /// The slot shows NA from job start until the new clip is ready.
    Na,
}

/// Where a produced clip is stored (ADR-0100 Decision 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SlotStorage {
    /// Decoded frames are read from disk at playout (the default).
    #[default]
    Disk,
    /// Decoded frames are held in RAM: no decode and no disk reads at playout.
    Memory,
}

/// An operator's acceptance of a site template's terms (ADR-0101 Decision 7).
///
/// Acceptance records **who** accepted **which** template's terms and **when**;
/// it does not create permission. If the template's terms version changes,
/// acceptance is needed again before the next run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TermsAcceptance {
    /// The user who accepted the terms.
    pub user: String,
    /// When the terms were accepted (RFC 3339).
    pub accepted_at: String,
    /// The template id the acceptance is for.
    pub template: String,
    /// The template's terms version that was accepted.
    pub terms_version: String,
}

impl MediaFill {
    /// Validate this fill's kind-specific and duration invariants (ADR-0100
    /// Decisions 1/3/6/7/8/10/12, ADR-0101 Decisions 3/7), independent of the
    /// document it sits in.
    ///
    /// Document-level rules (unique ids, a declared `player`, at most one fill
    /// per player) live in
    /// [`MultiviewConfig::validate`](crate::MultiviewConfig::validate).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] naming the first violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match &self.kind {
            MediaFillKind::WebCapture { template, view } => {
                self.validate_view(template, view)?;
                self.validate_terms(template)?;
            }
            MediaFillKind::UrlFetch { url } => {
                validate_fetch_url(url).map_err(|reason| {
                    ConfigError::Validation(format!("media fill {:?}: {reason}", self.id))
                })?;
            }
        }

        if let FillRefresh::Interval { minutes } = self.refresh {
            if !(1..=MAX_REFRESH_MINUTES).contains(&minutes) {
                return Err(ConfigError::Validation(format!(
                    "media fill {:?}: refresh interval {minutes} minutes is out of range \
                     1..={MAX_REFRESH_MINUTES}",
                    self.id
                )));
            }
        }

        Ok(())
    }

    /// Validate a `web_capture` view's geometry, timing and background.
    fn validate_view(&self, template: &str, view: &WebCaptureView) -> Result<(), ConfigError> {
        let dims = |edge: u32, max: u32, what: &str| -> Result<(), ConfigError> {
            // MSRV 1.85: `u32::is_multiple_of` (1.87) is not available yet.
            if edge % 2 != 0 {
                return Err(ConfigError::Validation(format!(
                    "media fill {:?}: view {what} {edge} must be even (NV12)",
                    self.id
                )));
            }
            if !(MIN_VIEW_EDGE..=max).contains(&edge) {
                return Err(ConfigError::Validation(format!(
                    "media fill {:?}: view {what} {edge} is out of range {MIN_VIEW_EDGE}..={max}",
                    self.id
                )));
            }
            Ok(())
        };
        dims(view.width, MAX_VIEW_WIDTH, "width")?;
        dims(view.height, MAX_VIEW_HEIGHT, "height")?;

        if template == "windy" && view.width < WINDY_MIN_WIDTH {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: template \"windy\" requires view width >= {WINDY_MIN_WIDTH} so \
                 the desktop layout applies (ADR-0101 decision 3); got {}",
                self.id, view.width
            )));
        }

        if !(MIN_FRAME_MS..=MAX_DURATION_MS).contains(&view.frame_ms) {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: frame_ms {} is out of range {MIN_FRAME_MS}..={MAX_DURATION_MS}",
                self.id, view.frame_ms
            )));
        }
        if view.end_hold_ms > MAX_DURATION_MS {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: end_hold_ms {} exceeds {MAX_DURATION_MS}",
                self.id, view.end_hold_ms
            )));
        }
        if view.crossfade_ms > MAX_DURATION_MS {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: crossfade_ms {} exceeds {MAX_DURATION_MS}",
                self.id, view.crossfade_ms
            )));
        }
        if view.background == ViewBackground::Transparent {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: background = \"transparent\" is not supported yet — alpha clips \
                 stay blocked until an FFV1 alpha round trip passes a pixel test (ADR-0100 step 7)",
                self.id
            )));
        }

        Ok(())
    }

    /// Validate the terms acceptance required for a captured template
    /// (ADR-0101 Decision 7).
    fn validate_terms(&self, template: &str) -> Result<(), ConfigError> {
        let Some(terms) = &self.terms else {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: a web_capture fill requires site-terms acceptance (ADR-0101 \
                 decision 7): add a `terms` block naming the user, time and template",
                self.id
            )));
        };
        if terms.template != template {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: terms.template {:?} does not match the fill template {template:?} \
                 (ADR-0101 decision 7)",
                self.id, terms.template
            )));
        }
        if chrono::DateTime::parse_from_rfc3339(&terms.accepted_at).is_err() {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: terms.accepted_at {:?} is not an RFC 3339 timestamp",
                self.id, terms.accepted_at
            )));
        }
        Ok(())
    }
}

/// Validate a `url_fetch` URL (ADR-0100 Decision 8): a parseable `https` URL
/// with no userinfo and a globally-routable host.
///
/// The IP-literal screen reuses the shared outbound-dial guard
/// ([`screen_ip`]) under the strictest policy: a literal host in a private,
/// loopback, link-local, ULA, carrier-grade-NAT, unspecified or multicast range
/// is refused. Name resolution is **not** performed here — the same guard
/// screens resolved answers at dial time, and ADR-0100 Decision 8 requires an
/// OS-enforced egress boundary regardless, because the fetch tool resolves
/// names itself.
fn validate_fetch_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("url_fetch `url` must not be empty".to_owned());
    }
    if url
        .chars()
        .any(|c| c.is_ascii_whitespace() || c.is_ascii_control())
    {
        return Err(format!(
            "url_fetch url {url:?} contains whitespace or control characters"
        ));
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("url_fetch url {url:?} is not a URL (expected https://…)"))?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(format!(
            "url_fetch url {url:?} must use the https scheme (ADR-0100 decision 8)"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("url_fetch url {url:?} has no host"));
    }
    if authority.contains('@') {
        return Err(format!(
            "url_fetch url {url:?} carries userinfo; credentials must never be embedded in a fill URL"
        ));
    }
    let (host, port) = split_host_port(authority)
        .ok_or_else(|| format!("url_fetch url {url:?} has an invalid authority"))?;
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(0) | Err(_) => {
                return Err(format!(
                    "url_fetch url {url:?} has an invalid port {port:?} (expected 1..=65535)"
                ));
            }
            Ok(_) => {}
        }
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if let Err(e) = screen_ip(ip, &DialPolicy::deny_all_allowlistable()) {
            return Err(format!(
                "url_fetch url host {ip} is blocked by the SSRF guard ({e})"
            ));
        }
    }
    Ok(())
}

/// Split an authority into its host and optional port, handling a bracketed
/// IPv6 literal. Returns `None` for an empty host or a malformed bracket shape.
fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        Some((host, port))
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (!host.is_empty()).then_some((host, Some(port))),
            None => (!authority.is_empty()).then_some((authority, None)),
        }
    }
}

/// The private known-keys-only wire form of [`MediaFill`].
///
/// serde rejects `deny_unknown_fields` on a struct that also has a
/// `#[serde(flatten)]` field, so the flattened, internally-tagged
/// [`MediaFillKind`] is spelled out here as `kind` + its possible payload keys
/// and converted after deserialization. This keeps unknown-field rejection for
/// the flat fill shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct MediaFillWire {
    /// Fill id.
    id: String,
    /// Bound media player id.
    player: String,
    /// The `kind` tag.
    kind: MediaFillKindName,
    /// `web_capture` payload.
    #[serde(default)]
    template: Option<String>,
    /// `web_capture` payload.
    #[serde(default)]
    view: Option<WebCaptureView>,
    /// `url_fetch` payload.
    #[serde(default)]
    url: Option<String>,
    /// Refresh schedule.
    refresh: FillRefresh,
    /// During-refresh behaviour.
    #[serde(default)]
    during_refresh: DuringRefresh,
    /// Slot storage.
    #[serde(default)]
    storage: SlotStorage,
    /// Terms acceptance.
    #[serde(default)]
    terms: Option<TermsAcceptance>,
}

/// The bare `kind` tag of a fill.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MediaFillKindName {
    /// `web_capture`.
    WebCapture,
    /// `url_fetch`.
    UrlFetch,
}

impl MediaFillWire {
    /// Convert the flat wire form into a [`MediaFill`], enforcing that exactly
    /// the payload keys for the declared `kind` are present.
    fn into_fill(self) -> Result<MediaFill, String> {
        let kind = match self.kind {
            MediaFillKindName::WebCapture => {
                if self.url.is_some() {
                    return Err(format!(
                        "media fill {:?}: `url` is only valid for kind = \"url_fetch\"",
                        self.id
                    ));
                }
                let template = self.template.ok_or_else(|| {
                    format!(
                        "media fill {:?}: kind = \"web_capture\" requires `template`",
                        self.id
                    )
                })?;
                let view = self.view.ok_or_else(|| {
                    format!(
                        "media fill {:?}: kind = \"web_capture\" requires `view`",
                        self.id
                    )
                })?;
                MediaFillKind::WebCapture { template, view }
            }
            MediaFillKindName::UrlFetch => {
                if self.template.is_some() || self.view.is_some() {
                    return Err(format!(
                        "media fill {:?}: `template`/`view` are only valid for kind = \"web_capture\"",
                        self.id
                    ));
                }
                let url = self.url.ok_or_else(|| {
                    format!(
                        "media fill {:?}: kind = \"url_fetch\" requires `url`",
                        self.id
                    )
                })?;
                MediaFillKind::UrlFetch { url }
            }
        };
        Ok(MediaFill {
            id: self.id,
            player: self.player,
            kind,
            refresh: self.refresh,
            during_refresh: self.during_refresh,
            storage: self.storage,
            terms: self.terms,
        })
    }
}

impl<'de> Deserialize<'de> for MediaFill {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // See `MediaFillWire`: `deny_unknown_fields` and `flatten` cannot be
        // combined on one derived struct, so the flat known-keys shape is a
        // helper and the tag/payload consistency is checked on conversion.
        MediaFillWire::deserialize(deserializer)?
            .into_fill()
            .map_err(D::Error::custom)
    }
}

/// `skip_serializing_if` predicate: the default [`DuringRefresh`].
// serde's `skip_serializing_if` contract calls the predicate by reference; the
// derive fixes the signature, so the by-value shape the lint asks for is not
// available here.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_during_refresh(value: &DuringRefresh) -> bool {
    matches!(value, DuringRefresh::PlayLast)
}

/// `skip_serializing_if` predicate: the default [`SlotStorage`].
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_slot_storage(value: &SlotStorage) -> bool {
    matches!(value, SlotStorage::Disk)
}

/// `skip_serializing_if` predicate: the default [`CaptureMode`].
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_capture_mode(value: &CaptureMode) -> bool {
    matches!(value, CaptureMode::Steps)
}

/// `skip_serializing_if` predicate: the default [`FrameTiming`].
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_frame_timing(value: &FrameTiming) -> bool {
    matches!(value, FrameTiming::Equal)
}

/// `skip_serializing_if` predicate: the default [`ViewBackground`].
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_view_background(value: &ViewBackground) -> bool {
    matches!(value, ViewBackground::Opaque)
}

/// serde default for `frame_ms` (600 ms; ADR-0100 Decision 1).
const fn default_frame_ms() -> u32 {
    DEFAULT_FRAME_MS
}

/// serde default for `end_hold_ms` (1000 ms; ADR-0100 Decision 1).
const fn default_end_hold_ms() -> u32 {
    DEFAULT_END_HOLD_MS
}

/// `skip_serializing_if` predicate: the default `frame_ms`.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_frame_ms(value: &u32) -> bool {
    *value == DEFAULT_FRAME_MS
}

/// `skip_serializing_if` predicate: the default `end_hold_ms`.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_end_hold_ms(value: &u32) -> bool {
    *value == DEFAULT_END_HOLD_MS
}

/// `skip_serializing_if` predicate: the default `crossfade_ms` (0).
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_crossfade_ms(value: &u32) -> bool {
    *value == 0
}
