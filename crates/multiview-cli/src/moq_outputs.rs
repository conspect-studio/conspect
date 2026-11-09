//! Shared encoded-window conversion, off the production clock. No encoder or synthetic feed.
/// The real `AuthorizedFeedFactory` over the production output-window registry.
pub(crate) mod factory;
/// Effective decoder metadata and pre-demand authorization.
pub mod metadata;

use bytes::Bytes;
use multiview_ffmpeg::packet::{EncodedPacket, StreamKind};
use multiview_moq::{av::AvWindow, Packet, Window};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

/// Exact encoder timebase, not floating-point frame rate.
#[derive(Clone, Copy)]
pub struct Timebase {
    /// Numerator in seconds.
    pub numerator: u32,
    /// Denominator in seconds.
    pub denominator: u32,
}
/// Immutable stream mapping obtained from the effective binding, not desired config.
#[derive(Clone)]
pub struct TrackMapping {
    /// Video encoder timebase.
    pub video: Timebase,
    /// Main mixed audio track name and timebase, if selected.
    pub mixed_audio: Option<(String, Timebase)>,
    /// Selected discrete audio identities and encoder timebases.
    pub discrete_audio: BTreeMap<u16, (String, Timebase)>,
}
/// Whole-window conversion rejects before any packet reaches the transport.
#[derive(Debug, PartialEq, Eq)]
pub enum ConversionError {
    /// The effective binding changed.
    Generation,
    /// The window exceeds bounded count, bytes or collection span.
    Budget,
    /// Missing, negative, reordered or unsupported timestamps.
    Timestamp,
    /// Empty payload or unconfigured elementary stream.
    Stream,
}
struct RetainedPacket<P> {
    packet: EncodedPacket,
    // Retain the actual whole-window BatchLease through every nonempty downstream
    // Bytes view. Empty slices retain no media and may discard their owner.
    _permit: Arc<P>,
}
impl<P> AsRef<[u8]> for RetainedPacket<P> {
    fn as_ref(&self) -> &[u8] {
        self.packet.payload().unwrap_or_default()
    }
}
fn micros(pts: i64, base: Timebase) -> Result<u64, ConversionError> {
    if pts < 0 || base.numerator == 0 || base.denominator == 0 {
        return Err(ConversionError::Timestamp);
    }
    let scaled = i128::from(pts) * i128::from(base.numerator) * 1_000_000;
    let micros = u64::try_from(scaled / i128::from(base.denominator))
        .map_err(|_| ConversionError::Timestamp)?;
    // MoQ timestamps use QUIC variable integers, whose maximum is 2^62 - 1.
    // Reject before returning an admitted window, not later in transport framing.
    if micros > (1_u64 << 62) - 1 {
        return Err(ConversionError::Timestamp);
    }
    Ok(micros)
}

struct AvcCopyBudget {
    used: std::sync::atomic::AtomicUsize,
    limit: usize,
}
impl AvcCopyBudget {
    const fn new(limit: usize) -> Self {
        Self {
            used: std::sync::atomic::AtomicUsize::new(0),
            limit,
        }
    }
    fn reserve(&'static self, bytes: usize) -> Result<AvcCopyCharge, ConversionError> {
        self.used
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |used| used.checked_add(bytes).filter(|total| *total <= self.limit),
            )
            .map_err(|_| ConversionError::Budget)?;
        Ok(AvcCopyCharge {
            budget: self,
            bytes,
        })
    }
}
static AVC_COPY_BUDGET: AvcCopyBudget = AvcCopyBudget::new(64 * 1024 * 1024);
struct AvcCopyCharge {
    budget: &'static AvcCopyBudget,
    bytes: usize,
}
impl Drop for AvcCopyCharge {
    fn drop(&mut self) {
        self.budget
            .used
            .fetch_sub(self.bytes, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Convert an Annex-B H.264 access unit to the four-byte NAL lengths required
/// by an avcC WebCodecs description. This copies framing, never encodes media.
/// The resulting owner retains its source bytes and therefore the BatchLease.
pub(crate) fn avcc_access_unit(source: Bytes) -> Result<Bytes, ConversionError> {
    avcc_access_unit_with_budget(source, &AVC_COPY_BUDGET)
}
fn avcc_access_unit_with_budget(
    source: Bytes,
    budget: &'static AvcCopyBudget,
) -> Result<Bytes, ConversionError> {
    fn start(bytes: &[u8], at: usize) -> Option<usize> {
        if bytes.get(at..at + 4) == Some(&[0, 0, 0, 1]) {
            Some(4)
        } else if bytes.get(at..at + 3) == Some(&[0, 0, 1]) {
            Some(3)
        } else {
            None
        }
    }
    if source.len() > 4 * 1024 * 1024 {
        return Err(ConversionError::Budget);
    }
    let mut cursor = 0;
    let mut units = Vec::new();
    while cursor < source.len() {
        let prefix = start(&source, cursor).ok_or(ConversionError::Stream)?;
        let begin = cursor + prefix;
        let mut end = begin;
        while end < source.len() && start(&source, end).is_none() {
            end += 1;
        }
        let mut nal_end = end;
        while nal_end > begin && source[nal_end - 1] == 0 {
            nal_end -= 1;
        }
        if nal_end == begin
            || units.len() >= 512
            || source[begin] & 0x80 != 0
            || source[begin] & 0x1f == 0
            || source[begin] & 0x1f > 23
        {
            return Err(ConversionError::Stream);
        }
        units.push((begin, nal_end));
        cursor = end;
    }
    if units.is_empty() {
        return Err(ConversionError::Stream);
    }
    let size = units
        .iter()
        .try_fold(0usize, |size, (begin, end)| {
            size.checked_add(4 + end - begin)
        })
        .ok_or(ConversionError::Budget)?;
    if size > 4 * 1024 * 1024 {
        return Err(ConversionError::Budget);
    }
    let charge = budget.reserve(size)?;
    struct Framed {
        data: Vec<u8>,
        _source: Bytes,
        _charge: AvcCopyCharge,
    }
    impl AsRef<[u8]> for Framed {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
    let mut data = Vec::new();
    data.try_reserve_exact(size)
        .map_err(|_| ConversionError::Budget)?;
    for (begin, end) in units {
        data.extend_from_slice(
            &u32::try_from(end - begin)
                .map_err(|_| ConversionError::Budget)?
                .to_be_bytes(),
        );
        data.extend_from_slice(&source[begin..end]);
    }
    Ok(Bytes::from_owner(Framed {
        data,
        _source: source,
        _charge: charge,
    }))
}

pub(crate) fn frame_avcc(
    window: &mut AvWindow,
    recovery: multiview_output::batch::RecoveryGate,
) -> Result<(), ConversionError> {
    if matches!(
        recovery,
        multiview_output::batch::RecoveryGate::StrictRap {
            codec: multiview_ffmpeg::CodecKind::H264,
            framing: multiview_ffmpeg::NalFraming::AnnexB
        }
    ) {
        for packet in &mut window.video.packets {
            packet.payload = avcc_access_unit(std::mem::take(&mut packet.payload))?;
        }
    }
    Ok(())
}

/// Consume an admitted `DeliveryWindow` payload and retain its real `BatchLease`.
/// The caller moves `DeliveryWindow.packets` and supplies its one shared permit;
/// it must not reconstruct a window from an independently accepted packet prefix.
/// `verified_rap` must inspect codec initialization and framing, never only a key flag.
/// Nonempty payload clones/slices retain the packet and permit. An empty `Bytes`
/// slice retains no media and is not required to retain either owner. Empty input
/// packets are rejected; no sentinel bytes are inserted into encoded media.
///
/// # Errors
/// Rejects the complete window for generation, stream, timestamp or budget mismatch.
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)] // Consume ownership so rejection releases the caller's permit.
pub fn convert_window<P: Send + Sync + 'static>(
    packets: Vec<EncodedPacket>,
    permit: Arc<P>,
    generation: u64,
    expected_generation: u64,
    media_span: Duration,
    mapping: &TrackMapping,
    verified_rap: impl Fn(&EncodedPacket) -> bool,
) -> Result<AvWindow, ConversionError> {
    if generation != expected_generation {
        return Err(ConversionError::Generation);
    }
    if packets.is_empty() || packets.len() > 512 || media_span > Duration::from_millis(250) {
        return Err(ConversionError::Budget);
    }
    let mut total = 0usize;
    // Validate the entire batch before retaining or publishing any payload.
    let mut metadata = Vec::with_capacity(packets.len());
    for packet in &packets {
        total = total
            .checked_add(packet.len())
            .ok_or(ConversionError::Budget)?;
        if total > 4 * 1024 * 1024 || packet.is_empty() {
            return Err(ConversionError::Budget);
        }
        let (track, base) = match packet.kind() {
            StreamKind::Video => (None, mapping.video),
            StreamKind::Audio => {
                let (name, base) = mapping
                    .mixed_audio
                    .as_ref()
                    .ok_or(ConversionError::Stream)?;
                (Some(name.clone()), *base)
            }
            StreamKind::AudioTrack(id) => {
                let (name, base) = mapping
                    .discrete_audio
                    .get(&id)
                    .ok_or(ConversionError::Stream)?;
                (Some(name.clone()), *base)
            }
            _ => return Err(ConversionError::Stream),
        };
        let pts = packet.pts().ok_or(ConversionError::Timestamp)?;
        // Current hang mapping carries presentation timestamps only. Refuse B-frame
        // decode-order streams rather than silently publish a corrupt dependency chain.
        if packet.dts().is_some_and(|dts| dts != pts) {
            return Err(ConversionError::Timestamp);
        }
        metadata.push((track, micros(pts, base)?, verified_rap(packet)));
    }
    let mut audio = BTreeMap::new();
    for (name, _) in mapping
        .mixed_audio
        .iter()
        .chain(mapping.discrete_audio.values())
    {
        if audio.insert(name.clone(), Vec::new()).is_some() {
            return Err(ConversionError::Stream);
        }
    }
    let mut video = Vec::new();
    for (packet, (track, pts_us, rap)) in packets.into_iter().zip(metadata) {
        let packet = Packet {
            payload: Bytes::from_owner(RetainedPacket {
                packet,
                _permit: permit.clone(),
            }),
            pts_us,
            verified_rap: rap,
        };
        match track {
            None => video.push(packet),
            Some(name) => audio
                .get_mut(&name)
                .ok_or(ConversionError::Stream)?
                .push(packet),
        }
    }
    let start = video.first().ok_or(ConversionError::Stream)?.pts_us;
    for packets in std::iter::once(&video).chain(audio.values()) {
        let mut previous = None;
        for packet in packets {
            if previous.is_some_and(|pts| packet.pts_us <= pts)
                || packet.pts_us < start
                || packet.pts_us - start > 2_000_000
            {
                return Err(ConversionError::Timestamp);
            }
            previous = Some(packet.pts_us);
        }
    }
    Ok(AvWindow {
        video: Window {
            generation,
            packets: video,
        },
        audio,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Permit(Arc<AtomicUsize>);
    impl Drop for Permit {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn packet(kind: StreamKind, pts: i64) -> EncodedPacket {
        let mut packet = ffmpeg_next::Packet::copy(&[0, 0, 0, 1, 0x65, 0x88]);
        packet.set_pts(Some(pts));
        packet.set_dts(Some(pts));
        EncodedPacket::with_kind(packet, kind)
    }
    fn mapping() -> TrackMapping {
        TrackMapping {
            video: Timebase {
                numerator: 1,
                denominator: 25,
            },
            mixed_audio: Some((
                "audio".into(),
                Timebase {
                    numerator: 1,
                    denominator: 48000,
                },
            )),
            discrete_audio: BTreeMap::new(),
        }
    }
    #[test]
    fn empty_view_does_not_pin_copy_allocation_or_budget() {
        static BUDGET: AvcCopyBudget = AvcCopyBudget::new(6);
        let framed =
            avcc_access_unit_with_budget(Bytes::from_static(&[0, 0, 1, 0x65, 0x88]), &BUDGET)
                .unwrap();
        let empty = framed.slice(0..0);
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 6);
        drop(framed);
        assert!(empty.is_empty());
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 0);
        drop(empty);
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn copied_framing_budget_refunds_only_after_final_clone_and_rejects_before_allocation() {
        static BUDGET: AvcCopyBudget = AvcCopyBudget::new(6);
        let source = Bytes::from_static(&[0, 0, 1, 0x65, 0x88]);
        let framed = avcc_access_unit_with_budget(source.clone(), &BUDGET).unwrap();
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 6);
        assert!(matches!(
            avcc_access_unit_with_budget(source.clone(), &BUDGET),
            Err(ConversionError::Budget)
        ));
        let slice = framed.slice(4..6);
        let clone = framed.clone();
        drop(framed);
        drop(clone);
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 6);
        drop(slice);
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 0);
        drop(avcc_access_unit_with_budget(source, &BUDGET).unwrap());
        assert_eq!(BUDGET.used.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn avcc_framing_converts_mixed_start_codes_and_retains_copy_charge() {
        let source = Bytes::from_static(&[
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ]);
        let framed = avcc_access_unit(source).unwrap();
        assert_eq!(
            &framed[..],
            &[0, 0, 0, 2, 0x67, 0x42, 0, 0, 0, 2, 0x68, 0xce, 0, 0, 0, 2, 0x65, 0x88]
        );
        let retained = framed.slice(4..6);
        assert!(
            AVC_COPY_BUDGET
                .used
                .load(std::sync::atomic::Ordering::Acquire)
                >= framed.len()
        );
        drop(framed);
        assert_eq!(&retained[..], &[0x67, 0x42]);
        drop(retained);
        assert_eq!(
            &avcc_access_unit(Bytes::from_static(&[0, 0, 1, 0x41, 0x99])).unwrap()[..],
            &[0, 0, 0, 2, 0x41, 0x99]
        );
        for malformed in [
            &[][..],
            &[0, 0, 1][..],
            &[0x65, 0x88][..],
            &[0, 0, 1, 0][..],
            &[0, 0, 1, 0x80][..],
        ] {
            assert!(avcc_access_unit(Bytes::copy_from_slice(malformed)).is_err());
        }
    }

    #[test]
    fn retains_shared_payload_and_permit_until_last_transport_clone() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let permit = Arc::new(Permit(dropped.clone()));
        let source = packet(StreamKind::Video, 1);
        let pointer = source.payload().expect("payload").as_ptr();
        let result = convert_window(
            vec![source, packet(StreamKind::Audio, 1920)],
            permit,
            7,
            7,
            Duration::from_millis(40),
            &mapping(),
            |_| true,
        )
        .expect("valid window");
        assert_eq!(result.video.packets[0].payload.as_ptr(), pointer);
        assert_eq!(result.video.packets[0].pts_us, 40000);
        assert_eq!(result.audio["audio"][0].pts_us, 40000);
        let retained = result.video.packets[0].payload.clone();
        drop(result);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(retained);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn wire_timestamp_boundary_and_overflow_release_whole_permit() {
        let mut tracks = mapping();
        tracks.video = Timebase {
            numerator: 1,
            denominator: 1_000_000,
        };
        let maximum = (1_i64 << 62) - 1;
        let accepted = convert_window(
            vec![packet(StreamKind::Video, maximum)],
            Arc::new(()),
            1,
            1,
            Duration::ZERO,
            &tracks,
            |_| true,
        )
        .expect("maximum varint");
        assert_eq!(
            accepted.video.packets[0].pts_us,
            u64::try_from(maximum).expect("positive")
        );
        let dropped = Arc::new(AtomicUsize::new(0));
        let rejected = convert_window(
            vec![
                packet(StreamKind::Video, maximum),
                packet(StreamKind::Video, maximum + 1),
            ],
            Arc::new(Permit(dropped.clone())),
            1,
            1,
            Duration::ZERO,
            &tracks,
            |_| true,
        );
        assert!(matches!(rejected, Err(ConversionError::Timestamp)));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn nonempty_slice_retains_media_but_empty_slice_does_not() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let window = convert_window(
            vec![packet(StreamKind::Video, 0)],
            Arc::new(Permit(dropped.clone())),
            1,
            1,
            Duration::ZERO,
            &mapping(),
            |_| true,
        )
        .expect("window");
        let nonempty = window.video.packets[0].payload.slice(1..2);
        let empty = window.video.packets[0].payload.slice(0..0);
        drop(window);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(nonempty);
        assert!(empty.is_empty());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn rejects_invalid_mapping_and_window_budgets() {
        let mut duplicate = mapping();
        duplicate
            .discrete_audio
            .insert(1, ("audio".into(), duplicate.video));
        let mut zero = mapping();
        zero.video.denominator = 0;
        for (tracks, packets, span, expected) in [
            (
                duplicate,
                vec![packet(StreamKind::Video, 0)],
                Duration::ZERO,
                ConversionError::Stream,
            ),
            (
                zero,
                vec![packet(StreamKind::Video, 0)],
                Duration::ZERO,
                ConversionError::Timestamp,
            ),
            (
                mapping(),
                (0..513).map(|pts| packet(StreamKind::Video, pts)).collect(),
                Duration::ZERO,
                ConversionError::Budget,
            ),
            (
                mapping(),
                vec![packet(StreamKind::Video, 0)],
                Duration::from_millis(251),
                ConversionError::Budget,
            ),
            (
                mapping(),
                vec![EncodedPacket::from_packet(ffmpeg_next::Packet::copy(
                    &vec![1; 4 * 1024 * 1024 + 1],
                ))],
                Duration::ZERO,
                ConversionError::Budget,
            ),
            (
                mapping(),
                vec![EncodedPacket::from_packet(ffmpeg_next::Packet::empty())],
                Duration::ZERO,
                ConversionError::Budget,
            ),
        ] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let result = convert_window(
                packets,
                Arc::new(Permit(dropped.clone())),
                1,
                1,
                span,
                &tracks,
                |_| true,
            );
            assert!(matches!(result, Err(error) if error == expected));
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
    }
    #[test]
    fn preserves_empty_tracks_and_rejects_reordered_timestamps() {
        let mut tracks = mapping();
        tracks.discrete_audio.insert(
            2,
            (
                "aux".into(),
                Timebase {
                    numerator: 1,
                    denominator: 48000,
                },
            ),
        );
        let result = convert_window(
            vec![packet(StreamKind::Video, 0)],
            Arc::new(()),
            1,
            1,
            Duration::ZERO,
            &tracks,
            |_| true,
        )
        .expect("window");
        assert!(result.audio["audio"].is_empty());
        assert!(result.audio["aux"].is_empty());
        let invalid = convert_window(
            vec![packet(StreamKind::Video, 1), packet(StreamKind::Video, 0)],
            Arc::new(()),
            1,
            1,
            Duration::ZERO,
            &tracks,
            |_| true,
        );
        assert!(matches!(invalid, Err(ConversionError::Timestamp)));
        let mut reordered = ffmpeg_next::Packet::copy(&[1]);
        reordered.set_pts(Some(2));
        reordered.set_dts(Some(1));
        let invalid = convert_window(
            vec![EncodedPacket::from_packet(reordered)],
            Arc::new(()),
            1,
            1,
            Duration::ZERO,
            &tracks,
            |_| true,
        );
        assert!(matches!(invalid, Err(ConversionError::Timestamp)));
    }
    #[test]
    fn rejects_generation_unknown_track_and_negative_pts_atomically() {
        for (kind, pts, generation, expected) in [
            (StreamKind::Video, 0, 2, ConversionError::Generation),
            (StreamKind::AudioTrack(9), 0, 1, ConversionError::Stream),
            (StreamKind::Video, -1, 1, ConversionError::Timestamp),
        ] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let result = convert_window(
                vec![packet(kind, pts)],
                Arc::new(Permit(dropped.clone())),
                generation,
                1,
                Duration::ZERO,
                &mapping(),
                |_| false,
            );
            assert!(matches!(result,Err(error) if error==expected));
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
    }
}
