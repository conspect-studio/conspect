//! Typed, libav-free muxer-option surface (GP-6 Piece B, ADR-0030 §4).
//!
//! A **guarded passthrough** copies coded packets and splices a pre-baked slate
//! on loss. To keep the muxer from aborting and to land leading timestamps
//! cleanly, the caller sets a handful of `AVOption`s on the output container
//! **before** `write_header`:
//!
//! * `avoid_negative_ts=make_zero` — a **one-shot leading shift**: libav offsets
//!   the *first* packets so the stream starts at timestamp 0. It is **not** a
//!   mid-stream monotonicity fix; the per-stream `last_dts + 1` clamp in
//!   [`multiview-output`](https://docs.rs)'s `RestampAccumulator` is the abort
//!   guard.
//! * `max_interleave_delta=<n>` — an **interleave-flush** knob bounding how long
//!   libav buffers a stream waiting to interleave another. It is **NOT** the
//!   abort guard; setting it "small" forces premature flushing and degrades A/V
//!   interleave at the seam, so it is set deliberately (commonly `0` to disable
//!   the cap), never as a monotonicity backstop.
//!
//! This module holds only the **pure** option model so it compiles in the
//! default (no-`ffmpeg`) build and is unit-tested there: it collects ordered
//! key/value pairs and validates up front that no key or value carries an
//! interior NUL (which could never become a C string for `av_dict_set`). The
//! feature-gated `Muxer::create_with_options` turns a validated [`MuxOptions`]
//! into a libav option dictionary and passes it to `avformat_write_header`.

/// An ordered, validated set of muxer `AVOption` key/value pairs to apply at
/// `write_header`.
///
/// Built either fluently (the two known knobs — [`avoid_negative_ts_make_zero`]
/// / [`max_interleave_delta`]) or from an arbitrary slice via [`from_pairs`]
/// (which validates each key/value). Order is preserved (libav applies options
/// in dictionary order; for these independent keys order is immaterial, but a
/// stable order keeps the surface deterministic and testable).
///
/// [`avoid_negative_ts_make_zero`]: MuxOptions::avoid_negative_ts_make_zero
/// [`max_interleave_delta`]: MuxOptions::max_interleave_delta
/// [`from_pairs`]: MuxOptions::from_pairs
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuxOptions {
    pairs: Vec<(String, String)>,
}

/// An option key or value that cannot be expressed as a libav option (it carries
/// an interior NUL byte, so it could never become a C string for `av_dict_set`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("muxer option {field} {kind} contains an interior NUL byte")]
pub struct MuxOptionError {
    /// Which side of the pair was malformed (`"key"` or `"value"`).
    field: &'static str,
    /// The malformed text (for diagnostics).
    kind: String,
}

impl MuxOptions {
    /// An empty option set (the additive identity — equivalent to no options).
    #[must_use]
    pub const fn new() -> Self {
        Self { pairs: Vec::new() }
    }

    /// Build from an arbitrary slice of `(key, value)` pairs, validating that
    /// neither side carries an interior NUL.
    ///
    /// # Errors
    /// Returns [`MuxOptionError`] if any key or value contains a `\0`.
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Result<Self, MuxOptionError> {
        let mut out = Self::new();
        for &(key, value) in pairs {
            out = out.try_set(key, value)?;
        }
        Ok(out)
    }

    /// Set one option, validating both sides for an interior NUL.
    ///
    /// # Errors
    /// Returns [`MuxOptionError`] if `key` or `value` contains a `\0`.
    pub fn try_set(mut self, key: &str, value: &str) -> Result<Self, MuxOptionError> {
        if key.contains('\0') {
            return Err(MuxOptionError {
                field: "key",
                kind: key.to_owned(),
            });
        }
        if value.contains('\0') {
            return Err(MuxOptionError {
                field: "value",
                kind: value.to_owned(),
            });
        }
        self.pairs.push((key.to_owned(), value.to_owned()));
        Ok(self)
    }

    /// Add `avoid_negative_ts=make_zero` — the one-shot leading shift that lands
    /// the first packets at timestamp 0. (The literal key/value carry no NUL, so
    /// this is infallible.)
    #[must_use]
    pub fn avoid_negative_ts_make_zero(mut self) -> Self {
        self.pairs
            .push(("avoid_negative_ts".to_owned(), "make_zero".to_owned()));
        self
    }

    /// Add `max_interleave_delta=<micros>` — the interleave-flush knob (NOT the
    /// abort guard). `0` disables the cap (libav buffers until it can interleave
    /// naturally); a deliberately chosen positive value bounds the buffering.
    #[must_use]
    pub fn max_interleave_delta(mut self, micros: u64) -> Self {
        self.pairs
            .push(("max_interleave_delta".to_owned(), micros.to_string()));
        self
    }

    /// The ordered, validated key/value pairs.
    #[must_use]
    pub fn as_pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Whether any option is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}
