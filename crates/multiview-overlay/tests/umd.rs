//! Tests for the UMD (Under-Monitor Display) live-text label model.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_overlay::umd::{UmdAlign, UmdField, UmdLabel};

#[test]
fn label_starts_with_configured_text() {
    let label = UmdLabel::new("CAM 1");
    assert_eq!(label.text(), "CAM 1");
    assert_eq!(label.fields().len(), 1);
}

#[test]
fn set_text_updates_without_changing_field_layout() {
    let mut label = UmdLabel::new("CAM 1");
    let revision_before = label.revision();
    let layout_before: Vec<UmdAlign> = label.fields().iter().map(UmdField::align).collect();

    label.set_text("CAM 2");

    assert_eq!(label.text(), "CAM 2", "text follows the live update");
    let layout_after: Vec<UmdAlign> = label.fields().iter().map(UmdField::align).collect();
    assert_eq!(
        layout_before, layout_after,
        "a text update must not change the field layout"
    );
    assert!(
        label.revision() > revision_before,
        "the revision bumps so a renderer knows to re-rasterize only the glyphs"
    );
}

#[test]
fn setting_identical_text_is_a_noop_and_does_not_bump_revision() {
    let mut label = UmdLabel::new("CAM 1");
    let rev = label.revision();
    label.set_text("CAM 1");
    assert_eq!(
        label.revision(),
        rev,
        "no visible change must not invalidate cached glyphs"
    );
}

#[test]
fn multi_field_label_addresses_fields_independently() {
    let mut label = UmdLabel::multi(["SRC", "00:00", "OK"]);
    assert_eq!(label.fields().len(), 3);

    let rev = label.revision();
    label
        .set_field_text(1, "00:42")
        .expect("field index in range");
    assert_eq!(label.fields()[1].text(), "00:42");
    assert_eq!(label.fields()[0].text(), "SRC", "other fields untouched");
    assert!(label.revision() > rev);
}

#[test]
fn set_field_text_out_of_range_is_an_error() {
    let mut label = UmdLabel::new("CAM 1");
    let err = label.set_field_text(5, "x");
    assert!(err.is_err(), "out-of-range field index must error");
}

#[test]
fn truncation_respects_a_max_glyph_budget() {
    // TSL v3.1/v4.0 carry a fixed 16-char field; a UMD label exposes a
    // displayed() projection clamped to a glyph budget for those wire formats.
    let label = UmdLabel::new("THIS IS A VERY LONG SOURCE NAME");
    let shown = label.displayed(16);
    assert_eq!(shown.chars().count(), 16);
    assert!("THIS IS A VERY LONG SOURCE NAME".starts_with(&shown));
}

#[test]
fn displayed_does_not_pad_short_text() {
    let label = UmdLabel::new("CAM");
    assert_eq!(label.displayed(16), "CAM");
}

#[test]
fn align_defaults_to_centre_and_is_settable() {
    let label = UmdLabel::new("CAM 1");
    assert_eq!(label.fields()[0].align(), UmdAlign::Center);

    let left = UmdLabel::new("CAM 1").with_align(UmdAlign::Left);
    assert_eq!(left.fields()[0].align(), UmdAlign::Left);
}

#[test]
fn serde_round_trips_label_tagged() {
    let label = UmdLabel::multi(["A", "B"]).with_align(UmdAlign::Right);
    let json = serde_json::to_string(&label).expect("serialize");
    let back: UmdLabel = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(label, back);
    // Align is serialized tagged (snake_case variant names), never untagged.
    assert!(json.contains("right"));
}
