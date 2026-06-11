//! Property tests for the media-playlist sliding window.
//!
//! The load-bearing invariant: for a window of size `w` after pushing `n`
//! segments, the playlist lists `min(n, w)` segments and the media sequence
//! advances by exactly the number evicted (`n - min(n, w)`), so the manifest is
//! always internally consistent (msn names the first listed segment).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::hls::{MediaPlaylist, Segment, SegmentType};
use proptest::prelude::*;

proptest! {
    #[test]
    fn media_sequence_tracks_evictions(
        window in 1usize..16,
        pushes in 0u32..64,
        start_msn in 0u64..1000,
    ) {
        let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
        pl.set_media_sequence(start_msn);
        pl.set_window(window);
        for i in 0..pushes {
            pl.push_segment(Segment::new(format!("seg{i}.m4s"), 2.0));
        }

        let window_u64 = u64::try_from(window).unwrap();
        let listed = u64::from(pushes).min(window_u64);
        let evicted = u64::from(pushes) - listed;

        prop_assert_eq!(u64::try_from(pl.segment_count()).unwrap(), listed);
        prop_assert_eq!(pl.media_sequence(), start_msn + evicted);

        // The rendered manifest names the first still-listed segment, whose
        // index equals `evicted`. Build the needle strings first: `prop_assert!`
        // expands `format!` through `concat!`, which cannot capture inline args.
        let out = pl.render();
        let msn_tag = format!("#EXT-X-MEDIA-SEQUENCE:{}\n", start_msn + evicted);
        prop_assert!(out.contains(&msn_tag));
        if pushes > 0 {
            let first_listed = format!("seg{evicted}.m4s\n");
            prop_assert!(out.contains(&first_listed));
            // The oldest evicted segment (index evicted-1) is gone.
            if evicted > 0 {
                let evicted_seg = format!("\nseg{}.m4s\n", evicted - 1);
                prop_assert!(!out.contains(&evicted_seg));
            }
        }
    }

    #[test]
    fn discontinuity_sequence_counts_evicted_discontinuities(
        window in 1usize..8,
        pushes in 0u32..40,
    ) {
        let mut pl = MediaPlaylist::new(SegmentType::Fmp4);
        pl.set_window(window);
        // Every third segment carries a discontinuity.
        for i in 0..pushes {
            let mut seg = Segment::new(format!("s{i}.m4s"), 2.0);
            seg.discontinuity = i % 3 == 0;
            pl.push_segment(seg);
        }

        let window_u64 = u64::try_from(window).unwrap();
        let listed = u64::from(pushes).min(window_u64);
        let evicted = u64::from(pushes) - listed;
        // Count how many of the evicted segments (indices 0..evicted) had a
        // discontinuity (index % 3 == 0).
        let expected_disc_seq =
            u64::try_from((0..evicted).filter(|i| i % 3 == 0).count()).unwrap();
        prop_assert_eq!(pl.discontinuity_sequence(), expected_disc_seq);
    }
}
