//! Display mode selection (ADR-0044 / brief §6): EDID preferred mode,
//! **exact-rational** refresh matching against the engine cadence (never
//! float fps — invariant #3), explicit operator overrides, and the CVT-RB
//! forced-mode computation for EDID-less heads.
//!
//! Everything here is a pure function over plain data and is CI-tested
//! without hardware; the KMS backend converts kernel `drm_mode_modeinfo`
//! records to/from [`DisplayModeInfo`] at the edge.

use multiview_core::time::Rational;
use thiserror::Error;

/// One display mode: active geometry plus full sync timings — enough to
/// reconstruct a kernel `drm_mode_modeinfo` exactly.
///
/// The refresh rate is **derived** ([`DisplayModeInfo::refresh`]) from the
/// pixel clock and totals as an exact rational, exactly as KMS computes it;
/// it is never stored as a float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayModeInfo {
    /// Active width in pixels.
    pub width: u32,
    /// Active height in pixels.
    pub height: u32,
    /// Pixel clock in kHz.
    pub clock_khz: u32,
    /// Horizontal sync start (pixels).
    pub hsync_start: u32,
    /// Horizontal sync end (pixels).
    pub hsync_end: u32,
    /// Horizontal total (pixels).
    pub htotal: u32,
    /// Vertical sync start (lines).
    pub vsync_start: u32,
    /// Vertical sync end (lines).
    pub vsync_end: u32,
    /// Vertical total (lines).
    pub vtotal: u32,
    /// Positive horizontal sync polarity.
    pub hsync_positive: bool,
    /// Positive vertical sync polarity.
    pub vsync_positive: bool,
    /// Whether EDID flags this as the sink's preferred mode.
    pub preferred: bool,
}

impl DisplayModeInfo {
    /// The refresh rate as an exact rational:
    /// `clock_khz * 1000 / (htotal * vtotal)`.
    ///
    /// Degenerate timings (zero totals, or a product outside `i64`) yield the
    /// invalid rational `0/1`, which matches nothing.
    #[must_use]
    pub fn refresh(&self) -> Rational {
        let num = u64::from(self.clock_khz).saturating_mul(1000);
        let den = u64::from(self.htotal).saturating_mul(u64::from(self.vtotal));
        match (i64::try_from(num), i64::try_from(den)) {
            (Ok(num), Ok(den)) if den > 0 => Rational::new(num, den),
            _ => Rational::new(0, 1),
        }
    }

    /// `"1920x1080@60000/1001"`-shaped diagnostic label.
    #[must_use]
    pub fn describe(&self) -> String {
        let r = self.refresh().reduce();
        format!("{}x{}@{}/{}", self.width, self.height, r.num, r.den)
    }
}

/// The authored mode request for a head.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModeRequest {
    /// Automatic: EDID preferred mode, upgraded to the sibling whose refresh
    /// rationally matches the engine cadence when one exists.
    Auto,
    /// Explicit override: the EDID mode with this exact geometry and a
    /// rationally-matching refresh. No match is an error (never a silent
    /// nearest-fit).
    Exact {
        /// Required active width.
        width: u32,
        /// Required active height.
        height: u32,
        /// Required refresh as an exact rational.
        refresh: Rational,
    },
}

/// The authored CVT-RB forced mode for an EDID-less connector (brief §6 — a
/// verified field condition, not a nice-to-have).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedMode {
    /// Active width in pixels.
    pub width: u32,
    /// Active height in pixels.
    pub height: u32,
    /// Requested refresh as an exact rational.
    pub refresh: Rational,
}

/// The outcome of mode selection: where the committed timing came from.
/// Deliberately exhaustive — a timing comes from exactly one of two places
/// (the sink's EDID, or a computed CVT-RB forced mode) and consumers are
/// expected to branch on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedMode {
    /// A mode the sink's EDID advertises.
    Edid(DisplayModeInfo),
    /// A CVT-RB timing computed from the per-connector forced-mode config
    /// (EDID-less head).
    ForcedCvtRb(DisplayModeInfo),
}

impl SelectedMode {
    /// The selected timing, however it was chosen.
    #[must_use]
    pub fn mode(&self) -> &DisplayModeInfo {
        match self {
            SelectedMode::Edid(m) | SelectedMode::ForcedCvtRb(m) => m,
        }
    }
}

/// Mode-selection failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModeError {
    /// The connector exposes no modes (EDID-less) and no forced mode is
    /// configured: there is nothing safe to commit.
    #[error(
        "connector exposes no EDID modes and no forced_mode is configured \
         (an EDID-less head needs `forced_mode = {{ width, height, refresh }}`): {hint}"
    )]
    NoModes {
        /// Context for the operator (connector name where known).
        hint: String,
    },
    /// An explicit override matched none of the EDID modes.
    #[error("no EDID mode matches the requested {requested} (available: {available:?})")]
    NoMatch {
        /// The requested geometry/refresh.
        requested: String,
        /// The modes the sink actually advertises.
        available: Vec<String>,
    },
    /// A forced-mode request with degenerate geometry or refresh.
    #[error("forced mode is not computable: {0}")]
    Geometry(String),
}

/// Whether a mode's refresh **rationally matches** a target cadence.
///
/// Pure integer cross-multiplication in `i128` (never float): true when
/// `|mode − target| <= target / 2000` (0.05 %). The window is chosen so that
/// the NTSC pair 60.000 vs 59.94 (0.1 % apart) never cross-matches, while
/// wire-rounded kernel clocks (≤ 0.001 % off) and CVT-RB step-down clocks
/// (≤ 0.03 % off) still match their requested rate.
#[must_use]
pub fn refresh_matches(mode: Rational, target: Rational) -> bool {
    if !mode.is_valid() || !target.is_valid() || mode.num <= 0 || target.num <= 0 {
        return false;
    }
    let a_num = i128::from(mode.num);
    let a_den = i128::from(mode.den);
    let b_num = i128::from(target.num);
    let b_den = i128::from(target.den);
    if a_den <= 0 || b_den <= 0 {
        return false;
    }
    // |a/b_target| difference scaled by a_den*b_den (both positive):
    // |a_num*b_den − b_num*a_den| <= b_num*a_den / 2000.
    let diff = (a_num * b_den - b_num * a_den).abs();
    diff.saturating_mul(2000) <= b_num * a_den
}

/// Select the timing for a head per ADR-0044 §6.
///
/// Policy, in order:
/// 1. **EDID-less** (`modes` empty): the configured `forced` CVT-RB timing,
///    else [`ModeError::NoModes`].
/// 2. **Explicit request**: the EDID mode with the exact geometry and a
///    rationally-matching refresh, else [`ModeError::NoMatch`].
/// 3. **Auto**: anchor on the EDID preferred mode (or the largest-area,
///    highest-refresh mode when none is flagged); among the modes at the
///    anchor's resolution, prefer the one whose refresh rationally matches
///    the engine `cadence` (zero steady-state repeat/drop); otherwise the
///    anchor itself.
///
/// # Errors
///
/// [`ModeError::NoModes`] / [`ModeError::NoMatch`] as above, and
/// [`ModeError::Geometry`] when a forced mode is not computable.
pub fn select_mode(
    modes: &[DisplayModeInfo],
    request: &ModeRequest,
    forced: Option<&ForcedMode>,
    cadence: Option<Rational>,
) -> Result<SelectedMode, ModeError> {
    if modes.is_empty() {
        let Some(forced) = forced else {
            return Err(ModeError::NoModes {
                hint: "no forced_mode configured".to_owned(),
            });
        };
        let mode = cvt_rb_mode(forced.width, forced.height, forced.refresh)?;
        return Ok(SelectedMode::ForcedCvtRb(mode));
    }
    match request {
        ModeRequest::Exact {
            width,
            height,
            refresh,
        } => modes
            .iter()
            .find(|m| {
                m.width == *width && m.height == *height && refresh_matches(m.refresh(), *refresh)
            })
            .cloned()
            .map(SelectedMode::Edid)
            .ok_or_else(|| {
                let r = refresh.reduce();
                ModeError::NoMatch {
                    requested: format!("{width}x{height}@{}/{}", r.num, r.den),
                    available: modes.iter().map(DisplayModeInfo::describe).collect(),
                }
            }),
        ModeRequest::Auto => {
            let anchor = anchor_mode(modes);
            if let Some(cadence) = cadence {
                if let Some(matched) = modes.iter().find(|m| {
                    m.width == anchor.width
                        && m.height == anchor.height
                        && refresh_matches(m.refresh(), cadence)
                }) {
                    return Ok(SelectedMode::Edid(matched.clone()));
                }
            }
            Ok(SelectedMode::Edid(anchor.clone()))
        }
    }
}

/// The auto-policy anchor: the EDID preferred mode, or — when none is
/// flagged — the largest-area mode, highest refresh as the tie-break.
fn anchor_mode(modes: &[DisplayModeInfo]) -> &DisplayModeInfo {
    if let Some(preferred) = modes.iter().find(|m| m.preferred) {
        return preferred;
    }
    let mut best: Option<&DisplayModeInfo> = None;
    for m in modes {
        let better = match best {
            None => true,
            Some(b) => {
                let area_m = u64::from(m.width) * u64::from(m.height);
                let area_b = u64::from(b.width) * u64::from(b.height);
                area_m > area_b || (area_m == area_b && rational_gt(m.refresh(), b.refresh()))
            }
        };
        if better {
            best = Some(m);
        }
    }
    // `modes` is non-empty at every call site (checked in `select_mode`), so
    // the fold above always produced a candidate; the first element is the
    // never-taken fallback that keeps this total without a panic path.
    best.map_or_else(|| FALLBACK_MODE_REF, |m| m)
}

/// See [`anchor_mode`] — a static degenerate mode used only as the
/// unreachable empty-slice fallback (kept to avoid any panicking path).
static FALLBACK_MODE: DisplayModeInfo = DisplayModeInfo {
    width: 0,
    height: 0,
    clock_khz: 0,
    hsync_start: 0,
    hsync_end: 0,
    htotal: 0,
    vsync_start: 0,
    vsync_end: 0,
    vtotal: 0,
    hsync_positive: false,
    vsync_positive: false,
    preferred: false,
};
static FALLBACK_MODE_REF: &DisplayModeInfo = &FALLBACK_MODE;

/// Compare two rationals (`a > b`) by `i128` cross-multiplication.
fn rational_gt(a: Rational, b: Rational) -> bool {
    if !a.is_valid() || !b.is_valid() {
        return false;
    }
    let (an, ad) = (i128::from(a.num), i128::from(a.den));
    let (bn, bd) = (i128::from(b.num), i128::from(b.den));
    // Normalize the sign of the denominators so the inequality direction holds.
    let (an, ad) = if ad < 0 { (-an, -ad) } else { (an, ad) };
    let (bn, bd) = if bd < 0 { (-bn, -bd) } else { (bn, bd) };
    an * bd > bn * ad
}

/// Compute a **CVT-RB v1** (VESA Coordinated Video Timings, reduced blanking)
/// mode for `width` x `height` at `refresh` — bit-identical to the kernel's
/// `drm_cvt_mode(..., reduced=true)` integer algorithm, so a config forced
/// mode lands on the same timing a `video=` kernel parameter would.
///
/// All arithmetic is exact integer/rational (`i128` intermediates); the pixel
/// clock floors to the 250 kHz CVT clock step. Reduced-blanking polarity:
/// **+hsync / −vsync**.
///
/// # Errors
///
/// [`ModeError::Geometry`] for zero geometry, a non-positive refresh, or a
/// refresh too high for the requested height (no blanking time left).
pub fn cvt_rb_mode(
    width: u32,
    height: u32,
    refresh: Rational,
) -> Result<DisplayModeInfo, ModeError> {
    /// Fixed CVT-RB horizontal blanking, pixels.
    const RB_H_BLANK: u32 = 160;
    /// CVT-RB horizontal sync width, pixels.
    const RB_H_SYNC: u32 = 32;
    /// Minimum vertical blanking interval, microseconds.
    const RB_MIN_VBLANK_US: i128 = 460;
    /// Vertical front porch, lines.
    const RB_V_FPORCH: u32 = 3;
    /// Minimum vertical back porch, lines.
    const RB_MIN_V_BPORCH: u32 = 6;
    /// CVT pixel-clock step, kHz.
    const CLOCK_STEP_KHZ: i128 = 250;

    if width == 0 || height == 0 {
        return Err(ModeError::Geometry(format!(
            "active geometry must be positive (got {width}x{height})"
        )));
    }
    let refresh = refresh.reduce();
    if !refresh.is_valid() || refresh.num <= 0 || refresh.den <= 0 {
        return Err(ModeError::Geometry(format!(
            "refresh must be a positive exact rational (got {}/{})",
            refresh.num, refresh.den
        )));
    }

    // Field period in µs, floored — the kernel's `1000000 / vfieldrate`.
    let period_us = 1_000_000i128 * i128::from(refresh.den) / i128::from(refresh.num);
    if period_us <= RB_MIN_VBLANK_US {
        return Err(ModeError::Geometry(format!(
            "refresh {}/{} leaves no vertical blanking time",
            refresh.num, refresh.den
        )));
    }
    // Estimated line period in µs, floored (kernel integer division).
    let hperiod_us = (period_us - RB_MIN_VBLANK_US) / i128::from(height);
    let vsync_width = cvt_vsync_width(width, height);
    let min_vbi = vsync_width + RB_V_FPORCH + RB_MIN_V_BPORCH;
    let vbi = if hperiod_us > 0 {
        let estimated = RB_MIN_VBLANK_US / hperiod_us + 1;
        u32::try_from(estimated).unwrap_or(u32::MAX).max(min_vbi)
    } else {
        min_vbi
    };
    let vtotal = height
        .checked_add(vbi)
        .ok_or_else(|| ModeError::Geometry("vertical total overflows".to_owned()))?;
    let htotal = width
        .checked_add(RB_H_BLANK)
        .ok_or_else(|| ModeError::Geometry("horizontal total overflows".to_owned()))?;

    // Pixel clock: htotal * vtotal * refresh, Hz → kHz floored, then floored
    // onto the 250 kHz CVT step (the kernel's CVT_CLOCK_STEP).
    let raw_hz =
        i128::from(htotal) * i128::from(vtotal) * i128::from(refresh.num) / i128::from(refresh.den);
    let mut stepped_khz = raw_hz / 1000;
    stepped_khz -= stepped_khz % CLOCK_STEP_KHZ;
    let clock_khz = u32::try_from(stepped_khz)
        .ok()
        .filter(|c| *c > 0)
        .ok_or_else(|| ModeError::Geometry("pixel clock is zero or out of range".to_owned()))?;

    Ok(DisplayModeInfo {
        width,
        height,
        clock_khz,
        // RB horizontal layout: sync ends at hdisplay + hblank/2; width 32.
        hsync_start: width + RB_H_BLANK / 2 - RB_H_SYNC,
        hsync_end: width + RB_H_BLANK / 2,
        htotal,
        vsync_start: height + RB_V_FPORCH,
        vsync_end: height + RB_V_FPORCH + vsync_width,
        vtotal,
        hsync_positive: true,
        vsync_positive: false,
        preferred: false,
    })
}

/// The CVT vertical sync width, by aspect ratio (VESA CVT table, as the
/// kernel implements it): 4:3 → 4, 16:9 → 5, 16:10 → 6, 5:4 → 7, 15:9 → 8,
/// anything else (custom) → 10.
fn cvt_vsync_width(width: u32, height: u32) -> u32 {
    let h = u64::from(height);
    let w = u64::from(width);
    if height % 3 == 0 && h * 4 / 3 == w {
        4
    } else if height % 9 == 0 && h * 16 / 9 == w {
        5
    } else if height % 10 == 0 && h * 16 / 10 == w {
        6
    } else if height % 4 == 0 && h * 5 / 4 == w {
        7
    } else if height % 9 == 0 && h * 15 / 9 == w {
        8
    } else {
        10
    }
}
