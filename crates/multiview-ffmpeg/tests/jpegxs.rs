//! Integration coverage for the JPEG XS (ST 2110-22) capability probe against
//! the **linked** `FFmpeg`.
//!
//! Two layers:
//!
//! * The **pure** path-selection algorithm is unit-tested inline in
//!   `src/jpegxs.rs`; here we additionally exercise it through the crate's
//!   public surface so the re-exports stay wired.
//! * The **gated** `probe`/`is_available` run only under the `ffmpeg` feature.
//!   In this environment `FFmpeg` 7.1 has no JPEG XS codec, so the only thing
//!   asserted at runtime is the contract that matters most: the probe **falls
//!   back cleanly** (no panic, no error) and reports unavailable. On a build
//!   that *does* ship JPEG XS, the same call would instead name the codec — the
//!   test asserts the invariant (availability ⇒ a JPEG XS codec name) that holds
//!   either way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::{
    resolve_availability, select_codec_name, JpegXsAvailability, JpegXsRole, JPEGXS_CODEC_NAMES,
};

#[test]
fn pure_selection_is_reachable_through_public_api() {
    // The pure algorithm is part of the always-compiled public surface (no
    // feature needed) so the HAL planner can reason about JPEG XS in the
    // default build.
    assert_eq!(JPEGXS_CODEC_NAMES, ["jpegxs", "libsvtjpegxs"]);
    assert_eq!(select_codec_name(|n| n == "jpegxs"), Some("jpegxs"));
    assert_eq!(select_codec_name(|_| false), None);

    let avail = resolve_availability(JpegXsRole::Decode, |n| n == "libsvtjpegxs");
    assert_eq!(
        avail,
        JpegXsAvailability::available(JpegXsRole::Decode, "libsvtjpegxs")
    );
    assert!(avail.is_available());
}

#[cfg(feature = "ffmpeg")]
mod gated {
    use super::{JpegXsAvailability, JpegXsRole};
    use multiview_ffmpeg::{jpegxs_is_available, jpegxs_probe};

    /// The core fallback guarantee: probing the linked `FFmpeg` for either role
    /// returns a well-formed answer without panicking, and never claims
    /// availability without naming a JPEG XS codec.
    #[test]
    fn probe_never_panics_and_is_self_consistent() {
        for role in [JpegXsRole::Decode, JpegXsRole::Encode] {
            let avail: JpegXsAvailability =
                jpegxs_probe(role).expect("probe only fails on libav init failure");
            assert_eq!(avail.role, role, "probe must carry the requested role");

            // Availability and the codec name must agree exactly: available
            // iff a JPEG XS codec name was selected.
            assert_eq!(
                avail.is_available(),
                avail.codec_name.is_some(),
                "is_available must track codec_name presence"
            );

            // If (and only if) a codec was found, it must be one of the known
            // JPEG XS candidates — never some unrelated codec.
            if let Some(name) = avail.codec_name {
                assert!(
                    multiview_ffmpeg::JPEGXS_CODEC_NAMES.contains(&name),
                    "selected codec {name:?} must be a known JPEG XS candidate"
                );
            }

            // `is_available` is the boolean view of the same probe.
            let flag = jpegxs_is_available(role).expect("is_available only fails on init failure");
            assert_eq!(flag, avail.is_available());
        }
    }

    /// In THIS environment the linked `FFmpeg` 7.1 has no JPEG XS codec, so the
    /// probe must report unavailable for both roles — the clean-fallback path
    /// the wrapper exists to guarantee.
    ///
    /// This asserts the environment-specific expectation honestly: it is the
    /// behaviour we can verify here (no NIC/SDK needed), and it documents that
    /// the real ST 2110-22 transport remains gated and unverified at runtime.
    #[test]
    fn ffmpeg_7_1_here_has_no_jpegxs_so_probe_falls_back() {
        for role in [JpegXsRole::Decode, JpegXsRole::Encode] {
            let avail = jpegxs_probe(role).expect("probe only fails on libav init failure");
            assert!(
                !avail.is_available(),
                "FFmpeg 7.1 in this image ships no JPEG XS {} codec; \
                 probe must fall back to unavailable, got {avail:?}",
                role.as_str()
            );
            assert_eq!(avail, JpegXsAvailability::unavailable(role));
        }
    }
}
