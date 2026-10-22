//! Pure Annex-B framing normalization for packet-fed H.264 decode (ADR-T014).
//!
//! The WHIP ingest path has no demuxer: the RTP depacketizer
//! (`multiview-input`'s `H264Depacketizer`) hands the decoder whatever bytes
//! RFC 6184 produced — a **bare NAL** (single-NAL packet or a reassembled
//! FU-A, no framing at all), a raw **STAP-A** aggregation payload (type-24
//! header + 16-bit-length-prefixed NALs), or, from other producers,
//! start-code-framed **Annex-B** or **AVCC** (4-byte big-endian NAL lengths).
//! libav's H.264 decoder wants Annex-B; [`to_annexb`] is the total, pure
//! normalizer that gets every shape there.
//!
//! Detection order (each step validates fully before claiming the framing):
//!
//! 1. Leading `00 00 01` / `00 00 00 01` → already Annex-B, **borrowed
//!    passthrough** (zero-copy on the common conforming-publisher path).
//! 2. A 4-byte-length walk that consumes the buffer exactly, with every NAL
//!    non-empty and header-plausible → AVCC, converted.
//! 3. A NAL header of type 24 whose 16-bit-length walk validates → STAP-A,
//!    de-aggregated.
//! 4. Anything else → one bare NAL: a single start code is prepended. This is
//!    also the conservative fallback for malformed input — bytes are framed
//!    verbatim and the decoder skips garbage NALs safely; nothing is dropped
//!    and nothing panics (CLAUDE.md §7).
//!
//! Pure byte logic: no libav dependency, always compiled, exhaustively
//! unit-testable in the default build (the [`crate::idr`] pattern).

use std::borrow::Cow;

/// The 4-byte Annex-B start code prepended to converted/bare NAL units.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// RFC 6184 STAP-A NAL unit type (single-time aggregation packet).
const NAL_TYPE_STAP_A: u8 = 24;

/// Whether `au` already starts with an Annex-B start code (3- or 4-byte form).
#[must_use]
pub fn is_annexb(au: &[u8]) -> bool {
    au.starts_with(&[0x00, 0x00, 0x01]) || au.starts_with(&START_CODE)
}

/// Normalize one access unit / NAL unit to Annex-B start-code framing.
///
/// Accepts every byte shape the RTP depacketizer (and an AVCC-normalizing
/// producer) can emit — see the [module docs](self) for the exact detection
/// policy. Total: malformed input degrades to a verbatim single-NAL wrap,
/// never an error, never a panic, never dropped bytes. Annex-B input is
/// returned **borrowed** (zero-copy).
#[must_use]
pub fn to_annexb(au: &[u8]) -> Cow<'_, [u8]> {
    if au.is_empty() || is_annexb(au) {
        return Cow::Borrowed(au);
    }
    if let Some(nals) = parse_length_prefixed(au, 4) {
        return Cow::Owned(join_with_start_codes(&nals));
    }
    if is_stap_a_header(au) {
        // Skip the one-byte STAP-A header; the payload is 16-bit-length NALs.
        if let Some(nals) = au.get(1..).and_then(|rest| parse_length_prefixed(rest, 2)) {
            return Cow::Owned(join_with_start_codes(&nals));
        }
    }
    // Bare single NAL (or unclassifiable bytes): one start code, verbatim body.
    let mut out = Vec::with_capacity(START_CODE.len() + au.len());
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(au);
    Cow::Owned(out)
}

/// Whether the first byte is a plausible STAP-A header (forbidden bit clear,
/// NAL type 24).
fn is_stap_a_header(au: &[u8]) -> bool {
    au.first()
        .is_some_and(|&b| (b & 0x80) == 0 && (b & 0x1F) == NAL_TYPE_STAP_A)
}

/// Walk `data` as `prefix_len`-byte big-endian length-prefixed NAL units.
///
/// Returns the NAL bodies only when the walk is fully valid: at least one NAL,
/// every length non-zero, every NAL header plausible (forbidden bit clear,
/// type 1..=31 — i.e. non-zero), and the buffer consumed **exactly**. Any
/// violation returns [`None`] so the caller falls through to the next
/// interpretation rather than mis-framing bytes.
fn parse_length_prefixed(data: &[u8], prefix_len: usize) -> Option<Vec<&[u8]>> {
    let mut nals = Vec::new();
    let mut offset = 0_usize;
    while offset < data.len() {
        let len_bytes = data.get(offset..offset.checked_add(prefix_len)?)?;
        let mut nal_len = 0_usize;
        for &b in len_bytes {
            nal_len = (nal_len << 8) | usize::from(b);
        }
        if nal_len == 0 {
            return None;
        }
        let body_start = offset.checked_add(prefix_len)?;
        let body_end = body_start.checked_add(nal_len)?;
        let nal = data.get(body_start..body_end)?;
        let &header = nal.first()?;
        let forbidden_bit_set = (header & 0x80) != 0;
        let nal_type = header & 0x1F;
        if forbidden_bit_set || nal_type == 0 {
            return None;
        }
        nals.push(nal);
        offset = body_end;
    }
    (!nals.is_empty()).then_some(nals)
}

/// The NAL unit bodies (start codes stripped) of an Annex-B buffer, in order.
///
/// Crate-internal and gated with its sole consumer: the packet-fed H.264
/// decoder (`crate::packet_decode::H264PacketDecoder`, `ffmpeg` feature) walks
/// the normalized bytes NAL-by-NAL to find access-unit boundaries — its
/// behaviour is exercised through that decoder's bare-NAL/AVCC tests. Bytes
/// before the first start code (none, for [`to_annexb`] output) are ignored;
/// empty bodies are skipped.
#[cfg(feature = "ffmpeg")]
pub(crate) fn nal_bodies(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0_usize;
    while let Some(window) = data.get(i..i.saturating_add(3)) {
        if window == [0x00, 0x00, 0x01] {
            starts.push(i.saturating_add(3));
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    let mut bodies = Vec::with_capacity(starts.len());
    for (n, &body_start) in starts.iter().enumerate() {
        // The body runs to the next start code, minus the leading zero of a
        // 4-byte form (`00 00 00 01`).
        let body_end = starts.get(n.saturating_add(1)).map_or(data.len(), |&next| {
            let sc = next.saturating_sub(3);
            if sc > 0 && data.get(sc.saturating_sub(1)) == Some(&0x00) {
                sc.saturating_sub(1)
            } else {
                sc
            }
        });
        if let Some(body) = data.get(body_start..body_end) {
            if !body.is_empty() {
                bodies.push(body);
            }
        }
    }
    bodies
}

/// Join NAL bodies into one Annex-B buffer, each prefixed with a start code.
fn join_with_start_codes(nals: &[&[u8]]) -> Vec<u8> {
    let total: usize = nals
        .iter()
        .map(|n| n.len().saturating_add(START_CODE.len()))
        .sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::{is_annexb, to_annexb};
    use std::borrow::Cow;

    #[test]
    fn is_annexb_detects_both_start_code_forms() {
        assert!(is_annexb(&[0x00, 0x00, 0x01, 0x65]));
        assert!(is_annexb(&[0x00, 0x00, 0x00, 0x01, 0x65]));
        assert!(!is_annexb(&[0x65, 0x00, 0x00, 0x01]));
        assert!(!is_annexb(&[]));
    }

    #[test]
    fn avcc_with_invalid_inner_header_falls_back_to_bare_wrap() {
        // First 4 bytes parse as a length that fits, but the "NAL" inside has
        // the forbidden bit set — the AVCC claim is rejected and the bytes are
        // wrapped verbatim instead of being mis-framed.
        let bogus = [0x00, 0x00, 0x00, 0x02, 0x80, 0xAA];
        let out = to_annexb(&bogus);
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(&out[..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&out[4..], &bogus);
    }

    #[test]
    fn avcc_trailing_garbage_is_not_claimed_as_avcc() {
        // A valid first NAL but the walk does not consume the buffer exactly:
        // not AVCC; fall through to the bare wrap (total, never dropped bytes).
        let data = [0x00, 0x00, 0x00, 0x02, 0x67, 0xAA, 0xFF];
        let out = to_annexb(&data);
        assert_eq!(&out[..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&out[4..], &data);
    }
}
