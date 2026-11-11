use multiview_webengine::capture::{Background, Capture, WindyView};
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
    };
    assert!(c.validate().is_err());
    c.width = 1280;
    assert!(c.validate().is_ok());
    c.document = "<script>bad()</script>".into();
    assert!(c.validate().is_err());
}
