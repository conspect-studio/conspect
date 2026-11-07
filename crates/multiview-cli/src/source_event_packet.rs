//! Bounded demux provenance fence for native metadata, independent of decoder delay.
//!
//! Only transport packet timestamps inside two ordered decoded anchors of the
//! same mapping generation are actionable. Cue-announced times are never inputs.
//! Ambiguous/reordered/missing provenance is observational, not a break trigger.
use multiview_input::normalize::WrapBits;
use std::collections::VecDeque;

/// Metadata codec identified by the demuxer (never guessed from payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataKind {
    /// SCTE-35 section.
    Scte35,
    /// Timed ID3 packet.
    Id3,
}
/// Raw metadata plus transport provenance. Payload is separately size-bounded by ingest.
#[derive(Debug)]
pub struct MetadataPacket {
    /// Explicit demux codec.
    pub kind: MetadataKind,
    /// Original bytes, including when timing cannot be trusted.
    pub bytes: Vec<u8>,
    /// Packet PTS, NOT a splice announcement's PTS.
    pub pts: Option<i64>,
    /// Packet timestamp timebase numerator.
    pub time_base_num: i32,
    /// Packet timestamp timebase denominator.
    pub time_base_den: i32,
    /// Last video packet ordinal preceding this packet in demux order.
    pub video_fence: u64,
}
/// Result consumed on the ingest thread, never the output clock.
#[derive(Debug)]
pub struct GatedPacket {
    /// Original observational evidence.
    pub packet: MetadataPacket,
    /// True only for a proven same-generation transport bracket.
    pub trusted: bool,
}
#[derive(Clone, Copy)]
struct Anchor {
    fence: u64,
    raw: i64,
    generation: u64,
    num: i32,
    den: i32,
    wrap: WrapBits,
}
/// Fixed-budget state; no I/O, synchronization or cue-time normalization.
#[derive(Default)]
pub struct PacketGate {
    packets: VecDeque<(u64, Option<i64>)>,
    anchors: VecDeque<Anchor>,
    pending: VecDeque<MetadataPacket>,
}
impl PacketGate {
    /// Record a submitted video packet; keep history across delayed/reordered output.
    pub fn submitted(&mut self, fence: u64, pts: Option<i64>) {
        if self.packets.len() == 256 {
            self.packets.pop_front();
        }
        self.packets.push_back((fence, pts));
    }
    /// Queue at most 32 metadata packets. Returns overflow evidence to the caller.
    pub fn push(&mut self, packet: MetadataPacket) -> Option<MetadataPacket> {
        if self.pending.len() == 32 {
            return Some(packet);
        }
        self.pending.push_back(packet);
        None
    }
    /// Resolve against THIS decoded anchor, matching packet PTS rather than send time.
    /// A bracket crossing a reset, B-frame reversal, or missing timestamp fails closed.
    /// This default never applies modular arithmetic, even at 90 kHz. Use
    /// [`Self::decoded_with_wrap`] only when the source clock is explicitly known.
    pub fn decoded(
        &mut self,
        raw: Option<i64>,
        generation: Option<u64>,
        num: i32,
        den: i32,
    ) -> Vec<GatedPacket> {
        self.decoded_with_wrap(raw, generation, num, den, WrapBits::None)
    }
    /// Resolve with an explicitly proven source clock, never inferred from rate.
    /// MPEG-33 wrapping requires a 90 kHz clock and in-range anchors/transport
    /// PTS after exact timebase conversion. Other clocks use exact comparisons.
    /// Clock changes within a bracket fail closed. Payload cue times are ignored.
    pub fn decoded_with_wrap(
        &mut self,
        raw: Option<i64>,
        generation: Option<u64>,
        num: i32,
        den: i32,
        wrap: WrapBits,
    ) -> Vec<GatedPacket> {
        let anchor = raw.zip(generation).and_then(|(raw, generation)| {
            let mut matches = self.packets.iter().filter(|(_, pts)| *pts == Some(raw));
            let fence = matches.next()?.0;
            if matches.next().is_some() {
                return None;
            }
            Some(Anchor {
                fence,
                raw,
                generation,
                num,
                den,
                wrap,
            })
        });
        let Some(anchor) = anchor else {
            self.anchors.clear();
            return self
                .pending
                .drain(..)
                .map(|packet| GatedPacket {
                    packet,
                    trusted: false,
                })
                .collect();
        };
        if self.anchors.len() == 256 {
            self.anchors.pop_front();
        }
        self.anchors.push_back(anchor);
        let mut result = Vec::new();
        let mut retained = VecDeque::new();
        while let Some(packet) = self.pending.pop_front() {
            if packet.pts.is_none() || packet.time_base_num <= 0 || packet.time_base_den <= 0 {
                result.push(GatedPacket {
                    packet,
                    trusted: false,
                });
                continue;
            }
            if packet.video_fence >= anchor.fence {
                retained.push_back(packet);
                continue;
            }
            let trusted = self
                .anchors
                .iter()
                .zip(self.anchors.iter().skip(1))
                .any(|(a, b)| {
                    a.fence == packet.video_fence
                        && a.fence.checked_add(1) == Some(b.fence)
                        && a.generation == b.generation
                        && b.generation == anchor.generation
                        && a.num == b.num
                        && b.num == num
                        && a.den == b.den
                        && b.den == den
                        && a.wrap == b.wrap
                        && b.wrap == wrap
                        && in_bracket(&packet, a.raw, b.raw, num, den, wrap)
                });
            result.push(GatedPacket { packet, trusted });
        }
        self.pending = retained;
        result
    }
}
fn in_bracket(
    packet: &MetadataPacket,
    start: i64,
    end: i64,
    num: i32,
    den: i32,
    wrap: WrapBits,
) -> bool {
    if num <= 0 || den <= 0 || packet.time_base_num <= 0 || packet.time_base_den <= 0 {
        return false;
    }
    // Work in rational video ticks. 33-bit wrap is safe only when both streams
    // share that clock; use modular delta after exact timebase conversion.
    let Some(pts) = packet.pts else {
        return false;
    };
    let numerator = i128::from(pts) * i128::from(packet.time_base_num) * i128::from(den);
    let denominator = i128::from(packet.time_base_den) * i128::from(num);
    if numerator % denominator != 0 {
        return false;
    }
    let converted = numerator / denominator;
    if wrap != WrapBits::Mpeg33 || i64::from(num) * 90_000 != i64::from(den) {
        return i128::from(start) <= converted && converted <= i128::from(end);
    }
    let modulus = 1_i128 << 33;
    if !(0..modulus).contains(&i128::from(start))
        || !(0..modulus).contains(&i128::from(end))
        || !(0..modulus).contains(&converted)
        || !(0..modulus).contains(&i128::from(pts))
    {
        return false;
    }
    let delta =
        |value: i128| (value - i128::from(start) + modulus / 2).rem_euclid(modulus) - modulus / 2;
    let span = delta(i128::from(end));
    let offset = delta(numerator / denominator);
    span >= 0 && offset >= 0 && offset <= span
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(pts: Option<i64>, fence: u64) -> MetadataPacket {
        MetadataPacket {
            kind: MetadataKind::Scte35,
            bytes: vec![1],
            pts,
            time_base_num: 1,
            time_base_den: 90_000,
            video_fence: fence,
        }
    }
    fn frame(g: &mut PacketGate, fence: u64, raw: i64, generation: u64) -> Vec<GatedPacket> {
        g.submitted(fence, Some(raw));
        g.decoded(Some(raw), Some(generation), 1, 90_000)
    }
    #[test]
    fn new_metadata_before_delayed_old_video_never_uses_old_mapping() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 900_000, 0);
        g.submitted(2, Some(936_000));
        g.push(packet(Some(18_000), 2));
        assert!(g.decoded(Some(936_000), Some(0), 1, 90_000).is_empty());
        let result = frame(&mut g, 3, 0, 1);
        assert_eq!(result.len(), 1);
        assert!(!result[0].trusted);
    }
    #[test]
    fn old_metadata_cannot_use_new_mapping() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 936_000, 0);
        g.push(packet(Some(936_000), 1));
        let result = frame(&mut g, 2, 0, 1);
        assert!(!result[0].trusted);
    }
    #[test]
    fn future_announcement_payload_does_not_define_transport_epoch() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 900_000, 0);
        let mut p = packet(Some(901_000), 1);
        p.bytes = 90_000_000_i64.to_be_bytes().to_vec();
        g.push(p);
        let result = frame(&mut g, 2, 903_600, 0);
        assert!(result[0].trusted);
    }
    /// libav stamps a PES-less SCTE-35 section with the last PCR, which leads
    /// the video PTS by the mux delay, so it lands before its bracket. This is
    /// why real sections arrive here as untrusted: the journal derives their
    /// times separately, and this gate stays the only path to a splice boundary.
    #[test]
    fn a_pcr_stamped_section_ahead_of_its_video_pts_is_not_trusted() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 133_200, 0);
        g.push(packet(Some(113_400), 1));
        let result = frame(&mut g, 2, 136_800, 0);
        assert_eq!(result.len(), 1);
        assert!(!result[0].trusted);
    }
    #[test]
    fn missing_transport_pts_is_observational_even_with_plausible_payload() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 900_000, 0);
        g.push(packet(None, 1));
        let result = frame(&mut g, 2, 903_600, 0);
        assert!(!result[0].trusted);
        assert_eq!(result[0].packet.bytes, vec![1]);
    }
    #[test]
    fn delayed_output_uses_matching_packet_not_latest_submitted() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 900_000, 0);
        g.submitted(2, Some(903_600));
        g.submitted(3, Some(907_200));
        g.push(packet(Some(905_000), 2));
        assert!(g.decoded(Some(903_600), Some(0), 1, 90_000).is_empty());
        assert!(g.decoded(Some(907_200), Some(0), 1, 90_000)[0].trusted);
    }
    #[test]
    fn duplicate_raw_packet_pts_fail_closed() {
        let mut g = PacketGate::default();
        g.submitted(1, Some(0));
        g.submitted(2, Some(0));
        g.push(packet(Some(0), 1));
        assert!(!g.decoded(Some(0), Some(0), 1, 90_000)[0].trusted);
    }
    #[test]
    fn b_frame_reordering_is_not_a_packet_epoch_reset() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 900_000, 0);
        g.submitted(2, Some(907_200));
        g.submitted(3, Some(903_600));
        g.push(packet(Some(901_000), 1));
        // Output jumps over a packet ordinal: do not invent a safe bracket.
        assert!(!g.decoded(Some(903_600), Some(0), 1, 90_000)[0].trusted);
        g.push(packet(Some(905_000), 2));
        assert!(g.decoded(Some(907_200), Some(0), 1, 90_000).is_empty());
        assert!(!frame(&mut g, 4, 910_800, 0)[0].trusted);
    }
    #[test]
    fn packet_timebase_is_rescaled_and_mpeg_wrap_is_preserved() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 900_000, 0);
        let mut p = packet(Some(10_010), 1);
        p.time_base_den = 1000;
        g.push(p);
        assert!(frame(&mut g, 2, 903_600, 0)[0].trusted);
        let mut g = PacketGate::default();
        g.submitted(1, Some((1_i64 << 33) - 1800));
        g.decoded_with_wrap(
            Some((1_i64 << 33) - 1800),
            Some(0),
            1,
            90_000,
            WrapBits::Mpeg33,
        );
        g.push(packet(Some(0), 1));
        g.submitted(2, Some(1800));
        assert!(g.decoded_with_wrap(Some(1800), Some(0), 1, 90_000, WrapBits::Mpeg33)[0].trusted);
    }
    #[test]
    fn default_clock_never_aliases_timestamps_modulo_mpeg_wrap() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 0, 0);
        g.push(packet(Some((1_i64 << 33) + 100), 1));
        assert!(!frame(&mut g, 2, 200, 0)[0].trusted);
    }
    #[test]
    fn non_mpeg_clock_reset_is_not_a_wrap() {
        let mut g = PacketGate::default();
        g.submitted(1, Some((1_i64 << 33) - 10));
        g.decoded(Some((1_i64 << 33) - 10), Some(0), 1, 1000);
        let mut p = packet(Some(0), 1);
        p.time_base_den = 1000;
        g.push(p);
        g.submitted(2, Some(10));
        assert!(!g.decoded(Some(10), Some(0), 1, 1000)[0].trusted);
    }
    #[test]
    fn explicit_wrap_rejects_range_and_half_modulus_ambiguity() {
        let modulus = 1_i64 << 33;
        for (start, end, pts) in [
            (0, 200, modulus + 100),
            (-1, 200, 100),
            (0, modulus + 200, 100),
            (0, modulus / 2, 100),
            (0, 200, modulus / 2),
        ] {
            assert!(!in_bracket(
                &packet(Some(pts), 1),
                start,
                end,
                1,
                90_000,
                WrapBits::Mpeg33
            ));
        }
        assert!(!in_bracket(
            &packet(Some(0), 1),
            modulus - 1800,
            1800,
            1,
            90_000,
            WrapBits::None
        ));
        let mut p = packet(Some(0), 1);
        p.time_base_den = 1000;
        assert!(!in_bracket(&p, modulus - 10, 10, 1, 1000, WrapBits::Mpeg33));
        p.pts = Some(101);
        assert!(in_bracket(&p, 100, 102, 1, 1000, WrapBits::None));
        p.time_base_den = 7;
        assert!(!in_bracket(&p, 0, 2_000_000, 1, 90_000, WrapBits::Mpeg33));
    }
    #[test]
    fn clock_mode_change_inside_bracket_fails_closed() {
        let mut g = PacketGate::default();
        frame(&mut g, 1, 0, 0);
        g.push(packet(Some(100), 1));
        g.submitted(2, Some(200));
        assert!(!g.decoded_with_wrap(Some(200), Some(0), 1, 90_000, WrapBits::Mpeg33)[0].trusted);
    }
    #[test]
    fn queue_is_bounded_and_id3_keeps_explicit_kind() {
        let mut g = PacketGate::default();
        for _ in 0..32 {
            assert!(g.push(packet(None, 0)).is_none());
        }
        let mut p = packet(None, 0);
        p.kind = MetadataKind::Id3;
        assert_eq!(g.push(p).unwrap().kind, MetadataKind::Id3);
    }
}
