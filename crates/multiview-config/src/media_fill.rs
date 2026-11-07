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
//! Those parameters are **not secret by design**: they are authored view
//! inputs, written into the config file and returned by the API, so they must
//! never hold a credential. Site credentials are references only, through the
//! shared `secret_ref` shape (`username = { secret_ref = "…" }`, ADR-0099
//! Decision 5).
//!
//! Template capture constraints the config schema cannot know (e.g. a
//! per-template minimum width) arrive through the [`TemplateConstraints`] hook,
//! which `multiview-webengine` supplies once templates land (ADR-0101
//! Decision 1); this crate itself holds no site knowledge.
//!
//! `url_fetch` URL validation here is **input hygiene only**. ADR-0100
//! Decision 8 requires an OS-enforced egress boundary at fetch time (a network
//! namespace or a cgroup network filter) because the fetch tool resolves names,
//! follows redirects and contacts hosts itself — a config check can never
//! substitute for that boundary.
//!
//! A `url_fetch` URL is **stored canonically**: the deserialize path keeps the
//! WHATWG parser's own [`Url::as_str`] rendering, so the fill carries the host
//! the hygiene screen actually judged (lower-cased, punycoded, and the
//! alternative IPv4 notations collapsed to a dotted quad) rather than the
//! operator's spelling, and `serialize → deserialize` stays stable.
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
use std::fmt;
use std::net::IpAddr;

use chrono::{DateTime, TimeDelta, Utc};
use serde::de::{Error as _, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use url::{Host, Url};

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

/// Longest `interval` refresh period, in minutes (24 h).
const MAX_REFRESH_MINUTES: u32 = 1440;

/// Longest accepted fill id or template id, in characters (ADR-0100 Decision 1).
const MAX_ID_CHARS: usize = 64;

/// Longest accepted `url_fetch` URL, in bytes (ADR-0100 Decision 8).
const MAX_FETCH_URL_BYTES: usize = 2048;

/// Longest accepted `terms.user` (ADR-0101 Decision 7).
const MAX_TERMS_USER_CHARS: usize = 256;

/// Longest accepted `terms.terms_version` (ADR-0101 Decision 7).
const MAX_TERMS_VERSION_CHARS: usize = 64;

/// How far `terms.accepted_at` may lead the injected clock. A small allowance
/// absorbs clock skew between the API that stamps the time and the host that
/// validates the document; a larger lead is a forged or mis-stamped timestamp.
const MAX_TERMS_CLOCK_SKEW_MINUTES: i64 = 5;

/// Most template parameters one capture view may carry (ADR-0101 Decision 1).
const MAX_PARAMS: usize = 32;

/// Longest template-parameter key, in characters.
const MAX_PARAM_KEY_CHARS: usize = 64;

/// Longest string template-parameter value, in characters.
const MAX_PARAM_STRING_CHARS: usize = 1024;

/// Special-use / local-only host names that a `url_fetch` must never name
/// (RFC 6761 `localhost`, RFC 6762 `.local`, RFC 8375 `home.arpa`, and the
/// `.internal` convention). Matched case-insensitively as an exact name or as a
/// dot-suffixed subdomain, with an optional trailing dot stripped first.
const BLOCKED_FETCH_HOSTS: [&str; 4] = ["localhost", "local", "internal", "home.arpa"];

/// Template-supplied capture constraints the config layer cannot know
/// (ADR-0101 Decision 1).
///
/// A template is **data plus a small script** living in
/// `multiview-webengine/templates/<id>/`, so any per-template capture rule
/// (e.g. "below this width the site switches to a mobile layout and cannot be
/// captured reliably") belongs to the template manifest, not to the generic
/// config schema. `multiview-config` therefore holds **no template knowledge**;
/// it only consults this hook when one is supplied.
///
/// `multiview-webengine` supplies the concrete implementation once templates
/// land (ADR-0101 Decision 1); [`MultiviewConfig::validate_with`] and
/// [`MediaFill::validate_with`] accept it as an optional parameter, and
/// [`validate`](MultiviewConfig::validate) passes `None` (no constraints).
pub trait TemplateConstraints {
    /// The smallest capture width the named template declares, or `None` when
    /// the template has no width floor.
    ///
    /// `template` is the fill's already shape-checked template id.
    #[must_use]
    fn min_width(&self, template: &str) -> Option<u32>;
}

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
        /// The source URL. Must be at most 2048 bytes, exactly `https`, carry
        /// no userinfo, and name a host that survives the input-hygiene screen
        /// (ADR-0100 Decision 8): a parsed `Domain` name that is not a
        /// local/special-use name, or an IP literal that passes
        /// [`screen_ip`]. This is input hygiene only — the OS-enforced egress
        /// boundary at fetch time is still required.
        ///
        /// Stored in the parser's **canonical** form ([`Url::as_str`]),
        /// normalised on the deserialize path (see [`MediaFillWire`]), so a
        /// validated fill never holds the operator's raw spelling: an
        /// alternative IPv4 notation is stored as the `127.0.0.1` the screen
        /// rejected, not as the text that named the loopback.
        url: String,
    },
}

/// One template-specific parameter value (ADR-0101 Decision 1): a template is
/// data-driven, so its parameters are a typed scalar map, not named fields.
///
/// Untagged, so a parameter round-trips as the scalar it was authored as
/// (`overlay = "radar"`, `zoom = 10`, `lat = -33.946`).
///
/// Deserialization is hand-written rather than derived so the two invalid
/// scalar shapes are rejected at parse time with a named reason instead of
/// silently becoming a different variant:
/// * a non-finite float (`nan`/`inf` in TOML) is refused — it cannot be
///   compared, serialized to JSON, or rendered;
/// * an unsigned integer above [`i64::MAX`] is refused rather than widening to
///   an `f64` (which would lose precision); the untagged derive would otherwise
///   fall through `Integer(i64)` to `Float(f64)`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ParamValue {
    /// A boolean parameter.
    Bool(bool),
    /// An integer parameter.
    Integer(i64),
    /// A floating-point parameter. Always finite (see the type docs).
    Float(f64),
    /// A string parameter.
    String(String),
}

impl<'de> Deserialize<'de> for ParamValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        /// Visit the scalar shapes a template parameter may take.
        struct ParamVisitor;

        impl Visitor<'_> for ParamVisitor {
            type Value = ParamValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a boolean, integer, finite float or string template parameter")
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(ParamValue::Bool(value))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(ParamValue::Integer(value))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                i64::try_from(value).map(ParamValue::Integer).map_err(|_| {
                    E::custom(format!(
                        "template parameter integer {value} exceeds i64::MAX; parameters are \
                         i64-bounded and are never widened to a float"
                    ))
                })
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                if value.is_finite() {
                    Ok(ParamValue::Float(value))
                } else {
                    Err(E::custom(format!(
                        "template parameter float {value} is not finite; nan/inf is not a \
                         usable parameter value"
                    )))
                }
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ParamValue::String(value.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(ParamValue::String(value))
            }
        }

        deserializer.deserialize_any(ParamVisitor)
    }
}

/// The declarative view of a `web_capture` fill (ADR-0100 Decisions 1/5/6/7,
/// ADR-0101 Decision 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WebCaptureView {
    /// Template-specific parameters (`overlay`, `lat`, `lon`, `zoom`, …).
    /// Empty ⇒ omitted.
    ///
    /// Bounded to at most 32 entries; each key is `1..=64` characters of
    /// `[a-z0-9_]`, each string value is at most 1024 characters with no
    /// control characters, and every float is finite.
    ///
    /// **Not secret by design**: these are authored view inputs that appear in
    /// the config file and the API, so they must never hold a credential. Site
    /// credentials are references only, via `secret_ref` (ADR-0099 Decision 5).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
    /// Captured width in pixels (even, `64..=7680`). A template may impose a
    /// larger floor through the [`TemplateConstraints`] hook; the config schema
    /// itself carries no per-template rule (ADR-0101 Decision 1).
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
///
/// ## Who stamps what (ADR-0101 Decision 7)
///
/// This block records a decision the operator made; the parts a config file
/// cannot prove are enforced elsewhere:
/// * the **API** must stamp `user` from the authenticated principal and
///   `accepted_at` from the server clock when it writes this block — a
///   client-supplied value is never trusted;
/// * the **job runner** must reject a run when `terms_version` differs from the
///   template's **current** terms version.
///
/// `multiview-config` sees only the authored document: it has no principal, no
/// template manifest and (in the default, clock-free build) no clock. Its
/// checks are therefore hygiene plus a bounded clock-skew look-ahead against an
/// **injected** `now` ([`MediaFill::validate_with`]); they cannot and do not
/// decide whether the acceptance is current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TermsAcceptance {
    /// The user who accepted the terms. Stamped by the API from the
    /// authenticated principal — never the client. Must be non-blank, trimmed,
    /// at most 256 characters and free of control characters.
    pub user: String,
    /// When the terms were accepted (RFC 3339, stamped server-side). Rejected
    /// when it leads the injected `now` by more than 5 minutes; the normal
    /// [`validate`](MediaFill::validate) has no clock and skips that check.
    pub accepted_at: String,
    /// The template id the acceptance is for.
    pub template: String,
    /// The template's terms version that was accepted. Must be non-blank,
    /// trimmed, at most 64 characters and free of control characters. The job
    /// runner compares it against the template's current version before a run.
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
    /// This is the clock-free, constraint-free form: it skips the
    /// `terms.accepted_at` future check (the default build carries no clock) and
    /// consults no [`TemplateConstraints`]. Use
    /// [`validate_with`](Self::validate_with) for a deterministic check, or
    /// [`validate_with_now`](Self::validate_with_now) for the common
    /// injected-clock case.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] naming the first violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_with(None, None)
    }

    /// [`validate`](Self::validate) with an optional [`TemplateConstraints`]
    /// hook and an optional injected `now` (ADR-0100 Decisions
    /// 1/3/6/7/8/10/12, ADR-0101 Decisions 1/3/7).
    ///
    /// `constraints` supplies the per-template capture rules the config schema
    /// deliberately does not know (ADR-0101 Decision 1); `None` applies none.
    /// `now` is the clock to check `terms.accepted_at` against; `None` skips
    /// that check, because the default build has no clock and therefore cannot
    /// prove anything about "now" (the API stamps and the runner enforces the
    /// version match — see [`TermsAcceptance`]).
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] naming the first violated rule.
    pub fn validate_with(
        &self,
        constraints: Option<&dyn TemplateConstraints>,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), ConfigError> {
        if !id_is_well_formed(&self.id) {
            return Err(ConfigError::Validation(format!(
                "media fill id {:?} must be 1..={MAX_ID_CHARS} characters of [A-Za-z0-9_-] \
                 (ADR-0100 decision 1)",
                self.id
            )));
        }

        match &self.kind {
            MediaFillKind::WebCapture { template, view } => {
                self.validate_view(template, view, constraints)?;
                self.validate_terms(template, now)?;
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

    /// [`validate`](Self::validate) with a deterministic injected `now`
    /// (ADR-0101 Decision 7): `terms.accepted_at` may not lead `now` by more
    /// than five minutes. No template constraints are applied.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Validation`] naming the first violated rule.
    pub fn validate_with_now(&self, now: DateTime<Utc>) -> Result<(), ConfigError> {
        self.validate_with(None, Some(now))
    }

    /// Validate a `web_capture` view's template id, geometry, parameters,
    /// timing and background.
    fn validate_view(
        &self,
        template: &str,
        view: &WebCaptureView,
        constraints: Option<&dyn TemplateConstraints>,
    ) -> Result<(), ConfigError> {
        if !id_is_well_formed(template) {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: template id {template:?} must be 1..={MAX_ID_CHARS} characters \
                 of [A-Za-z0-9_-] (ADR-0100 decision 1)",
                self.id
            )));
        }

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
        self.validate_params(&view.params)?;

        // Template-supplied capture constraints (ADR-0101 Decision 1): the hook
        // is the ONLY source of a per-template width floor, so the generic
        // config carries no site knowledge and `multiview-webengine` supplies
        // the real rules once templates land.
        if let Some(min_width) = constraints.and_then(|c| c.min_width(template)) {
            if view.width < min_width {
                return Err(ConfigError::Validation(format!(
                    "media fill {:?}: template {template:?} requires view width >= {min_width} \
                     (template constraint, ADR-0101 decision 1); got {}",
                    self.id, view.width
                )));
            }
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

    /// Validate the template-parameter map (ADR-0101 Decision 1): a bounded
    /// entry count, `[a-z0-9_]` keys, bounded control-free string values and
    /// finite floats.
    ///
    /// Parameters are **not secret by design** — a credential placed here is a
    /// configuration error, not a supported feature (credentials are
    /// `secret_ref` references, ADR-0099 Decision 5).
    fn validate_params(&self, params: &BTreeMap<String, ParamValue>) -> Result<(), ConfigError> {
        if params.len() > MAX_PARAMS {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: {} template parameters exceeds the bound of {MAX_PARAMS}",
                self.id,
                params.len()
            )));
        }
        for (key, value) in params {
            let key_len = key.chars().count();
            let key_shaped = key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !(1..=MAX_PARAM_KEY_CHARS).contains(&key_len) || !key_shaped {
                return Err(ConfigError::Validation(format!(
                    "media fill {:?}: template parameter key {key:?} must be 1..=\
                     {MAX_PARAM_KEY_CHARS} characters of [a-z0-9_]",
                    self.id
                )));
            }
            match value {
                ParamValue::String(text) => {
                    if text.chars().count() > MAX_PARAM_STRING_CHARS {
                        return Err(ConfigError::Validation(format!(
                            "media fill {:?}: template parameter {key:?} is {} characters; the \
                             maximum is {MAX_PARAM_STRING_CHARS}",
                            self.id,
                            text.chars().count()
                        )));
                    }
                    if let Some(control) = text.chars().find(|c| c.is_control()) {
                        return Err(ConfigError::Validation(format!(
                            "media fill {:?}: template parameter {key:?} contains control \
                             character {control:?}",
                            self.id
                        )));
                    }
                }
                ParamValue::Float(value) if !value.is_finite() => {
                    return Err(ConfigError::Validation(format!(
                        "media fill {:?}: template parameter {key:?} is not a finite float",
                        self.id
                    )));
                }
                ParamValue::Bool(_) | ParamValue::Integer(_) | ParamValue::Float(_) => {}
            }
        }
        Ok(())
    }

    /// Validate the terms acceptance required for a captured template
    /// (ADR-0101 Decision 7). `now` is the injected clock, if any: `None` skips
    /// the future check (the default build cannot read a clock).
    fn validate_terms(
        &self,
        template: &str,
        now: Option<DateTime<Utc>>,
    ) -> Result<(), ConfigError> {
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
        self.validate_terms_text(&terms.user, "terms.user", MAX_TERMS_USER_CHARS)?;
        self.validate_terms_text(
            &terms.terms_version,
            "terms.terms_version",
            MAX_TERMS_VERSION_CHARS,
        )?;
        let accepted_at = DateTime::parse_from_rfc3339(&terms.accepted_at).map_err(|_| {
            ConfigError::Validation(format!(
                "media fill {:?}: terms.accepted_at {:?} is not an RFC 3339 timestamp",
                self.id, terms.accepted_at
            ))
        })?;
        // Config has no clock; only an injected `now` can prove the timestamp
        // is not forged into the future. The API stamps this server-side, so a
        // lead larger than a small skew allowance is a hard error.
        let leads_now = now.is_some_and(|now| {
            accepted_at.signed_duration_since(now)
                > TimeDelta::minutes(MAX_TERMS_CLOCK_SKEW_MINUTES)
        });
        if leads_now {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: terms.accepted_at {:?} is more than \
                 {MAX_TERMS_CLOCK_SKEW_MINUTES} minutes in the future; the API must stamp it from \
                 the server clock (ADR-0101 decision 7)",
                self.id, terms.accepted_at
            )));
        }
        Ok(())
    }

    /// Validate one required text field: non-blank after trimming, stored
    /// trimmed, bounded in characters and free of control characters.
    fn validate_terms_text(
        &self,
        value: &str,
        field: &str,
        max_chars: usize,
    ) -> Result<(), ConfigError> {
        if value.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: {field} must not be blank (ADR-0101 decision 7)",
                self.id
            )));
        }
        if value.trim() != value {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: {field} must be trimmed (no leading/trailing whitespace)",
                self.id
            )));
        }
        let len = value.chars().count();
        if len > max_chars {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: {field} is {len} characters; the maximum is {max_chars}",
                self.id
            )));
        }
        if let Some(control) = value.chars().find(|c| c.is_control()) {
            return Err(ConfigError::Validation(format!(
                "media fill {:?}: {field} contains control character {control:?}",
                self.id
            )));
        }
        Ok(())
    }
}

/// Whether an id is `1..=64` characters of `[A-Za-z0-9_-]` (ADR-0100
/// Decision 1). Fill ids and template ids share the shape.
fn id_is_well_formed(id: &str) -> bool {
    let len = id.chars().count();
    (1..=MAX_ID_CHARS).contains(&len)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Validate a `url_fetch` URL (ADR-0100 Decision 8): at most 2048 bytes, an
/// exactly-`https` scheme, no userinfo, and a host that survives the hygiene
/// screen below.
///
/// The URL is parsed by the WHATWG parser ([`Url::parse`]) and the **parsed**
/// host is screened — never the input text. That matters because the parser is
/// the only thing that:
/// * normalises an IDN (Unicode) name to its punycode `Domain` form, and
/// * normalises the decimal/hex/octal IPv4 notations (`2130706433`, `0x7f.1`,
///   `017700000001`) to a real `Ipv4` host.
///
/// So `https://2130706433/` reaches the [`screen_ip`] guard as `127.0.0.1` and
/// is refused, even though its literal text is not an IP.
///
/// Screens applied:
/// * non-empty, at most [`MAX_FETCH_URL_BYTES`] bytes;
/// * no ASCII whitespace or control character anywhere in the input (the parser
///   silently drops tab/newline and percent-encodes spaces, so a raw one would
///   otherwise be reinterpreted);
/// * scheme exactly `https` (the parser lower-cases it);
/// * no username or password component;
/// * a `Domain` host that is non-empty, ASCII domain-shaped, and not
///   `localhost` / `.localhost` / `.local` / `.internal` / `.home.arpa` (or a
///   trailing-dot variant of any of them);
/// * an `Ipv4`/`Ipv6` host that passes [`screen_ip`] under the strictest policy
///   (private, loopback, link-local, ULA, carrier-grade-NAT, unspecified and
///   multicast literals are all refused).
///
/// Name resolution is **not** performed here, ports are not restricted, and
/// this is **input hygiene only**: ADR-0100 Decision 8 still requires an
/// OS-enforced egress boundary at fetch time, because the fetch tool resolves
/// names, follows redirects and contacts media hosts itself.
fn validate_fetch_url(url: &str) -> Result<(), String> {
    let parsed = parse_fetch_url(url)?;
    match parsed.host() {
        Some(Host::Domain(domain)) => screen_fetch_domain(domain, url),
        Some(Host::Ipv4(ip)) => screen_fetch_literal(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => screen_fetch_literal(IpAddr::V6(ip)),
        None => Err(format!("url_fetch url {url:?} has no host")),
    }
}

/// Parse a `url_fetch` URL and apply the shape checks that do not depend on the
/// host: non-empty, byte length, raw whitespace/control characters, scheme, and
/// the absence of userinfo. Returns the parsed URL so the caller screens the
/// **parsed** host, and so tests can observe the parser's normalisation.
fn parse_fetch_url(url: &str) -> Result<Url, String> {
    if url.is_empty() {
        return Err("url_fetch `url` must not be empty".to_owned());
    }
    if url.len() > MAX_FETCH_URL_BYTES {
        return Err(format!(
            "url_fetch url is {} bytes; the maximum is {MAX_FETCH_URL_BYTES} (ADR-0100 decision 8)",
            url.len()
        ));
    }
    if let Some(bad) = url
        .chars()
        .find(|c| c.is_ascii_whitespace() || c.is_ascii_control())
    {
        return Err(format!(
            "url_fetch url {url:?} contains whitespace or control character {bad:?}"
        ));
    }
    let parsed = Url::parse(url)
        .map_err(|e| format!("url_fetch url {url:?} is not a URL (expected https://…): {e}"))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "url_fetch url {url:?} must use the https scheme (ADR-0100 decision 8); got {:?}",
            parsed.scheme()
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "url_fetch url {url:?} carries userinfo; credentials must never be embedded in a fill URL"
        ));
    }
    Ok(parsed)
}

/// The canonical **stored** form of a `url_fetch` URL: the WHATWG parser's own
/// [`Url::as_str`] rendering, so a fill stores exactly what the parser
/// interpreted (lower-cased scheme and host, punycoded IDN, and the
/// decimal/hex/octal IPv4 notations collapsed to a dotted quad) rather than the
/// operator's spelling.
///
/// Normalisation happens on the **deserialize** path (the wire path), so a
/// stored fill is canonical before [`validate`](MediaFill::validate) ever sees
/// it and `serialize → deserialize` stays stable — a canonical URL re-parses to
/// itself, so a reloaded document is identical to the one it came from.
///
/// A URL that fails the hygiene screen ([`parse_fetch_url`]) is kept verbatim so
/// validation reports the precise reason: normalising first would silently
/// reinterpret the input (the parser drops tab/newline, for example, which the
/// raw-whitespace screen must still refuse). Every such URL is rejected by
/// [`validate`](MediaFill::validate) anyway, so a stored, validated fill is
/// always canonical.
fn canonical_fetch_url(url: String) -> String {
    match parse_fetch_url(&url) {
        Ok(parsed) => parsed.as_str().to_owned(),
        Err(_) => url,
    }
}

/// Screen a parsed `Domain` fetch host: non-empty, ASCII domain-shaped, and not
/// a local/special-use name.
fn screen_fetch_domain(domain: &str, url: &str) -> Result<(), String> {
    // The parser keeps a trailing root dot (`localhost.`), so strip it before
    // the reserved-name comparison — otherwise `https://localhost./` would slip
    // past the exact match.
    let host = domain.trim_end_matches('.');
    if host.is_empty() {
        return Err(format!("url_fetch url {url:?} has an empty host"));
    }
    // IDN labels are already punycoded and every IPv4 notation already became
    // `Host::Ipv4`, so what is left must be plain ASCII domain characters.
    // Anything else is a shape the parser should not have produced: refuse it
    // rather than hand an unexamined host to a child process.
    let shaped = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !shaped {
        return Err(format!(
            "url_fetch url host {domain:?} contains characters outside the parsed host form"
        ));
    }
    let lower = host.to_ascii_lowercase();
    for name in BLOCKED_FETCH_HOSTS {
        if lower == name || lower.ends_with(&format!(".{name}")) {
            return Err(format!(
                "url_fetch url host {domain:?} is the local/special-use name {name:?}; it cannot \
                 be fetched (ADR-0100 decision 8)"
            ));
        }
    }
    Ok(())
}

/// Screen a parsed IP-literal fetch host against the shared SSRF guard under
/// the strictest policy (no allowlistable range is re-enabled).
fn screen_fetch_literal(ip: IpAddr) -> Result<(), String> {
    screen_ip(ip, &DialPolicy::deny_all_allowlistable())
        .map_err(|e| format!("url_fetch url host {ip} is blocked by the SSRF guard ({e})"))
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
                MediaFillKind::UrlFetch {
                    url: canonical_fetch_url(url),
                }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use url::Url;

    use super::{parse_fetch_url, validate_fetch_url};

    /// The WHATWG parser punycodes a Unicode IDN host; the screen then sees the
    /// normalised ASCII name (ADR-0100 Decision 8, input hygiene).
    #[test]
    fn a_unicode_idn_host_is_normalised_to_punycode() {
        let parsed = parse_fetch_url("https://bücher.example/clip.mp4")
            .expect("a normal Unicode IDN name is accepted");
        assert_eq!(parsed.host_str(), Some("xn--bcher-kva.example"));
        assert_eq!(
            Url::parse("https://bücher.example/clip.mp4")
                .unwrap()
                .host_str(),
            Some("xn--bcher-kva.example")
        );
    }

    /// The decimal IPv4 notation is normalised to `Host::Ipv4`, so it reaches
    /// the SSRF guard as `127.0.0.1` rather than passing as a name.
    #[test]
    fn a_decimal_ipv4_host_is_normalised_and_then_blocked() {
        let parsed =
            Url::parse("https://2130706433/").expect("the WHATWG parser accepts decimal IPv4");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        let err = validate_fetch_url("https://2130706433/").expect_err("loopback is blocked");
        assert!(err.contains("SSRF guard"), "{err}");
    }

    /// The hex/octal IPv4 notations are normalised the same way.
    #[test]
    fn hex_and_octal_ipv4_hosts_are_normalised_and_blocked() {
        for url in ["https://0x7f.1/", "https://017700000001/"] {
            let parsed = Url::parse(url).expect("the WHATWG parser accepts the notation");
            assert_eq!(parsed.host_str(), Some("127.0.0.1"), "{url}");
            let err = validate_fetch_url(url).expect_err("loopback is blocked");
            assert!(err.contains("SSRF guard"), "{url}: {err}");
        }
    }
}
