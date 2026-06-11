//! The serde schema for a Multiview config document.
//!
//! These types mirror the authored TOML (and the canonical JSON wire form)
//! exactly — see `docs/templates/layout-and-config.md` and the shipped
//! `examples/*.toml`. The shape is: a top-level `schema_version`, a `[canvas]`
//! (with `fps` as an exact rational **string**, never a float — invariant #3),
//! a `[layout]` (CSS-grid / absolute / preset), tagged `[[sources]]`,
//! `[[cells]]`, `[[overlays]]`, and `[[outputs]]`.
//!
//! All unions are **internally tagged** by `kind` (`#[serde(tag = "kind")]`),
//! never `untagged`: that is the only encoding robust across non-self-describing
//! TOML and JSON (ADR-0010).

use std::fmt;
use std::str::FromStr;

use multiview_core::time::Rational;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::audio::{OutputAudio, OutputAudioCapability, TrackCapacity, TrackDelivery};
use crate::error::ConfigError;
use crate::failover::{default_failover_slate, FailoverSlate};
use crate::grid::{GridLayout, Track};
use crate::placement::DevicePin;

/// An exact frame rate parsed from a `"num/den"` string (e.g. `"30000/1001"`).
///
/// A bare TOML/JSON float (e.g. `29.97`) deliberately fails to deserialize:
/// frame rates are exact rationals, never floats (invariant #3). The value is
/// carried as a [`Rational`] and re-serialized back to its `"num/den"` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fps(Rational);

impl Fps {
    /// The underlying exact [`Rational`] cadence.
    #[must_use]
    pub const fn rational(self) -> Rational {
        self.0
    }
}

impl FromStr for Fps {
    type Err = ConfigError;

    /// Parse `"num/den"` into an exact frame rate.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidFps`] if the string is not exactly two
    /// integers separated by a single `/`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let mut parts = trimmed.split('/');
        let (Some(num_str), Some(den_str), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(ConfigError::InvalidFps {
                value: trimmed.to_owned(),
                reason: "expected exactly one '/' separating numerator and denominator".to_owned(),
            });
        };
        let num: i64 = num_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidFps {
                value: trimmed.to_owned(),
                reason: "numerator is not an integer".to_owned(),
            })?;
        let den: i64 = den_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidFps {
                value: trimmed.to_owned(),
                reason: "denominator is not an integer".to_owned(),
            })?;
        Ok(Self(Rational::new(num, den)))
    }
}

impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.0.num, self.0.den)
    }
}

impl Serialize for Fps {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Fps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Visitor that accepts only strings (a float would be a wrong type).
        struct FpsVisitor;
        impl Visitor<'_> for FpsVisitor {
            type Value = Fps;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a rational frame-rate string like \"30000/1001\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_str(FpsVisitor)
    }
}

/// Per-axis color override on a source (each axis `auto` or an explicit token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ColorOverride {
    /// Primaries axis (`auto` or an explicit primaries token).
    #[serde(default = "auto_token")]
    pub primaries: String,
    /// Transfer axis.
    #[serde(default = "auto_token")]
    pub transfer: String,
    /// Matrix axis.
    #[serde(default = "auto_token")]
    pub matrix: String,
    /// Range axis.
    #[serde(default = "auto_token")]
    pub range: String,
}

/// The default `"auto"` token for an unspecified color-override axis.
fn auto_token() -> String {
    "auto".to_owned()
}

/// The canvas working-color-space block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CanvasColor {
    /// Working-color-space profile name (e.g. `sdr-bt709-limited`, `custom`).
    pub profile: String,
    /// Explicit primaries (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    /// Explicit transfer (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    /// Explicit matrix (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<String>,
    /// Explicit range (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

/// The output canvas: geometry, cadence, pixel format, background, color space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Canvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Output cadence as an exact rational (parsed from a `"num/den"` string).
    pub fps: Fps,
    /// Working pixel format (`nv12` 8-bit, `p010` 10-bit).
    pub pixel_format: String,
    /// Background fill (hex color, e.g. `#101014`).
    pub background: String,
    /// Working color space.
    pub color: CanvasColor,
}

/// RTSP-specific ingest options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RtspOptions {
    /// Lower-transport selection (`tcp` / `udp`).
    pub transport: String,
}

/// Reference-only credential pointer for a source (never plaintext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SourceAuth {
    /// A secret reference (e.g. `op://Servers/cam/credentials`).
    pub secret_ref: String,
}

/// The face a [`SourceKind::Clock`] source renders (config-level mirror of the
/// overlay clock face; mapped onto `multiview_overlay`'s model at render time).
///
/// A plain string enum (`analog` / `digital` / `dual`); the `twelve_hour` flag
/// rides alongside it on the [`SourceKind::Clock`] payload. `dual` draws both an
/// analogue face and a digital readout in one source (ADR-0047).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockFaceConfig {
    /// Analog face: hour / minute / second hands on a ticked bezel.
    #[default]
    Analog,
    /// Digital `HH:MM:SS` readout (see `twelve_hour`).
    Digital,
    /// Both: an analogue face with a digital readout beneath it.
    Dual,
}

/// The kind-specific payload of a managed input, internally tagged by `kind`.
///
/// Synthetic sources (`bars`, `solid`, `clock`) are produced in-process and are
/// first-class peers of the decoded kinds (ADR-0027): nothing downstream of
/// ingest treats them differently. The network kinds carry a `url`; NDI binds by
/// source `name`; `file` a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceKind {
    /// Built-in SMPTE/EBU colour bars (the line-up signal). `test` is accepted as
    /// a back-compat alias and canonicalizes to `bars`.
    #[serde(alias = "test")]
    Bars,
    /// A solid-colour slate (hex, e.g. `#101014`).
    Solid {
        /// Fill colour as a `#RRGGBB` (or `#RGB`) hex string.
        color: String,
    },
    /// A full-frame clock disciplined by the system wall clock.
    Clock {
        /// Analog (default), digital, or dual face.
        #[serde(default)]
        face: ClockFaceConfig,
        /// Selects 12-hour vs 24-hour mode for **both** faces: a digital readout
        /// shows AM/PM (`true`) or `00`–`23` (`false`); an analog dial uses a
        /// 12-hour dial (`true`, hour hand two revolutions/day) or a 24-hour dial
        /// (`false`, 24 ticks, one revolution/day). Defaults to `false` (24-hour).
        #[serde(default)]
        twelve_hour: bool,
        /// IANA timezone id (e.g. `Australia/Sydney`). **Preferred** over
        /// `tz_offset_minutes`: the displayed offset is resolved DST-correct per
        /// instant. Absent ⇒ the fixed `tz_offset_minutes` is used. If both are
        /// present, `timezone` wins and `tz_offset_minutes` is ignored (a
        /// validation warning is emitted via [`Source::clock_warnings`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
        /// Fixed timezone offset from UTC in minutes (e.g. `600` = UTC+10), the
        /// legacy / no-DST field. Real offsets span `-720..=840`. Ignored when
        /// `timezone` is set.
        #[serde(default)]
        tz_offset_minutes: i32,
        /// Operator location/label drawn on the face (e.g. `Sydney`, `Studio A`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Draw a `UTC±HH:MM` offset badge for the displayed instant.
        #[serde(default)]
        show_offset: bool,
        /// Draw the disciplined-reference badge (PTP/NTP/SYS lock). Display only —
        /// never paces (ADR-T012). Default off.
        #[serde(default)]
        show_reference: bool,
        /// Draw hour numerals on the analogue / dual face.
        #[serde(default)]
        numerals: bool,
    },
    /// A digital countdown / count-up to a target instant (ADR-0047). The target
    /// is a wall-clock time-of-day (optionally recurring) or an absolute
    /// date+time, resolved in an IANA zone (DST-correct) or a fixed offset; the
    /// displayed duration is **integer seconds** (never float). Silent, pure
    /// pixels, in-process — a synthetic source like `clock`.
    Timer {
        /// The target instant, internally tagged on `target`
        /// (`time_of_day` | `date_time`) and flattened to the top level.
        #[serde(flatten)]
        target: crate::timer::TimerTarget,
        /// Count `down` (default) to the target or `up` from it.
        #[serde(default)]
        direction: crate::timer::TimerDirection,
        /// What to do at/after the target: `hold` (default) | `continue` |
        /// `zero_then_up` | `recur`.
        #[serde(default)]
        on_target: crate::timer::TimerOnTarget,
        /// The display format: `d_hh_mm_ss` (default) | `hh_mm_ss` | `mm_ss` |
        /// `hh_mm_ss_ff` | `auto`.
        #[serde(default)]
        format: crate::timer::TimerFormat,
        /// Operator label drawn with the count (e.g. `ON AIR IN`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Overrun prefix override (default `+` past the target; `-`/none before).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overrun_prefix: Option<String>,
        /// Draw the overrun a11y badge (`OVER` / `ELAPSED`) past the target.
        #[serde(default = "default_true")]
        overrun_badge: bool,
    },
    /// RTSP pull.
    Rtsp {
        /// Source URL.
        url: String,
        /// RTSP transport options.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtsp: Option<RtspOptions>,
    },
    /// HLS / M3U pull.
    Hls {
        /// Playlist URL.
        url: String,
    },
    /// `YouTube` live, bound by a watch/live/channel URL (ADR-0015).
    ///
    /// `YouTube` publishes no stable manifest URL, so this is a thin wrapper over
    /// `hls`: an external, runtime-discovered `yt-dlp` resolver (the
    /// off-by-default `youtube` feature in `multiview-input`) turns this URL into a
    /// concrete `*.googlevideo.com` HLS master that the standard HLS ingest path
    /// consumes, re-resolving before the manifest expires. Bound by the `YouTube`
    /// URL — never a hand-copied manifest, which expires.
    Youtube {
        /// `YouTube` watch/live/channel URL (e.g. `https://www.youtube.com/watch?v=…`).
        url: String,
    },
    /// MPEG-TS input.
    Ts {
        /// Source URL.
        url: String,
    },
    /// SRT input.
    Srt {
        /// Source URL.
        url: String,
    },
    /// RTMP input.
    Rtmp {
        /// Source URL.
        url: String,
    },
    /// NDI input, bound by source name.
    Ndi {
        /// NDI source name (e.g. `STUDIO (CAM 1)`).
        name: String,
    },
    /// File input.
    File {
        /// Filesystem path.
        path: String,
    },
    /// AES67 / SMPTE ST 2110-30 PCM-audio RTP input (open-audio over IP).
    ///
    /// Tier 0 binding is a static SDP session (RFC 4566/8866) pasted or fetched
    /// once at config load; the multicast group, codec (L16/L24), channel count,
    /// packet time, and PTP reference clock are described there. Tier 1/2
    /// (SAP/NMOS dynamic discovery) is identified by `session_id` and is a later
    /// slice. IPv6-first (ADR-0042): the SDP connection line is `c=IN IP6` and
    /// `multicast` carries a bracketed IPv6 group (`[ff3e::1]:5004`).
    Aes67 {
        /// Static SDP session description (RFC 4566/8866), as text or a URL. The
        /// Tier 0 binding: the codec/clock/PTP/multicast are read from here.
        sdp: String,
        /// Optional SAP session id or NMOS sender id for dynamic discovery
        /// (Tier 1/2, a later slice). Absent ⇒ the static `sdp` is authoritative.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Optional multicast `group:port` override (`[ff3e::1]:5004`). Absent ⇒
        /// derived from the SDP connection + `m=audio` lines at ingest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        multicast: Option<String>,
        /// Optional receive jitter-buffer lead in milliseconds (the AES67 link
        /// offset). Absent ⇒ the engine's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link_offset_ms: Option<u32>,
        /// Optional PTP domain (`0` for ST 2110-30-strict, `1..=127` otherwise).
        /// Absent ⇒ derived from the SDP `a=ts-refclk` domain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ptp_domain: Option<u8>,
    },
}

impl SourceKind {
    /// Whether this kind is produced **in-process** (`bars`/`solid`/`clock`,
    /// ADR-0027) rather than decoded from external media.
    ///
    /// This is the live-apply classification point (ADR-W018): synthetic kinds
    /// can be added/edited on the running engine (`X-Multiview-Apply: live`),
    /// while decoded kinds currently apply on restart. The CLI's synthetic
    /// renderer (`SyntheticKind::from_source_kind`) accepts exactly this set.
    #[must_use]
    pub const fn is_synthetic(&self) -> bool {
        matches!(
            self,
            Self::Bars | Self::Solid { .. } | Self::Clock { .. } | Self::Timer { .. }
        )
    }
}

/// A managed input: a stable `id`, a display name, the kind-specific payload,
/// and optional auth/color overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Source {
    /// Stable input id (referenced by `cells.source.input_id`).
    pub id: String,
    /// Human-friendly display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The kind-specific payload (flattened so `kind`/`url` sit at top level).
    #[serde(flatten)]
    pub kind: SourceKind,
    /// Reference-only credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SourceAuth>,
    /// Per-source color override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_override: Option<ColorOverride>,
    /// Per-source caption/subtitle selector for native in-pipeline decode.
    ///
    /// Absent means no captions are decoded for this source — the engine never
    /// decodes a track it will not display (an efficiency lever, not a default
    /// cost). See [`CaptionSelector`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<CaptionSelector>,
    /// Operator pin for this source's **decode** stage to a stable GPU
    /// ([`DevicePin`], ADR-0018 §2.1). Absent ⇒ the placement engine auto-places
    /// the source's decode. A pin always wins (it is never silently relocated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_pin: Option<DevicePin>,
    /// Per-source **wall-clock** Use/Discard verb (ADR-0038, SYNC-0).
    ///
    /// Absent means the default behaviour (reclock-to-house). When present and set
    /// to [`WallClockUse::Use`], a source whose wall-clock is measured **Trusted**
    /// at runtime (e.g. HLS `PROGRAM-DATE-TIME`) is rebased onto the common
    /// wall-clock timeline; [`WallClockUse::Discard`] keeps the as-built
    /// reclock-to-house anchor. Config carries **only** this operator verb — the
    /// trust *tier* is measured at runtime, never authored. See [`SourceWallClock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallclock: Option<SourceWallClock>,
}

/// Per-source wall-clock configuration (ADR-0038, SYNC-0): the operator's
/// Use/Discard verb for the source's detected wall-clock.
///
/// Internally a tagged record (`{ use = "use" | "discard" }`), robust across TOML
/// and JSON — never `untagged`. The detected trust *tier* is a runtime measurement
/// and is **not** carried here; this struct holds only the authored verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceWallClock {
    /// Whether to **use** the source's detected wall-clock (rebase onto the common
    /// timeline when Trusted) or **discard** it (reclock-to-house). Defaults to
    /// [`WallClockUse::Use`] when the `wallclock` table is present without a `use`
    /// key, matching ADR-0038's default-Use stance.
    #[serde(rename = "use", default)]
    pub use_: WallClockUse,
}

/// The per-source wall-clock operator verb (ADR-0038, SYNC-0).
///
/// Serializes as a snake-case string tag (`"use"` / `"discard"`); never an integer
/// or untagged positional form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WallClockUse {
    /// Use the detected wall-clock: rebase the source onto the common wall-clock
    /// timeline when its runtime-measured tier is Trusted. The default.
    #[default]
    Use,
    /// Discard the detected wall-clock; keep the as-built reclock-to-house anchor.
    Discard,
}

/// How captions/subtitles are sourced for one input, decoded **natively from the
/// source stream** (the primary path, superseding external sidecar files).
///
/// Internally tagged by `mode` (robust across TOML and JSON; never `untagged`).
/// Each family maps onto `multiview_ffmpeg`'s `CaptionDecoder` at ingest time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptionSelector {
    /// Auto-select the first usable caption track on the source (surface
    /// whatever the stream carries).
    Auto,
    /// Captions explicitly disabled — equivalent to omitting the field, but
    /// expressible so a template can pin "no captions".
    Off,
    /// DVB teletext, addressed by page (e.g. `801` for English subtitles).
    TeletextPage {
        /// Teletext page number (magazine-addressed, typically `100`–`899`).
        page: u16,
    },
    /// A subtitle track identified by stream id or language tag (e.g. `"eng"`).
    Track {
        /// The track identifier (a language tag or a stream-relative id).
        id: String,
    },
    /// Embedded CEA-608/708 captions carried in the video stream, addressed by
    /// field/service (e.g. `"cc1"`).
    EmbeddedCc {
        /// The caption field/service selector (e.g. `cc1`..`cc4` or a service).
        field: String,
    },
    /// An external sidecar subtitle file (SRT/WebVTT) — the legacy path, kept so
    /// it routes through the same per-tile burn-in as native decode.
    Sidecar {
        /// Filesystem path to the `.srt`/`.vtt` sidecar.
        path: String,
    },
}

/// The layout placement strategy, internally tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Layout {
    /// A named factory preset.
    Preset {
        /// Preset name (`2x2`, `3x3`, `1+5`, `pip`).
        preset: String,
    },
    /// A CSS-grid layout (fr/px/% tracks + areas).
    Grid {
        /// Column tracks.
        columns: Vec<String>,
        /// Row tracks.
        rows: Vec<String>,
        /// Uniform gap in pixels.
        #[serde(default)]
        gap: u32,
        /// Row-gap override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row_gap: Option<u32>,
        /// Column-gap override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column_gap: Option<u32>,
        /// `grid-template-areas` map.
        areas: Vec<String>,
    },
    /// Absolute normalized rects (placement carried per-cell).
    Absolute,
}

/// A normalized rectangle (`0.0..=1.0`) for an absolutely-placed cell.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// A cell border specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Border {
    /// Border width in pixels.
    #[serde(default)]
    pub width_px: u32,
    /// Border color (hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Border style (`solid`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// The per-cell `QoS` / degradation policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CellQos {
    /// Relative priority (higher is shed last).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// Degradation strategy (`maintain-fps`, `maintain-resolution`, `balanced`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<String>,
}

/// A cell's source binding: a managed `input_id` (preferred) or an inline spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CellSource {
    /// Reference to a managed input by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    /// Inline source kind (`ndi`, `rtmp`, …) when not referencing a managed id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Inline NDI source name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Inline URL (rtmp/rtsp/hls/…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Inline offline fallback behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// One cell: placement (by grid `area` or absolute `rect`), fit/z/styling, and
/// a source binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Cell {
    /// Stable cell id.
    pub id: String,
    /// Grid area name (mutually exclusive with `rect`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// Absolute normalized rect (mutually exclusive with `area`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
    /// Stacking order (higher draws on top).
    #[serde(default)]
    pub z: i32,
    /// Fit mode (`fill`/`contain`/`cover`/`none`/`scale_down`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
    /// Anchor for crop/letterbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<String>,
    /// Opacity (premultiplied, linear).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Corner-radius clip in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner_radius: Option<u32>,
    /// Scaler selection (`auto`/`bilinear`/`lanczos`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaler: Option<String>,
    /// Whether the cell is visible (`false` => decode-skip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Hint that the source is largely static.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_friendly: Option<bool>,
    /// Border specification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Border>,
    /// `QoS` / degradation policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<CellQos>,
    /// What this tile shows when its source is lost or misbehaving — the
    /// configurable failover-slate policy (ADR-0027 / ADR-0030). Defaults to
    /// [`FailoverSlate::Bars`] (the broadcast standard) when omitted, so a
    /// pre-existing document gets the default rather than a dead screen. The
    /// engine's compositor drive composites this per cell once the tile state
    /// machine reaches the down state (it changes *what* shows, not *when*).
    #[serde(default = "default_failover_slate")]
    pub on_loss: FailoverSlate,
    /// Source binding.
    pub source: CellSource,
}

/// An overlay layer, internally tagged by `kind`.
///
/// Overlays carry a large, kind-dependent parameter set; the rarely-uniform
/// extras are captured verbatim so the document round-trips losslessly without
/// this crate having to model every overlay kind's fields up front.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Overlay {
    /// Stable overlay id.
    pub id: String,
    /// Overlay kind (`clock`, `tally_border`, `label`, …).
    pub kind: String,
    /// Attachment target (`canvas` or a cell id).
    pub target: String,
    /// Stacking order.
    #[serde(default)]
    pub z: i32,
    /// Kind-specific parameters captured verbatim (lossless round-trip).
    #[serde(flatten)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// An output sink/server, internally tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Output {
    /// RTSP server.
    RtspServer {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ a stable id is
        /// **derived** from [`Output::label`] (back-compat for v1/v2 docs), so a
        /// crosspoint can address this output via an [`crate::OutputRef`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Mount point (e.g. `/multiview`).
        mount: String,
        /// Video codec (`h264`, `hevc`, …).
        codec: String,
        /// Latency profile hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latency_profile: Option<String>,
        /// Operator pin for this output's **encode** stage to a stable GPU
        /// ([`DevicePin`], ADR-0018 §2.1). Absent ⇒ auto-placed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection (program bus vs explicit tracks). Absent
        /// ⇒ the engine's default (the mixed program bus only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// Low-latency HLS packager.
    LlHls {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Output path.
        path: String,
        /// Video codec.
        codec: String,
        /// Target part duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part_target_ms: Option<u32>,
        /// Segment duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_ms: Option<u32>,
        /// GOP duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gop_ms: Option<u32>,
        /// Operator pin for this output's **encode** stage to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. Absent ⇒ the mixed program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// HLS packager.
    Hls {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Output path.
        path: String,
        /// Video codec.
        codec: String,
        /// Segment duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_ms: Option<u32>,
        /// Operator pin for this output's **encode** stage to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. Absent ⇒ the mixed program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// NDI output.
    Ndi {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// NDI source name to advertise.
        name: String,
        /// Operator pin for this output's frame source to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. NDI carries a channel-map (not selectable
        /// tracks); the capability matrix in `multiview-audio` validates this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// RTMP push.
    Rtmp {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Destination URL.
        url: String,
        /// Video codec.
        codec: String,
        /// Operator pin for this output's **encode** stage to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Whether the **endpoint** supports Enhanced-RTMP v2 multitrack audio
        /// (ADR-R005 §4.2). RTMP's discrete-track capability is *endpoint-gated*,
        /// not format-gated: the legacy default (`false`) carries one audio track
        /// only, while `true` declares an endpoint (Enhanced-RTMP v2 + a modern
        /// `flvenc`) that carries N tracks via `audioTrackId`. Multitrack
        /// selections are rejected at config time unless this is set — degrade
        /// explicitly to the mixed bus, never silently drop tracks.
        #[serde(default)]
        multitrack: bool,
        /// Per-output audio selection. RTMP multitrack is endpoint-gated by
        /// [`multitrack`](Output::Rtmp::multitrack); the capability matrix
        /// ([`Output::audio_capability`]) validates this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// SRT push.
    Srt {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Destination URL.
        url: String,
        /// Video codec.
        codec: String,
        /// Operator pin for this output's **encode** stage to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. Absent ⇒ the mixed program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// AES67 / SMPTE ST 2110-30 PCM-audio RTP output (open-audio over IP).
    ///
    /// The first output with **no encode/GPU stage**: it packetizes the program
    /// bus to raw L16/L24 PCM and multicasts it. IPv6-first (ADR-0042): the
    /// `multicast` group is a bracketed IPv6 SSM literal (`[ff3e::1]:5004`).
    Aes67 {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Display name (AES67 outputs carry an explicit label, since there is no
        /// mount/path/url to derive one from).
        label: String,
        /// Multicast `group:port` to send to (`[ff3e::1]:5004`).
        multicast: String,
        /// PCM depth: `"L24"` (Class A interop default) or `"L16"`.
        #[serde(default = "default_aes67_depth")]
        depth: String,
        /// Packet time in milliseconds (`1` = 48 samples @ 48 kHz = Class A).
        #[serde(default = "default_aes67_ptime_ms")]
        ptime_ms: u32,
        /// Optional PTP domain (`0..=127`, `0` = ST 2110-30-strict). Absent ⇒ the
        /// engine's reference default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ptp_domain: Option<u8>,
        /// Operator GPU pin. **Always `None`** for AES67 (raw PCM, no encode
        /// stage); the field exists so every [`Output`] variant exposes
        /// [`Output::gpu_pin`] uniformly without a hand-coded exception.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection (program bus vs explicit tracks). Absent ⇒
        /// the mixed program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
    /// Local DRM/KMS display-head output (HDMI/DisplayPort glass) — ADR-0044.
    ///
    /// A **raw-frame sink**: it scans the pre-encode NV12 canvas out to one
    /// KMS connector via atomic page flips and never joins the packet fan-out
    /// (invariant #7 untouched — no encode, no mux). Runnable only in a
    /// `display-kms` build of the `multiview` binary; a build without that
    /// feature **fails validation** of a document declaring one (never a
    /// silent skip).
    Display {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// KMS connector name (`DP-1`, `HDMI-A-1`, …); `"auto"` selects the
        /// first connected connector. Defaults to `"auto"`.
        #[serde(default = "default_display_connector")]
        connector: String,
        /// Optional explicit mode override: the EDID mode matching this exact
        /// geometry + exact-rational refresh is committed; no match is a
        /// startup error naming the available modes. Absent ⇒ automatic
        /// selection (EDID preferred mode, engine-cadence rational match).
        /// Mutually exclusive with `forced_mode`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<DisplayModeSpec>,
        /// CVT-RB computed forced mode for **EDID-less** connectors (a
        /// verified field condition — brief §6): used only when the connector
        /// exposes no EDID modes. Mutually exclusive with `mode`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forced_mode: Option<DisplayModeSpec>,
        /// Operator pin to a stable GPU ([`DevicePin`], ADR-0018 §2.1).
        /// Scanout additionally implies the connector-owning-GPU locality
        /// constraint (ADR-0044 §3), which this hint never overrides.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. HDMI/DP audio carries one LPCM
        /// channel-map feed (ELD-gated at runtime), never selectable discrete
        /// tracks — like NDI/AES67, a discrete-track route is a capability
        /// error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
    },
}

/// A display mode requested for an [`Output::Display`] head: exact pixel
/// geometry plus the refresh as an **exact rational** `"num/den"` string
/// (never a float — invariant #3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DisplayModeSpec {
    /// Active width in pixels (`> 0`).
    pub width: u32,
    /// Active height in pixels (`> 0`).
    pub height: u32,
    /// Refresh rate as an exact rational string (e.g. `"60000/1001"`).
    pub refresh: Fps,
}

/// Default [`Output::Display`] connector selector: the first connected one.
fn default_display_connector() -> String {
    "auto".to_owned()
}

/// Default `true` (serde `default` for an opt-out boolean, e.g. the timer
/// overrun badge).
const fn default_true() -> bool {
    true
}

/// Default AES67 output PCM depth (Class A interop): 24-bit L24.
fn default_aes67_depth() -> String {
    "L24".to_owned()
}

/// Default AES67 output packet time: 1 ms (Class A; 48 samples @ 48 kHz).
const fn default_aes67_ptime_ms() -> u32 {
    1
}

impl Output {
    /// The **explicit** operator id this output declares, if any (ADR-0034 /
    /// RT-12). `None` ⇒ no `id` was authored, and [`Output::id`] derives a
    /// stable one from [`Output::label`].
    #[must_use]
    pub fn explicit_id(&self) -> Option<&str> {
        match self {
            Output::RtspServer { id, .. }
            | Output::LlHls { id, .. }
            | Output::Hls { id, .. }
            | Output::Ndi { id, .. }
            | Output::Rtmp { id, .. }
            | Output::Srt { id, .. }
            | Output::Aes67 { id, .. }
            | Output::Display { id, .. } => id.as_deref(),
        }
    }

    /// This output's **stable id** — the operator-addressable handle a routing
    /// [`crate::OutputRef`] binds to (ADR-0034 / RT-12).
    ///
    /// Returns the explicitly-authored `id` when present, otherwise a stable id
    /// **derived** from [`Output::label`]. The derivation keeps v1/v2 documents
    /// (which carry no `id`) routing identically: the desugared output
    /// crosspoints reference this same string. An explicit id should not collide
    /// with another output's resolved id — [`crate::MultiviewConfig::validate`]
    /// rejects a document where two outputs resolve to the same id.
    #[must_use]
    pub fn id(&self) -> String {
        self.explicit_id()
            .map_or_else(|| self.label(), ToOwned::to_owned)
    }

    /// The operator GPU pin for this output's encode stage, if any (ADR-0018
    /// §2.1). `None` ⇒ the placement engine auto-places the output's encode.
    #[must_use]
    pub const fn gpu_pin(&self) -> Option<&DevicePin> {
        match self {
            Output::RtspServer { gpu_pin, .. }
            | Output::LlHls { gpu_pin, .. }
            | Output::Hls { gpu_pin, .. }
            | Output::Ndi { gpu_pin, .. }
            | Output::Rtmp { gpu_pin, .. }
            | Output::Srt { gpu_pin, .. }
            // AES67 carries a `gpu_pin` field that is always `None` (no encode
            // stage); it is matched uniformly here and returns `None`.
            | Output::Aes67 { gpu_pin, .. }
            // Display has no encode stage either; its pin is a scanout-device
            // hint consumed by placement (ADR-0044 §3), matched uniformly.
            | Output::Display { gpu_pin, .. } => gpu_pin.as_ref(),
        }
    }

    /// The per-output audio selection, if any. `None` ⇒ the engine carries the
    /// mixed program bus by default for this output.
    #[must_use]
    pub const fn audio(&self) -> Option<&OutputAudio> {
        match self {
            Output::RtspServer { audio, .. }
            | Output::LlHls { audio, .. }
            | Output::Hls { audio, .. }
            | Output::Ndi { audio, .. }
            | Output::Rtmp { audio, .. }
            | Output::Srt { audio, .. }
            | Output::Aes67 { audio, .. }
            | Output::Display { audio, .. } => audio.as_ref(),
        }
    }

    /// This output's **audio capability** — the verified per-transport matrix
    /// from ADR-R005 §4.2 as a first-class, machine-readable value.
    ///
    /// - **RTSP** carries N simultaneous `m=audio` subsessions ⇒ unlimited
    ///   simultaneous discrete tracks.
    /// - **MPEG-TS over SRT** carries N PIDs ⇒ unlimited simultaneous.
    /// - **HLS / LL-HLS** carry N renditions but the player plays one at a time
    ///   ⇒ unlimited but *select-one* (a UI selector, not simultaneous monitors).
    /// - **NDI** carries no selectable discrete tracks (channel-map only) ⇒ a
    ///   discrete-track selection is a capability error.
    /// - **RTMP** is endpoint-gated: legacy carries one track; an endpoint that
    ///   declares [`multitrack`](Output::Rtmp::multitrack) carries N.
    ///
    /// Consumed by config-time validation and by the Web UI routing matrix
    /// (AUD-8), which greys out cells a transport cannot deliver.
    #[must_use]
    pub const fn audio_capability(&self) -> OutputAudioCapability {
        match self {
            // RTSP: N simultaneous `m=audio` subsessions. SRT carries MPEG-TS ⇒
            // N PIDs, also simultaneous (the receiver-dependent first-PID-only
            // behaviour is a delivery caveat, not a config-time capacity cap).
            Output::RtspServer { .. } | Output::Srt { .. } => {
                OutputAudioCapability::new(TrackDelivery::Simultaneous, TrackCapacity::Unlimited)
            }
            // HLS/LL-HLS: N renditions, but the player plays one at a time.
            Output::Hls { .. } | Output::LlHls { .. } => {
                OutputAudioCapability::new(TrackDelivery::SelectOne, TrackCapacity::Unlimited)
            }
            // NDI, AES67 / ST 2110-30, and a local display head all carry one
            // multiplexed PCM channel-map flow, never selectable discrete tracks —
            // a discrete-track route is a capability error for any of them.
            // (HDMI/DP audio is the sink's ELD-gated LPCM channel map; multiple
            // program tracks would be multiple heads/senders/sessions.)
            Output::Ndi { .. } | Output::Aes67 { .. } | Output::Display { .. } => {
                OutputAudioCapability::new(TrackDelivery::None, TrackCapacity::AtMost(0))
            }
            // RTMP: endpoint-gated. Legacy = one track; Enhanced-RTMP v2 = N.
            Output::Rtmp { multitrack, .. } => {
                if *multitrack {
                    OutputAudioCapability::new(
                        TrackDelivery::Simultaneous,
                        TrackCapacity::Unlimited,
                    )
                } else {
                    OutputAudioCapability::new(
                        TrackDelivery::Simultaneous,
                        TrackCapacity::AtMost(1),
                    )
                }
            }
        }
    }

    /// A stable label for this output (its kind + addressed endpoint) used in
    /// validation diagnostics. Outputs carry no operator id, so the mount/path/
    /// url/name addresses it.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Output::RtspServer { mount, .. } => format!("rtsp_server {mount}"),
            Output::LlHls { path, .. } => format!("ll_hls {path}"),
            Output::Hls { path, .. } => format!("hls {path}"),
            Output::Ndi { name, .. } => format!("ndi {name}"),
            Output::Rtmp { url, .. } => format!("rtmp {url}"),
            Output::Srt { url, .. } => format!("srt {url}"),
            // AES67 carries an explicit operator label (it has no mount/path/url
            // to derive one from); use it verbatim.
            Output::Aes67 { label, .. } => label.clone(),
            // A display head is addressed by its KMS connector.
            Output::Display { connector, .. } => format!("display {connector}"),
        }
    }
}

impl Layout {
    /// Build a [`GridLayout`] (parsed tracks) when this is a grid layout.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidTrack`] if any track string is malformed.
    pub fn as_grid_layout(&self) -> Result<Option<GridLayout>, ConfigError> {
        let Self::Grid {
            columns,
            rows,
            gap,
            row_gap,
            column_gap,
            areas,
        } = self
        else {
            return Ok(None);
        };
        let columns = parse_tracks(columns)?;
        let rows = parse_tracks(rows)?;
        Ok(Some(GridLayout {
            columns,
            rows,
            gap: *gap,
            row_gap: *row_gap,
            column_gap: *column_gap,
            areas: areas.clone(),
        }))
    }
}

/// Parse a list of track strings into [`Track`] values.
fn parse_tracks(tracks: &[String]) -> Result<Vec<Track>, ConfigError> {
    tracks.iter().map(|t| t.parse::<Track>()).collect()
}
