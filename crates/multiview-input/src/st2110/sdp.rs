//! AES67 / SMPTE **ST 2110-30** audio **SDP** parser and generator (pure).
//!
//! AES67 (and ST 2110-30, which constrains it) describes each PCM-audio RTP
//! stream with a Session Description Protocol document (RFC 4566 / its bis
//! RFC 8866) carrying a single `m=audio` media section. This module models the
//! audio-relevant subset of that document — the codec (`a=rtpmap` L16/L24, clock
//! rate, channels), the packet time (`a=ptime`), and the RFC 7273 media-clock /
//! reference-clock attributes (`a=mediaclk`, `a=ts-refclk`) that bind the stream
//! to a PTP grandmaster — into one [`AudioSdpSession`] value.
//!
//! It is **pure** text-in / text-out: [`AudioSdpSession::parse`] turns SDP lines
//! into a typed value and [`AudioSdpSession::generate`] turns one back into
//! well-formed lines. There is no network, no RTP, and no timestamp handling
//! here — those live in the (feature-gated) transport layer and the audio
//! producer. Parsing never panics on malformed input; every failure is a typed
//! [`SdpError`].
//!
//! ## Timing model (invariant #3)
//!
//! Packet time is carried as **integer thousandths of a millisecond**
//! ([`AudioSdpSession::ptime_ms_x1000`]) so the Class B `0.125 ms` (125 µs)
//! cadence round-trips exactly without ever materializing a float — consistent
//! with the crate-wide "never float fps / never float cadence" rule.
//!
//! ## IPv6-first (ADR-0042)
//!
//! AES67 sessions are multicast; the connection line is `c=IN IP6 <group>` with
//! an IPv6 SSM group (`FF3x::/32`). This parser ignores the connection line for
//! the value model (the multicast binding is carried in config / derived by the
//! transport), but the model never assumes or emits an IPv4-only form.

use super::v30::{Aes3Format, SampleDepth};

/// A parsed AES67 / ST 2110-30 audio SDP session: the codec, channel count,
/// clock rate, packet time, and PTP reference-clock binding for one `m=audio`
/// media section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSdpSession {
    /// The RTP/UDP destination port from the `m=audio` line.
    pub port: u16,
    /// The dynamic RTP payload type (typically `96..=127`).
    pub payload_type: u8,
    /// The PCM format (channel count + L16/L24 depth) resolved from `a=rtpmap`.
    pub format: Aes3Format,
    /// The RTP media clock rate in Hz (`48000` or `96000`).
    pub clock_rate: u32,
    /// Packet time in **thousandths of a millisecond** (`a=ptime`). Class A is
    /// `1 ms` (`1000`); Class B is `0.125 ms` (`125`). Integer to avoid floats.
    pub ptime_ms_x1000: u32,
    /// The RFC 7273 reference-clock locator (`a=ts-refclk`).
    pub ts_refclk: TsRefclk,
    /// The RFC 7273 media-clock direct offset in samples (`a=mediaclk:direct`).
    pub mediaclk_offset: u32,
}

/// The RFC 7273 reference-clock locator from `a=ts-refclk`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TsRefclk {
    /// IEEE 1588-2008 PTP, identified by its grandmaster clock id (the
    /// EUI-64-style hex string, verbatim) and PTP domain.
    Ptp {
        /// The grandmaster clock identity, as written in the SDP (e.g.
        /// `AA-BB-CC-DD-EE-FF-00-11`).
        gmid: String,
        /// The PTP domain number (`0..=127`).
        domain: u8,
    },
    /// The `localmac` form (the local MAC address) used when no PTP grandmaster
    /// id is available.
    LocalMac {
        /// The six-octet MAC address.
        mac: [u8; 6],
    },
}

/// Errors raised while parsing an AES67 / ST 2110-30 audio SDP document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SdpError {
    /// No `m=audio` media section was found.
    #[error("sdp: missing m=audio section")]
    MissingAudioSection,
    /// The `m=audio` line was malformed (missing port or payload type).
    #[error("sdp: malformed m=audio line: {0}")]
    BadAudioLine(String),
    /// The audio section had no `a=rtpmap` attribute.
    #[error("sdp: missing a=rtpmap")]
    MissingRtpmap,
    /// The `a=rtpmap` attribute could not be parsed.
    #[error("sdp: invalid rtpmap format: {0}")]
    BadRtpmap(String),
    /// The rtpmap named a codec other than L16 or L24.
    #[error("sdp: rtpmap unknown codec (expected L16 or L24): {0}")]
    UnknownCodec(String),
    /// The rtpmap declared zero channels.
    #[error("sdp: bad channel count in rtpmap: {0}")]
    BadChannelCount(u8),
    /// The rtpmap clock rate was not a supported AES67 / ST 2110-30 rate.
    #[error("sdp: bad clock rate (expected 48000 or 96000): {0}")]
    BadClockRate(u32),
    /// The `a=ptime` attribute was absent.
    #[error("sdp: missing a=ptime")]
    MissingPtime,
    /// The `a=ptime` value was unparseable or out of the `0.125..=1000 ms` range.
    #[error("sdp: invalid ptime (must be 0.125..=1000 ms): {0}")]
    BadPtime(String),
    /// The `a=ts-refclk` attribute was absent.
    #[error("sdp: missing a=ts-refclk")]
    MissingTsRefclk,
    /// The `a=ts-refclk` value was malformed.
    #[error("sdp: invalid ts-refclk format: {0}")]
    BadTsRefclk(String),
    /// The `a=mediaclk:direct` offset was unparseable.
    #[error("sdp: invalid mediaclk offset: {0}")]
    BadMediaclk(String),
}

/// The accumulating audio-section fields while scanning SDP lines.
#[derive(Default)]
struct AudioSection {
    port: u16,
    payload_type: u8,
    /// `(channels, depth, clock_rate)` from the matching rtpmap.
    rtpmap: Option<(u8, SampleDepth, u32)>,
    ptime_ms_x1000: Option<u32>,
    ts_refclk: Option<TsRefclk>,
    mediaclk_offset: Option<u32>,
}

impl AudioSdpSession {
    /// Parse the first `m=audio` media section of an SDP document.
    ///
    /// Lines for other media (`m=video`, …) and session-level lines are ignored;
    /// only attributes that follow the audio `m=` line are applied to it. Both
    /// `\n` and `\r\n` endings are accepted.
    ///
    /// # Errors
    ///
    /// * [`SdpError::MissingAudioSection`] — no `m=audio` line.
    /// * [`SdpError::MissingRtpmap`] / [`SdpError::BadRtpmap`] /
    ///   [`SdpError::UnknownCodec`] / [`SdpError::BadChannelCount`] /
    ///   [`SdpError::BadClockRate`] — the codec line is absent or invalid.
    /// * [`SdpError::MissingPtime`] / [`SdpError::BadPtime`] — packet time
    ///   absent or out of range.
    /// * [`SdpError::MissingTsRefclk`] / [`SdpError::BadTsRefclk`] — the
    ///   reference clock is absent or malformed.
    /// * [`SdpError::BadMediaclk`] — a present `a=mediaclk:direct` is unparseable.
    pub fn parse(sdp: &str) -> Result<Self, SdpError> {
        let mut section: Option<AudioSection> = None;

        for raw_line in sdp.lines() {
            let line = raw_line.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }
            let Some((ty, value)) = line.split_once('=') else {
                // Not a `type=value` line; skip (SDP is line-oriented).
                continue;
            };
            match ty {
                "m" => {
                    // A new media section. We keep the FIRST audio section and
                    // stop applying attributes once a non-audio section starts.
                    if let Some(rest) = value.strip_prefix("audio") {
                        if section.is_none() {
                            section = Some(parse_audio_m_line(rest.trim_start())?);
                        }
                    } else if section.is_some() {
                        // A later non-audio section: stop collecting attributes
                        // for the audio section we already captured.
                        break;
                    }
                }
                "a" => {
                    if let Some(sec) = section.as_mut() {
                        apply_audio_attribute(sec, value)?;
                    }
                }
                // c=, o=, s=, t=, v=, … carry no audio-codec information here.
                _ => {}
            }
        }

        let sec = section.ok_or(SdpError::MissingAudioSection)?;
        let (channels, depth, clock_rate) = sec.rtpmap.ok_or(SdpError::MissingRtpmap)?;
        let format = Aes3Format::new(channels, depth).map_err(|_e| SdpError::BadChannelCount(0))?;
        let ptime_ms_x1000 = sec.ptime_ms_x1000.ok_or(SdpError::MissingPtime)?;
        let ts_refclk = sec.ts_refclk.ok_or(SdpError::MissingTsRefclk)?;

        Ok(Self {
            port: sec.port,
            payload_type: sec.payload_type,
            format,
            clock_rate,
            ptime_ms_x1000,
            ts_refclk,
            // RFC 7273: `a=mediaclk:direct=0` is the common default; absence is
            // treated as offset 0.
            mediaclk_offset: sec.mediaclk_offset.unwrap_or(0),
        })
    }

    /// Render this session as RFC 4566/8866-compliant SDP lines (newlines NOT
    /// included; the caller joins with `\n` or `\r\n`).
    ///
    /// The emitted lines are the audio media section and its attributes only:
    /// `m=audio`, `a=rtpmap`, `a=ptime`, `a=ts-refclk`, `a=mediaclk`. They
    /// re-parse to an equal [`AudioSdpSession`].
    #[must_use]
    pub fn generate(&self) -> Vec<String> {
        // `SampleDepth` is `#[non_exhaustive]`, but it is defined in this crate,
        // so this match is exhaustive over its current variants (AES67 only
        // standardizes L16 and L24). A new depth added here later forces this
        // arm to be updated, which is the desired non-breaking-within-crate
        // behaviour.
        let depth = match self.format.depth {
            SampleDepth::L16 => "L16",
            SampleDepth::L24 => "L24",
        };
        let mut lines = Vec::with_capacity(5);
        lines.push(format!(
            "m=audio {} RTP/AVP {}",
            self.port, self.payload_type
        ));
        lines.push(format!(
            "a=rtpmap:{} {depth}/{}/{}",
            self.payload_type, self.clock_rate, self.format.channels
        ));
        lines.push(format!("a=ptime:{}", render_ptime(self.ptime_ms_x1000)));
        lines.push(format!("a=ts-refclk:{}", render_ts_refclk(&self.ts_refclk)));
        lines.push(format!("a=mediaclk:direct={}", self.mediaclk_offset));
        lines
    }
}

/// Parse the part of the `m=audio` line after the `audio` token:
/// `<port>[/<count>] <proto> <fmt> ...` — we read the port and the first format
/// (the dynamic payload type).
fn parse_audio_m_line(rest: &str) -> Result<AudioSection, SdpError> {
    let mut parts = rest.split_whitespace();
    let port_token = parts
        .next()
        .ok_or_else(|| SdpError::BadAudioLine(rest.to_owned()))?;
    // The port may be `port/count`; take the leading number.
    let port_str = port_token.split('/').next().unwrap_or(port_token);
    let port: u16 = port_str
        .parse()
        .map_err(|_e| SdpError::BadAudioLine(rest.to_owned()))?;
    // Skip the transport proto token (e.g. `RTP/AVP`).
    let _proto = parts
        .next()
        .ok_or_else(|| SdpError::BadAudioLine(rest.to_owned()))?;
    let payload_type: u8 = parts
        .next()
        .ok_or_else(|| SdpError::BadAudioLine(rest.to_owned()))?
        .parse()
        .map_err(|_e| SdpError::BadAudioLine(rest.to_owned()))?;
    Ok(AudioSection {
        port,
        payload_type,
        ..AudioSection::default()
    })
}

/// Apply one `a=` attribute body to the audio section.
fn apply_audio_attribute(sec: &mut AudioSection, value: &str) -> Result<(), SdpError> {
    if let Some(body) = value.strip_prefix("rtpmap:") {
        sec.rtpmap = Some(parse_rtpmap(body)?);
    } else if let Some(body) = value.strip_prefix("ptime:") {
        sec.ptime_ms_x1000 = Some(parse_ptime(body)?);
    } else if let Some(body) = value.strip_prefix("ts-refclk:") {
        sec.ts_refclk = Some(parse_ts_refclk(body)?);
    } else if let Some(body) = value.strip_prefix("mediaclk:") {
        sec.mediaclk_offset = Some(parse_mediaclk(body)?);
    }
    // Other attributes (e.g. recvonly, framecount) are not part of the codec
    // model.
    Ok(())
}

/// Parse an rtpmap body: `<PT> <name>/<clock>/<channels>` into
/// `(channels, depth, clock_rate)`. The payload type is validated against the
/// `m=` line's payload type implicitly by being the same section.
fn parse_rtpmap(body: &str) -> Result<(u8, SampleDepth, u32), SdpError> {
    let (_pt, rest) = body
        .trim()
        .split_once(char::is_whitespace)
        .ok_or_else(|| SdpError::BadRtpmap(body.to_owned()))?;
    let mut fields = rest.trim().split('/');
    let name = fields
        .next()
        .ok_or_else(|| SdpError::BadRtpmap(body.to_owned()))?;
    let depth = match name {
        "L16" => SampleDepth::L16,
        "L24" => SampleDepth::L24,
        other => return Err(SdpError::UnknownCodec(other.to_owned())),
    };
    let clock_str = fields
        .next()
        .ok_or_else(|| SdpError::BadRtpmap(body.to_owned()))?;
    let clock_rate: u32 = clock_str
        .parse()
        .map_err(|_e| SdpError::BadRtpmap(body.to_owned()))?;
    if clock_rate != 48_000 && clock_rate != 96_000 {
        return Err(SdpError::BadClockRate(clock_rate));
    }
    let channels_str = fields
        .next()
        .ok_or_else(|| SdpError::BadRtpmap(body.to_owned()))?;
    let channels: u8 = channels_str
        .parse()
        .map_err(|_e| SdpError::BadRtpmap(body.to_owned()))?;
    if channels == 0 {
        return Err(SdpError::BadChannelCount(0));
    }
    Ok((channels, depth, clock_rate))
}

/// Parse an `a=ptime` body (milliseconds, possibly fractional like `0.125`) into
/// integer thousandths of a millisecond. Rejects values outside `0.125..=1000`.
fn parse_ptime(body: &str) -> Result<u32, SdpError> {
    let trimmed = body.trim();
    let x1000 = match trimmed.split_once('.') {
        None => {
            let whole: u32 = trimmed
                .parse()
                .map_err(|_e| SdpError::BadPtime(body.to_owned()))?;
            whole
                .checked_mul(1000)
                .ok_or_else(|| SdpError::BadPtime(body.to_owned()))?
        }
        Some((whole_str, frac_str)) => {
            // Accept up to three fractional digits (millisecond → thousandths).
            if frac_str.is_empty()
                || frac_str.len() > 3
                || !frac_str.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(SdpError::BadPtime(body.to_owned()));
            }
            let whole: u32 = if whole_str.is_empty() {
                0
            } else {
                whole_str
                    .parse()
                    .map_err(|_e| SdpError::BadPtime(body.to_owned()))?
            };
            // Right-pad the fraction to exactly three digits, then it is already
            // in thousandths.
            let mut frac_padded = String::with_capacity(3);
            frac_padded.push_str(frac_str);
            while frac_padded.len() < 3 {
                frac_padded.push('0');
            }
            let frac: u32 = frac_padded
                .parse()
                .map_err(|_e| SdpError::BadPtime(body.to_owned()))?;
            whole
                .checked_mul(1000)
                .and_then(|w| w.checked_add(frac))
                .ok_or_else(|| SdpError::BadPtime(body.to_owned()))?
        }
    };
    if !(125..=1_000_000).contains(&x1000) {
        return Err(SdpError::BadPtime(body.to_owned()));
    }
    Ok(x1000)
}

/// Parse an `a=ts-refclk` body: `ptp=IEEE1588-2008:<gmid>:<domain>` or
/// `localmac=<mac>`.
fn parse_ts_refclk(body: &str) -> Result<TsRefclk, SdpError> {
    let trimmed = body.trim();
    if let Some(ptp) = trimmed.strip_prefix("ptp=") {
        // `IEEE1588-2008:<gmid>:<domain>`. The gmid itself contains hyphens but
        // no colons, so split on ':' yields [version, gmid, domain].
        let mut parts = ptp.split(':');
        let _version = parts
            .next()
            .ok_or_else(|| SdpError::BadTsRefclk(body.to_owned()))?;
        let gmid = parts
            .next()
            .ok_or_else(|| SdpError::BadTsRefclk(body.to_owned()))?
            .to_owned();
        if gmid.is_empty() {
            return Err(SdpError::BadTsRefclk(body.to_owned()));
        }
        let domain: u8 = parts
            .next()
            .ok_or_else(|| SdpError::BadTsRefclk(body.to_owned()))?
            .parse()
            .map_err(|_e| SdpError::BadTsRefclk(body.to_owned()))?;
        if parts.next().is_some() {
            return Err(SdpError::BadTsRefclk(body.to_owned()));
        }
        Ok(TsRefclk::Ptp { gmid, domain })
    } else if let Some(mac_str) = trimmed.strip_prefix("localmac=") {
        Ok(TsRefclk::LocalMac {
            mac: parse_mac(mac_str).ok_or_else(|| SdpError::BadTsRefclk(body.to_owned()))?,
        })
    } else {
        Err(SdpError::BadTsRefclk(body.to_owned()))
    }
}

/// Parse a six-octet `aa-bb-cc-dd-ee-ff` MAC address.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut count = 0usize;
    for (slot, octet) in out.iter_mut().zip(s.split('-')) {
        *slot = u8::from_str_radix(octet, 16).ok()?;
        count += 1;
    }
    if count == 6 && s.split('-').count() == 6 {
        Some(out)
    } else {
        None
    }
}

/// Parse an `a=mediaclk` body: `direct=<offset>` (other forms are not modelled).
fn parse_mediaclk(body: &str) -> Result<u32, SdpError> {
    let trimmed = body.trim();
    let offset_str = trimmed
        .strip_prefix("direct=")
        .ok_or_else(|| SdpError::BadMediaclk(body.to_owned()))?;
    offset_str
        .parse()
        .map_err(|_e| SdpError::BadMediaclk(body.to_owned()))
}

/// Render integer thousandths-of-a-millisecond back into an SDP `ptime` value:
/// a whole number when divisible by 1000, else a decimal with no trailing zeros.
fn render_ptime(x1000: u32) -> String {
    if x1000 % 1000 == 0 {
        (x1000 / 1000).to_string()
    } else {
        let whole = x1000 / 1000;
        let frac = x1000 % 1000;
        // Three-digit fraction, trailing zeros trimmed (125 -> "125", 500 -> "5").
        let mut frac_str = format!("{frac:03}");
        while frac_str.ends_with('0') {
            frac_str.pop();
        }
        format!("{whole}.{frac_str}")
    }
}

/// Render a [`TsRefclk`] back into an SDP `ts-refclk` value.
fn render_ts_refclk(refclk: &TsRefclk) -> String {
    match refclk {
        TsRefclk::Ptp { gmid, domain } => {
            format!("ptp=IEEE1588-2008:{gmid}:{domain}")
        }
        TsRefclk::LocalMac { mac } => {
            let hex = mac
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join("-");
            format!("localmac={hex}")
        }
    }
}
