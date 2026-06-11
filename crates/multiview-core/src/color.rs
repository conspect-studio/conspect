//! The four INDEPENDENT color axes and the untagged-default policy.
//!
//! Per invariant #8, a frame's color is described by four orthogonal axes:
//! primaries, transfer, matrix, and range. `Unspecified` on any axis means the
//! source did **not** signal that axis. [`ColorInfo::resolve_defaults`]
//! implements the player-style untagged-default policy (matrix/primaries inferred
//! from geometry, transfer always SDR BT.709, range limited), filling only the
//! unspecified axes and never overwriting a signalled one. Per ADR-C002 it never
//! promotes to BT.2020/PQ/HLG from resolution.
use serde::{Deserialize, Serialize};

/// Color primaries (gamut).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ColorPrimaries {
    /// Not signalled.
    #[default]
    Unspecified,
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.601 525-line / SMPTE-170M (NTSC; CICP code point 6/7).
    ///
    /// The 525-line (NTSC) primaries. The matrix coefficients for both BT.601
    /// systems are the single [`MatrixCoefficients::Bt601`] matrix, but the
    /// *primaries* (chromaticities/gamut) differ between 525-line and 625-line
    /// SD, so they are distinct variants here per ADR-C002.
    Bt601_525,
    /// ITU-R BT.601 625-line / BT.470 System B/G (PAL/SECAM; CICP code point 5).
    ///
    /// The 625-line (PAL) primaries, distinct from the 525-line/NTSC gamut.
    Bt601_625,
    /// ITU-R BT.2020.
    ///
    /// Wide-gamut. Per ADR-C002 this is **only ever** selected when a source
    /// explicitly signals it — never inferred from resolution.
    Bt2020,
}

/// Transfer characteristics (opto-electronic transfer / gamma).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TransferCharacteristic {
    /// Not signalled.
    #[default]
    Unspecified,
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.601 / SMPTE-170M (SD).
    Bt601,
    /// ITU-R BT.2020 (10/12-bit).
    Bt2020,
    /// sRGB.
    Srgb,
    /// SMPTE ST 2084 (PQ).
    Pq,
    /// ARIB STD-B67 (HLG).
    Hlg,
}

/// Matrix coefficients (YUV<->RGB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MatrixCoefficients {
    /// Not signalled.
    #[default]
    Unspecified,
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.601.
    ///
    /// The single BT.601 YUV<->RGB matrix shared by both 525-line (NTSC) and
    /// 625-line (PAL) SD systems; the 525/625 distinction lives on
    /// [`ColorPrimaries`], not here.
    Bt601,
    /// ITU-R BT.2020 non-constant luminance.
    Bt2020Ncl,
    /// Identity (samples are already RGB).
    Rgb,
}

/// Quantization range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ColorRange {
    /// Not signalled.
    #[default]
    Unspecified,
    /// Limited / "TV" / MPEG (e.g. 16-235 luma at 8-bit).
    Limited,
    /// Full / "PC" / JPEG (0-255 at 8-bit).
    Full,
}

/// The untagged-default *gamut/matrix class* chosen by the player-style
/// resolution heuristic (ADR-C002 / color-management.md §3.2).
///
/// This mirrors libplacebo/mpv, **not** swscale's flat BT.601. Crucially it has
/// **no UHD/BT.2020 tier**: the heuristic NEVER promotes to BT.2020/PQ/HLG from
/// resolution (4K is still guessed BT.709, exactly as libplacebo does), because
/// mis-promoting SDR to HDR is catastrophic. Wide-gamut/HDR is honoured only
/// when a source *explicitly* signals it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SdrColorClass {
    /// BT.601 525-line (NTSC / SMPTE-170M): primaries [`ColorPrimaries::Bt601_525`],
    /// matrix [`MatrixCoefficients::Bt601`].
    Bt601Ntsc,
    /// BT.601 625-line (PAL / BT.470 B/G): primaries [`ColorPrimaries::Bt601_625`],
    /// matrix [`MatrixCoefficients::Bt601`].
    Bt601Pal,
    /// BT.709: primaries [`ColorPrimaries::Bt709`], matrix
    /// [`MatrixCoefficients::Bt709`]. The default for HD and anything that does
    /// not match an SD line count — including UHD.
    Bt709,
}

impl SdrColorClass {
    /// Classify by frame geometry per the player heuristic
    /// (color-management.md §3.2):
    ///
    /// - `width >= 1280 OR height > 576` => BT.709 (covers HD **and UHD**);
    /// - `height == 576` => BT.601 625-line (PAL);
    /// - `height == 480` or `height == 486` => BT.601 525-line (NTSC);
    /// - otherwise => BT.709 (the libplacebo default for everything else,
    ///   including a zero/unknown height).
    ///
    /// The order matters: the `width >= 1280 OR height > 576` rule is checked
    /// first so 4K (and any wide HD) classifies as BT.709 before the SD
    /// line-count cases can apply.
    const fn classify(width: u32, height: u32) -> Self {
        if width >= 1280 || height > 576 {
            Self::Bt709
        } else if height == 576 {
            Self::Bt601Pal
        } else if height == 480 || height == 486 {
            Self::Bt601Ntsc
        } else {
            Self::Bt709
        }
    }

    /// The primaries this class resolves to.
    const fn primaries(self) -> ColorPrimaries {
        match self {
            Self::Bt601Ntsc => ColorPrimaries::Bt601_525,
            Self::Bt601Pal => ColorPrimaries::Bt601_625,
            Self::Bt709 => ColorPrimaries::Bt709,
        }
    }

    /// The matrix coefficients this class resolves to. Both BT.601 systems share
    /// the single BT.601 matrix; the 525/625 distinction is carried on the
    /// primaries.
    const fn matrix(self) -> MatrixCoefficients {
        match self {
            Self::Bt601Ntsc | Self::Bt601Pal => MatrixCoefficients::Bt601,
            Self::Bt709 => MatrixCoefficients::Bt709,
        }
    }
}

/// The complete, independent color description of a frame (all four axes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ColorInfo {
    /// Primaries axis.
    pub primaries: ColorPrimaries,
    /// Transfer axis.
    pub transfer: TransferCharacteristic,
    /// Matrix axis.
    pub matrix: MatrixCoefficients,
    /// Range axis.
    pub range: ColorRange,
}

impl ColorInfo {
    /// Fill any `Unspecified` axes from the player-style untagged-default policy
    /// (ADR-C002 / color-management.md §3.2), leaving every signalled axis
    /// untouched.
    ///
    /// This is the "detect 4 axes" step of the color pipeline (invariant #8).
    /// It reproduces libplacebo/mpv, **not** swscale's flat BT.601:
    ///
    /// - **Matrix + primaries** key on geometry — `width >= 1280 OR
    ///   height > 576` => BT.709; `height == 576` => BT.601 625-line (PAL);
    ///   `height == 480` or `486` => BT.601 525-line (NTSC); otherwise BT.709.
    /// - **Transfer (TRC)** is **always** BT.709/BT.1886 for untagged SDR video,
    ///   regardless of resolution. sRGB is the default only for RGB/graphics
    ///   sources, which this resolution heuristic does not detect, so the video
    ///   default is BT.709.
    /// - **Range** defaults to [`ColorRange::Limited`] (broadcast video).
    ///
    /// It **NEVER** selects BT.2020/PQ/HLG from resolution — wide-gamut/HDR is
    /// honoured only when a source explicitly signals it (mis-promoting SDR to
    /// HDR is catastrophic; ADR-C002). A genuinely signalled BT.2020/PQ/HLG axis
    /// therefore passes through untouched, and a zero/unknown geometry falls
    /// back to the BT.709 default.
    #[must_use]
    pub fn resolve_defaults(self, width: u32, height: u32) -> Self {
        let class = SdrColorClass::classify(width, height);
        Self {
            primaries: match self.primaries {
                // Fill only when unspecified; preserve any signalled gamut
                // (including a genuinely-tagged BT.2020).
                ColorPrimaries::Unspecified => class.primaries(),
                signalled => signalled,
            },
            transfer: match self.transfer {
                // Untagged SDR video transfer is always BT.709 — never inferred
                // from resolution, never auto-promoted to PQ/HLG/BT.2020.
                TransferCharacteristic::Unspecified => TransferCharacteristic::Bt709,
                signalled => signalled,
            },
            matrix: match self.matrix {
                MatrixCoefficients::Unspecified => class.matrix(),
                signalled => signalled,
            },
            range: match self.range {
                ColorRange::Unspecified => ColorRange::Limited,
                signalled => signalled,
            },
        }
    }
}
