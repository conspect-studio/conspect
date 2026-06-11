//! ST 2110-22 **JPEG XS** (ISO/IEC 21122, VSF TR-08) codec support and
//! capability detection.
//!
//! ST 2110-22 carries *lightly-compressed* video over IP; JPEG XS is the codec
//! used in practice. Whether a usable JPEG XS decoder/encoder exists depends
//! entirely on **how the linked `FFmpeg` was built** — JPEG XS support arrived
//! in FFmpeg after the 7.1 line this crate validates against, and it may be
//! provided either by a native `jpegxs` decoder/encoder or by the external
//! `libsvtjpegxs` wrapper. So a usable JPEG XS path can never be assumed at
//! compile time; it must be **detected at runtime against the real build** and
//! fall back cleanly when absent.
//!
//! ## What is pure vs. gated
//!
//! This module is split so the decision logic is testable without any native
//! dependency:
//!
//! * **Pure (always compiled, unit-tested):** the canonical list of libav codec
//!   *names* to probe for JPEG XS ([`JPEGXS_CODEC_NAMES`]), the
//!   [`JpegXsRole`]/[`JpegXsAvailability`] value types, and
//!   [`select_codec_name`] — the path-selection algorithm that, given the raw
//!   "is each candidate present?" answers from a probe, picks the codec name to
//!   use (highest-priority present one) or reports unavailability. None of this
//!   touches libav.
//! * **Gated (`ffmpeg` feature, compile-verified here only):** `probe` /
//!   `is_available` (named as plain code spans because they are absent from the
//!   default pure-Rust doc build), which run [`select_codec_name`] against the
//!   **linked** `FFmpeg` by asking libav `avcodec_find_{decoder,encoder}_by_name`.
//!   Because the `FFmpeg` in this environment has no JPEG XS codec, these report
//!   "unavailable" rather than panicking.
//!
//! ## Why name-based, not `AVCodecID`-based
//!
//! The `ffmpeg-next` 7.1 binding's `Id` enum has **no** JPEG XS variant (it
//! predates `AV_CODEC_ID_JPEGXS`), so an id-based lookup cannot even name the
//! codec. Probing by the registered *codec name*
//! (`"jpegxs"`, `"libsvtjpegxs"`, …) is both expressible today and
//! forward-compatible: it starts working the moment a future linked `FFmpeg`
//! registers such a codec, with no change to the binding or this crate.
//!
//! ## Invariants
//!
//! JPEG XS only ever appears as a per-input *codec*; it does not change Multiview's
//! timing model. A JPEG XS source is decoded into the normal NV12 timeline and
//! sampled by the compositor like any other input (invariants #1/#5) — detection
//! here selects a decoder; it never paces the output clock.

/// Whether JPEG XS support is needed for decoding (ingest, ST 2110-22 in) or
/// encoding (egress, ST 2110-22 out).
///
/// A given `FFmpeg` build can ship one direction without the other (e.g. a
/// decoder but no encoder), so capability is always probed per role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JpegXsRole {
    /// Decoding a JPEG XS elementary stream (ST 2110-22 ingest).
    Decode,
    /// Encoding to a JPEG XS elementary stream (ST 2110-22 egress).
    Encode,
}

impl JpegXsRole {
    /// A short, stable label for diagnostics and error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Encode => "encode",
        }
    }
}

/// Registered libav codec names to probe for JPEG XS, in **descending priority**
/// order (the first one present wins in [`select_codec_name`]).
///
/// * `"jpegxs"` — the native in-tree codec name used once `AV_CODEC_ID_JPEGXS`
///   lands in `FFmpeg`.
/// * `"libsvtjpegxs"` — the SVT-JPEG-XS external wrapper (the form most builds
///   that have JPEG XS today actually ship).
///
/// The native name is preferred because it carries no extra external-library
/// licensing/runtime surface; the SVT wrapper is the practical fallback.
pub const JPEGXS_CODEC_NAMES: [&str; 2] = ["jpegxs", "libsvtjpegxs"];

/// The outcome of a JPEG XS capability probe for one [`JpegXsRole`].
///
/// This is a plain owned value with no libav handle, so it can be cached,
/// logged, surfaced over the control API, or fed into the HAL planner. When
/// [`codec_name`](Self::codec_name) is [`Some`], that exact libav codec name can
/// be opened for the role; when [`None`], no JPEG XS path exists in the linked
/// build and callers must fall back (e.g. refuse the ST 2110-22 source with a
/// typed error rather than crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JpegXsAvailability {
    /// The role this availability was probed for.
    pub role: JpegXsRole,
    /// The libav codec name to open for this role, or [`None`] if the linked
    /// `FFmpeg` exposes no JPEG XS codec for it.
    pub codec_name: Option<&'static str>,
}

impl JpegXsAvailability {
    /// Construct an "available" result naming the selected libav codec.
    #[must_use]
    pub const fn available(role: JpegXsRole, codec_name: &'static str) -> Self {
        Self {
            role,
            codec_name: Some(codec_name),
        }
    }

    /// Construct an "unavailable" result for the given role.
    #[must_use]
    pub const fn unavailable(role: JpegXsRole) -> Self {
        Self {
            role,
            codec_name: None,
        }
    }

    /// Whether a usable JPEG XS codec was found for this role.
    #[must_use]
    pub const fn is_available(self) -> bool {
        self.codec_name.is_some()
    }
}

/// Pure path-selection: given a `present` predicate that answers "is the libav
/// codec with this name registered for the wanted role?", pick the codec name to
/// use.
///
/// The candidates in [`JPEGXS_CODEC_NAMES`] are tried in priority order and the
/// **first present** one is returned; if none is present the result is [`None`]
/// (fall back, never panic). The `present` closure is the only place native
/// state enters, which is what makes this function fully unit-testable with a
/// stub — the gated `probe` supplies the real libav-backed predicate.
#[must_use]
pub fn select_codec_name<F>(present: F) -> Option<&'static str>
where
    F: Fn(&str) -> bool,
{
    JPEGXS_CODEC_NAMES.into_iter().find(|name| present(name))
}

/// Pure capability assembly: run [`select_codec_name`] for `role` against the
/// supplied `present` predicate and wrap the outcome in a [`JpegXsAvailability`].
///
/// This is the exact logic the gated `probe` runs; keeping it pure lets the
/// full decision (including the role wiring) be tested without libav.
#[must_use]
pub fn resolve_availability<F>(role: JpegXsRole, present: F) -> JpegXsAvailability
where
    F: Fn(&str) -> bool,
{
    match select_codec_name(present) {
        Some(name) => JpegXsAvailability::available(role, name),
        None => JpegXsAvailability::unavailable(role),
    }
}

/// Probe the **linked** `FFmpeg` for a usable JPEG XS codec for `role`.
///
/// Asks libav (`avcodec_find_decoder_by_name` / `avcodec_find_encoder_by_name`,
/// via `ffmpeg_next`'s safe wrappers) for each [`JPEGXS_CODEC_NAMES`] candidate
/// in priority order and returns the first match. No JPEG XS codec ⇒
/// [`JpegXsAvailability::unavailable`] — a clean, non-panicking fallback.
///
/// One-time global libav initialization is run first so codec registration has
/// happened before the lookup.
///
/// # Errors
/// Returns [`FfmpegError::Init`](crate::error::FfmpegError::Init) only if global
/// libav initialization fails; the codec lookup itself never errors (a missing
/// codec is reported as unavailable, not as an error).
#[cfg(feature = "ffmpeg")]
pub fn probe(role: JpegXsRole) -> crate::error::Result<JpegXsAvailability> {
    use ffmpeg_next::codec::{decoder, encoder};

    crate::decode::ensure_initialized()?;

    // The role decides which libav registry we consult. `find_by_name` returns
    // `Option<Codec>` (safe wrapper over the raw `avcodec_find_*_by_name`); a
    // present codec also self-reports the correct direction, which we assert
    // belt-and-suspenders so a mis-registered codec can never be selected.
    let present = |name: &str| -> bool {
        match role {
            JpegXsRole::Decode => decoder::find_by_name(name).is_some_and(|c| c.is_decoder()),
            JpegXsRole::Encode => encoder::find_by_name(name).is_some_and(|c| c.is_encoder()),
        }
    };

    Ok(resolve_availability(role, present))
}

/// Convenience wrapper over [`probe`] returning just whether JPEG XS is usable
/// for `role` in the linked build.
///
/// # Errors
/// Propagates [`probe`]'s initialization error.
#[cfg(feature = "ffmpeg")]
pub fn is_available(role: JpegXsRole) -> crate::error::Result<bool> {
    Ok(probe(role)?.is_available())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_names_are_priority_ordered_native_first() {
        // The native in-tree name must outrank the external SVT wrapper so a
        // build that ships both opens the lower-licensing-surface path.
        assert_eq!(JPEGXS_CODEC_NAMES, ["jpegxs", "libsvtjpegxs"]);
    }

    #[test]
    fn select_picks_highest_priority_present_candidate() {
        // Both present ⇒ the native `jpegxs` wins, never the SVT wrapper.
        let selected = select_codec_name(|name| name == "jpegxs" || name == "libsvtjpegxs");
        assert_eq!(selected, Some("jpegxs"));
    }

    #[test]
    fn select_falls_through_to_lower_priority_when_native_absent() {
        // Only the SVT wrapper is present ⇒ it is selected (the real-world case
        // for builds that have JPEG XS today).
        let selected = select_codec_name(|name| name == "libsvtjpegxs");
        assert_eq!(selected, Some("libsvtjpegxs"));
    }

    #[test]
    fn select_reports_none_when_no_candidate_present() {
        // The fallback path: a build with no JPEG XS codec selects nothing and
        // must NOT panic (this environment's FFmpeg 7.1).
        let selected = select_codec_name(|_| false);
        assert_eq!(selected, None);
    }

    #[test]
    fn select_only_consults_known_candidates() {
        // A predicate that says "yes" to an unrelated codec must not leak it
        // into the selection — only `JPEGXS_CODEC_NAMES` may be returned.
        let selected = select_codec_name(|name| name == "h264");
        assert_eq!(selected, None);
    }

    #[test]
    fn resolve_availability_wraps_present_codec_with_role() {
        let avail = resolve_availability(JpegXsRole::Decode, |name| name == "jpegxs");
        assert_eq!(
            avail,
            JpegXsAvailability::available(JpegXsRole::Decode, "jpegxs")
        );
        assert_eq!(avail.role, JpegXsRole::Decode);
        assert_eq!(avail.codec_name, Some("jpegxs"));
        assert!(avail.is_available());
    }

    #[test]
    fn resolve_availability_reports_unavailable_with_role_preserved() {
        // No candidate present ⇒ unavailable, but the role is still carried so a
        // caller knows which direction failed.
        let avail = resolve_availability(JpegXsRole::Encode, |_| false);
        assert_eq!(avail, JpegXsAvailability::unavailable(JpegXsRole::Encode));
        assert_eq!(avail.role, JpegXsRole::Encode);
        assert_eq!(avail.codec_name, None);
        assert!(!avail.is_available());
    }

    #[test]
    fn availability_constructors_round_trip_through_predicates() {
        // `available`/`unavailable`/`is_available` agree with the value fields
        // for both roles.
        for role in [JpegXsRole::Decode, JpegXsRole::Encode] {
            let yes = JpegXsAvailability::available(role, "libsvtjpegxs");
            assert!(yes.is_available());
            assert_eq!(yes.codec_name, Some("libsvtjpegxs"));
            assert_eq!(yes.role, role);

            let no = JpegXsAvailability::unavailable(role);
            assert!(!no.is_available());
            assert_eq!(no.codec_name, None);
            assert_eq!(no.role, role);
        }
    }

    #[test]
    fn roles_have_distinct_stable_labels() {
        assert_eq!(JpegXsRole::Decode.as_str(), "decode");
        assert_eq!(JpegXsRole::Encode.as_str(), "encode");
        assert_ne!(JpegXsRole::Decode.as_str(), JpegXsRole::Encode.as_str());
    }

    #[test]
    fn select_is_consistent_across_all_present_subsets() {
        // Exhaustive over the 2^N presence subsets of the candidate list: the
        // selected name is always the FIRST present candidate in priority order,
        // and `None` exactly when the subset is empty. This pins the algorithm
        // without trusting any single hand-picked case.
        let names = JPEGXS_CODEC_NAMES;
        let n = names.len();
        let combos = 1_usize << n;
        for mask in 0..combos {
            let present = |query: &str| {
                names
                    .iter()
                    .enumerate()
                    .any(|(i, candidate)| *candidate == query && (mask & (1 << i)) != 0)
            };
            let got = select_codec_name(present);
            let expected = names
                .iter()
                .enumerate()
                .find(|(i, _)| (mask & (1 << i)) != 0)
                .map(|(_, name)| *name);
            assert_eq!(got, expected, "mask={mask:b}");
        }
    }
}
