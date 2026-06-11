//! Property tests for the pure overlay model: scope-bucketing conservation,
//! timecode frame-count round-trips (including 29.97 drop-frame), safe-area
//! concentricity, and the caption-probe timeout monotonicity. These complement
//! the example-based tests with universally-quantified invariants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::MediaTime;
use multiview_overlay::caption_probe::{CaptionPresence, CaptionProbe};
use multiview_overlay::resolve::CanvasSize;
use multiview_overlay::safearea::SafeAreaKind;
use multiview_overlay::scopes::{Histogram, RgbParade};
use multiview_overlay::timecode::{TcRate, Timecode};
use proptest::prelude::*;

proptest! {
    /// A histogram conserves sample count: the sum of all bins equals the number
    /// of samples fed in, for any sample data and bin count.
    #[test]
    fn histogram_conserves_sample_count(samples in prop::collection::vec(any::<u8>(), 0..512)) {
        let n = u64::try_from(samples.len()).unwrap();
        let h256 = Histogram::<256>::from_luma(&samples);
        prop_assert_eq!(h256.total(), n);
        let h64 = Histogram::<64>::from_luma(&samples);
        prop_assert_eq!(h64.total(), n);
        let h32 = Histogram::<32>::from_luma(&samples);
        prop_assert_eq!(h32.total(), n);
    }

    /// Every RGB-parade channel histogram totals the pixel count.
    #[test]
    fn rgb_parade_channels_total_pixel_count(
        rgb in prop::collection::vec(any::<u8>(), 0..510).prop_map(|mut v| {
            v.truncate(v.len() - v.len() % 3);
            v
        })
    ) {
        let parade = RgbParade::<256>::from_rgb(&rgb).unwrap();
        let pixels = u64::try_from(rgb.len() / 3).unwrap();
        prop_assert_eq!(parade.red.total(), pixels);
        prop_assert_eq!(parade.green.total(), pixels);
        prop_assert_eq!(parade.blue.total(), pixels);
    }

    /// Non-drop timecode round-trips through frame count exactly for any count
    /// within a day at 25/30 fps.
    #[test]
    fn non_drop_timecode_round_trips(count in 0u64..(30 * 60 * 60 * 24)) {
        for rate in [TcRate::Fps25, TcRate::Fps30, TcRate::Fps24] {
            let frames_per_day = u64::from(rate.nominal_frames()) * 60 * 60 * 24;
            let c = count % frames_per_day;
            let tc = Timecode::from_frame_count(c, rate);
            prop_assert_eq!(tc.to_frame_count(rate), c);
        }
    }

    /// 29.97 drop-frame timecode round-trips through frame count exactly for any
    /// count within a day.
    #[test]
    fn drop_frame_timecode_round_trips(count in 0u64..107_892) {
        let rate = TcRate::Fps2997Drop;
        let tc = Timecode::from_frame_count(count, rate);
        prop_assert_eq!(tc.to_frame_count(rate), count);
    }

    /// The frames field never exceeds the nominal rate; the time fields stay in
    /// range for any frame count.
    #[test]
    fn timecode_fields_are_in_range(count in 0u64..10_000_000, drop in any::<bool>()) {
        let rate = if drop { TcRate::Fps2997Drop } else { TcRate::Fps30 };
        let tc = Timecode::from_frame_count(count, rate);
        prop_assert!(u32::from(tc.frames) < rate.nominal_frames());
        prop_assert!(tc.seconds < 60);
        prop_assert!(tc.minutes < 60);
        prop_assert!(tc.hours < 24);
    }

    /// Title-safe (90 %) always nests strictly inside action-safe (93 %) and both
    /// share the canvas centre, for any non-trivial canvas size.
    #[test]
    fn safe_areas_are_concentric_and_nested(w in 16u32..7680, h in 16u32..4320) {
        let canvas = CanvasSize::new(w, h);
        let action = SafeAreaKind::ActionSafe.rect(canvas);
        let title = SafeAreaKind::TitleSafe.rect(canvas);
        // Shared centre (within sub-pixel tolerance).
        let acx = action.x + action.width / 2.0;
        let tcx = title.x + title.width / 2.0;
        let acy = action.y + action.height / 2.0;
        let tcy = title.y + title.height / 2.0;
        prop_assert!((acx - tcx).abs() < 1e-2);
        prop_assert!((acy - tcy).abs() < 1e-2);
        // Title-safe nests inside action-safe.
        prop_assert!(title.x >= action.x);
        prop_assert!(title.right() <= action.right());
        prop_assert!(title.y >= action.y);
        prop_assert!(title.bottom() <= action.bottom());
    }

    /// The caption probe is "present" iff the latest tick time is before the
    /// deadline set by the last observation; ticking never resurrects a present
    /// state without an observation.
    #[test]
    fn caption_probe_loss_is_sticky_without_observation(
        timeout_ms in 1i64..5000,
        ticks in prop::collection::vec(0i64..20_000, 1..16),
    ) {
        let mut probe = CaptionProbe::new(MediaTime::from_nanos(timeout_ms * 1_000_000));
        probe.observe_caption(MediaTime::ZERO);
        let mut last_was_lost = false;
        let mut t = 0i64;
        for dt in ticks {
            t += dt; // monotonic non-decreasing tick times
            probe.tick(MediaTime::from_nanos(t * 1_000_000));
            if probe.presence() == CaptionPresence::Lost {
                last_was_lost = true;
            }
            // Once lost, it stays lost until an observation (none happen here).
            if last_was_lost {
                prop_assert_eq!(probe.presence(), CaptionPresence::Lost);
            }
        }
    }
}
