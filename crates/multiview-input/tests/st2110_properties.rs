//! Property tests for the ST 2110 depacketizers (never panic on hostile input)
//! and the **ST 2022-7 hitless dual-path reconstruction** algorithm: any
//! interleaving / loss pattern of two redundant streams must merge to the correct
//! gap-minimized in-order stream.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeSet;

use multiview_input::st2022_6::Hbrmt;
use multiview_input::st2022_7::{HitlessReconstructor, Path, PushOutcome};
use multiview_input::st2110::rtp::RtpPacket;
use multiview_input::st2110::v20::V20Payload;
use multiview_input::st2110::v40::V40Payload;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Depacketizers never panic on arbitrary input (they parse untrusted network
// bytes; malformed input must surface a typed error, never a panic).
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rtp_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(pkt) = RtpPacket::parse(&bytes) {
            // An accepted packet's payload is a sub-slice of the input.
            prop_assert!(pkt.payload.len() <= bytes.len());
        }
    }

    #[test]
    fn v20_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..128),
        seq in any::<u16>(),
    ) {
        if let Ok(decoded) = V20Payload::parse(&bytes, seq) {
            // Low 16 bits of the reconstructed sequence equal the RTP sequence.
            prop_assert_eq!((decoded.full_sequence & 0xFFFF) as u16, seq);
            // Every segment's range stays inside the payload.
            for seg in &decoded.segments {
                let r = seg.data_range();
                prop_assert!(r.end <= bytes.len());
            }
        }
    }

    #[test]
    fn v40_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..128),
        seq in any::<u16>(),
    ) {
        let _ = V40Payload::parse(&bytes, seq);
    }

    #[test]
    fn hbrmt_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(h) = Hbrmt::parse(&bytes) {
            prop_assert!(h.sdi.len() <= bytes.len());
        }
    }
}

// ---------------------------------------------------------------------------
// ST 2022-7 hitless reconstruction — the load-bearing property.
//
// Model: a "true" stream of sequence numbers s0, s0+1, ... s0+n-1. Each is sent
// on path A and/or path B (lossy: each path independently drops some). The two
// per-path arrival streams are interleaved in an arbitrary order. After feeding
// every arrival through the reconstructor and draining, the merged output must
// be the in-order, de-duplicated set of sequences that arrived on AT LEAST ONE
// path — provided the reorder window was large enough to hold the in-flight
// span (we size it to cover the whole run so no sequence is evicted as a forced
// gap, isolating the merge/de-dup/reorder correctness).
// ---------------------------------------------------------------------------

/// A scheduled arrival: which sequence, on which path, in interleaving order.
#[derive(Debug, Clone, Copy)]
struct Arrival {
    seq: u16,
    path: Path,
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// With a window covering the whole run, the merge yields exactly the set of
    /// sequences present on at least one path, strictly increasing, no
    /// duplicates.
    #[test]
    fn hitless_merges_two_lossy_paths(
        base in 0u16..40000,
        // For each of up to `n` consecutive sequences, a 2-bit mask: bit0 = sent
        // on A, bit1 = sent on B (0 => lost on both).
        masks in proptest::collection::vec(0u8..4, 1..40),
        // A permutation seed to interleave arrivals.
        shuffle in proptest::collection::vec(any::<u32>(), 1..120),
    ) {
        // Build the arrival list from the masks.
        let mut arrivals: Vec<Arrival> = Vec::new();
        let mut present: BTreeSet<u16> = BTreeSet::new();
        for (i, &mask) in masks.iter().enumerate() {
            let seq = base.wrapping_add(i as u16);
            if mask & 0b01 != 0 {
                arrivals.push(Arrival { seq, path: Path::A });
                present.insert(seq);
            }
            if mask & 0b10 != 0 {
                arrivals.push(Arrival { seq, path: Path::B });
                present.insert(seq);
            }
        }

        // Deterministically shuffle the arrivals using the shuffle seed (a simple
        // Fisher-Yates driven by the seed vector — keeps it reproducible).
        let mut order: Vec<usize> = (0..arrivals.len()).collect();
        let len = order.len();
        for k in (1..len).rev() {
            let pick = shuffle[k % shuffle.len()] as usize % (k + 1);
            order.swap(k, pick);
        }

        // Window large enough to hold the whole in-flight span so nothing is
        // evicted as a forced gap (isolates the merge/de-dup/ordering correctness
        // from release-timing). We feed every arrival in the (arbitrary) order,
        // then drain once at flush — proving the MERGED SET is correct for ANY
        // interleaving, which is the ST 2022-7 contract.
        let window = masks.len() + 4;
        let mut recon: HitlessReconstructor<u16> = HitlessReconstructor::new(window);

        for &oi in &order {
            let a = arrivals[oi];
            // First copy of a sequence is accepted; a redundant copy is a
            // duplicate (or too-late if its slot already advanced) — both
            // correctly discard it.
            let _outcome: PushOutcome = recon.push(a.path, a.seq, a.seq);
        }
        let merged: Vec<u16> = recon.flush();

        // 1. Strictly increasing (in-order).
        for w in merged.windows(2) {
            prop_assert!(w[0] < w[1], "not strictly increasing: {} !< {}", w[0], w[1]);
        }
        // 2. No duplicates.
        let unique: BTreeSet<u16> = merged.iter().copied().collect();
        prop_assert_eq!(unique.len(), merged.len(), "duplicate in merged output");
        // 3. Exactly the sequences present on at least one path.
        let expected: Vec<u16> = present.iter().copied().collect();
        prop_assert_eq!(merged, expected);
    }

    /// A duplicate copy of every sequence (both paths carry everything) yields
    /// each sequence exactly once — the no-hit redundancy case.
    #[test]
    fn hitless_dedups_full_redundancy(
        base in 0u16..40000,
        n in 1usize..30,
    ) {
        let window = n + 4;
        let mut recon: HitlessReconstructor<u16> = HitlessReconstructor::new(window);
        let mut merged = Vec::new();
        // Interleave A then B for each sequence.
        for i in 0..n {
            let seq = base.wrapping_add(i as u16);
            let o1 = recon.push(Path::A, seq, seq);
            let o2 = recon.push(Path::B, seq, seq);
            prop_assert_eq!(o1, PushOutcome::Accepted);
            prop_assert_eq!(o2, PushOutcome::Duplicate);
            merged.extend(recon.drain());
        }
        merged.extend(recon.flush());
        let expected: Vec<u16> = (0..n).map(|i| base.wrapping_add(i as u16)).collect();
        prop_assert_eq!(merged, expected);
    }

    /// Continuous-drain streaming: when per-path reordering stays within the
    /// hold-back depth, draining after every push still releases EVERY present
    /// sequence exactly once, strictly in order — the realistic live path.
    #[test]
    fn hitless_streaming_bounded_reorder_loses_nothing(
        base in 0u16..40000,
        // Per consecutive sequence: bit0 = on A, bit1 = on B (never both-lost so
        // every sequence is present on at least one path).
        masks in proptest::collection::vec(1u8..4, 1..50),
        // Small local jitter (0..=depth) applied to each arrival's position.
        jitter in proptest::collection::vec(0u8..4, 1..200),
    ) {
        // Build arrivals (A before B for a given seq), each tagged with its
        // emission index, then reorder by a key that perturbs each index by a
        // *bounded* jitter (0..=3). A stable sort on `index*4 + jitter` displaces
        // any arrival by at most a few positions — never a cascading swap — so
        // reordering stays well within the hold-back depth.
        let mut tagged: Vec<(usize, u16)> = Vec::new();
        let mut present: BTreeSet<u16> = BTreeSet::new();
        for (i, &mask) in masks.iter().enumerate() {
            let seq = base.wrapping_add(i as u16);
            present.insert(seq);
            if mask & 0b01 != 0 { tagged.push((tagged.len(), seq)); }
            if mask & 0b10 != 0 { tagged.push((tagged.len(), seq)); }
        }
        let mut keyed: Vec<(usize, u16)> = tagged
            .iter()
            .map(|&(idx, seq)| {
                let j = (jitter[idx % jitter.len()] % 4) as usize;
                (idx * 4 + j, seq)
            })
            .collect();
        keyed.sort_by_key(|&(k, _)| k);
        let arrivals: Vec<u16> = keyed.into_iter().map(|(_, seq)| seq).collect();

        // Window comfortably covers the whole run; depth (capacity/2) >> the
        // bounded jitter, so nothing is lost even with continuous draining.
        let cap = masks.len() + 16;
        let mut recon: HitlessReconstructor<u16> = HitlessReconstructor::new(cap);
        let mut merged: Vec<u16> = Vec::new();
        for seq in arrivals {
            let _ = recon.push(Path::A, seq, seq);
            merged.extend(recon.drain());
        }
        merged.extend(recon.flush());

        // Strictly increasing, no duplicates, exactly the present set.
        for w in merged.windows(2) {
            prop_assert!(w[0] < w[1]);
        }
        let expected: Vec<u16> = present.iter().copied().collect();
        prop_assert_eq!(merged, expected);
    }

    /// The reorder window is a hard bound: no matter the input, the number of
    /// buffered packets never exceeds the configured capacity.
    #[test]
    fn hitless_window_is_bounded(
        seqs in proptest::collection::vec(0u16..200, 1..300),
        cap in 1usize..16,
    ) {
        let mut recon: HitlessReconstructor<u16> = HitlessReconstructor::new(cap);
        for &s in &seqs {
            recon.push(Path::A, s, s);
            prop_assert!(recon.buffered() <= cap, "buffered {} > cap {}", recon.buffered(), cap);
            // Drain opportunistically so it mirrors real use.
            let _ = recon.drain();
            prop_assert!(recon.buffered() <= cap);
        }
    }
}
