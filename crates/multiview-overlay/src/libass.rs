//! ASS/SSA subtitle rendering capability + graceful fallback (ADR-R007).
//!
//! Full ASS/SSA (Advanced `SubStation` Alpha) styling — karaoke, positioning,
//! per-event fonts and transforms — is rendered by **libass** (which pulls in
//! `HarfBuzz` + `FriBidi`), a native C toolchain. Per the repo's licensing and
//! build discipline that is an **off-by-default** capability behind the
//! `libass` feature; the default build is pure Rust with no native dependency.
//!
//! This module is the thin, always-compiled **capability gate**: it reports
//! whether ASS rendering is available in this build ([`AssCapability::detect`])
//! and, when it is not, names the [`SubtitleFallback`] the engine uses instead —
//! the pure SRT/VTT path ([`crate::subtitle`]) rendered by the stage-1 text
//! engine. The actual libass binding (frame → premultiplied bitmaps) lives
//! downstream behind the feature; nothing here links libass. The point is that
//! a config naming an `.ass` track still produces output: with `libass` it is
//! styled, without it the plain text is burned in (graceful degradation).

use serde::{Deserialize, Serialize};

/// Whether native ASS/SSA rendering (libass) is compiled into this build.
///
/// This is a **compile-time** capability: the `libass` Cargo feature decides it,
/// so [`AssCapability::detect`] is `const`-evaluable and never probes the
/// filesystem or dlopen's anything. A runtime probe of an actually-loadable
/// library is a downstream concern of the feature-gated binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AssCapability {
    /// libass is compiled in: ASS/SSA is rendered with full styling.
    Available,
    /// libass is **not** compiled in: ASS/SSA degrades to plain-text burn-in via
    /// the SRT/VTT path.
    Unavailable,
}

impl AssCapability {
    /// Detect whether ASS rendering is available in this build.
    ///
    /// `Available` iff the crate was built with the `libass` feature; otherwise
    /// `Unavailable`. Compile-time, total, and panic-free.
    #[must_use]
    pub const fn detect() -> Self {
        #[cfg(feature = "libass")]
        {
            Self::Available
        }
        #[cfg(not(feature = "libass"))]
        {
            Self::Unavailable
        }
    }

    /// Whether full ASS styling is available.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    /// The rendering path the engine should take for an ASS/SSA track in this
    /// build: native libass when available, else the SRT/VTT text fallback.
    #[must_use]
    pub const fn fallback(self) -> SubtitleFallback {
        match self {
            Self::Available => SubtitleFallback::Libass,
            Self::Unavailable => SubtitleFallback::PlainText,
        }
    }
}

/// How an ASS/SSA track is actually rendered in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubtitleFallback {
    /// Rendered by native libass with full styling.
    Libass,
    /// Rendered as plain text (markup stripped) by the stage-1 text engine — the
    /// graceful fallback when libass is not compiled in.
    PlainText,
}

impl SubtitleFallback {
    /// A short label for diagnostics / the management surface.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Libass => "libass (styled)",
            Self::PlainText => "plain-text fallback",
        }
    }
}
