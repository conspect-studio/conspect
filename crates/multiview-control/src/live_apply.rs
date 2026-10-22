//! Per-collection **live-apply capability** (ADR-W022).
//!
//! The control plane cannot see the engine binary's compiled features or which
//! run path (`--software` vs the full pipeline) is driving the output — but the
//! `X-Multiview-Apply` header must tell the truth per mutation (ADR-W015/W018).
//! The **binary** therefore injects, at wiring time, what the *running* engine
//! can actually take live, and the routes consult it before promising `live`.
//!
//! Today the capability covers the overlays collection; source/output
//! collections plug into the same struct as their live paths land
//! (`#[non_exhaustive]` + builder, so widening it is non-breaking).

use std::fmt;
use std::sync::Arc;

/// What the **running** engine can take live, per stored collection.
///
/// The default carries no capability: every mutation honestly declares
/// `restart` (the store-only / test / software-path posture). The binary
/// widens it with the builder methods where the run path has a live seam.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct LiveApplyCaps {
    /// The overlay live-apply capability. [`None`] ⇒ no live overlay seam on
    /// this run path: every overlay mutation is `restart` and no overlay
    /// command is enqueued.
    pub overlays: Option<OverlayLiveCapability>,
}

impl LiveApplyCaps {
    /// Declare the overlay live-apply capability (builder-style).
    #[must_use]
    pub fn with_overlays(mut self, capability: OverlayLiveCapability) -> Self {
        self.overlays = Some(capability);
        self
    }
}

impl fmt::Debug for LiveApplyCaps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveApplyCaps")
            .field("overlays", &self.overlays.as_ref().map(|_| "live"))
            .finish()
    }
}

/// The overlay live-apply capability: a binary-injected **render-truth
/// predicate** over the validated document (ADR-W022 §3).
///
/// Render-ability is finer than the kind string — e.g. a `clock` document is
/// drawn by the `overlay`-featured renderer iff its `face` is `analog` — so
/// the capability carries the predicate rather than a kind list. The binary is
/// the one place that knows both the compiled features and the run path, so it
/// owns this truth; the routes only consult it.
#[derive(Clone)]
pub struct OverlayLiveCapability {
    renders: Arc<dyn Fn(&multiview_config::Overlay) -> bool + Send + Sync>,
}

impl OverlayLiveCapability {
    /// Wrap the binary's render-truth predicate: does the **running** renderer
    /// visibly draw this overlay document?
    #[must_use]
    pub fn new(
        renders: impl Fn(&multiview_config::Overlay) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            renders: Arc::new(renders),
        }
    }

    /// Whether the running renderer visibly draws `overlay`. `true` ⇒ a
    /// successfully-enqueued mutation may declare `live`; `false` ⇒ the
    /// document is still mirrored to the engine (the working set stays
    /// coherent) but the header stays `restart` and the drain warns.
    #[must_use]
    pub fn renders(&self, overlay: &multiview_config::Overlay) -> bool {
        (self.renders)(overlay)
    }
}

impl fmt::Debug for OverlayLiveCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OverlayLiveCapability(<predicate>)")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn overlay(kind: &str) -> multiview_config::Overlay {
        serde_json::from_value(serde_json::json!({
            "id": "o1", "kind": kind, "target": "canvas"
        }))
        .expect("valid overlay")
    }

    #[test]
    fn default_caps_carry_no_overlay_capability() {
        let caps = LiveApplyCaps::default();
        assert!(caps.overlays.is_none(), "default = nothing live (honest)");
    }

    #[test]
    fn the_injected_predicate_decides_render_truth() {
        let caps = LiveApplyCaps::default()
            .with_overlays(OverlayLiveCapability::new(|o| o.kind == "clock"));
        let capability = caps.overlays.expect("declared");
        assert!(capability.renders(&overlay("clock")));
        assert!(!capability.renders(&overlay("label")));
    }
}
