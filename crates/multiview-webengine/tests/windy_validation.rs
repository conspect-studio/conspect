#![allow(clippy::unwrap_used)]
use multiview_webengine::capture::{
    frames_for_ms, Background, BrowserGl, Capture, CaptureMode, CaptureOptions, StepTiming, Tools,
    WindyView,
};
fn view() -> WindyView {
    WindyView {
        lat: -33.946,
        lon: 150.37,
        zoom: 10,
        overlay: "radar".into(),
        terms_accepted: true,
        require_premium: true,
        credentials: None,
    }
}
#[test]
fn requested_layer_coordinates_and_acceptance_are_required() {
    assert!(view().validate().is_ok());
    let mut v = view();
    v.terms_accepted = false;
    assert!(v.validate().is_err());
    let mut v = view();
    v.lat = f64::NAN;
    assert!(v.validate().is_err());
    let mut v = view();
    v.lon = 181.0;
    assert!(v.validate().is_err());
    let mut v = view();
    v.zoom = 0;
    assert!(v.validate().is_err());
    let mut v = view();
    v.overlay = "javascript:bad".into();
    assert!(v.validate().is_err());
}
#[test]
fn windy_is_not_a_mobile_or_arbitrary_html_capture() {
    let mut c = Capture {
        width: 640,
        height: 360,
        fps: 5,
        frames: 5,
        document: String::new(),
        windy: Some(view()),
        background: Background::Opaque,
        capture: multiview_webengine::capture::CaptureOptions::default(),
        output_cadence: None,
    };
    assert!(c.validate().is_err());
    c.width = 1280;
    assert!(c.validate().is_ok());
    c.document = "<script>bad()</script>".into();
    assert!(c.validate().is_err());
}
fn windy(overlay: &str) -> Capture {
    let mut v = view();
    v.overlay = overlay.into();
    Capture {
        width: 1280,
        height: 720,
        fps: 10,
        frames: 2,
        document: String::new(),
        windy: Some(v),
        background: Background::Opaque,
        capture: CaptureOptions::default(),
        output_cadence: None,
    }
}
#[test]
fn capture_options_default_to_radar_steps_and_wind_realtime() {
    let c: Capture =
        serde_json::from_str(r#"{"width":320,"height":180,"fps":5,"frames":3,"document":"x"}"#)
            .unwrap();
    assert_eq!(c.capture, CaptureOptions::default());
    assert_eq!(c.capture.mode, None);
    assert_eq!(c.capture.timing, StepTiming::Equal);
    assert_eq!(
        (
            c.capture.frame_ms,
            c.capture.end_hold_ms,
            c.capture.max_steps
        ),
        (600, 1000, 12)
    );
    assert_eq!(c.effective_capture(), CaptureMode::Realtime);
    assert_eq!(windy("radar").effective_capture(), CaptureMode::Steps);
    assert_eq!(windy("wind").effective_capture(), CaptureMode::Realtime);
    let o: CaptureOptions =
        serde_json::from_str(r#"{"mode":"realtime","timing":"proportional"}"#).unwrap();
    assert_eq!(o.mode, Some(CaptureMode::Realtime));
    assert_eq!(o.timing, StepTiming::Proportional);
    assert!(serde_json::from_str::<CaptureOptions>(r#"{"timing":"real"}"#).is_err());
    assert!(serde_json::from_str::<CaptureOptions>(r#"{"bogus":1}"#).is_err());
}
#[test]
fn step_options_are_bounded() {
    assert!(windy("radar").validate().is_ok());
    let bad: [fn(&mut CaptureOptions); 6] = [
        |o| o.frame_ms = 99,
        |o| o.frame_ms = 5001,
        |o| o.end_hold_ms = 10_001,
        |o| o.max_steps = 1,
        |o| o.max_steps = 49,
        // 48 x 5000 ms at 10 fps = 2400 frames > 600-frame cap.
        |o| {
            o.max_steps = 48;
            o.frame_ms = 5000;
        },
    ];
    for f in bad {
        let mut c = windy("radar");
        f(&mut c.capture);
        assert!(c.validate().is_err());
    }
    // Wind has no step timeline: explicit steps is refused, realtime is fine.
    let mut c = windy("wind");
    c.capture.mode = Some(CaptureMode::Steps);
    assert!(c.validate().is_err());
    c.capture.mode = Some(CaptureMode::Realtime);
    assert!(c.validate().is_ok());
    // Realtime ignores step knobs (back-compat with existing clients).
    let mut c = windy("radar");
    c.capture.mode = Some(CaptureMode::Realtime);
    c.capture.max_steps = 0;
    assert!(c.validate().is_ok());
    assert_eq!(frames_for_ms(600, 10), 6);
    assert_eq!(frames_for_ms(250, 6), 2);
    assert_eq!(frames_for_ms(100, 1), 0);
}
#[test]
fn browser_gl_mode_is_bounded_and_defaults_to_no_flags() {
    assert_eq!("default".parse::<BrowserGl>().unwrap(), BrowserGl::Default);
    assert_eq!(
        "swiftshader".parse::<BrowserGl>().unwrap(),
        BrowserGl::Swiftshader
    );
    assert_eq!("egl".parse::<BrowserGl>().unwrap(), BrowserGl::Egl);
    assert!("disable-gpu".parse::<BrowserGl>().is_err());
    let t: Tools = serde_json::from_str(r#"{"node":"/n","browser":"/b","ffmpeg":"/f"}"#).unwrap();
    assert_eq!(t.gl, BrowserGl::Default);
    assert!(serde_json::from_str::<Tools>(
        r#"{"node":"/n","browser":"/b","ffmpeg":"/f","gl":"desktop"}"#
    )
    .is_err());
}

/// Issue #474: the conform cadence is an exact, bounded rational.
#[test]
fn output_cadence_is_bounded_and_exact() {
    use multiview_webengine::capture::OutputCadence;
    let ok = |num, den| OutputCadence { num, den }.validate().is_ok();
    assert!(ok(30000, 1001));
    assert!(ok(25, 1));
    assert!(ok(60000, 1001));
    assert!(ok(120, 1));
    assert!(ok(1, 1));
    assert!(!ok(0, 1), "zero rate");
    assert!(!ok(25, 0), "zero denominator");
    assert!(!ok(1, 2), "below 1 fps");
    assert!(!ok(121, 1), "above 120 fps");
    assert!(!ok(240_001, 1001), "numerator bound");
    assert!(!ok(30, 1002), "denominator bound");
}

/// The expected conformed count matches ffmpeg's `fps` filter (round=near,
/// ties up), measured with ffmpeg 7.1 for these exact inputs.
#[test]
fn conformed_frames_matches_the_fps_filter() {
    use multiview_webengine::capture::OutputCadence;
    let c = |num, den| OutputCadence { num, den };
    // (input frames, input fps, cadence, frames ffmpeg 7.1 emitted)
    for (n, fps, cadence, want) in [
        (25, 25, c(30000, 1001), 30),
        (2, 25, c(30000, 1001), 2),
        (250, 25, c(60000, 1001), 599),
        (11, 25, c(60000, 1001), 26),
        (5, 30, c(25, 1), 4),
        (100, 30, c(25, 1), 83),
        (250, 10, c(30000, 1001), 749),
        (100, 1, c(30000, 1001), 2997),
        (3, 2, c(1, 1), 2),
        (2, 4, c(6, 1), 3),
        (3, 6, c(1, 1), 1),
        (100, 7, c(30000, 1001), 428),
    ] {
        assert_eq!(
            cadence.conformed_frames(n, fps),
            want,
            "{n}@{fps} -> {cadence:?}"
        );
    }
}

/// A conform cadence makes the capture invalid when out of range, and the
/// conformed output is bounded.
#[test]
fn capture_validate_checks_the_output_cadence() {
    use multiview_webengine::capture::OutputCadence;
    let mut c = windy("wind");
    c.capture.mode = Some(CaptureMode::Realtime);
    c.output_cadence = Some(OutputCadence {
        num: 30000,
        den: 1001,
    });
    assert!(c.validate().is_ok());
    c.output_cadence = Some(OutputCadence { num: 25, den: 0 });
    assert!(c.validate().is_err());
    // 600 frames at 1 fps conformed to 120 fps = 72000 frames: over the cap.
    c.fps = 1;
    c.frames = 600;
    c.output_cadence = Some(OutputCadence { num: 120, den: 1 });
    assert!(c.validate().is_err());
}
