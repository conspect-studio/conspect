//! The real ALSA PCM + ELD readers (feature `display-kms`, DEV-B4 /
//! display-out §5).
//!
//! This is the **hardware-only** leg of the display-audio sink: everything that
//! talks to libasound or reads `/proc` lives here, behind the off-by-default
//! `display-kms` feature, so CI (which has neither) exercises the pure
//! servo/ELD/xrun/FIFO/sink-seam over the mocks in [`super`]. The leg is
//! validated on the t630 / Raspberry Pi node targets after merge.
//!
//! - [`ProcEldSource`] implements [`EldSource`](super::EldSource) by reading
//!   the x86 HDA driver's **textual** dump at `/proc/asound/cardN/eld#C.P` and
//!   handing it to the pure [`parse_proc_eld_text`](super::parse_proc_eld_text).
//!   A missing/empty/`eld_valid 0` file (EDID-less head, or the pipe not lit)
//!   yields [`None`] — no audio path — never an error that stops the run.
//! - [`CtlEldSource`] implements the same seam for **ASoC/hdmi-codec** cards
//!   (the Pi's vc4-hdmi), which expose no proc file: the ELD is a binary
//!   `"ELD"` byte control element on the PCM iface, parsed by the pure
//!   [`parse_eld`](super::parse_eld).
//! - [`AlsaPcmSink`] implements [`AlsaSink`](super::AlsaSink) over the SAFE
//!   `alsa` crate (libasound is dynamically linked, LGPL-2.1, never vendored).
//!   It opens the **`hdmi:CARD=…,DEV=…`** PCM (the `hdmi:` config layer sets
//!   the IEC958/AES channel-status bits and, on the Pi, applies the vc4-hdmi
//!   alsa-lib card config that a raw `hw:` device does not) in **nonblocking
//!   mode**, so no device call can wedge the drain thread indefinitely; a
//!   raw-`hw:` fallback covers cards without an `hdmi:` config entry. The
//!   sample format is **negotiated** (float → S32 → S24 → S16, via
//!   `snd_pcm_hw_params_test_format` and the pure
//!   [`negotiate_sample_format`](super::negotiate_sample_format)) because real
//!   HDA/vc4 devices typically refuse float; integer formats get a
//!   sample-accurate float→int conversion at the write boundary. Writes go
//!   through the typed `snd_pcm_writei`, paced by bounded `snd_pcm_wait`
//!   slices when the ring is full (`-EAGAIN`) and re-offering short-write
//!   tails up to a hard per-call bound; an underrun/suspend is mapped onto
//!   [`PcmOutcome`](super::PcmOutcome) for the recovery machine.
//! - [`discover_for_connector`] applies the pure
//!   [`discover`](super::discover) policy to the live system: vc4 card-per-port
//!   first, then the HDA `eld#D.P` scan (the entry ordinal doubling as the
//!   `hdmi:` `DEV=` index — the same pin-ordering assumption alsa-lib makes).
//!
//! This module writes **no `unsafe`** — the `alsa` crate owns all FFI — so
//! `multiview-output` stays `forbid(unsafe_code)`.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alsa::ctl::{Ctl, ElemId, ElemIface, ElemType, ElemValue};
use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};

use super::discover::{pick_eld_entry, vc4_card_candidates};
use super::eld::{parse_eld, parse_proc_eld_text, EldCapability};
use super::pcm_format::{
    f32_to_s16, f32_to_s24, f32_to_s32, negotiate_sample_format, PcmSampleFormat,
};
use super::sink::{AlsaSink, EldSource, PcmParams};
use super::xrun::PcmOutcome;

/// Reads a connector's ELD from the x86 HDA textual proc dump
/// (`/proc/asound/cardN/eld#C.P`).
///
/// The kernel publishes the parsed EDID audio block at that path while the
/// pipe is lit. The reader re-reads on each [`read_capability`] call so a
/// hotplug that lights (or drops) the audio path is picked up.
///
/// [`read_capability`]: super::EldSource::read_capability
#[derive(Debug, Clone)]
pub struct ProcEldSource {
    path: PathBuf,
}

impl ProcEldSource {
    /// Build a reader from an explicit ELD proc path (as found by
    /// [`discover_for_connector`], or operator diagnostics).
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl EldSource for ProcEldSource {
    fn read_capability(&mut self) -> Option<EldCapability> {
        let text = fs::read_to_string(&self.path).ok()?;
        parse_proc_eld_text(&text)
    }
}

/// Reads a connector's ELD from the ASoC/hdmi-codec **binary** `"ELD"` byte
/// control element (the Pi vc4-hdmi shape — those cards expose no proc dump).
///
/// The element lives on the card's PCM iface, device `pcm_device`; while the
/// pipe is dark the codec publishes an all-zero/empty blob, which the pure
/// [`parse_eld`](super::parse_eld) maps to [`None`] (no audio path).
#[derive(Debug, Clone)]
pub struct CtlEldSource {
    /// The ALSA ctl name (`hw:vc4hdmi0`).
    ctl: String,
    /// The PCM device the ELD element is attached to (0 on vc4).
    pcm_device: u32,
}

impl CtlEldSource {
    /// Build a reader for `card_id`'s ELD element on PCM device `pcm_device`.
    #[must_use]
    pub fn new(card_id: &str, pcm_device: u32) -> Self {
        Self {
            ctl: format!("hw:{card_id}"),
            pcm_device,
        }
    }

    /// One read attempt; [`None`] for any failure (no card, no element, dark
    /// pipe) — the sink just keeps polling.
    fn read_once(&self) -> Option<EldCapability> {
        let ctl = Ctl::new(&self.ctl, false).ok()?;
        let name = CString::new("ELD").ok()?;
        let mut id = ElemId::new(ElemIface::PCM);
        id.set_device(self.pcm_device);
        id.set_name(&name);
        let mut value = ElemValue::new(ElemType::Bytes).ok()?;
        value.set_id(&id);
        ctl.elem_read(&mut value).ok()?;
        parse_eld(value.get_bytes()?)
    }
}

impl EldSource for CtlEldSource {
    fn read_capability(&mut self) -> Option<EldCapability> {
        self.read_once()
    }
}

/// A discovered ELD source: proc-text (x86 HDA) or binary ctl element
/// (ASoC/vc4), behind one [`EldSource`] face for the sink loop.
#[derive(Debug, Clone)]
pub enum DiscoveredEldSource {
    /// The x86 HDA textual proc dump.
    Proc(ProcEldSource),
    /// The ASoC/hdmi-codec binary control element.
    Ctl(CtlEldSource),
}

impl EldSource for DiscoveredEldSource {
    fn read_capability(&mut self) -> Option<EldCapability> {
        match self {
            Self::Proc(src) => src.read_capability(),
            Self::Ctl(src) => src.read_capability(),
        }
    }
}

/// The ALSA endpoints serving one KMS connector: where to watch the ELD and
/// which PCM to play into.
#[derive(Debug)]
pub struct DiscoveredDisplayAudio {
    /// The ELD gate for the connector.
    pub eld: DiscoveredEldSource,
    /// The PCM sink (`hdmi:CARD=…,DEV=…` with a raw-`hw:` fallback).
    pub pcm: AlsaPcmSink,
    /// The ALSA card id the pair lives on (diagnostics).
    pub card_id: String,
}

/// Find the ALSA card/device/ELD serving `connector`, applying the pure
/// [`discover`](super::discover) policy to the live `/proc/asound` tree:
///
/// 1. **vc4 (Pi)**: a card id mapped from the connector index
///    ([`vc4_card_candidates`]) → that card's `"ELD"` ctl element + its
///    `hdmi:CARD=…,DEV=0` PCM.
/// 2. **x86 HDA**: scan every card's `eld#D.P` proc entries and pick per
///    [`pick_eld_entry`] (first lit, else first — so a dark pipe keeps being
///    polled); the entry ordinal is used as the `hdmi:` `DEV=` index.
///
/// [`None`] when no candidate exists at all (no ALSA cards / no HDMI pins) —
/// the caller logs and runs the head video-only. Heuristic corners (several
/// simultaneously-lit pins on one card) are logged at the call site via
/// `card_id`; the mapping is hardware-validated on the t630/Pi legs.
#[must_use]
pub fn discover_for_connector(connector: &str) -> Option<DiscoveredDisplayAudio> {
    discover_in(Path::new("/proc/asound"), connector)
}

/// [`discover_for_connector`] over an explicit proc root (separated for
/// diagnostics and so the scan logic is exercisable against a fixture tree).
fn discover_in(proc_asound: &Path, connector: &str) -> Option<DiscoveredDisplayAudio> {
    // --- vc4: one ALSA card per HDMI port, ELD via the ctl element. ---
    for card_id in vc4_card_candidates(connector) {
        if proc_asound.join(&card_id).is_dir() {
            return Some(DiscoveredDisplayAudio {
                eld: DiscoveredEldSource::Ctl(CtlEldSource::new(&card_id, 0)),
                pcm: AlsaPcmSink::for_connector(&card_id, 0),
                card_id,
            });
        }
    }

    // --- HDA: scan card*/eld#* and pick the first lit (else first) entry. ---
    let mut cards: Vec<PathBuf> = fs::read_dir(proc_asound)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card") && n.len() > 4)
        })
        .collect();
    cards.sort();
    for card in cards {
        let mut elds: Vec<PathBuf> = fs::read_dir(&card)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("eld#"))
            })
            .collect();
        if elds.is_empty() {
            continue;
        }
        elds.sort();
        let parsed: Vec<Option<EldCapability>> = elds
            .iter()
            .map(|p| {
                fs::read_to_string(p)
                    .ok()
                    .and_then(|t| parse_proc_eld_text(&t))
            })
            .collect();
        let Some(index) = pick_eld_entry(&parsed) else {
            continue;
        };
        let eld_path = elds.get(index)?.clone();
        // The card id (`/proc/asound/cardN/id`) names the `hdmi:CARD=` PCM; the
        // chosen entry ordinal is the `DEV=` index (alsa-lib's pin ordering).
        let card_id = fs::read_to_string(card.join("id")).ok()?.trim().to_owned();
        let dev = u32::try_from(index).unwrap_or(0);
        return Some(DiscoveredDisplayAudio {
            eld: DiscoveredEldSource::Proc(ProcEldSource::from_path(eld_path)),
            pcm: AlsaPcmSink::for_connector(&card_id, dev),
            card_id,
        });
    }
    None
}

/// A libasound PCM playback sink for an HDMI/DP connector.
///
/// Prefers the `hdmi:CARD=…,DEV=…` ALSA PCM (the config layer that sets IEC958
/// channel-status and the vc4-hdmi card setup); a raw `hw:CARD=…,DEV=…` is the
/// fallback for cards without an `hdmi:` config entry. The PCM is opened
/// **nonblocking** with a negotiated sample format (float → S32 → S24 → S16);
/// writes convert at the boundary and pace themselves with bounded
/// `snd_pcm_wait` slices, so every call returns within [`WRITE_CALL_BOUND`].
#[derive(Debug)]
pub struct AlsaPcmSink {
    device: String,
    fallback_hw: Option<String>,
    pcm: Option<PCM>,
    channels: usize,
    /// The sample format negotiated at open (the typed `writei` + conversion
    /// the write path uses). `Float` until a successful open.
    format: PcmSampleFormat,
}

impl AlsaPcmSink {
    /// Build a sink for the `hdmi:` PCM of `card_name`/`dev`, with a `hw:`
    /// fallback. `card_name` is the ALSA card id (e.g. `vc4hdmi0` on the Pi or
    /// `PCH` on x86 HDA).
    #[must_use]
    pub fn for_connector(card_name: &str, dev: u32) -> Self {
        Self {
            device: format!("hdmi:CARD={card_name},DEV={dev}"),
            fallback_hw: Some(format!("hw:CARD={card_name},DEV={dev}")),
            pcm: None,
            channels: 2,
            format: PcmSampleFormat::Float,
        }
    }

    /// Build a sink for an explicit ALSA device string (diagnostics / override).
    #[must_use]
    pub fn for_device(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            fallback_hw: None,
            pcm: None,
            channels: 2,
            format: PcmSampleFormat::Float,
        }
    }

    /// Try to open `name` **nonblocking** and configure it for `params`,
    /// negotiating the sample format the device actually supports
    /// (float → S32 → S24 → S16 — real HDA/vc4 devices typically refuse
    /// float). Returns the configured PCM plus the negotiated format.
    fn try_open(name: &str, params: PcmParams) -> Result<(PCM, PcmSampleFormat), String> {
        // Nonblocking (MAJOR-2): a wedged driver can then never hold the drain
        // thread inside `writei`; pacing happens via bounded `snd_pcm_wait`
        // slices in `write`.
        let pcm =
            PCM::new(name, Direction::Playback, true).map_err(|e| format!("open {name}: {e}"))?;
        let format;
        {
            let hwp = HwParams::any(&pcm).map_err(|e| format!("hw_params {name}: {e}"))?;
            hwp.set_access(Access::RWInterleaved)
                .map_err(|e| format!("set_access: {e}"))?;
            format = negotiate_sample_format(|f| hwp.test_format(alsa_format(f)).is_ok())
                .ok_or_else(|| {
                    format!("no supported sample format on {name} (tried float/S32/S24/S16)")
                })?;
            hwp.set_format(alsa_format(format))
                .map_err(|e| format!("set_format {format:?}: {e}"))?;
            hwp.set_channels(u32::from(params.channels))
                .map_err(|e| format!("set_channels: {e}"))?;
            hwp.set_rate(params.sample_rate, ValueOr::Nearest)
                .map_err(|e| format!("set_rate: {e}"))?;
            hwp.set_period_size_near(
                alsa::pcm::Frames::from(i32::try_from(params.period_frames).unwrap_or(480)),
                ValueOr::Nearest,
            )
            .map_err(|e| format!("set_period_size: {e}"))?;
            hwp.set_periods(params.periods, ValueOr::Nearest)
                .map_err(|e| format!("set_periods: {e}"))?;
            pcm.hw_params(&hwp)
                .map_err(|e| format!("commit hw_params {name}: {e}"))?;
        }
        pcm.prepare().map_err(|e| format!("prepare {name}: {e}"))?;
        Ok((pcm, format))
    }

    /// Map a libasound write error onto a [`PcmOutcome`] for the recovery
    /// machine. `EPIPE` is an underrun, `ESTRPIPE` a suspend; anything else is
    /// surfaced as an underrun so the loop attempts a prepare (the conservative
    /// recover). `EAGAIN` (nonblocking ring-full) never reaches here — the
    /// write loop handles it with a bounded wait.
    fn classify(err: &alsa::Error) -> PcmOutcome {
        let errno = err.errno();
        if errno == EPIPE {
            PcmOutcome::Underrun
        } else if errno == ESTRPIPE {
            PcmOutcome::Suspended
        } else {
            PcmOutcome::Underrun
        }
    }

    /// One typed `writei` of the (already-converted) tail starting at sample
    /// index `from_sample`.
    fn writei_at(
        pcm: &PCM,
        format: PcmSampleFormat,
        interleaved: &[f32],
        i16buf: &[i16],
        i32buf: &[i32],
        from_sample: usize,
    ) -> Result<usize, alsa::Error> {
        match format {
            PcmSampleFormat::Float => pcm
                .io_f32()
                .and_then(|io| io.writei(interleaved.get(from_sample..).unwrap_or(&[]))),
            PcmSampleFormat::S32 => pcm
                .io_i32()
                .and_then(|io| io.writei(i32buf.get(from_sample..).unwrap_or(&[]))),
            PcmSampleFormat::S24 => pcm
                .io_i32_s24()
                .and_then(|io| io.writei(i32buf.get(from_sample..).unwrap_or(&[]))),
            PcmSampleFormat::S16 => pcm
                .io_i16()
                .and_then(|io| io.writei(i16buf.get(from_sample..).unwrap_or(&[]))),
        }
    }
}

impl AlsaSink for AlsaPcmSink {
    fn open(&mut self, params: PcmParams) -> Result<(), String> {
        self.channels = usize::from(params.channels.max(1));
        match Self::try_open(&self.device, params) {
            Ok((pcm, format)) => {
                self.pcm = Some(pcm);
                self.format = format;
                Ok(())
            }
            Err(primary) => {
                // The `hdmi:` config may be absent on this card; try the raw
                // `hw:` device before giving up (the sink then stays silent).
                if let Some(hw) = self.fallback_hw.clone() {
                    match Self::try_open(&hw, params) {
                        Ok((pcm, format)) => {
                            tracing::warn!(
                                device = %self.device,
                                fallback = %hw,
                                "hdmi: PCM unavailable; using raw hw: (no IEC958 config layer)"
                            );
                            self.pcm = Some(pcm);
                            self.format = format;
                            return Ok(());
                        }
                        Err(secondary) => {
                            return Err(format!("{primary}; fallback {secondary}"));
                        }
                    }
                }
                Err(primary)
            }
        }
    }

    fn write(&mut self, interleaved: &[f32], _channels: usize) -> PcmOutcome {
        let Some(pcm) = self.pcm.as_ref() else {
            return PcmOutcome::Underrun;
        };
        let channels = self.channels.max(1);
        let total_frames = interleaved.len() / channels;
        if total_frames == 0 {
            return PcmOutcome::Wrote(0);
        }
        // Convert once at the write boundary when the negotiated format is an
        // integer one (MEDIUM-3): sample-accurate float→int, then the typed
        // `writei`. A few KB per 10 ms quantum on the dedicated drain thread —
        // never the engine hot path.
        let (i16buf, i32buf): (Vec<i16>, Vec<i32>) = match self.format {
            PcmSampleFormat::Float => (Vec::new(), Vec::new()),
            PcmSampleFormat::S32 => (Vec::new(), f32_to_s32(interleaved)),
            PcmSampleFormat::S24 => (Vec::new(), f32_to_s24(interleaved)),
            PcmSampleFormat::S16 => (f32_to_s16(interleaved), Vec::new()),
        };

        // Nonblocking write loop, bounded by WRITE_CALL_BOUND: a full ring
        // (`-EAGAIN` / a zero-frame write) waits in WAIT_SLICE_MS slices for
        // space; a wedged device therefore stalls this call for at most the
        // bound — teardown stays responsive — and a short write re-offers the
        // tail so content is not dropped.
        let deadline = Instant::now() + WRITE_CALL_BOUND;
        let mut written_frames = 0usize;
        while written_frames < total_frames {
            let from_sample = written_frames.saturating_mul(channels);
            match Self::writei_at(pcm, self.format, interleaved, &i16buf, &i32buf, from_sample) {
                Ok(frames) => {
                    written_frames = written_frames.saturating_add(frames).min(total_frames);
                    if frames == 0 {
                        if Instant::now() >= deadline {
                            break;
                        }
                        let _ = pcm.wait(Some(WAIT_SLICE_MS));
                    }
                }
                Err(e) if e.errno() == EAGAIN => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    let _ = pcm.wait(Some(WAIT_SLICE_MS));
                }
                Err(e) => {
                    // Surface the fault only when nothing was delivered this
                    // call; otherwise report the partial write (the fault
                    // recurs on the next call and is classified there).
                    if written_frames == 0 {
                        return Self::classify(&e);
                    }
                    break;
                }
            }
        }
        PcmOutcome::Wrote(written_frames)
    }

    fn recover(&mut self) -> PcmOutcome {
        let Some(pcm) = self.pcm.as_ref() else {
            return PcmOutcome::RecoverFailed;
        };
        // A suspended PCM needs resume; an underrun needs prepare. `prepare`
        // covers both for our purposes (resume-then-prepare); a remaining error
        // means the device is gone.
        if pcm.state() == State::Suspended {
            // Best-effort resume; ignore EAGAIN spins by falling through to
            // prepare.
            let _ = pcm.resume();
        }
        match pcm.prepare() {
            Ok(()) => PcmOutcome::Recovered,
            Err(_) => PcmOutcome::RecoverFailed,
        }
    }

    fn close(&mut self) {
        // Dropping the PCM closes it; we do not drain (teardown).
        self.pcm = None;
    }

    fn delay_frames(&mut self) -> Option<i64> {
        // `snd_pcm_delay`: frames delivered but not yet audible — the skew
        // measurement's refinement term. `Frames` is the platform `c_long`
        // (i64 on the 64-bit Linux targets we ship display nodes on).
        let pcm = self.pcm.as_ref()?;
        pcm.delay().ok()
    }
}

/// Map the pure negotiation format onto the alsa-crate `Format`. `S24` is the
/// 24-bit-in-32-bit container (`S24_LE`), matching [`f32_to_s24`]'s
/// LSB-justified output and the `io_i32_s24` typed writer.
const fn alsa_format(format: PcmSampleFormat) -> Format {
    match format {
        PcmSampleFormat::Float => Format::float(),
        PcmSampleFormat::S32 => Format::s32(),
        PcmSampleFormat::S24 => Format::s24(),
        PcmSampleFormat::S16 => Format::s16(),
    }
}

/// `EPIPE` errno (an xrun/underrun on a playback PCM).
const EPIPE: i32 = 32;

/// `ESTRPIPE` errno (a suspended PCM stream awaiting resume).
const ESTRPIPE: i32 = 86;

/// `EAGAIN` errno (a nonblocking PCM whose ring is currently full).
const EAGAIN: i32 = 11;

/// The hard bound on one [`AlsaSink::write`] call: ample for a healthy device
/// to drain a 10 ms quantum (the ring holds ~40 ms), but a wedged driver can
/// hold the drain thread no longer than this — the stop flag is then seen and
/// teardown stays bounded.
const WRITE_CALL_BOUND: Duration = Duration::from_millis(250);

/// One `snd_pcm_wait` slice while the ring is full (about two periods at the
/// negotiated 480-frame period), so a stop request is honoured promptly.
const WAIT_SLICE_MS: u32 = 20;
