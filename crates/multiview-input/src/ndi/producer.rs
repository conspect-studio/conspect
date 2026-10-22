//! [`NdiProducer`]: an NDI receive source as a
//! [`FrameProducer`](crate::source::FrameProducer).
//!
//! This is the IN-3 bridge, the NDI peer of the IN-2 `St2110Producer`: it samples
//! an [`NdiReceiver`] (non-blocking), converts each received UYVY/BGRA host frame
//! into an NV12 [`ProducedFrame`], and hands it to the
//! [`IngestPump`](crate::source::IngestPump) — which normalises the NDI 100 ns
//! timecode onto the internal nanosecond timeline and publishes into the per-tile
//! last-good store (invariants #1 / #2 / #3). The producer **only pulls**; it
//! never paces or blocks the output clock and never back-pressures the engine
//! (invariant #10). A receive with no frame this instant is surfaced as
//! `Ok(None)` (the tile holds its last-good frame); a receive fault is surfaced as
//! an [`Error`](crate::Error) the supervisor reconnects from.

use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};

use crate::error::Result;
use crate::normalize::WrapBits;
use crate::source::{FrameProducer, ProducedFrame};

use super::convert::{bgra_to_nv12, uyvy_to_nv12, ReceivedVideoFrame};
use super::license::{LicenseAcceptance, NdiLicense, NdiLicenseError};
use super::receiver::{NdiReceiver, NdiRecvFourCc, ReceivedFrame};

/// NDI's timecode media clock: a 64-bit monotonic value in **100 ns units**, so
/// the producer's timebase is `1 / 10_000_000` seconds per tick. NDI timecodes do
/// not wrap at a fixed width within a session, so [`WrapBits::None`] is reported
/// and the normalizer treats the (already monotonic) value as a continuous
/// timeline (invariant #3).
const NDI_TIMECODE_HZ: i64 = 10_000_000;

/// The genpts fallback cadence when a received frame carries no timecode. NDI
/// frames generally carry a real timecode; this only matters for a synthesise
/// sentinel. 30 fps is a neutral default (exact-rational, never float fps).
const NDI_FALLBACK_FPS: i64 = 30;

/// An NDI receive source presented as a [`FrameProducer`].
///
/// Owns a boxed [`NdiReceiver`] (the real SDK-backed receiver behind the live-only
/// path, or a [`FakeNdiReceiver`](super::receiver::FakeNdiReceiver) in tests). Each
/// [`FrameProducer::next_frame`] samples the receiver once and converts a ready
/// frame to NV12; a quiet sample (no frame this instant) returns `Ok(None)`.
pub struct NdiProducer {
    /// The accepted NDI license guard (ADR-0008 §7.5). Held to enforce acceptance
    /// by construction and to expose the audit record (who/when) for export — the
    /// ingest mirror of `multiview_output`'s `NdiOutput`.
    license: NdiLicense,
    receiver: Box<dyn NdiReceiver + Send>,
}

impl NdiProducer {
    /// Build a producer over an application-supplied [`NdiReceiver`], gated by an
    /// accepted [`NdiLicense`].
    ///
    /// Mirrors `multiview_output`'s `NdiOutput::new`: the only way to construct an
    /// NDI source is with an accepted license, so — ADR-0008 §7.5 — a source
    /// cannot start receiving without acceptance. Obtain the [`NdiLicense`] via
    /// [`NdiProducer::start`] (from the `[system.ndi] accept_license` setting) or
    /// [`NdiLicense::accept`].
    #[must_use]
    pub fn new(license: NdiLicense, receiver: Box<dyn NdiReceiver + Send>) -> Self {
        Self { license, receiver }
    }

    /// Evaluate the `[system.ndi] accept_license` setting + its audit record and,
    /// when accepted, build the gated producer — the single decision point an NDI
    /// source start flows through.
    ///
    /// When the operator has not accepted the NDI SDK license (or the audit record
    /// is incomplete), returns the typed [`NdiLicenseError`] refusal and **no
    /// producer is constructed**, so the receive seam is never sampled and no
    /// frames flow (the tile degrades; the output clock is untouched).
    ///
    /// # Errors
    /// [`NdiLicenseError::NotAccepted`] when `accept_license` is `false`;
    /// [`NdiLicenseError::IncompleteAcceptance`] when accepted but the audit
    /// fields (who/when) are blank.
    pub fn start(
        accept_license: bool,
        acceptance: LicenseAcceptance,
        receiver: Box<dyn NdiReceiver + Send>,
    ) -> core::result::Result<Self, NdiLicenseError> {
        // NOTE: the crate-local `Result` alias fixes `E = crate::Error`; this gate
        // returns the typed `NdiLicenseError`, so spell out the std `Result`.
        let license = NdiLicense::from_setting(accept_license, acceptance)?;
        Ok(Self::new(license, receiver))
    }

    /// The accepted license guard, exposing the audit record (who/when) for the
    /// audit log / config export.
    #[must_use]
    pub fn license(&self) -> &NdiLicense {
        &self.license
    }

    /// Convert a received video frame into an NV12 [`ProducedFrame`], mapping the
    /// `FourCC` to the matching conversion. Returns the producer's
    /// [`Error::NdiConvert`](crate::Error) (a reconnectable fault) on an
    /// unsupported layout or a malformed frame — never a panic.
    fn to_produced(frame: &ReceivedVideoFrame) -> Result<ProducedFrame> {
        let nv12 = match frame.fourcc() {
            NdiRecvFourCc::Uyvy => uyvy_to_nv12(frame),
            NdiRecvFourCc::Bgra => bgra_to_nv12(frame),
        }?;

        let meta = FrameMeta {
            // The pump overwrites this with the normalized instant; a placeholder
            // here (invariants #1/#3).
            pts: MediaTime::ZERO,
            width: nv12.width(),
            height: nv12.height(),
            format: PixelFormat::Nv12,
            color: nv12.color(),
        };
        Ok(ProducedFrame {
            pixels: nv12.into_bytes(),
            // The NDI 100 ns timecode is the raw PTS; the pump rebases it via the
            // normalizer ([`WrapBits::None`]) onto the ns timeline. A frame with no
            // timecode yields `None` (genpts fallback).
            raw_pts: frame.timecode_100ns(),
            discontinuity: false,
            meta,
        })
    }
}

impl core::fmt::Debug for NdiProducer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NdiProducer").finish_non_exhaustive()
    }
}

impl FrameProducer for NdiProducer {
    fn next_frame(&mut self) -> Result<Option<ProducedFrame>> {
        // A single non-blocking sample. A ready video frame is converted and
        // returned; a quiet sample (no frame this instant) is `Ok(None)` — the
        // pump re-polls on the next tick and the tile holds its last-good frame
        // (invariants #1/#2). A receive fault is an `Err` the supervisor
        // reconnects from. The producer never blocks or paces the output clock.
        match self.receiver.receive()? {
            ReceivedFrame::Video(frame) => Self::to_produced(&frame).map(Some),
            ReceivedFrame::None => Ok(None),
        }
    }

    fn timebase(&self) -> Rational {
        // NDI timecodes ride a 100 ns (10 MHz) media clock.
        Rational::new(1, NDI_TIMECODE_HZ)
    }

    fn cadence(&self) -> Rational {
        // No cadence is carried per frame; the genpts fallback only applies to a
        // timecode-less frame (rare for NDI). Neutral 30 fps, exact rational.
        Rational::new(NDI_FALLBACK_FPS, 1)
    }

    fn wrap_bits(&self) -> WrapBits {
        // NDI timecodes are a continuous 64-bit monotonic value (no fixed-width
        // wrap within a session).
        WrapBits::None
    }
}

/// The default colour tag for an NDI-received NV12 frame (BT.709 limited); the
/// compositor re-tags/converts per tile (invariant #8). Exposed for the live
/// receiver binding to reuse the same tag the converter applies.
#[must_use]
pub fn default_ndi_color() -> ColorInfo {
    ColorInfo::default()
}
