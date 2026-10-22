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

use multiview_core::layout::QuarterTurn;
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

/// RIST (Reliable Internet Stream Transport, VSF `TR-06`) profile selector.
///
/// Maps to the `FFmpeg` `librist` protocol's `rist_profile` `AVOption`
/// (`simple`/`main`/`advanced`). `main` is `librist`/`FFmpeg`'s own default and
/// is the schema default. `advanced` (`TR-06-3` `EAP-SRP` auth) is accepted in
/// the schema but is a Tier-1/2 direct-FFI feature — the Tier-0 `FFmpeg` path
/// exposes only `PSK-AES`, so an `advanced`-with-auth deployment is a later
/// slice (ADR-0095 §2/§6); the token round-trips losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RistProfile {
    /// Simple Profile (`TR-06-1`): `RTP`/`UDP` + `RTCP` `NACK` `ARQ`. Bonding
    /// lives here in the protocol but is unreachable via `FFmpeg`'s single-peer
    /// path (Tier-2).
    Simple,
    /// Main Profile (`TR-06-2`): `GRE` tunnel + `PSK`/`DTLS` encryption +
    /// multiplexing. The default (matches `librist`/`FFmpeg`).
    #[default]
    Main,
    /// Advanced Profile (`TR-06-3`): `EAP-SHA256-SRP6a` auth. Tier-1/2 only.
    Advanced,
}

impl RistProfile {
    /// The `FFmpeg` `librist` `rist_profile=` numeric token (`0`/`1`/`2`).
    #[must_use]
    pub const fn as_ffmpeg_token(self) -> &'static str {
        match self {
            Self::Simple => "0",
            Self::Main => "1",
            Self::Advanced => "2",
        }
    }
}

/// `AES` key length for RIST pre-shared-key (`PSK`) encryption.
///
/// Maps to the `FFmpeg` `librist` `encryption=128|256` `AVOption` — the one
/// encryption mode the `FFmpeg` protocol exposes (ADR-0095 §2). `DTLS` /
/// `EAP-SRP` are Tier-1/2 direct-FFI features, not offered here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RistAesBits {
    /// `AES-128`.
    Aes128,
    /// `AES-256`.
    Aes256,
}

impl RistAesBits {
    /// The `AES` key length in bits (`128` / `256`) — the `FFmpeg` `encryption=`
    /// value.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Aes128 => 128,
            Self::Aes256 => 256,
        }
    }
}

/// RIST pre-shared-key encryption: the `AES` key length plus a **secret-manager
/// reference** (never a plaintext key).
///
/// The passphrase is held as a [`secret_ref`](RistEncryption::secret_ref)
/// resolved at run time (the [`SourceAuth`] pattern); it never lives in the
/// config file or in logs. The plaintext only ever materializes in the
/// in-memory `rist://…?secret=…` `AVIO` URL passed to libav.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RistEncryption {
    /// `AES` key length (`aes128` / `aes256`). Maps to `FFmpeg` `encryption`.
    pub aes_bits: RistAesBits,
    /// Reference to the pre-shared passphrase (e.g. `op://Servers/feed/rist-psk`
    /// or `env:RIST_PSK`). Resolved at run time; NEVER stored or logged in
    /// plaintext. Maps (resolved) to `FFmpeg` `secret`.
    pub secret_ref: String,
}

/// A bonding/load-sharing peer endpoint (Tier-2, multi-ISP).
///
/// **Tier-0 does not implement bonding** — `FFmpeg`'s `librist` protocol calls
/// `rist_peer_create()` exactly once, so a non-empty bonding list is rejected at
/// validate time with a clear error on a Tier-0 build (ADR-0095 §4), never
/// silently single-linked. The field carries the seam so a future Tier-2
/// direct-FFI build is a clean addition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RistPeer {
    /// The peer's `rist://host:port` URL (IPv6-first, bracketed literals).
    pub url: String,
}

/// RIST-specific connection options (input or output), typed rather than a raw
/// opaque `rist://?…` query (ADR-0095 §2, mirroring the multicast/SRT
/// typed-config precedent).
///
/// All fields are optional with serde `skip_serializing_if`. Lowered to the
/// `rist://…?rist_profile=…&buffer_size=…&encryption=…&secret=…&pkt_size=…`
/// `AVIO` URL for the Tier-0 `FFmpeg` path (the lowering lives in
/// `multiview-input`, where the resolved secret is injected).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RistOptions {
    /// RIST profile. Absent ⇒ `FFmpeg`'s default (`main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<RistProfile>,
    /// Receiver recovery/jitter buffer depth in milliseconds (the `ARQ` window).
    /// Absent / `0` ⇒ `librist` auto (`RTT`-Echo derived). Maps to `buffer_size`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<u32>,
    /// `MPEG-TS`-aligned packet size (default 1316 = 7×188). Maps to `pkt_size`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkt_size: Option<u16>,
    /// Pre-shared-key `AES` encryption (Main Profile). Cipher + a secret
    /// **reference** only — never a plaintext key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<RistEncryption>,
    /// Tier-2 only: bonding/load-sharing peer endpoints (multi-ISP). Empty ⇒
    /// single link. A non-empty list is **rejected** on a Tier-0 build (the
    /// `FFmpeg` protocol is single-peer) — honest capability reporting, never a
    /// silent single-link (ADR-0095 §4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bonding: Vec<RistPeer>,
}

/// Errors raised while lowering [`RistOptions`] to a `rist://…?…` `AVIO` URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RistUrlError {
    /// A non-empty `bonding` list was configured against the Tier-0 `FFmpeg`
    /// path, which is single-peer (ADR-0095 §4). Bonding needs the Tier-2
    /// direct-FFI build.
    #[error(
        "rist bonding/load-sharing requires the `rist` direct-FFI build (Tier-2); the Tier-0 \
         FFmpeg `rist://` path is single-link only"
    )]
    BondingUnsupported,
    /// Encryption was configured but the caller supplied no resolved secret
    /// (the `secret_ref` could not be resolved from the secret manager).
    #[error("rist encryption is configured but the pre-shared key (secret_ref) was not resolved")]
    UnresolvedSecret,
}

/// Lower a base `rist://host:port` URL + typed [`RistOptions`] + a
/// **resolved** pre-shared key into the `rist://…?…` `AVIO` URL the libav
/// `librist` protocol opens (ADR-0095 Tier-0).
///
/// The lowered query carries the `FFmpeg` `librist` option names: `rist_profile`
/// (`0`/`1`/`2`), `buffer_size` (ms), `pkt_size`, `encryption` (`128`/`256`),
/// and `secret`. Options absent from [`RistOptions`] are omitted so `FFmpeg`'s
/// own defaults apply. When the base URL already carries a query the lowered
/// options are appended with `&`.
///
/// `resolved_secret` is the plaintext passphrase the **caller** resolved from
/// [`RistEncryption::secret_ref`] (the config never holds it). When `redact` is
/// `true` the `secret=` value is replaced by `***` for logging — the plaintext
/// PSK never reaches a redacted (loggable) URL.
///
/// # Errors
///
/// - [`RistUrlError::BondingUnsupported`] when `opts.bonding` is non-empty
///   (Tier-0 is single-link).
/// - [`RistUrlError::UnresolvedSecret`] when `opts.encryption` is set but
///   `resolved_secret` is `None`.
pub fn lower_rist_url(
    base_url: &str,
    opts: &RistOptions,
    resolved_secret: Option<&str>,
    redact: bool,
) -> Result<String, RistUrlError> {
    if !opts.bonding.is_empty() {
        return Err(RistUrlError::BondingUnsupported);
    }
    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(profile) = opts.profile {
        params.push(("rist_profile", profile.as_ffmpeg_token().to_owned()));
    }
    if let Some(buffer_ms) = opts.buffer_ms {
        params.push(("buffer_size", buffer_ms.to_string()));
    }
    if let Some(pkt_size) = opts.pkt_size {
        params.push(("pkt_size", pkt_size.to_string()));
    }
    if let Some(enc) = &opts.encryption {
        params.push(("encryption", enc.aes_bits.bits().to_string()));
        let Some(secret) = resolved_secret else {
            return Err(RistUrlError::UnresolvedSecret);
        };
        let value = if redact {
            "***".to_owned()
        } else {
            rist_percent_encode(secret)
        };
        params.push(("secret", value));
    }
    if params.is_empty() {
        return Ok(base_url.to_owned());
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    // Append to an existing query with `&`, otherwise open one with `?`.
    let sep = if base_url.contains('?') { '&' } else { '?' };
    Ok(format!("{base_url}{sep}{query}"))
}

/// Percent-encode the characters that would break a `rist://` URL query value
/// (`&`, `=`, `?`, `#`, space, and `%` itself) — the SRT precedent's encoder.
fn rist_percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'&' | b'=' | b'?' | b'#' | b'%' | b' ' => {
                out.push('%');
                out.push(rist_hex_digit(byte >> 4));
                out.push(rist_hex_digit(byte & 0x0F));
            }
            other => match char::from_u32(u32::from(other)) {
                Some(c) => out.push(c),
                None => out.push('?'),
            },
        }
    }
    out
}

/// Map a nibble (`0..=15`) to its uppercase hex digit.
const fn rist_hex_digit(nibble: u8) -> char {
    match nibble {
        0 => '0',
        1 => '1',
        2 => '2',
        3 => '3',
        4 => '4',
        5 => '5',
        6 => '6',
        7 => '7',
        8 => '8',
        9 => '9',
        10 => 'A',
        11 => 'B',
        12 => 'C',
        13 => 'D',
        14 => 'E',
        _ => 'F',
    }
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
    /// RIST (Reliable Internet Stream Transport, VSF `TR-06`) input — the
    /// open-standard sibling of SRT (ADR-0095). Single-link Simple/Main Profile
    /// with `PSK-AES` rides `FFmpeg`'s `librist` protocol (Tier-0); the typed
    /// [`RistOptions`] lower to the `rist://…?…` `AVIO` URL.
    Rist {
        /// Source URL (`rist://[::]:port` listen, or a peer `rist://host:port`).
        /// IPv6-first: bracket IPv6 literals.
        url: String,
        /// Optional typed RIST options (profile, buffer, pkt size, PSK
        /// encryption, bonding peers).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rist: Option<RistOptions>,
    },
    /// RTMP input.
    Rtmp {
        /// Source URL.
        url: String,
    },
    /// WHIP ingest (RFC 9725): a WebRTC **contribution** source — Multiview is
    /// the server, and a browser or encoder (OBS ≥ 30, `GStreamer`
    /// `whipclientsink`) publishes media *to* it (ADR-T014 §1).
    ///
    /// The WHIP endpoint URL is **derived from the source id, never
    /// configured**: `POST /api/v1/whip/{source_id}` (e.g.
    /// `https://[2001:db8::10]:8443/api/v1/whip/cam-field-1`), with the
    /// session resource at `/api/v1/whip/{source_id}/sessions/{session_id}`.
    /// One publisher per source at a time; a configured-but-unpublished source
    /// shows the `NO_SIGNAL` placeholder (there is nothing to dial).
    Webrtc {
        /// Optional per-source bearer token (RFC 6750) a publisher presents on
        /// the WHIP `POST`. `None` ⇒ publishing requires a control-plane API
        /// key with **Write** scope — a publish endpoint is never anonymous
        /// (ADR-T014 §2). When set it must be non-empty; the plaintext token
        /// follows the existing config-secret posture (like the stream keys
        /// embedded in `rtmp`/`srt` URLs).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// Whether the SDP answer accepts the publisher's Opus audio m-line
        /// (`true`, the default) or answers it `inactive` (`false`). Accepted
        /// audio rides the standard `AudioStore` → program-bus path
        /// (ADR-T014 §5).
        #[serde(
            default = "default_webrtc_source_audio",
            skip_serializing_if = "is_true"
        )]
        audio: bool,
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
    /// A still image bound by a media-library asset id (ADR-0057 Decision 1).
    ///
    /// The single decoded frame of the referenced
    /// [`MediaAsset`](crate::MediaAsset) (a `still`-kind asset) held
    /// indefinitely. The `asset` ref is resolved against `media_library.assets`
    /// by [`MultiviewConfig::validate`](crate::MultiviewConfig::validate); a
    /// distinct serde tag from the player binding makes a both-populated payload
    /// structurally impossible.
    Still {
        /// The referenced [`MediaAsset::id`](crate::MediaAsset::id) (a
        /// `still`-kind asset in `media_library.assets`).
        asset: String,
    },
    /// A binding to a pre-declared media-player channel (ADR-0057 Decision 2).
    ///
    /// References a [`MediaPlayer`](crate::MediaPlayer) by id; the player owns
    /// one supervised ingest from boot and load/cue swaps what it plays. The
    /// `player` ref is resolved against `media_players` by
    /// [`MultiviewConfig::validate`](crate::MultiviewConfig::validate).
    MediaPlayer {
        /// The referenced [`MediaPlayer::id`](crate::MediaPlayer::id) in
        /// `media_players`.
        player: String,
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
        /// The multicast `group:port` the AES67 receiver binds and joins
        /// (`[ff3e::1]:5004`). **Required**: the ST 2110-30 SDP parser deliberately
        /// ignores the SDP `c=` connection line (the transport binding is carried
        /// in config by design), so absent ⇒ a typed error when the pipeline is
        /// built, never a value derived from the SDP. (`Option` only so a config
        /// omitting it parses, then fails closed with a clear message.)
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

/// Default for [`SourceKind::Webrtc`] `audio`: accept the publisher's Opus
/// m-line (ADR-T014 §5).
const fn default_webrtc_source_audio() -> bool {
    true
}

/// Skip-serializing predicate for a default-`true` bool field.
// serde's `skip_serializing_if` contract calls the predicate with the field by
// reference; the derive fixes the signature, so the by-value shape the lint
// asks for cannot be used here.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(value: &bool) -> bool {
    *value
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

    /// Whether this kind is decoded from a libav-openable URL or path —
    /// the ADR-W018 **level 2** live-add set (`rtsp`/`hls`/`ts`/`srt`/`rtmp`/
    /// `file`/`rist`; file and rist ride the same demux/decode ingest path as
    /// the network transports).
    ///
    /// This is the network half of the live-apply classification (ADR-W018):
    /// on a run whose engine declares the network capability these kinds can
    /// be added/edited live (a hub-spawned supervised `ingest_loop`).
    /// `ndi` (runtime-loaded host-memory receive), `youtube` (external
    /// `yt-dlp` resolution) and `aes67` (audio-only) are **not** in this set —
    /// they apply on restart.
    #[must_use]
    pub const fn is_network_media(&self) -> bool {
        matches!(
            self,
            Self::Rtsp { .. }
                | Self::Hls { .. }
                | Self::Ts { .. }
                | Self::Srt { .. }
                | Self::Rtmp { .. }
                | Self::File { .. }
                | Self::Rist { .. }
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
///
/// The cosmetic / `QoS` fields (`align`, `corner_radius`, `scaler`, `visible`,
/// `static_friendly`, `border`, `qos`) are **carried but not yet rendered**
/// (ADR-W019): the config document persists/exports/round-trips them and the
/// `apply-layout` API reports them as `carried_only`, but the compositor drive
/// does not composite them yet, so the config→core cell mapper intentionally
/// omits them. They are planned-but-unwired, not dead — keep them until the
/// compositor renders them.
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

/// Per-output **declarative metadata intent** (ADR-0088): a service/program
/// title, provider, language, an optional DVB/TS service id, a free-text
/// description, and timed-metadata opt-ins.
///
/// This is **operator intent**, not a per-transport union: the output layer
/// projects it onto whatever the chosen transport+codec can carry
/// ([`Output::metadata_capability`] / [`Output::metadata_plan`]) and surfaces
/// the rest as a visible [`MetadataField::Dropped`], never a silent no-op. It
/// lives in config (not `multiview-core` — no FFI/encode there; not the canvas
/// — it is per-output, two outputs of one program may carry different service
/// names). `#[non_exhaustive]` for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OutputMetadata {
    /// Service/program title (TS SDT `service_name`, RTMP `onMetaData` title,
    /// container `title` tag, SDP `s=`). Absent ⇒ the engine's derived default
    /// (from [`Output::label`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Service provider (TS SDT `provider_name`; container `artist`/`author`
    /// where carried). No carrier on RTMP/RTSP/NDI ⇒ [`MetadataField::Dropped`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Primary program language as an ISO-639-2 (three-letter) code (TS PMT
    /// language descriptor / container `language` tag). Validated as exactly
    /// three ASCII lowercase letters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// DVB / MPEG-TS service id (program number), `1..=65535`. Only the TS
    /// family carries it; elsewhere it is [`MetadataField::Dropped`]. A value of
    /// `0` or `> 65535` is a **validation error** (a contradiction, not a
    /// harmless unsupported field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<u32>,
    /// Free-text description / comment (TS SDT free-text where carried,
    /// container `comment` tag, SDP `i=`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Timed-metadata opt-ins for the HLS/TS now-playing/cue side stream
    /// (in-band ID3 PES + out-of-band `EXT-X-DATERANGE`). Absent ⇒ no timed
    /// metadata. These are injected off the hot path from the cue stream
    /// (ADR-0088 §4 / inv #1/#10) — they never pace a frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed: Option<OutputTimedMetadata>,
}

/// Timed-metadata opt-ins (ADR-0088 §4): the two HLS/TS now-playing/cue
/// carriers, each independently enabled. Both default `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OutputTimedMetadata {
    /// Emit in-band **ID3** metadata frames as MPEG-TS PES (Apple HLS Metadata
    /// spec). The free-form per-sample channel.
    #[serde(default)]
    pub id3: bool,
    /// Emit out-of-band **`EXT-X-DATERANGE`** playlist tags (a date-interval
    /// event with mandatory `ID` + `START-DATE`; not a generic per-sample
    /// channel).
    #[serde(default)]
    pub daterange: bool,
}

impl OutputMetadata {
    /// Construct an [`OutputMetadata`] from its named fields. Because the struct
    /// is `#[non_exhaustive]` (forward-compatible), downstream crates build it
    /// through this constructor rather than a struct literal. Pass `None` for
    /// any unset field.
    #[must_use]
    pub const fn new(
        title: Option<String>,
        provider: Option<String>,
        language: Option<String>,
        service_id: Option<u32>,
        description: Option<String>,
        timed: Option<OutputTimedMetadata>,
    ) -> Self {
        Self {
            title,
            provider,
            language,
            service_id,
            description,
            timed,
        }
    }

    /// An [`OutputMetadata`] carrying only a `title` — the common case.
    #[must_use]
    pub fn with_title(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::default()
        }
    }

    /// Whether this intent carries no requested field at all (every field is
    /// absent and no timed carrier is enabled) — i.e. it would project to
    /// nothing. Used to keep an empty `[outputs.metadata]` block harmless.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.provider.is_none()
            && self.language.is_none()
            && self.service_id.is_none()
            && self.description.is_none()
            && self.timed.is_none_or(|t| !t.id3 && !t.daterange)
    }

    /// Validate the **contradictions only** (ADR-0088 §1.3): a `service_id`
    /// outside `1..=65535` and a malformed ISO-639-2 `language`. An
    /// unsupported-but-harmless field is **never** an error here — it degrades
    /// to a visible [`MetadataField::Dropped`] in [`Output::metadata_plan`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self, owner: &str) -> Result<(), ConfigError> {
        if let Some(id) = self.service_id {
            if !(1..=65535).contains(&id) {
                return Err(ConfigError::Validation(format!(
                    "output {owner:?}: metadata.service_id {id} is out of range — a DVB/TS \
                     service id (program number) must be 1..=65535"
                )));
            }
        }
        if let Some(lang) = &self.language {
            let ok = lang.len() == 3 && lang.bytes().all(|b| b.is_ascii_lowercase());
            if !ok {
                return Err(ConfigError::Validation(format!(
                    "output {owner:?}: metadata.language {lang:?} is not a valid ISO-639-2 code \
                     (expected exactly three lowercase ASCII letters, e.g. \"eng\")"
                )));
            }
        }
        Ok(())
    }
}

/// Per-output **presentation orientation** (ADR-0089): a quarter-turn rotation
/// with an optional flip, applied either as a zero-cost display-rotation **tag**
/// or by **rotating the pixels** into a distinct rendition.
///
/// Reuses the existing core [`QuarterTurn`] vocabulary (`none`/`cw90`/`cw180`/
/// `cw270`) — it does **not** invent a new rotation enum. Distinct from
/// `Head.orientation` (the scanout portrait/landscape axis, ADR-0044) and from
/// per-cell tile rotation. `#[non_exhaustive]` for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OutputOrientation {
    /// The quarter-turn (clockwise), reusing core [`QuarterTurn`]. Default
    /// [`QuarterTurn::None`].
    #[serde(default)]
    pub turn: QuarterTurn,
    /// The orientation mechanism: tag-only, pixels, or `auto` (default).
    #[serde(default)]
    pub mode: OrientationMode,
    /// Optional horizontal/vertical flip. A non-[`OutputFlip::None`] flip is a
    /// **pixel-only** operation (no container "flip" tag exists), so it forces
    /// the pixels path. Default [`OutputFlip::None`].
    #[serde(default)]
    pub flip: OutputFlip,
}

/// The orientation mechanism (ADR-0089 §2.2): which of the two genuinely
/// different ways to orient an output is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrientationMode {
    /// Prefer the display-rotation **tag** where the transport carries one
    /// (MP4/MOV/HLS-fMP4), else rotate the **pixels**. Zero-cost and
    /// tag-aware-player-correct where supported; pixel-correct for tag-less
    /// transports. The default.
    #[default]
    Auto,
    /// Emit the display-rotation **tag** only (zero pixel cost). **Rejected at
    /// validation** on transports that carry no rotation tag (MPEG-TS, RTSP,
    /// NDI) — explicit, never a silent no-op.
    Tag,
    /// Produce a **rotated-canvas** rendition (real pixels). Always available;
    /// a 90°/270° turn swaps the encode geometry (W↔H).
    Pixels,
}

/// An output flip (ADR-0089 §2.2), reusing the cell-transform flip vocabulary.
/// A flip is **pixel-only** — there is no container "flip" tag — so any
/// non-[`OutputFlip::None`] value forces the pixels orientation path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputFlip {
    /// No flip. The default.
    #[default]
    None,
    /// Mirror horizontally (left↔right).
    Horizontal,
    /// Mirror vertically (top↔bottom).
    Vertical,
}

impl OutputOrientation {
    /// Construct an [`OutputOrientation`] from its parts. Because the struct is
    /// `#[non_exhaustive]`, downstream crates build it through this constructor
    /// rather than a struct literal.
    #[must_use]
    pub const fn new(turn: QuarterTurn, mode: OrientationMode, flip: OutputFlip) -> Self {
        Self { turn, mode, flip }
    }

    /// Whether this orientation is the identity (no turn, no flip) — it changes
    /// nothing and needs neither a tag nor a pixel pass.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.turn == QuarterTurn::None && self.flip == OutputFlip::None
    }

    /// Whether a flip is requested (forces the pixels path — no container "flip"
    /// tag exists).
    #[must_use]
    pub const fn has_flip(&self) -> bool {
        !matches!(self.flip, OutputFlip::None)
    }

    /// Resolve the **effective mechanism** for a given transport's tag
    /// capability ([`OrientationTagCapability`]):
    /// - a flip always forces [`OrientationMechanism::Pixels`];
    /// - [`OrientationMode::Tag`] yields a tag (validation has already rejected
    ///   `tag` on a tag-less transport);
    /// - [`OrientationMode::Pixels`] always yields pixels;
    /// - [`OrientationMode::Auto`] yields a tag where the transport carries one,
    ///   else pixels.
    #[must_use]
    pub const fn mechanism(&self, tag: OrientationTagCapability) -> OrientationMechanism {
        if self.has_flip() {
            return OrientationMechanism::Pixels;
        }
        match self.mode {
            OrientationMode::Tag => OrientationMechanism::Tag,
            OrientationMode::Pixels => OrientationMechanism::Pixels,
            OrientationMode::Auto => match tag {
                OrientationTagCapability::DisplayMatrix => OrientationMechanism::Tag,
                OrientationTagCapability::None => OrientationMechanism::Pixels,
            },
        }
    }

    /// Validate this orientation against a transport's tag capability
    /// (ADR-0089 §2.2): an explicit [`OrientationMode::Tag`] on a transport with
    /// no rotation tag is rejected (choose `pixels`). `auto`/`pixels` and any
    /// flip are always valid (flip ⇒ pixels).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when `tag` is requested on a
    /// transport that carries no display-rotation tag.
    pub fn validate(&self, owner: &str, tag: OrientationTagCapability) -> Result<(), ConfigError> {
        if matches!(self.mode, OrientationMode::Tag)
            && matches!(tag, OrientationTagCapability::None)
        {
            return Err(ConfigError::Validation(format!(
                "output {owner:?}: orientation.mode = \"tag\" is unsupported on this transport \
                 (MPEG-TS, RTSP and NDI carry no display-rotation tag) — use \"pixels\" (rotate \
                 the canvas) or \"auto\""
            )));
        }
        Ok(())
    }
}

/// Whether a transport can carry a **display-rotation tag** (ADR-0089 §2.1):
/// the container/bitstream rotation matrix a tag-aware player honors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OrientationTagCapability {
    /// Carries a display-rotation matrix (MP4/MOV `tkhd` / `displaymatrix`
    /// side-data; HLS-fMP4 init segment).
    DisplayMatrix,
    /// No dependable rotation tag (MPEG-TS, RTSP, NDI) — only the pixels path
    /// orients correctly.
    None,
}

/// The **resolved** orientation mechanism for one output (ADR-0089 §2.2): the
/// concrete path the mux/compositor takes after intersecting the requested
/// [`OutputOrientation`] with the transport's [`OrientationTagCapability`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OrientationMechanism {
    /// Emit the display-rotation tag only; pixels are unrotated (zero cost).
    Tag,
    /// Rotate the composited pixels into this rendition's encode target.
    Pixels,
}

/// The per-field projection result for one requested metadata field
/// (ADR-0088 §1.3): what the chosen transport did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataField {
    /// The field was applied, naming the concrete carrier it landed in (e.g.
    /// `"TS SDT service_name"`, `"onMetaData title"`, `"container language"`).
    Applied {
        /// Human-readable carrier the field landed in.
        target: &'static str,
    },
    /// The field was requested but this transport carries no place for it; it is
    /// surfaced (never silently dropped), naming why.
    Dropped {
        /// Human-readable reason the field could not be carried.
        reason: &'static str,
    },
}

impl MetadataField {
    /// Whether this field was applied (landed in a carrier).
    #[must_use]
    pub const fn is_applied(&self) -> bool {
        matches!(self, MetadataField::Applied { .. })
    }
}

/// The capability + projection plan for one output's [`OutputMetadata`]
/// (ADR-0088 §1.3): per requested field, `Applied(target)` or
/// `Dropped(reason)`. Returned from the dry-run plan **before** apply so the
/// operator sees what will and will not land. The concrete `(key, value)`
/// muxer entries are computed by the output layer from the `Applied` subset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct OutputMetadataPlan {
    /// Title projection, if `title` was requested.
    pub title: Option<MetadataField>,
    /// Provider projection, if `provider` was requested.
    pub provider: Option<MetadataField>,
    /// Language projection, if `language` was requested.
    pub language: Option<MetadataField>,
    /// Service-id projection, if `service_id` was requested.
    pub service_id: Option<MetadataField>,
    /// Description projection, if `description` was requested.
    pub description: Option<MetadataField>,
}

impl OutputMetadataPlan {
    /// Every field in this plan that was **dropped** (requested but uncarried),
    /// for surfacing to the operator. Order: title, provider, language,
    /// `service_id`, description.
    #[must_use]
    pub fn dropped(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        for (name, field) in [
            ("title", &self.title),
            ("provider", &self.provider),
            ("language", &self.language),
            ("service_id", &self.service_id),
            ("description", &self.description),
        ] {
            if let Some(MetadataField::Dropped { reason }) = field {
                out.push((name, *reason));
            }
        }
        out
    }
}

/// The per-transport metadata-carrier capability (ADR-0088 §1.2): which of the
/// named [`OutputMetadata`] fields a transport can express, as a typed matrix.
/// Intersected with the requested intent to build an [`OutputMetadataPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetadataCapability {
    /// Carrier for `title` (e.g. `Some("TS SDT service_name")`), or `None` ⇒
    /// dropped.
    pub title: Option<&'static str>,
    /// Carrier for `provider`.
    pub provider: Option<&'static str>,
    /// Carrier for `language`.
    pub language: Option<&'static str>,
    /// Carrier for `service_id`.
    pub service_id: Option<&'static str>,
    /// Carrier for `description`.
    pub description: Option<&'static str>,
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
        /// Operator pin for this output's **encode** stage to a stable GPU
        /// ([`DevicePin`], ADR-0018 §2.1). Absent ⇒ auto-placed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection (program bus vs explicit tracks). Absent
        /// ⇒ the engine's default (the mixed program bus only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Segment duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_ms: Option<u32>,
        /// Operator pin for this output's **encode** stage to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. Absent ⇒ the mixed program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
    },
    /// RIST push — the open-standard sibling of the SRT push (ADR-0095). A
    /// `PushProtocol::Rist` (`mpegts` muxer) consuming the **same** encoded
    /// packets as every other push sink (invariant #7); the typed [`RistOptions`]
    /// lower to the `rist://…?…` `AVIO` URL the libav muxer opens.
    Rist {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Destination URL (`rist://host:port`, IPv6-first bracketed).
        url: String,
        /// Video codec.
        codec: String,
        /// Operator pin for this output's **encode** stage to a stable GPU.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. Absent ⇒ the mixed program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
        /// Optional typed RIST options (profile, buffer, pkt size, PSK
        /// encryption, bonding peers).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rist: Option<RistOptions>,
    },
    /// WebRTC program output: serves the program to N browser viewers over
    /// WHEP (ADR-0049).
    ///
    /// **Never encodes video** (invariant #7): it is a fan-out consumer of an
    /// already-encoded H.264 program rendition — per-viewer marginal cost is
    /// packetization only. The WHEP endpoint URL is derived from the output's
    /// stable id: `POST /api/v1/whep/{output_id}`. Audio is **single-track**
    /// (one Opus m-line): multitrack selections are rejected at config time
    /// and degrade explicitly to the mixed program bus, never silently.
    Webrtc {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Display name (like `Aes67`, a WebRTC output carries an explicit
        /// label — there is no mount/path/url to derive one from).
        label: String,
        /// Maximum concurrent WHEP viewer sessions on this output (must be
        /// `>= 1`; default `8`). Viewers beyond this — or beyond the
        /// endpoint-global `webrtc.max_sessions` pool (ADR-0048 §8) — receive
        /// `503` + `Retry-After`.
        #[serde(
            default = "default_webrtc_max_viewers",
            skip_serializing_if = "is_default_webrtc_max_viewers"
        )]
        max_viewers: u32,
        /// Optional per-output bearer token (RFC 6750) a viewer presents on
        /// the WHEP `POST`. `None` ⇒ viewing requires a control-plane API key
        /// with **View** scope — never anonymous. When set it must be
        /// non-empty; plaintext in v1, following the existing config-secret
        /// posture (like the stream keys embedded in `rtmp`/`srt` URLs).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// The **program rendition to consume** — not an encode to spawn
        /// (ADR-0049). `"h264"` is the only v1 value (default; enforced by
        /// `Output::validate`). ADR-0049 additionally requires the consumed
        /// rendition to carry B-frames off + repeat-headers on — enforced
        /// where the encoder settings live (the cli rendition builder), since
        /// this schema has no rendition-settings surface to check.
        #[serde(
            default = "default_webrtc_codec",
            skip_serializing_if = "is_default_webrtc_codec"
        )]
        codec: String,
        /// Operator pin for the **encode stage of the rendition this output
        /// consumes** to a stable GPU ([`DevicePin`], ADR-0018 §2.1). Absent ⇒
        /// auto-placed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. WebRTC carries **one** Opus m-line
        /// (single-track, [`Output::audio_capability`]); absent ⇒ the mixed
        /// program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
    },
    /// WHIP push (RFC 9725 **client**): publishes the program to a remote WHIP
    /// endpoint — the WebRTC sibling of the `rtmp`/`srt` push variants
    /// (ADR-0049).
    ///
    /// **Never encodes video** (invariant #7): it fans the same encoded
    /// program rendition packets to the remote origin, supervised with backoff
    /// reconnect exactly like the RTMP/SRT push clients. Audio is
    /// **single-track** (one Opus m-line).
    WhipPush {
        /// Stable operator id (ADR-0034 / RT-12). Absent ⇒ derived from
        /// [`Output::label`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The remote WHIP endpoint URL. Must be `http(s)`, with **https
        /// recommended** (the client follows 307/308 redirects https-only and
        /// aborts on a plaintext downgrade — ADR-0049). IPv6-first
        /// (ADR-0042): bracket IPv6 literals, e.g.
        /// `https://[2001:db8::15]:8443/whip/pgm1`.
        url: String,
        /// Optional bearer token (RFC 6750) sent on the WHIP `POST`. When set
        /// it must be non-empty; plaintext in v1 (the config-secret posture of
        /// `rtmp`/`srt` url-embedded keys).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// The **program rendition to consume** — not an encode to spawn
        /// (ADR-0049). `"h264"` is the only v1 value (default).
        #[serde(
            default = "default_webrtc_codec",
            skip_serializing_if = "is_default_webrtc_codec"
        )]
        codec: String,
        /// Operator pin for the **encode stage of the rendition this output
        /// consumes** to a stable GPU ([`DevicePin`], ADR-0018 §2.1). Absent ⇒
        /// auto-placed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gpu_pin: Option<DevicePin>,
        /// Per-output audio selection. WHIP push carries **one** Opus m-line
        /// (single-track, [`Output::audio_capability`]); absent ⇒ the mixed
        /// program bus only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio: Option<OutputAudio>,
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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
        /// Per-output declarative metadata intent (ADR-0088): service/program
        /// title, provider, language, service id, description, and timed-metadata
        /// opt-ins. Projected onto whatever this transport can carry
        /// ([`Output::metadata_plan`]); unsupported fields surface as a visible
        /// `Dropped`, never a silent no-op. Absent ⇒ the engine's honest defaults.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<OutputMetadata>,
        /// Per-output presentation orientation (ADR-0089): a quarter-turn + flip,
        /// applied as a zero-cost display-rotation tag or by rotating the pixels
        /// into a distinct rendition. Reuses core `QuarterTurn`. Absent ⇒ no
        /// reorientation. `mode = "tag"` is rejected on transports with no
        /// rotation tag (MPEG-TS/RTSP/NDI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        orientation: Option<OutputOrientation>,
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

/// Default `max_viewers` for a [`Output::Webrtc`] WHEP output (ADR-0049): 8.
const fn default_webrtc_max_viewers() -> u32 {
    8
}

/// Skip-serializing predicate for the default WHEP `max_viewers`.
// serde's `skip_serializing_if` contract calls the predicate with the field by
// reference; the derive fixes the signature, so the by-value shape the lint
// asks for cannot be used here.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_default_webrtc_max_viewers(viewers: &u32) -> bool {
    *viewers == 8
}

/// Default codec for the WebRTC output kinds (ADR-0049): the H.264 program
/// rendition — the only v1 value.
fn default_webrtc_codec() -> String {
    "h264".to_owned()
}

/// Skip-serializing predicate for the default WebRTC codec (`"h264"`).
fn is_default_webrtc_codec(codec: &str) -> bool {
    codec == "h264"
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
            | Output::Rist { id, .. }
            | Output::Webrtc { id, .. }
            | Output::WhipPush { id, .. }
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
            | Output::Rist { gpu_pin, .. }
            // The WebRTC kinds never encode (invariant #7): their pin targets
            // the encode stage of the rendition they consume (ADR-0049).
            | Output::Webrtc { gpu_pin, .. }
            | Output::WhipPush { gpu_pin, .. }
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
            | Output::Rist { audio, .. }
            | Output::Webrtc { audio, .. }
            | Output::WhipPush { audio, .. }
            | Output::Aes67 { audio, .. }
            | Output::Display { audio, .. } => audio.as_ref(),
        }
    }

    /// The per-output **metadata intent** ([`OutputMetadata`], ADR-0088), if
    /// any. `None` ⇒ the engine's honest defaults (a derived title, the canvas
    /// language/color tags).
    #[must_use]
    pub const fn metadata(&self) -> Option<&OutputMetadata> {
        match self {
            Output::RtspServer { metadata, .. }
            | Output::LlHls { metadata, .. }
            | Output::Hls { metadata, .. }
            | Output::Ndi { metadata, .. }
            | Output::Rtmp { metadata, .. }
            | Output::Srt { metadata, .. }
            | Output::Rist { metadata, .. }
            | Output::Webrtc { metadata, .. }
            | Output::WhipPush { metadata, .. }
            | Output::Aes67 { metadata, .. }
            | Output::Display { metadata, .. } => metadata.as_ref(),
        }
    }

    /// The per-output **presentation orientation** ([`OutputOrientation`],
    /// ADR-0089), if any. `None` ⇒ no reorientation (the canvas as composited).
    #[must_use]
    pub const fn orientation(&self) -> Option<&OutputOrientation> {
        match self {
            Output::RtspServer { orientation, .. }
            | Output::LlHls { orientation, .. }
            | Output::Hls { orientation, .. }
            | Output::Ndi { orientation, .. }
            | Output::Rtmp { orientation, .. }
            | Output::Srt { orientation, .. }
            | Output::Rist { orientation, .. }
            | Output::Webrtc { orientation, .. }
            | Output::WhipPush { orientation, .. }
            | Output::Aes67 { orientation, .. }
            | Output::Display { orientation, .. } => orientation.as_ref(),
        }
    }

    /// Whether this transport can carry a **display-rotation tag** (ADR-0089
    /// §2.1): the container/bitstream rotation matrix a tag-aware player honors.
    /// MP4/MOV/HLS-fMP4 carry one; MPEG-TS (TS-segment HLS, SRT, RIST), RTSP and
    /// NDI do not.
    ///
    /// Note the HLS subtlety (brief §2.1 / open question 3): an fMP4-segment HLS
    /// output *can* tag, a TS-segment one cannot. This conservatively reports
    /// `DisplayMatrix` for HLS (its default is fMP4 init segments); a TS-segment
    /// deployment validates `mode = "tag"` at the muxer.
    #[must_use]
    pub const fn orientation_tag_capability(&self) -> OrientationTagCapability {
        match self {
            // MP4/MOV file via RTMP-less file paths is not a distinct variant
            // here; the tag-bearing network/file transports are the HLS family
            // (fMP4 init segment) and a future file sink. HLS/LL-HLS carry the
            // display matrix in the init segment.
            Output::Hls { .. } | Output::LlHls { .. } => OrientationTagCapability::DisplayMatrix,
            // RTMP/FLV `onMetaData` has no rotation matrix; SRT and RIST carry
            // MPEG-TS (no tag); RTSP carries only SDP; NDI has no rotation tag;
            // AES67 is audio-only; a display head scans out raw pixels (rotation
            // is a KMS transform, not a container tag). All ⇒ pixels-only.
            Output::RtspServer { .. }
            | Output::Ndi { .. }
            | Output::Rtmp { .. }
            | Output::Srt { .. }
            | Output::Rist { .. }
            | Output::Webrtc { .. }
            | Output::WhipPush { .. }
            | Output::Aes67 { .. }
            | Output::Display { .. } => OrientationTagCapability::None,
        }
    }

    /// This output's **metadata carrier capability** (ADR-0088 §1.2): which
    /// named [`OutputMetadata`] fields this transport can express, as a typed
    /// matrix. Intersected with the requested intent by [`metadata_plan`].
    ///
    /// [`metadata_plan`]: Output::metadata_plan
    #[must_use]
    pub const fn metadata_capability(&self) -> MetadataCapability {
        match self {
            // MPEG-TS family (SRT-as-TS, RIST-as-TS): SDT service_name/provider,
            // PMT per-ES language, PAT program number, SDT free-text.
            Output::Srt { .. } | Output::Rist { .. } => MetadataCapability {
                title: Some("TS SDT service_name"),
                provider: Some("TS SDT provider_name"),
                language: Some("TS PMT ISO-639 language descriptor"),
                service_id: Some("TS PAT/PMT program number"),
                description: Some("TS SDT free-text descriptor"),
            },
            // HLS/LL-HLS: per-track language (fMP4 init / TS PMT); title rides
            // the init-segment/SDT; no DVB service id; description as a tag.
            Output::Hls { .. } | Output::LlHls { .. } => MetadataCapability {
                title: Some("HLS init-segment/SDT title"),
                provider: None,
                language: Some("HLS per-track language"),
                service_id: None,
                description: Some("HLS container comment"),
            },
            // RTSP: only SDP session-level `s=` (title) / `i=` (description).
            Output::RtspServer { .. } => MetadataCapability {
                title: Some("RTSP SDP s="),
                provider: None,
                language: None,
                service_id: None,
                description: Some("RTSP SDP i="),
            },
            // RTMP/FLV onMetaData: endpoint-read title/extras; no language /
            // provider / service-id key.
            Output::Rtmp { .. } => MetadataCapability {
                title: Some("RTMP onMetaData title"),
                provider: None,
                language: None,
                service_id: None,
                description: Some("RTMP onMetaData description"),
            },
            // NDI: sender name only (runtime-loaded SDK, nominative; ADR-0088
            // defers deeper NDI metadata to the NDI owner). Title → sender name.
            Output::Ndi { .. } => MetadataCapability {
                title: Some("NDI sender name"),
                provider: None,
                language: None,
                service_id: None,
                description: None,
            },
            // WebRTC (WHEP serve / WHIP push): SDP session-level only.
            Output::Webrtc { .. } | Output::WhipPush { .. } => MetadataCapability {
                title: Some("WebRTC SDP s="),
                provider: None,
                language: None,
                service_id: None,
                description: Some("WebRTC SDP i="),
            },
            // AES67 / ST 2110-30: SDP session-level name/info.
            Output::Aes67 { .. } => MetadataCapability {
                title: Some("AES67 SDP s="),
                provider: None,
                language: None,
                service_id: None,
                description: Some("AES67 SDP i="),
            },
            // Local display head: no container, no metadata carrier.
            Output::Display { .. } => MetadataCapability {
                title: None,
                provider: None,
                language: None,
                service_id: None,
                description: None,
            },
        }
    }

    /// Project this output's [`OutputMetadata`] intent onto its transport
    /// capability (ADR-0088 §1.3), producing an [`OutputMetadataPlan`]: per
    /// requested field, `Applied(target)` or `Dropped(reason)`. Returns the
    /// empty plan when no metadata is authored.
    #[must_use]
    pub fn metadata_plan(&self) -> OutputMetadataPlan {
        let Some(meta) = self.metadata() else {
            return OutputMetadataPlan::default();
        };
        let cap = self.metadata_capability();
        let project = |present: bool, carrier: Option<&'static str>, reason: &'static str| {
            if !present {
                return None;
            }
            Some(match carrier {
                Some(target) => MetadataField::Applied { target },
                None => MetadataField::Dropped { reason },
            })
        };
        OutputMetadataPlan {
            title: project(
                meta.title.is_some(),
                cap.title,
                "this transport carries no title/service-name field",
            ),
            provider: project(
                meta.provider.is_some(),
                cap.provider,
                "this transport carries no provider field (only the MPEG-TS SDT does)",
            ),
            language: project(
                meta.language.is_some(),
                cap.language,
                "this transport carries no per-track language field",
            ),
            service_id: project(
                meta.service_id.is_some(),
                cap.service_id,
                "this transport carries no DVB/TS service id (only the MPEG-TS family does)",
            ),
            description: project(
                meta.description.is_some(),
                cap.description,
                "this transport carries no description/comment field",
            ),
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
            // RTSP: N simultaneous `m=audio` subsessions. SRT and RIST both carry
            // an MPEG-TS payload ⇒ N PIDs, also simultaneous (the
            // receiver-dependent first-PID-only behaviour is a delivery caveat,
            // not a config-time capacity cap).
            Output::RtspServer { .. } | Output::Srt { .. } | Output::Rist { .. } => {
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
            // WebRTC (WHEP serve and WHIP push): one Opus m-line per session —
            // single-track (ADR-0049). A multitrack selection is rejected at
            // config time and degrades explicitly to the mixed program bus.
            Output::Webrtc { .. } | Output::WhipPush { .. } => {
                OutputAudioCapability::new(TrackDelivery::Simultaneous, TrackCapacity::AtMost(1))
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
            Output::Rist { url, .. } => format!("rist {url}"),
            Output::WhipPush { url, .. } => format!("whip_push {url}"),
            // WebRTC (WHEP serve) and AES67 carry an explicit operator label
            // (they have no mount/path/url to derive one from); use it verbatim.
            Output::Webrtc { label, .. } | Output::Aes67 { label, .. } => label.clone(),
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
