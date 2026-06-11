//! Display mode-selection policy tests (DEV-B1 / ADR-0044 §6): EDID preferred
//! mode, exact-rational refresh matching against the engine cadence (never
//! float fps — invariant #3), explicit overrides, and the CVT-RB forced-mode
//! computation for EDID-less heads. Pure functions over mock EDID mode lists —
//! no hardware, no ioctls.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::Rational;
use multiview_output::display::{
    cvt_rb_mode, refresh_matches, select_mode, DisplayModeInfo, ModeError, ModeRequest,
    SelectedMode,
};
use proptest::prelude::*;

/// Build a mock EDID mode with real-world-shaped CEA/DMT timings.
///
/// `clock_khz` and the totals determine the exact-rational refresh
/// (`clock_khz * 1000 / (htotal * vtotal)`), exactly as KMS reports it.
fn mode(width: u32, height: u32, clock_khz: u32, htotal: u32, vtotal: u32) -> DisplayModeInfo {
    DisplayModeInfo {
        width,
        height,
        clock_khz,
        hsync_start: width + 8,
        hsync_end: width + 16,
        htotal,
        vsync_start: height + 2,
        vsync_end: height + 4,
        vtotal,
        hsync_positive: true,
        vsync_positive: true,
        preferred: false,
    }
}

/// CEA-861 1080p60: 148.5 MHz / (2200 x 1125) = exactly 60 Hz.
fn mode_1080p60() -> DisplayModeInfo {
    mode(1920, 1080, 148_500, 2200, 1125)
}

/// CEA-861 1080p59.94: 148500/1.001 kHz rounds to 148352 kHz on the wire.
fn mode_1080p5994() -> DisplayModeInfo {
    mode(1920, 1080, 148_352, 2200, 1125)
}

/// CEA-861 720p60: 74.25 MHz / (1650 x 750) = exactly 60 Hz.
fn mode_720p60() -> DisplayModeInfo {
    mode(1280, 720, 74_250, 1650, 750)
}

/// CEA-861 1080p50: 148.5 MHz / (2640 x 1125) = exactly 50 Hz.
fn mode_1080p50() -> DisplayModeInfo {
    mode(1920, 1080, 148_500, 2640, 1125)
}

// ---------------------------------------------------------------------------
// Exact-rational refresh handling
// ---------------------------------------------------------------------------

#[test]
fn refresh_is_an_exact_rational_from_the_timings() {
    // 148500 * 1000 / (2200 * 1125) = 148500000 / 2475000 = exactly 60/1.
    let r = mode_1080p60().refresh();
    assert_eq!(r.reduce(), Rational::new(60, 1).reduce());
}

#[test]
fn refresh_matches_distinguishes_60_from_5994() {
    // The wire-rounded 59.94 mode (clock 148352 kHz) must match the NTSC
    // cadence 60000/1001 and must NOT match integer 60 — and vice versa.
    let ntsc = Rational::new(60_000, 1001);
    let sixty = Rational::new(60, 1);
    assert!(refresh_matches(mode_1080p5994().refresh(), ntsc));
    assert!(!refresh_matches(mode_1080p5994().refresh(), sixty));
    assert!(refresh_matches(mode_1080p60().refresh(), sixty));
    assert!(!refresh_matches(mode_1080p60().refresh(), ntsc));
}

// ---------------------------------------------------------------------------
// Auto policy: EDID preferred + exact-rational cadence match
// ---------------------------------------------------------------------------

#[test]
fn auto_picks_the_edid_preferred_mode() {
    let mut preferred = mode_1080p60();
    preferred.preferred = true;
    let modes = vec![mode_720p60(), preferred.clone(), mode_1080p50()];
    let selected = select_mode(&modes, &ModeRequest::Auto, None, None).expect("a mode");
    match selected {
        SelectedMode::Edid(m) => assert_eq!(m, preferred),
        SelectedMode::ForcedCvtRb(_) => panic!("EDID modes exist; must not force"),
    }
}

#[test]
fn auto_prefers_the_cadence_rational_match_at_the_preferred_resolution() {
    // Preferred mode is 1080p60 but the engine runs NTSC 60000/1001: the
    // 59.94 sibling at the SAME resolution wins (zero steady-state repeat/drop).
    let mut preferred = mode_1080p60();
    preferred.preferred = true;
    let modes = vec![preferred, mode_1080p5994(), mode_720p60()];
    let cadence = Rational::new(60_000, 1001);
    let selected = select_mode(&modes, &ModeRequest::Auto, None, Some(cadence)).expect("a mode");
    match selected {
        SelectedMode::Edid(m) => {
            assert_eq!((m.width, m.height), (1920, 1080));
            assert!(refresh_matches(m.refresh(), cadence));
        }
        SelectedMode::ForcedCvtRb(_) => panic!("EDID modes exist; must not force"),
    }
}

#[test]
fn auto_without_a_preferred_flag_anchors_on_the_largest_mode() {
    let modes = vec![mode_720p60(), mode_1080p50()];
    let selected = select_mode(&modes, &ModeRequest::Auto, None, None).expect("a mode");
    match selected {
        SelectedMode::Edid(m) => assert_eq!((m.width, m.height), (1920, 1080)),
        SelectedMode::ForcedCvtRb(_) => panic!("EDID modes exist; must not force"),
    }
}

// ---------------------------------------------------------------------------
// Explicit override
// ---------------------------------------------------------------------------

#[test]
fn exact_override_picks_the_matching_edid_mode() {
    let modes = vec![mode_1080p60(), mode_1080p5994(), mode_720p60()];
    let request = ModeRequest::Exact {
        width: 1280,
        height: 720,
        refresh: Rational::new(60, 1),
    };
    let selected = select_mode(&modes, &request, None, None).expect("a mode");
    match selected {
        SelectedMode::Edid(m) => assert_eq!((m.width, m.height), (1280, 720)),
        SelectedMode::ForcedCvtRb(_) => panic!("an exact EDID match exists"),
    }
}

#[test]
fn exact_override_with_no_match_is_an_error_naming_the_request() {
    let modes = vec![mode_1080p60()];
    let request = ModeRequest::Exact {
        width: 2560,
        height: 1440,
        refresh: Rational::new(60, 1),
    };
    let err = select_mode(&modes, &request, None, None).expect_err("no 1440p mode");
    assert!(
        matches!(err, ModeError::NoMatch { .. }),
        "expected NoMatch, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// CVT-RB forced fallback (EDID-less heads)
// ---------------------------------------------------------------------------

#[test]
fn edidless_head_falls_back_to_the_forced_cvt_rb_mode() {
    let forced = multiview_output::display::ForcedMode {
        width: 1920,
        height: 1080,
        refresh: Rational::new(50, 1),
    };
    let selected =
        select_mode(&[], &ModeRequest::Auto, Some(&forced), None).expect("forced fallback");
    match selected {
        SelectedMode::ForcedCvtRb(m) => {
            assert_eq!((m.width, m.height), (1920, 1080));
        }
        SelectedMode::Edid(_) => panic!("no EDID modes exist; must force"),
    }
}

#[test]
fn edidless_head_without_a_forced_mode_is_an_error() {
    let err = select_mode(&[], &ModeRequest::Auto, None, None).expect_err("nothing to scan out");
    assert!(
        matches!(err, ModeError::NoModes { .. }),
        "expected NoModes, got {err:?}"
    );
}

#[test]
fn cvt_rb_1080p60_matches_the_kernel_golden_timings() {
    // The kernel's drm_cvt_mode(1920, 1080, 60, reduced=true) — the canonical
    // 138.5 MHz reduced-blanking 1080p60 — is the golden reference.
    let m = cvt_rb_mode(1920, 1080, Rational::new(60, 1)).expect("computable");
    assert_eq!(m.clock_khz, 138_500);
    assert_eq!(m.width, 1920);
    assert_eq!(m.hsync_start, 1968);
    assert_eq!(m.hsync_end, 2000);
    assert_eq!(m.htotal, 2080);
    assert_eq!(m.height, 1080);
    assert_eq!(m.vsync_start, 1083);
    assert_eq!(m.vsync_end, 1088);
    assert_eq!(m.vtotal, 1111);
    // CVT-RB polarity: positive hsync, negative vsync.
    assert!(m.hsync_positive);
    assert!(!m.vsync_positive);
}

#[test]
fn cvt_rb_1080p50_matches_the_field_unit_shape() {
    // The t630's EDID-less chain runs a CVT-RB-style 1080p at ~50 Hz today
    // (brief §6/§12). Verify the deterministic timing our computation commits.
    let m = cvt_rb_mode(1920, 1080, Rational::new(50, 1)).expect("computable");
    assert_eq!(m.htotal, 2080, "RB horizontal blanking is fixed at 160 px");
    assert_eq!(m.vtotal, 1106);
    assert_eq!(m.clock_khz, 115_000, "clock floors to the 250 kHz CVT step");
    // The achieved refresh lands just below the requested 50 Hz (clock
    // step-down), still within the rational match window.
    assert!(refresh_matches(m.refresh(), Rational::new(50, 1)));
}

#[test]
fn cvt_rb_720p60_uses_the_reduced_blanking_constants() {
    let m = cvt_rb_mode(1280, 720, Rational::new(60, 1)).expect("computable");
    assert_eq!(m.htotal, 1440, "1280 + the fixed 160 px RB blank");
    assert_eq!(m.hsync_start, 1328, "front porch 48");
    assert_eq!(m.hsync_end, 1360, "sync width 32");
    assert_eq!(m.vtotal, 741);
    assert_eq!(m.clock_khz, 64_000);
}

#[test]
fn cvt_rb_rejects_degenerate_geometry() {
    assert!(cvt_rb_mode(0, 1080, Rational::new(60, 1)).is_err());
    assert!(cvt_rb_mode(1920, 0, Rational::new(60, 1)).is_err());
    assert!(cvt_rb_mode(1920, 1080, Rational::new(0, 1)).is_err());
    assert!(cvt_rb_mode(1920, 1080, Rational::new(60, 0)).is_err());
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

proptest! {
    /// Whatever the inputs, a selected EDID mode is always drawn from the
    /// candidate list (never fabricated), and the forced path is taken only
    /// when the candidate list is empty.
    #[test]
    fn selection_is_always_from_the_candidates(
        widths in proptest::collection::vec(1u32..=4096, 1..8),
        cadence_num in 1i64..=240,
    ) {
        let modes: Vec<DisplayModeInfo> = widths
            .iter()
            .map(|&w| {
                let w = w.max(2) & !1;
                mode(w, (w * 9 / 16).max(2) & !1, w * 80, w + 200, (w * 9 / 16) + 50)
            })
            .collect();
        let cadence = Rational::new(cadence_num, 1);
        let selected = select_mode(&modes, &ModeRequest::Auto, None, Some(cadence))
            .expect("non-empty candidates always select");
        match selected {
            SelectedMode::Edid(m) => prop_assert!(modes.contains(&m)),
            SelectedMode::ForcedCvtRb(_) => prop_assert!(false, "candidates exist"),
        }
    }

    /// CVT-RB output is structurally sound for any plausible geometry: totals
    /// strictly exceed the active area, the sync window is ordered, and the
    /// pixel clock is on the 250 kHz CVT step.
    #[test]
    fn cvt_rb_is_structurally_sound(
        width in 64u32..=4096,
        height in 64u32..=2400,
        refresh in 24i64..=120,
    ) {
        let width = width & !1;
        let height = height & !1;
        let m = cvt_rb_mode(width, height, Rational::new(refresh, 1)).expect("computable");
        prop_assert_eq!(m.htotal, width + 160);
        prop_assert!(m.vtotal > height);
        prop_assert!(m.width < m.hsync_start);
        prop_assert!(m.hsync_start < m.hsync_end);
        prop_assert!(m.hsync_end <= m.htotal);
        prop_assert!(m.height < m.vsync_start);
        prop_assert!(m.vsync_start < m.vsync_end);
        prop_assert!(m.vsync_end <= m.vtotal);
        prop_assert_eq!(m.clock_khz % 250, 0);
        prop_assert!(m.clock_khz > 0);
    }
}
