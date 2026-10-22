//! Subscription topics — the coarse, group-level routing keys.
//!
//! Per the realtime-api brief (§3) topics are deliberately **coarse**: a small
//! fixed set of group-level subscription units, each carrying many fine-grained
//! event types (`Event` variants). Fine scoping is done with an `id` filter,
//! not by adding more topics. The `$control` pseudo-topic carries the control
//! frames ([`crate::event::Event`] control variants) in both directions.
use serde::{Deserialize, Serialize};

/// A subscription routing key (the envelope `topic` field).
///
/// The wire form is the lowercase dotted string in each `#[serde(rename)]`;
/// `$control` is the reserved pseudo-topic for control frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Topic {
    /// Control frames (`$hello`, `$subscribe`, `$snapshot`, `$resume`, …).
    #[serde(rename = "$control")]
    Control,
    /// Process-level health, GPU events, degradation steps.
    #[serde(rename = "system")]
    System,
    /// Backend/codec capability matrix.
    #[serde(rename = "capabilities")]
    Capabilities,
    /// Input source add/remove, connection, format, supervision, errors.
    #[serde(rename = "inputs")]
    Inputs,
    /// Tile state machine + fps + binding changes.
    #[serde(rename = "tiles")]
    Tiles,
    /// Output sink status, bitrate, client counts, validity probes.
    #[serde(rename = "outputs")]
    Outputs,
    /// High-rate per-input/track audio meters (conflated/sampled).
    #[serde(rename = "audio.meters")]
    AudioMeters,
    /// EBU R128 loudness (M/S/I/LRA/dBTP).
    #[serde(rename = "audio.loudness")]
    AudioLoudness,
    /// Operator alerts (raised/cleared/updated).
    #[serde(rename = "alerts")]
    Alerts,
    /// Content-aware monitoring alarms (X.733): raised/cleared/updated/acked,
    /// carrying [`multiview_core::alarm::AlarmRecord`].
    #[serde(rename = "alarms")]
    Alarms,
    /// Tally lamp + UMD state and salvo arm/take lifecycle.
    #[serde(rename = "tally")]
    Tally,
    /// Layout / `DrawQuad` changes and `Preview`->`Program` transitions.
    #[serde(rename = "layout")]
    Layout,
    /// Config apply/validate/reject.
    #[serde(rename = "config")]
    Config,
    /// Structured log tail.
    #[serde(rename = "logs")]
    Logs,
    /// Long-running REST command job lifecycle (correlated by `corr`).
    #[serde(rename = "jobs")]
    Jobs,
    /// WHEP preview signaling (offer/answer/ICE/closed).
    #[serde(rename = "preview")]
    Preview,
    /// Managed-device registry + telemetry (ADR-RT007): the conflated
    /// `device.status` / `timing.status` lanes plus the lossless low-rate
    /// device lifecycle events, scoped finer with the `ids` filter.
    #[serde(rename = "devices")]
    Devices,
    /// Broadcast-switcher lifecycle (ADR-RT008): the lossless, operator-meaningful
    /// transition events (M/E transition + FTB + keyer state, **media-player
    /// transport state**, and macro progress), scoped finer with the envelope `id`
    /// (an M/E, keyer, player, or macro id). Deliberately coarse: every switcher
    /// element rides this one topic and a client filters by `id`. NOT high-rate —
    /// its events stay in the lossless replay ring (ADR-RT003).
    #[serde(rename = "switcher")]
    Switcher,
}

impl Topic {
    /// The wire string for this topic (matches the `#[serde(rename)]`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "$control",
            Self::System => "system",
            Self::Capabilities => "capabilities",
            Self::Inputs => "inputs",
            Self::Tiles => "tiles",
            Self::Outputs => "outputs",
            Self::AudioMeters => "audio.meters",
            Self::AudioLoudness => "audio.loudness",
            Self::Alerts => "alerts",
            Self::Alarms => "alarms",
            Self::Tally => "tally",
            Self::Layout => "layout",
            Self::Config => "config",
            Self::Logs => "logs",
            Self::Jobs => "jobs",
            Self::Preview => "preview",
            Self::Devices => "devices",
            Self::Switcher => "switcher",
        }
    }

    /// Whether this is the reserved `$control` pseudo-topic.
    #[must_use]
    pub const fn is_control(self) -> bool {
        matches!(self, Self::Control)
    }

    /// Whether this topic is a **high-rate** lane that is conflated/sampled and
    /// excluded from the lossless replay ring (ADR-RT003): `audio.meters`,
    /// `audio.loudness`, and `system` (cpu/gpu/encoder telemetry).
    ///
    /// High-rate lanes are latest-only and re-snapshotable, so they must not be
    /// kept in the bounded replay ring; the engine never blocks on a slow client
    /// (inv #10) — a lagging UI simply skips samples, it never polls them.
    ///
    /// [`Topic::Devices`] is deliberately **not** in this set: it is the one
    /// mixed-cadence topic (ADR-RT007), whose lossless lifecycle events must
    /// stay in the replay ring while its conflated `device.status` /
    /// `timing.status` lanes are excluded **per event type** via
    /// [`crate::event::Event::is_conflated`]. The full ring-exclusion rule the
    /// session pump must apply once the Devices producers land (DEV-A3) is
    /// `topic.is_high_rate() || event.is_conflated()`; no producer emits
    /// Devices events yet.
    #[must_use]
    pub const fn is_high_rate(self) -> bool {
        matches!(self, Self::AudioMeters | Self::AudioLoudness | Self::System)
    }
}
