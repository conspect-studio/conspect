//! Output metadata + orientation **apply model** (ADR-0088 / ADR-0089), the
//! pure-Rust seam between the config-layer intent/plan and the libav muxer.
//!
//! This module owns the *transport-neutral* application data:
//!
//! - [`MuxMetadata`] — the ordered format-level + per-stream `(key, value)`
//!   dictionary entries the muxer sets on the output `AVFormatContext` / streams
//!   **before `write_header`** (TS SDT `service_name`/`service_provider`, PMT
//!   `language`, container `title`/`comment`, RTMP `onMetaData` keys, …). It is
//!   built from the config-layer `Applied` subset (the caller maps its
//!   transport's carriers to libav dict keys) and applied by the
//!   feature-gated [`crate::sink`] muxer wiring.
//! - [`display_matrix`] / [`DisplayMatrix`] — the 3×3 display-rotation matrix
//!   (the `tkhd` / `AV_PKT_DATA_DISPLAYMATRIX` side-data form) for the
//!   **tag-path** orientation (ADR-0089 mechanism *a*). Zero pixel cost: the
//!   container declares "present at θ" and a tag-aware player rotates on render.
//!   This is invariant #8 applied to orientation — *tag, never convert*.
//! - [`rotated_geometry`] — the **pixels-path** encode geometry (ADR-0089
//!   mechanism *b*): a 90°/270° turn swaps W↔H (a distinct rendition);
//!   180°/flip keep geometry. The whole-canvas quarter-turn itself is the
//!   compositor's existing lossless sampling transform; this only computes the
//!   target the rotated rendition encodes into.
//!
//! Pure Rust, always compiled (no `ffmpeg` feature), unit-tested here. The
//! `multiview-output` crate is a leaf media crate and deliberately does **not**
//! depend on `multiview-config`: the caller (`multiview-cli`) translates a
//! `multiview_config::Output`'s metadata plan + orientation into these
//! transport-neutral values. Geometry math reuses the core
//! [`multiview_core::layout::QuarterTurn`] vocabulary (no fourth rotation enum).

use multiview_core::layout::QuarterTurn;

/// Which side of an output container a metadata dictionary entry targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataScope {
    /// The output **format** (container) metadata — `AVFormatContext.metadata`
    /// (e.g. `title`, `comment`, the TS SDT `service_name`/`service_provider`).
    Format,
    /// A specific **stream's** metadata — `AVStream.metadata` (e.g. the
    /// per-track `language` PMT/container tag). Carries the 0-based stream
    /// index the muxer assigned at `add_stream`.
    Stream {
        /// The 0-based output stream index this entry tags.
        index: usize,
    },
}

/// One libav dictionary entry to set before `write_header`: a scope, a key and
/// a value. Validated (no interior NUL) by [`MuxMetadata::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetadataEntry {
    /// Whether this entry tags the format or a specific stream.
    pub scope: MetadataScope,
    /// The libav dictionary key (e.g. `"service_name"`, `"title"`,
    /// `"language"`, `"comment"`).
    pub key: String,
    /// The dictionary value (operator-supplied text).
    pub value: String,
}

/// An entry whose key or value cannot become a C string for `av_dict_set` (it
/// carries an interior NUL byte). Surfaced rather than silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("mux metadata {field} {text:?} contains an interior NUL byte")]
pub struct MetadataEntryError {
    /// Which side of the entry was malformed (`"key"` or `"value"`).
    pub field: &'static str,
    /// The malformed text (for diagnostics).
    pub text: String,
}

/// An ordered, validated set of [`MetadataEntry`] values the muxer applies to
/// the output container + its streams before `write_header` (ADR-0088 §3).
///
/// Order is preserved so the surface is deterministic and testable. Empty is
/// the additive identity (no metadata to apply). The caller builds this from
/// the config-layer `Applied` projection — an unsupported/`Dropped` field is
/// never pushed here (it was already surfaced in the plan).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuxMetadata {
    entries: Vec<MetadataEntry>,
}

impl MuxMetadata {
    /// An empty metadata set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Whether no entries are set (nothing for the muxer to apply).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The ordered entries to apply.
    #[must_use]
    pub fn entries(&self) -> &[MetadataEntry] {
        &self.entries
    }

    /// Push a **format**-scoped entry, validating both sides for an interior
    /// NUL.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataEntryError`] if `key` or `value` carries a `\0`.
    pub fn push_format(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), MetadataEntryError> {
        self.push(MetadataScope::Format, key, value)
    }

    /// Push a **stream**-scoped entry (tagging `AVStream[index].metadata`),
    /// validating both sides for an interior NUL.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataEntryError`] if `key` or `value` carries a `\0`.
    pub fn push_stream(
        &mut self,
        index: usize,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), MetadataEntryError> {
        self.push(MetadataScope::Stream { index }, key, value)
    }

    /// Push an entry with an explicit scope, validating both sides for an
    /// interior NUL.
    ///
    /// # Errors
    ///
    /// Returns [`MetadataEntryError`] if `key` or `value` carries a `\0`.
    pub fn push(
        &mut self,
        scope: MetadataScope,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), MetadataEntryError> {
        let key = key.into();
        let value = value.into();
        if key.contains('\0') {
            return Err(MetadataEntryError {
                field: "key",
                text: key,
            });
        }
        if value.contains('\0') {
            return Err(MetadataEntryError {
                field: "value",
                text: value,
            });
        }
        self.entries.push(MetadataEntry { scope, key, value });
        Ok(())
    }
}

/// A 3×3 display matrix in libav's 16.16 fixed-point form (`AV_PKT_DATA_/
/// AV_FRAME_DATA_DISPLAYMATRIX`, written into the MP4/MOV `tkhd`), row-major:
/// `[ a b u ; c d v ; x y w ]`. The rotation lives in the upper-left 2×2; the
/// `u`/`v`/`w` projection column is the standard `[0, 0, 1<<30]`.
///
/// Players read this and rotate on render (the **tag** path, ADR-0089
/// mechanism *a*) — zero pixel cost. ffmpeg's `av_display_rotation_get` recovers
/// the clockwise-screen angle from this matrix.
pub type DisplayMatrix = [i32; 9];

/// libav fixed-point one (`1 << 16`) used for the rotation 2×2.
const FP_ONE: i32 = 1 << 16;
/// libav projection one (`1 << 30`) used for the matrix `w` term.
const FP_W: i32 = 1 << 30;

/// Build the [`DisplayMatrix`] for a clockwise [`QuarterTurn`] (the tag path).
///
/// The matrix encodes the rotation libav's `av_display_rotation_set(θ)` would
/// produce. libav defines the rotation angle as the **anticlockwise** angle the
/// transformation applies; a `QuarterTurn::Cw90` (a 90° *clockwise* picture
/// rotation) corresponds to `av_display_rotation_set(-90)`. We build the matrix
/// directly so the result is exact integer fixed-point (no float fps / no
/// trig rounding — invariant #3 spirit):
///
/// - `None`  → identity.
/// - `Cw90`  → `[ 0  +1 ; -1  0 ]` scaled (screen rotates 90° clockwise).
/// - `Cw180` → `[ -1  0 ; 0 -1 ]`.
/// - `Cw270` → `[ 0 -1 ; +1  0 ]`.
#[must_use]
pub const fn display_matrix(turn: QuarterTurn) -> DisplayMatrix {
    // Upper-left 2×2 (a, b / c, d) in 16.16; projection column [0,0,1<<30].
    let (a, b, c, d) = match turn {
        QuarterTurn::Cw90 => (0, FP_ONE, -FP_ONE, 0),
        QuarterTurn::Cw180 => (-FP_ONE, 0, 0, -FP_ONE),
        QuarterTurn::Cw270 => (0, -FP_ONE, FP_ONE, 0),
        // `QuarterTurn` is `#[non_exhaustive]`; `None` and any future variant
        // map to the identity (no rotation tag) — a forward-compatible default,
        // never a panic on the apply path.
        _ => (FP_ONE, 0, 0, FP_ONE),
    };
    [a, b, 0, c, d, 0, 0, 0, FP_W]
}

/// The clockwise screen-rotation degrees a [`DisplayMatrix`] encodes, recovered
/// from the upper-left 2×2 (the inverse of [`display_matrix`]). Returns `0`,
/// `90`, `180`, or `270` for the quarter-turn matrices this module produces, and
/// `0` for the identity. Used by the post-mux ffprobe verification (ADR-0089
/// §6) to assert the requested rotation landed.
#[must_use]
pub const fn display_matrix_degrees(m: DisplayMatrix) -> u16 {
    // Inspect (a=m[0], b=m[1], c=m[3], d=m[4]).
    match (m[0], m[1], m[3], m[4]) {
        (0, FP_ONE, n, 0) if n == -FP_ONE => 90,
        (n, 0, 0, p) if n == -FP_ONE && p == -FP_ONE => 180,
        (0, n, FP_ONE, 0) if n == -FP_ONE => 270,
        _ => 0,
    }
}

/// The encode geometry of a **pixels-path** rotated rendition (ADR-0089
/// mechanism *b*): a 90°/270° quarter-turn swaps width↔height (a distinct
/// rendition geometry, [ADR-M002]); 180° and a pure flip keep geometry.
///
/// The compositor produces the rotated pixels via its existing lossless
/// quarter-turn sampling transform; this only computes the dimensions the
/// rotated rendition's `EncodeProfile` targets. `flip` never changes geometry.
///
/// [ADR-M002]: https://example.invalid/ADR-M002
#[must_use]
pub const fn rotated_geometry(width: u32, height: u32, turn: QuarterTurn) -> (u32, u32) {
    if turn.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metadata_is_identity() {
        let m = MuxMetadata::new();
        assert!(m.is_empty());
        assert_eq!(m.entries(), &[]);
    }

    #[test]
    fn push_format_and_stream_preserve_order_and_scope() {
        let mut m = MuxMetadata::new();
        m.push_format("service_name", "Studio A").expect("ok");
        m.push_format("service_provider", "Aperim").expect("ok");
        m.push_stream(0, "language", "eng").expect("ok");
        let e = m.entries();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].scope, MetadataScope::Format);
        assert_eq!(e[0].key, "service_name");
        assert_eq!(e[0].value, "Studio A");
        assert_eq!(e[2].scope, MetadataScope::Stream { index: 0 });
        assert_eq!(e[2].key, "language");
    }

    #[test]
    fn interior_nul_key_is_rejected() {
        let mut m = MuxMetadata::new();
        let err = m
            .push_format("ser\0vice", "x")
            .expect_err("interior NUL key is rejected");
        assert_eq!(err.field, "key");
        assert!(m.is_empty(), "rejected entry is not stored");
    }

    #[test]
    fn interior_nul_value_is_rejected() {
        let mut m = MuxMetadata::new();
        let err = m
            .push_format("title", "a\0b")
            .expect_err("interior NUL value is rejected");
        assert_eq!(err.field, "value");
    }

    #[test]
    fn display_matrix_none_is_identity() {
        let m = display_matrix(QuarterTurn::None);
        assert_eq!(m, [FP_ONE, 0, 0, 0, FP_ONE, 0, 0, 0, FP_W]);
        assert_eq!(display_matrix_degrees(m), 0);
    }

    #[test]
    fn display_matrix_roundtrips_each_quarter_turn() {
        assert_eq!(
            display_matrix_degrees(display_matrix(QuarterTurn::Cw90)),
            90
        );
        assert_eq!(
            display_matrix_degrees(display_matrix(QuarterTurn::Cw180)),
            180
        );
        assert_eq!(
            display_matrix_degrees(display_matrix(QuarterTurn::Cw270)),
            270
        );
    }

    #[test]
    fn display_matrix_cw90_has_expected_entries() {
        // Cw90: a=0, b=+1, c=-1, d=0 (in 16.16), projection w=1<<30.
        let m = display_matrix(QuarterTurn::Cw90);
        assert_eq!(m[0], 0);
        assert_eq!(m[1], FP_ONE);
        assert_eq!(m[3], -FP_ONE);
        assert_eq!(m[4], 0);
        assert_eq!(m[8], FP_W);
    }

    #[test]
    fn odd_turns_swap_geometry_even_turns_do_not() {
        assert_eq!(
            rotated_geometry(1920, 1080, QuarterTurn::None),
            (1920, 1080)
        );
        assert_eq!(
            rotated_geometry(1920, 1080, QuarterTurn::Cw180),
            (1920, 1080)
        );
        assert_eq!(
            rotated_geometry(1920, 1080, QuarterTurn::Cw90),
            (1080, 1920)
        );
        assert_eq!(
            rotated_geometry(1920, 1080, QuarterTurn::Cw270),
            (1080, 1920)
        );
    }
}
