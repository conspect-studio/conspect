//! The mandatory NDI® attribution constants must be present and correct
//! (ADR-0008 §7.2 / `docs/io/ndi.md` §7.2 — load-bearing, not optional docs).
#![cfg(feature = "ndi")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::ndi;

#[test]
fn trademark_notice_is_exact() {
    assert_eq!(
        ndi::NDI_TRADEMARK_NOTICE,
        "NDI® is a registered trademark of Vizrt NDI AB"
    );
}

#[test]
fn attribution_url_points_at_ndi_video() {
    assert_eq!(ndi::NDI_ATTRIBUTION_URL, "https://ndi.video");
}

#[test]
fn attribution_block_contains_both_notice_and_link() {
    let block = ndi::attribution();
    assert!(
        block.contains("Vizrt NDI AB"),
        "must name the trademark holder"
    );
    assert!(block.contains("ndi.video"), "must carry the ndi.video link");
}
