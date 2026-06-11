//! Lookup-table evaluation of the per-pixel transfer functions (ADR-0022).
//!
//! The CPU reference compositor's hot loop runs the transcendental EOTF/OETF
//! ([`transfer::eotf`]/[`transfer::oetf`]) three times per channel per covering
//! tile per pixel — each a `pow`/`ln`/`exp`. At 1080p × N tiles that dominates
//! the per-tick cost. This module replaces the *evaluation* of those two
//! functions with sampled tables + linear interpolation, **without changing the
//! fixed pipeline order (invariant #8)**: the front half still runs
//! range-expand → YUV→RGB matrix → EOTF → primaries-convert, and the back half
//! still runs OETF → RGB→YUV → range-compress; only the EOTF/OETF nodes are
//! looked up instead of computed.
//!
//! The un-LUT'd [`transfer`] module remains the **golden oracle**: each table is
//! built by sampling it, the equivalence is gated by `tests/lut_vs_reference.rs`
//! (front half within `2e-3` linear, frame bytes within `±1`), and any transfer
//! the oracle does not support still routes through the oracle dispatch so it
//! returns the *same* [`Error::UnsupportedTransfer`] (the LUT never panics on an
//! out-of-domain key or an unsupported axis).
//!
//! ## EOTF table — keyed on the post-matrix gamma `f32`
//!
//! The EOTF input at the pipeline's step 3 is `rgb_gamma[i]`, a **continuous**
//! gamma-encoded value (not an 8-bit sample): for saturated chroma the YUV→RGB
//! matrix overshoots, so the input can land roughly in `[-0.5, 1.5]`. The table
//! covers exactly that domain with [`EOTF_NODES`] nodes and linearly
//! interpolates between the two bracketing nodes. Out-of-domain keys saturate to
//! the nearest endpoint (never panic). Sign for negative inputs is preserved
//! automatically because the oracle (`bt709_eotf` via `signed_pow`) is sampled
//! at the negative nodes directly.
//!
//! ## OETF table — linear `f32` → gamma `f32` over `[0, 1]`
//!
//! The encode-side OETF maps a linear-light channel in `[0, 1]` to a gamma code
//! value; the table has [`OETF_NODES`] nodes with linear interpolation. The
//! range-compression that follows ([`range::compress_luma`]/`compress_chroma`)
//! is **not** folded in — RGB→YUV + range-compress stay exactly as the oracle
//! runs them.

use multiview_core::color::{ColorInfo, TransferCharacteristic};

use crate::error::{Error, Result};
use crate::pipeline::CanvasColor;
use crate::{matrix, primaries, range, transfer};

/// EOTF table domain: the post-matrix gamma value can overshoot `[0, 1]` for
/// saturated chroma. Across BT.601/709/2020 in both limited and full range the
/// widest realisable post-matrix R'/B' is ≈ `[-0.95, 1.95]`; `[-1.0, 2.0]`
/// brackets it with headroom (out-of-domain keys saturate at the endpoint, as
/// the oracle's `signed_pow` does for extreme corners).
const EOTF_DOMAIN_LO: f32 = -1.0;
/// Upper bound of the EOTF table domain (see [`EOTF_DOMAIN_LO`]).
const EOTF_DOMAIN_HI: f32 = 2.0;

/// Number of sample nodes in an EOTF table (6144 → step ≈ 4.9e-4 over the 3.0
/// domain). PQ is the steepest curve; if `tests/lut_vs_reference.rs` ever needs
/// more resolution for PQ, bump [`pq_eotf_nodes`] — never weaken the test.
const EOTF_NODES: usize = 6144;
/// Node count for the PQ EOTF table specifically. PQ's EOTF is extremely steep
/// near the top of its domain, so it gets a finer grid to stay within the
/// `±1`-byte frame tolerance (the `[0, 1]` density matches an 8192-node table
/// over the narrower `[-0.5, 1.5]` domain it was first sized for).
const PQ_EOTF_NODES: usize = 12288;
/// OETF table domain: the in-gamut canvas-linear range `[0, 1]`.
///
/// Node 0 sits exactly on `l = 0` and the last on `l = 1`, so OETF(0)=0 and
/// OETF(1)=1 are represented **exactly** (the inverse SDR transfer
/// `l^(1/2.4)` has an infinite slope at 0, so an exact node there is what keeps
/// deep-shadow output within `±1`). The encode-side OETF is fed the *blended*
/// canvas-gamut linear value, which can overshoot this range for out-of-gamut
/// wide-gamut tiles (BT.2020 / PQ / HLG → SDR BT.709); those out-of-domain
/// values fall back to the transcendental oracle (rare, and the only way to
/// reproduce its `signed_pow` extrapolation bit-for-bit). See
/// [`LutSet::oetf`].
const OETF_DOMAIN_LO: f32 = 0.0;
/// Upper bound of the OETF table domain (see [`OETF_DOMAIN_LO`]).
const OETF_DOMAIN_HI: f32 = 1.0;

/// Number of sample nodes in an OETF table over the `[0, 1]` linear domain
/// (4096 → step ≈ 2.4e-4).
const OETF_NODES: usize = 4096;

/// A single transfer function sampled into a uniform table over a fixed domain,
/// evaluated with linear interpolation between bracketing nodes.
#[derive(Debug, Clone)]
struct Lut1d {
    lo: f32,
    hi: f32,
    /// Precomputed `(nodes - 1) / (hi - lo)` so lookup is a multiply, not a
    /// divide.
    inv_step: f32,
    /// The highest valid node index (`nodes - 1`); `samples.len() == last + 1`.
    last: u16,
    /// `nodes` sampled values, `samples[0] == f(lo)`, `samples[last] == f(hi)`.
    samples: Vec<f32>,
}

impl Lut1d {
    /// Build a table of `nodes` samples of `f` over `[lo, hi]` (uniform grid).
    ///
    /// `nodes` is clamped to at least 2 so the grid always has a step. Each
    /// sample is taken at `lo + (i / (n-1)) * (hi - lo)` for integer `i`, so the
    /// only float<->int conversions here are the lossless `u16 -> f32`
    /// (int->float) widening — never a float->int cast.
    fn build(lo: f32, hi: f32, nodes: usize, mut f: impl FnMut(f32) -> f32) -> Self {
        // The node counts are small compile-time constants (<= 8192), so the
        // grid index always fits a u16 and `f32::from(u16)` is exact.
        let n = nodes.max(2).min(usize::from(u16::MAX) + 1);
        let last = u16::try_from(n - 1).unwrap_or(u16::MAX);
        let span = hi - lo;
        let denom = f32::from(last);
        // Lookup needs `(x - lo) * inv_step`, where `inv_step` maps the domain to
        // node units (`last / span`).
        let inv_step = if span == 0.0 { 0.0 } else { denom / span };
        let mut samples = Vec::with_capacity(n);
        for i in 0..=last {
            let frac = f32::from(i) / denom;
            samples.push(f(lo + frac * span));
        }
        Self {
            lo,
            hi,
            inv_step,
            last,
            samples,
        }
    }

    /// Evaluate the table at `x` only when `x` lies within `[lo, hi]`; returns
    /// [`None`] otherwise so the caller can defer to the exact transcendental
    /// oracle for out-of-domain values (used by the OETF, whose input can
    /// overshoot the in-gamut `[0, 1]` range). NaN is treated as out-of-domain.
    fn eval_in_domain(&self, x: f32) -> Option<f32> {
        if x.is_nan() || x < self.lo || x > self.hi {
            None
        } else {
            Some(self.eval(x))
        }
    }

    /// Evaluate the table at `x` with linear interpolation; out-of-domain keys
    /// saturate to the nearest endpoint (never panic, never index out of
    /// bounds).
    fn eval(&self, x: f32) -> f32 {
        let clamped = x.clamp(self.lo, self.hi);
        // `pos` is in `[0, last]` after the clamp. The lower node is `floor(pos)`
        // and the fraction is the remainder; the upper node is `i + 1`.
        let pos = ((clamped - self.lo) * self.inv_step).clamp(0.0, f32::from(self.last));
        let floor = pos.floor();
        let frac = pos - floor;
        let i = node_index(floor, self.last);
        let lo = self.samples.get(i).copied();
        let hi = self.samples.get(i + 1).copied();
        match (lo, hi) {
            (Some(a), Some(b)) => a + (b - a) * frac,
            // On (or past) the final node `i + 1` is absent: return the endpoint.
            (Some(a), None) => a,
            // `i` out of range can only happen for an empty table, which `build`
            // forbids; fall back to 0.0 defensively.
            _ => 0.0,
        }
    }
}

/// Convert an integral, clamped table position `floor` (already in
/// `0.0..=last`) to a `usize` node index. `floor` is the result of
/// `pos.floor()` where `pos` was clamped to `[0, last]`, so it is a small
/// non-negative integral `f32` (`<= 8192`); the cast is exact and in range.
fn node_index(floor: f32, last: u16) -> usize {
    let bounded = floor.clamp(0.0, f32::from(last));
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    // reason: `bounded` is clamped to `0.0..=last` (<= 8192) and is integral
    // after `floor()`, so this float->int truncation is exact and in range —
    // the same localized, documented pattern as `range::quantize_u8`.
    let idx = bounded as usize;
    idx
}

/// A pair of tables (EOTF + OETF) for one transfer characteristic.
#[derive(Debug, Clone)]
struct TransferLut {
    transfer: TransferCharacteristic,
    eotf: Lut1d,
    oetf: Lut1d,
}

/// The set of transfer LUTs needed for one composite: one [`TransferLut`] per
/// distinct supported transfer present among the tiles + canvas.
///
/// Build it **once** per composite (cheap: a handful of 4–8 K-entry tables) and
/// thread `&LutSet` into the per-pixel path. Unsupported transfers are simply
/// absent from the set; the lookup methods fall back to the oracle dispatch for
/// those, which returns the identical [`Error`].
#[derive(Debug, Clone, Default)]
pub struct LutSet {
    luts: Vec<TransferLut>,
}

impl LutSet {
    /// Build the LUT set for exactly the transfer characteristics in
    /// `transfers` (duplicates and unsupported variants are ignored — the
    /// latter route through the oracle on lookup and return its `Err`).
    #[must_use]
    pub fn for_transfers<I>(transfers: I) -> Self
    where
        I: IntoIterator<Item = TransferCharacteristic>,
    {
        let mut luts: Vec<TransferLut> = Vec::new();
        for t in transfers {
            if luts.iter().any(|l| l.transfer == t) {
                continue;
            }
            if let Some(lut) = build_transfer_lut(t) {
                luts.push(lut);
            }
        }
        Self { luts }
    }

    /// Look up the EOTF for `transfer` at the post-matrix gamma value `c`.
    ///
    /// Falls back to the oracle [`transfer::eotf`] when no table was built for
    /// `transfer` (an unsupported axis), preserving the exact error contract.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedTransfer`] for a transfer with no
    /// CPU-reference implementation (exactly as the oracle does).
    fn eotf(&self, c: f32, transfer: TransferCharacteristic) -> Result<f32> {
        match self.luts.iter().find(|l| l.transfer == transfer) {
            Some(lut) => Ok(lut.eotf.eval(c)),
            None => transfer::eotf(c, transfer),
        }
    }

    /// Look up the OETF for `transfer` at the linear-light value `l`.
    ///
    /// The table covers the in-gamut `[0, 1]` linear range; values that
    /// overshoot it (out-of-gamut wide-gamut tiles converted into the SDR
    /// canvas) and unsupported transfers fall back to the transcendental oracle
    /// [`transfer::oetf`] — the only way to reproduce its `signed_pow`
    /// extrapolation exactly. Overshoot pixels are rare, so the fallback's cost
    /// is negligible.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedTransfer`] for an unsupported transfer.
    fn oetf(&self, l: f32, transfer: TransferCharacteristic) -> Result<f32> {
        match self.luts.iter().find(|l2| l2.transfer == transfer) {
            Some(lut) => match lut.oetf.eval_in_domain(l) {
                Some(v) => Ok(v),
                None => transfer::oetf(l, transfer),
            },
            None => transfer::oetf(l, transfer),
        }
    }

    /// LUT-backed twin of [`crate::pipeline::tile_yuv_to_canvas_linear`]: the
    /// front half of the fixed pipeline with the EOTF evaluated by table.
    ///
    /// Order is identical: range-expand → YUV→RGB matrix → EOTF (LUT) →
    /// primaries convert. Only the `eotf` calls differ from the oracle.
    ///
    /// # Errors
    ///
    /// Same as [`crate::pipeline::tile_yuv_to_canvas_linear`].
    pub fn tile_yuv_to_canvas_linear(
        &self,
        y8: u8,
        cb8: u8,
        cr8: u8,
        tile_color: ColorInfo,
        canvas: CanvasColor,
    ) -> Result<[f32; 3]> {
        let tile_range = range::require_resolved(tile_color.range)?;
        if tile_color.transfer == TransferCharacteristic::Unspecified {
            return Err(Error::UnresolvedColor("transfer"));
        }
        if tile_color.matrix == multiview_core::color::MatrixCoefficients::Unspecified {
            return Err(Error::UnresolvedColor("matrix"));
        }
        if tile_color.primaries == multiview_core::color::ColorPrimaries::Unspecified {
            return Err(Error::UnresolvedColor("primaries"));
        }

        let y = range::expand_luma(y8, tile_range);
        let cb = range::expand_chroma(cb8, tile_range);
        let cr = range::expand_chroma(cr8, tile_range);

        let rgb_gamma = matrix::yuv_to_rgb(y, cb, cr, tile_color.matrix)?;

        let lin = [
            self.eotf(rgb_gamma[0], tile_color.transfer)?,
            self.eotf(rgb_gamma[1], tile_color.transfer)?,
            self.eotf(rgb_gamma[2], tile_color.transfer)?,
        ];

        let conv = primaries::convert_matrix(tile_color.primaries, canvas.primaries)?;
        Ok(primaries::apply(conv, lin))
    }

    /// LUT-backed twin of [`crate::pipeline::canvas_linear_to_output_yuv`]: the
    /// back half with the OETF evaluated by table.
    ///
    /// Order is identical: OETF (LUT) → RGB→YUV → range-compress. Only the
    /// `oetf` calls differ from the oracle; RGB→YUV and range-compress are
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Same as [`crate::pipeline::canvas_linear_to_output_yuv`].
    pub fn canvas_linear_to_output_yuv(
        &self,
        lin: [f32; 3],
        canvas: CanvasColor,
    ) -> Result<[u8; 3]> {
        let gamma = [
            self.oetf(lin[0], canvas.transfer)?,
            self.oetf(lin[1], canvas.transfer)?,
            self.oetf(lin[2], canvas.transfer)?,
        ];
        let yuv = matrix::rgb_to_yuv(gamma[0], gamma[1], gamma[2], canvas.matrix)?;
        Ok([
            range::compress_luma(yuv[0], canvas.range),
            range::compress_chroma(yuv[1], canvas.range),
            range::compress_chroma(yuv[2], canvas.range),
        ])
    }
}

/// Build the EOTF+OETF table pair for one transfer, or [`None`] if the oracle
/// has no implementation for it (so the caller falls back to the oracle and
/// surfaces its `Err`).
fn build_transfer_lut(transfer: TransferCharacteristic) -> Option<TransferLut> {
    // Probe the oracle once: if it errors here it errors everywhere, so there is
    // nothing to table — return None and let the lookup path replay the error.
    if transfer::eotf(0.0, transfer).is_err() {
        return None;
    }
    let eotf_nodes = if transfer == TransferCharacteristic::Pq {
        PQ_EOTF_NODES
    } else {
        EOTF_NODES
    };
    let eotf = Lut1d::build(EOTF_DOMAIN_LO, EOTF_DOMAIN_HI, eotf_nodes, |c| {
        // Sampling the oracle inside the domain; it is total for a supported
        // transfer (checked above), so the `unwrap_or` is never taken.
        transfer::eotf(c, transfer).unwrap_or(0.0)
    });
    let oetf = Lut1d::build(OETF_DOMAIN_LO, OETF_DOMAIN_HI, OETF_NODES, |l| {
        transfer::oetf(l, transfer).unwrap_or(0.0)
    });
    Some(TransferLut {
        transfer,
        eotf,
        oetf,
    })
}

/// Expose the PQ EOTF table node count for documentation/tests referencing the
/// resolution decision.
#[must_use]
pub const fn pq_eotf_nodes() -> usize {
    PQ_EOTF_NODES
}
