//! ELD (EDID-Like Data) parsing — the sink's HDMI/DP audio capability gate
//! (DEV-B4 / display-out §5).
//!
//! The video driver parses the sink's EDID audio data block and publishes it as
//! ELD at `/proc/asound/cardN/eld#C.P` (channel counts, sample rates, monitor
//! name). The ELD is valid **only while the display pipe is lit** (HDMI audio
//! rides data islands in the video stream), which our always-lit scanout sink
//! guarantees. This module parses the **bytes** into an [`EldCapability`]; the
//! `/proc` read that supplies those bytes lives in [`super::alsa`] behind the
//! `display-kms` feature, so the parser is fully unit-testable over mock bytes.
//!
//! An EDID-less head publishes a zero/empty/truncated ELD: [`parse_eld`] returns
//! [`None`] (no audio path), never a panic — the documented field condition (the
//! t630 EDID-less head gets video only).
//!
//! ## Format (HDA ELD v2, the kernel's `hda_eld.c` layout)
//!
//! Byte offsets verified against the kernel's `snd_hdmi_parse_eld`
//! (`GRAB_BITS` calls in `sound/pci/hda/hda_eld.c`):
//!
//! ```text
//! byte 0: (eld_ver << 3)                     — GRAB_BITS(buf, 0, 3, 5); version 2 is current
//! byte 2: baseline_eld_len                   — GRAB_BITS(buf, 2, 0, 8), in 4-byte words
//! byte 4 (baseline +0): (cea_edid_ver << 5) | (mnl & 0x1f)   — GRAB_BITS(buf, 4, 0, 5)/(4, 5, 3)
//! byte 5 (baseline +1): (sad_count << 4) | conn_type | ...   — GRAB_BITS(buf, 5, 4, 4)
//! ...    16-byte baseline block (header is 4 bytes; ELD_FIXED_BYTES = 20) ...
//! then:  `mnl` monitor-name bytes, then `sad_count` × 3-byte CEA short audio descriptors.
//! ```
//!
//! Each CEA short audio descriptor (SAD) is 3 bytes: byte0 carries
//! `(format_code << 3) | (channels - 1)`, byte1 a sample-rate bitmap
//! (bit0 = 32 kHz, bit1 = 44.1 kHz, bit2 = 48 kHz, …), byte2 format-specific
//! (for LPCM, the bit-depth bitmap). We read the maximum-channel LPCM descriptor
//! and the union of advertised rates — enough to gate the sink (it only ever
//! emits canonical 48 kHz LPCM).

/// The audio capability an HDMI/DP sink declares through its ELD.
///
/// Constructed by [`parse_eld`] from the published bytes; a plain value the sink
/// stashes and compares on hotplug re-read (a changed capability is a Class-2
/// reconfigure). `has_audio` is implied by existence — a sink with no usable
/// LPCM descriptor parses to [`None`], not an empty capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EldCapability {
    max_channels: u8,
    /// Advertised LPCM sample rates (Hz), sorted, de-duplicated.
    rates: Vec<u32>,
    lpcm: bool,
    monitor: String,
}

impl EldCapability {
    /// Build a capability directly (tests, and the parser).
    #[must_use]
    pub fn lpcm(max_channels: u8, rates: &[u32], monitor: &str) -> Self {
        let mut rates: Vec<u32> = rates.to_vec();
        rates.sort_unstable();
        rates.dedup();
        Self {
            max_channels: max_channels.max(1),
            rates,
            lpcm: true,
            monitor: monitor.to_owned(),
        }
    }

    /// Whether this capability carries a usable audio path (always `true` for a
    /// constructed capability; an absent path is [`None`] from [`parse_eld`]).
    #[must_use]
    pub const fn has_audio(&self) -> bool {
        self.lpcm && self.max_channels >= 1
    }

    /// The maximum channel count the sink declares.
    #[must_use]
    pub const fn max_channels(&self) -> u8 {
        self.max_channels
    }

    /// Whether the sink advertises this exact sample rate.
    #[must_use]
    pub fn supports_rate(&self, rate: u32) -> bool {
        self.rates.contains(&rate)
    }

    /// Whether the sink advertises an LPCM descriptor (we never send
    /// compressed/bitstream audio, so non-LPCM is treated as no audio path).
    #[must_use]
    pub const fn supports_lpcm(&self) -> bool {
        self.lpcm
    }

    /// The sink's monitor name (best-effort; empty when the ELD omits it).
    #[must_use]
    pub fn monitor_name(&self) -> &str {
        &self.monitor
    }

    /// Negotiate a `(rate, channels)` the sink can take for a desired output
    /// format: the rate must be advertised; the channel count clamps down to
    /// the ELD ceiling (an over-ask never fails the whole audio path — it
    /// down-mixes to what the sink supports) **and a below-minimum ask is
    /// raised to the HDMI stereo minimum** (never past the ELD ceiling): HDMI/
    /// DP LPCM rides IEC 60958 subframe pairs, so real devices refuse a
    /// 1-channel PCM — the sink upmixes a mono program instead of running
    /// silent. Returns [`None`] when the rate is unsupported or the sink
    /// declares no LPCM path.
    #[must_use]
    pub fn negotiate(&self, rate: u32, channels: u8) -> Option<(u32, u8)> {
        if !self.lpcm || !self.supports_rate(rate) {
            return None;
        }
        // `max_channels >= 1` by construction, so floor is 1..=2 and never
        // exceeds the ceiling.
        let floor = HDMI_MIN_CHANNELS.min(self.max_channels).max(1);
        Some((rate, channels.clamp(floor, self.max_channels)))
    }
}

/// Parse published ELD bytes into an [`EldCapability`], or [`None`] when the
/// blob declares no usable LPCM audio path (EDID-less, empty, truncated, or
/// non-LPCM-only sink).
///
/// Never panics and never reads out of bounds — every field access is a checked
/// `get`, so a hostile/truncated blob degrades to [`None`].
#[must_use]
pub fn parse_eld(bytes: &[u8]) -> Option<EldCapability> {
    // The fixed 4-byte ELD header plus the 16-byte baseline block must be
    // present before any field is meaningful.
    const HEADER: usize = 4;
    const BASELINE: usize = 16;
    if bytes.len() < HEADER + BASELINE {
        return None;
    }
    // An all-zero header is the EDID-less / not-yet-valid sentinel: version 0,
    // baseline length 0 → no audio path.
    let eld_ver = bytes.first().copied()? >> 3;
    if eld_ver == 0 {
        return None;
    }
    let baseline_words = usize::from(bytes.get(2).copied()?);
    if baseline_words == 0 {
        return None;
    }
    // Monitor-name length is the low 5 bits of baseline byte 0 (== byte 4),
    // kernel GRAB_BITS(buf, 4, 0, 5).
    let mnl = usize::from(bytes.get(HEADER).copied()? & 0x1f);
    // SAD count is the high nibble of baseline byte 1 (== byte 5), kernel
    // GRAB_BITS(buf, 5, 4, 4).
    let sad_count = usize::from(bytes.get(HEADER + 1).copied()? >> 4);
    if sad_count == 0 {
        return None;
    }

    // Monitor name follows the 16-byte baseline block.
    let name_start = HEADER + BASELINE;
    let name_end = name_start.checked_add(mnl)?;
    let monitor = bytes
        .get(name_start..name_end)
        .map(|s| String::from_utf8_lossy(s).trim_end_matches('\0').to_owned())
        .unwrap_or_default();

    // SADs follow the monitor name, 3 bytes each.
    let sad_start = name_end;
    let mut max_channels: u8 = 0;
    let mut rates: Vec<u32> = Vec::new();
    let mut found_lpcm = false;
    for i in 0..sad_count {
        let base = sad_start.checked_add(i.checked_mul(3)?)?;
        let b0 = match bytes.get(base) {
            Some(b) => *b,
            // A SAD that runs past the buffer means a malformed/truncated ELD:
            // bail rather than index out of bounds.
            None => {
                return if found_lpcm {
                    finish(max_channels, &rates, &monitor)
                } else {
                    None
                }
            }
        };
        let b1 = bytes.get(base + 1).copied().unwrap_or(0);
        let format_code = b0 >> 3;
        // Format code 1 is LPCM — the only path we drive.
        if format_code == 1 {
            found_lpcm = true;
            let channels = (b0 & 0x07).saturating_add(1);
            max_channels = max_channels.max(channels);
            for (bit, rate) in RATE_BITS.iter().enumerate() {
                if b1 & (1u8 << bit) != 0 {
                    rates.push(*rate);
                }
            }
        }
    }
    finish(max_channels, &rates, &monitor)
}

/// The HDMI/DP LPCM channel floor: IEC 60958 frames carry subframe **pairs**,
/// so a sink's PCM never takes fewer than two channels and a 1-channel ask is
/// refused by real devices. A (degenerate) mono-only ELD keeps its declared
/// ceiling — we never ask a sink for more than it declared.
const HDMI_MIN_CHANNELS: u8 = 2;

/// Sample rates carried by the CEA SAD rate bitmap, bit 0..6.
const RATE_BITS: [u32; 7] = [32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Assemble the final capability, or [`None`] when no usable LPCM rate/channel
/// was found.
fn finish(max_channels: u8, rates: &[u32], monitor: &str) -> Option<EldCapability> {
    if max_channels == 0 || rates.is_empty() {
        return None;
    }
    Some(EldCapability::lpcm(max_channels, rates, monitor))
}

/// Parse the kernel's **textual** `/proc/asound/cardN/eld#C.P` dump into an
/// [`EldCapability`], or [`None`] when it declares no usable LPCM audio path.
///
/// The x86 HDA driver publishes the ELD as text, one `key\tvalue` line per
/// field (`snd_hdmi_print_eld_info` in `sound/pci/hda/hda_eld.c`). The fields
/// we gate on, with the kernel's verbatim print formats:
///
/// ```text
/// eld_valid\t\t%d                      — 0 while the pipe is dark / EDID-less
/// monitor_name\t\t%s                   — may contain spaces
/// sad%d_coding_type\t[0x%x] %s         — the LPCM name is exactly "LPCM"
/// sad%d_channels\t\t%d
/// sad%d_rates\t\t[0x%x] 32000 44100 …  — bitmap hex then the Hz list
/// ```
///
/// An EDID-less head publishes `eld_valid 0` (or an empty file) → [`None`].
/// Pure (no I/O): the `/proc` read that supplies the text lives in the
/// feature-gated ALSA backend, so this parser is CI-tested over fixtures.
#[must_use]
pub fn parse_proc_eld_text(text: &str) -> Option<EldCapability> {
    let mut eld_valid = false;
    let mut monitor = String::new();
    let mut max_channels: u8 = 0;
    let mut rates: Vec<u32> = Vec::new();
    // Channels/rates lines are only honoured for SADs whose coding type is
    // LPCM; the kernel prints coding_type before channels/rates per SAD, so a
    // single pass tracking the current SAD's LPCM-ness is sufficient.
    let mut sad_is_lpcm = false;
    let mut found_lpcm = false;

    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        match key {
            "eld_valid" => {
                eld_valid = parts.next() == Some("1");
            }
            "monitor_name" => {
                monitor = parts.collect::<Vec<_>>().join(" ");
            }
            // `sad%d_coding_type\t[0x%x] %s` — e.g. `sad0_coding_type [0x1] LPCM`.
            k if k.starts_with("sad") && k.ends_with("coding_type") => {
                sad_is_lpcm = parts.any(|t| t == "LPCM");
                found_lpcm |= sad_is_lpcm;
            }
            k if sad_is_lpcm && k.starts_with("sad") && k.ends_with("channels") => {
                if let Some(n) = parts.next().and_then(|v| v.parse::<u8>().ok()) {
                    max_channels = max_channels.max(n);
                }
            }
            // `sad%d_rates\t\t[0x%x] 32000 44100 …` — the `[0x…]` bitmap token
            // fails the integer parse and is skipped; the Hz values collect.
            k if sad_is_lpcm && k.starts_with("sad") && k.ends_with("rates") => {
                for tok in parts {
                    let cleaned = tok.trim_matches(|c| c == '[' || c == ']' || c == ',');
                    if let Ok(hz) = cleaned.parse::<u32>() {
                        rates.push(hz);
                    }
                }
            }
            _ => {}
        }
    }

    if !eld_valid || !found_lpcm {
        return None;
    }
    finish(max_channels, &rates, &monitor)
}
