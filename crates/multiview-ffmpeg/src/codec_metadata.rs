//! Decoder descriptions from actual coded bytes, independent of packet framing.
//! Unsupported syntax fails closed; no requested profile or codec name is used.
use crate::idr::NalFraming;

/// Validated AVC decoder configuration and RFC 6381 codec token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcDecoderConfig {
    /// RFC 6381 profile/compatibility/level from the SPS bytes.
    pub codec: String,
    /// AVCDecoderConfigurationRecord. Samples require four-byte NAL lengths.
    pub description: Vec<u8>,
}

const MAX_CONFIG: usize = 64 * 1024;

impl AvcDecoderConfig {
    /// Read a complete avcC record or Annex-B parameter sets from an opened encoder.
    #[must_use]
    pub fn from_extradata(bytes: &[u8]) -> Option<Self> {
        if bytes.first() != Some(&1) {
            return Self::from_annex_b(bytes, false);
        }
        if bytes.len() > MAX_CONFIG || bytes.len() < 7 || bytes[4] != 0xff || bytes[5] != 0xe1 {
            return None;
        }
        let mut offset = 6;
        let sps = record_nal(bytes, &mut offset)?;
        if *bytes.get(offset)? != 1 {
            return None;
        }
        offset += 1;
        let pps = record_nal(bytes, &mut offset)?;
        // High-profile extension fields are deliberately not guessed or discarded.
        let config = Self::from_sets(sps, pps)?;
        // Compare the complete canonical record, including high-profile fields.
        if offset > bytes.len() {
            return None;
        }
        (config.description == bytes).then_some(config)
    }

    /// Inspect one complete, bounded actual access unit. Require a closed IDR
    /// with matching PPS/SPS before admitting metadata from emitted bytes.
    #[must_use]
    pub fn from_initial_rap(bytes: &[u8]) -> Option<Self> {
        Self::from_annex_b(bytes, true)
    }

    fn from_annex_b(bytes: &[u8], require_idr: bool) -> Option<Self> {
        let nals = annex_b_nals(bytes)?;
        let mut sps = None;
        let mut pps = None;
        let mut slices = Vec::new();
        for nal in nals {
            let header = *nal.first()?;
            if header & 0x80 != 0 {
                return None;
            }
            match header & 31 {
                7 if sps.is_none() => sps = Some(nal),
                8 if pps.is_none() => pps = Some(nal),
                5 if header & 0x60 != 0 => slices.push(nal),
                6 | 9 | 12 => {}
                // Mixed VCL, duplicate/ambiguous parameter sets and HEVC fail closed.
                _ => return None,
            }
        }
        let sps = sps?;
        let pps = pps?;
        let (_, pps_id) = validate_sets(sps, pps)?;
        if require_idr && slices.is_empty() {
            return None;
        }
        for slice in slices {
            let rbsp = rbsp(slice.get(1..)?)?;
            let mut bits = Bits::new(&rbsp);
            bits.ue()?; // first_mb_in_slice
            if bits.ue()? % 5 != 2 || bits.ue()? != pps_id {
                return None;
            }
            // The full coded slice stays opaque; the decoder validates its entropy data.
            if bits.remaining() < 8 {
                return None;
            }
        }
        Self::from_sets(sps, pps)
    }

    fn from_sets(sps: &[u8], pps: &[u8]) -> Option<Self> {
        validate_sets(sps, pps)?;
        let mut description = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        description.extend_from_slice(&u16::try_from(sps.len()).ok()?.to_be_bytes());
        description.extend_from_slice(sps);
        description.push(1);
        description.extend_from_slice(&u16::try_from(pps.len()).ok()?.to_be_bytes());
        description.extend_from_slice(pps);
        if matches!(sps[1], 100 | 110 | 122 | 144) {
            let rbsp = rbsp(&sps[1..])?;
            let mut bits = Bits::new(&rbsp);
            bits.read(24)?;
            bits.ue()?;
            let chroma = u8::try_from(bits.ue()?).ok()?;
            if chroma == 3 {
                bits.read(1)?;
            }
            let luma_depth = u8::try_from(bits.ue()?).ok()?;
            let chroma_depth = u8::try_from(bits.ue()?).ok()?;
            description.extend_from_slice(&[
                0xfc | chroma,
                0xf8 | luma_depth,
                0xf8 | chroma_depth,
                0,
            ]);
        }
        Some(Self {
            codec: format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3]),
            description,
        })
    }
}

fn record_nal<'a>(bytes: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    let length = u16::from_be_bytes(bytes.get(*offset..*offset + 2)?.try_into().ok()?) as usize;
    *offset += 2;
    let nal = bytes.get(*offset..offset.checked_add(length)?)?;
    *offset += length;
    (!nal.is_empty()).then_some(nal)
}

/// Strict complete Annex-B walk. No prefix garbage, empty NAL or partial suffix.
fn annex_b_nals(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    let prefix = |at: usize| -> Option<usize> {
        if bytes.get(at..at + 4) == Some(&[0, 0, 0, 1]) {
            Some(4)
        } else if bytes.get(at..at + 3) == Some(&[0, 0, 1]) {
            Some(3)
        } else {
            None
        }
    };
    let mut pos = prefix(0)?;
    let mut result = Vec::new();
    loop {
        let start = pos;
        while pos < bytes.len() && prefix(pos).is_none() {
            pos += 1;
        }
        let mut end = pos;
        while end > start && bytes[end - 1] == 0 {
            end -= 1;
        }
        if end == start || result.len() >= 128 {
            return None;
        }
        result.push(&bytes[start..end]);
        if pos == bytes.len() {
            break;
        }
        pos += prefix(pos)?;
    }
    Some(result)
}

fn rbsp(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut zeros = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        let b = bytes[pos];
        if zeros >= 2 {
            if b == 3 {
                if *bytes.get(pos + 1)? > 3 {
                    return None;
                }
                zeros = 0;
                pos += 1;
                continue;
            }
            if b < 3 {
                return None;
            }
        }
        result.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        pos += 1;
    }
    Some(result)
}

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len() * 8 - self.pos
    }
    fn read(&mut self, count: usize) -> Option<u32> {
        if count > 32 || self.remaining() < count {
            return None;
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(value)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 30 {
                return None;
            }
        }
        Some(((1_u32 << zeros) - 1) + self.read(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let v = self.ue()?;
        Some(if v & 1 == 0 {
            -(i32::try_from(v / 2).ok()?)
        } else {
            i32::try_from(v / 2 + 1).ok()?
        })
    }
    fn trailing(&mut self) -> Option<()> {
        if self.read(1)? != 1 {
            return None;
        }
        while self.remaining() > 0 {
            if self.read(1)? != 0 {
                return None;
            }
        }
        Some(())
    }
}

fn validate_sets(sps: &[u8], pps: &[u8]) -> Option<(u32, u32)> {
    if sps.len() > MAX_CONFIG
        || pps.len() > MAX_CONFIG
        || sps.first()? & 0x60 == 0
        || pps.first()? & 0x60 == 0
        || sps.first()? & 0x9f != 7
        || pps.first()? & 0x9f != 8
    {
        return None;
    }
    let s = rbsp(sps.get(1..)?)?;
    let mut b = Bits::new(&s);
    let profile = b.read(8)?;
    if b.read(8)? & 3 != 0 || b.read(8)? == 0 {
        return None;
    }
    let sps_id = b.ue()?;
    if sps_id > 31 {
        return None;
    }
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = b.ue()?;
        if chroma > 3 {
            return None;
        }
        if chroma == 3 {
            b.read(1)?;
        }
        if b.ue()? > 6 || b.ue()? > 6 {
            return None;
        }
        b.read(1)?;
        // Scaling matrices need a complete additional parser. Refuse, never guess.
        if b.read(1)? != 0 {
            return None;
        }
    } else if !matches!(profile, 66 | 77 | 88) {
        return None;
    }
    if b.ue()? > 12 {
        return None;
    }
    match b.ue()? {
        0 => {
            if b.ue()? > 12 {
                return None;
            }
        }
        1 => {
            b.read(1)?;
            b.se()?;
            b.se()?;
            let count = b.ue()?;
            if count > 255 {
                return None;
            }
            for _ in 0..count {
                b.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    if b.ue()? > 16 {
        return None;
    }
    b.read(1)?;
    if b.ue()? > 65535 || b.ue()? > 65535 {
        return None;
    }
    if b.read(1)? == 0 {
        b.read(1)?;
    }
    b.read(1)?;
    if b.read(1)? != 0 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    if b.read(1)? != 0 {
        vui(&mut b)?;
    }
    b.trailing()?;
    let p = rbsp(pps.get(1..)?)?;
    let mut b = Bits::new(&p);
    let pps_id = b.ue()?;
    if pps_id > 255 || b.ue()? != sps_id {
        return None;
    }
    b.read(1)?;
    b.read(1)?;
    if b.ue()? != 0 {
        return None;
    } // slice groups unsupported
    if b.ue()? > 31 || b.ue()? > 31 {
        return None;
    }
    b.read(1)?;
    b.read(2)?;
    b.se()?;
    b.se()?;
    b.se()?;
    b.read(1)?;
    b.read(1)?;
    b.read(1)?;
    // Optional high-profile PPS extension: transform_8x8, scaling matrices, QP.
    if b.remaining() == 0 {
        return None;
    }
    if b.remaining() > 8 || (b.bytes[b.pos / 8] << (b.pos % 8)) != 0x80 {
        b.read(1)?;
        if b.read(1)? != 0 {
            return None;
        }
        b.se()?;
    }
    b.trailing()?;
    Some((sps_id, pps_id))
}
fn hrd(b: &mut Bits<'_>) -> Option<()> {
    let count = b.ue()?;
    if count > 31 {
        return None;
    }
    b.read(8)?;
    for _ in 0..=count {
        b.ue()?;
        b.ue()?;
        b.read(1)?;
    }
    b.read(20)?;
    Some(())
}
fn vui(b: &mut Bits<'_>) -> Option<()> {
    if b.read(1)? != 0 && b.read(8)? == 255 {
        b.read(16)?;
        b.read(16)?;
    }
    if b.read(1)? != 0 {
        b.read(1)?;
    }
    if b.read(1)? != 0 {
        b.read(4)?;
        if b.read(1)? != 0 {
            b.read(24)?;
        }
    }
    if b.read(1)? != 0 {
        b.ue()?;
        b.ue()?;
    }
    if b.read(1)? != 0 {
        b.read(32)?;
        b.read(32)?;
        b.read(1)?;
    }
    let nal = b.read(1)? != 0;
    if nal {
        hrd(b)?;
    }
    let vcl = b.read(1)? != 0;
    if vcl {
        hrd(b)?;
    }
    if nal || vcl {
        b.read(1)?;
    }
    b.read(1)?;
    if b.read(1)? != 0 {
        b.read(1)?;
        for _ in 0..6 {
            b.ue()?;
        }
    }
    Some(())
}

/// Actual description sample framing, not the framing of an encoder packet.
#[must_use]
pub fn avc_description_framing() -> NalFraming {
    NalFraming::LengthPrefixed { nal_length_size: 4 }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn real_rap() -> Vec<u8> {
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=size=64x64:rate=25",
                "-frames:v",
                "1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-f",
                "h264",
                "pipe:1",
            ])
            .output()
            .expect("real libx264 encoder opens and emits");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
    #[test]
    fn real_libx264_annex_b_to_avcc_roundtrip() {
        let bytes = real_rap();
        let before = bytes.clone();
        let config = AvcDecoderConfig::from_initial_rap(&bytes).expect("actual SPS/PPS and IDR");
        assert!(config.codec.starts_with("avc1."));
        assert_eq!(
            AvcDecoderConfig::from_extradata(&config.description),
            Some(config.clone())
        );
        assert_eq!(bytes, before, "metadata does not mutate encoding");
        assert_eq!(
            config.codec,
            format!(
                "avc1.{:02X}{:02X}{:02X}",
                config.description[1], config.description[2], config.description[3]
            )
        );
        for length in 0..config.description.len() {
            assert!(AvcDecoderConfig::from_extradata(&config.description[..length]).is_none());
        }
    }
    #[test]
    fn malformed_mixed_and_truncated_parameter_sets_fail_closed() {
        let bytes = real_rap();
        let nals = annex_b_nals(&bytes).expect("NAL units");
        let mut sets = Vec::new();
        for nal in nals.iter().filter(|nal| matches!(nal[0] & 31, 7 | 8)) {
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(nal);
        }
        assert!(AvcDecoderConfig::from_initial_rap(&sets).is_none());
        for length in 0..sets.len() {
            assert!(AvcDecoderConfig::from_extradata(&sets[..length]).is_none());
        }
        let mut mixed = bytes.clone();
        mixed.extend_from_slice(&[0, 0, 1, 0x41, 0xff]);
        assert!(AvcDecoderConfig::from_initial_rap(&mixed).is_none());
        let mut duplicate = bytes.clone();
        duplicate.extend_from_slice(&sets);
        assert!(AvcDecoderConfig::from_initial_rap(&duplicate).is_none());
        for bad in [
            &[0, 0, 1, 0x67][..],
            &[0, 0, 1, 0x40, 1][..],
            &[1, 2, 3][..],
        ] {
            assert!(AvcDecoderConfig::from_extradata(bad).is_none());
        }
    }
}
