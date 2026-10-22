//! The live overlay working-set seam (ADR-W022): the lock-free slot between
//! the frame-boundary command drain and the bake consumer.
//!
//! The engine command drain applies `UpsertOverlay`/`RemoveOverlay` at the
//! frame boundary as **pure data mutation**: it upserts/removes the document
//! in its working-config mirror and publishes the full set — with a bumped
//! generation — into the [`OverlayApplySlot`]. The bake consumer (already off
//! the output-clock thread) loads the slot once per frame (wait-free) and
//! re-derives its overlay render state only when the generation advanced, so
//! a change lands cleanly between two whole frames (Class-1) and neither side
//! can block or pace the other (invariants #1 + #10).
//!
//! No off-thread hub is needed (contrast ADR-W018's `LiveSourceHub`): an
//! overlay apply spawns nothing and rasterizes nothing — the analog face is
//! ring/stroke primitives and glyphs are rasterized lazily (cached) on the
//! consumer thread at draw time.

use std::sync::Arc;

use arc_swap::ArcSwap;

/// One published overlay working set: a monotonic generation plus the full
/// set (working-config order). Immutable once published — the drain publishes
/// a fresh `Arc` per applied change.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlaySet {
    generation: u64,
    overlays: Vec<multiview_config::Overlay>,
}

impl OverlaySet {
    /// The monotonic generation of this set (`0` = the boot config's set).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The full overlay working set, in working-config order.
    #[must_use]
    pub fn overlays(&self) -> &[multiview_config::Overlay] {
        &self.overlays
    }
}

/// The lock-free slot carrying the live overlay working set from the
/// frame-boundary drain (the single writer) to the bake consumer (the
/// reader). Wait-free on both sides ([`ArcSwap`]); bounded by construction
/// (exactly one set is ever held).
pub type OverlayApplySlot = Arc<ArcSwap<OverlaySet>>;

/// Seed a slot with the boot config's overlay set (generation `0`), so the
/// consumer's boot-derived render state and the slot agree before any
/// command arrives.
#[must_use]
pub fn overlay_apply_slot(overlays: Vec<multiview_config::Overlay>) -> OverlayApplySlot {
    Arc::new(ArcSwap::from_pointee(OverlaySet {
        generation: 0,
        overlays,
    }))
}

/// Publish `overlays` as the next generation, returning the new generation.
///
/// Single-writer discipline: only the frame-boundary drain publishes during a
/// run, so the load-increment-store is race-free. The store is one atomic
/// pointer swap; the previous set's `Arc` drops here (operator-scale, the
/// same order of work as the drain's existing config-mirror mutations).
pub fn publish_set(slot: &OverlayApplySlot, overlays: Vec<multiview_config::Overlay>) -> u64 {
    let generation = slot.load().generation.saturating_add(1);
    slot.store(Arc::new(OverlaySet {
        generation,
        overlays,
    }));
    generation
}

/// Whether the `overlay`-featured renderer **visibly draws** this document —
/// the single render-truth predicate (ADR-W022 §3/§4): the binary injects it
/// into the control plane's [`LiveApplyCaps`](multiview_control::LiveApplyCaps)
/// (so `X-Multiview-Apply` headers tell the same truth) and the drain consults
/// it to warn for documents that change no pixels.
///
/// Today that is exactly a `clock` with an `analog` face (the derivation
/// `analog_clock_from_config` consumes). A digital-faced clock coincides with
/// the always-on chrome readout (no document-driven change), and `label`/
/// `tally_border`/`image`/`subtitle` have no renderer in any current build.
#[must_use]
pub fn renders_live(overlay: &multiview_config::Overlay) -> bool {
    overlay.kind == "clock"
        && overlay
            .params
            .get("face")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|f| f.eq_ignore_ascii_case("analog"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn doc(json: serde_json::Value) -> multiview_config::Overlay {
        serde_json::from_value(json).expect("valid overlay")
    }

    #[test]
    fn publish_advances_the_generation_monotonically() {
        let slot = overlay_apply_slot(Vec::new());
        assert_eq!(slot.load().generation(), 0);
        let g1 = publish_set(
            &slot,
            vec![doc(serde_json::json!({
                "id": "a", "kind": "clock", "target": "canvas", "face": "analog"
            }))],
        );
        assert_eq!(g1, 1);
        assert_eq!(slot.load().overlays().len(), 1);
        let g2 = publish_set(&slot, Vec::new());
        assert_eq!(g2, 2);
        assert!(slot.load().overlays().is_empty());
    }

    #[test]
    fn render_truth_is_analog_clock_only() {
        assert!(renders_live(&doc(serde_json::json!({
            "id": "c", "kind": "clock", "target": "canvas", "face": "Analog"
        }))));
        assert!(!renders_live(&doc(serde_json::json!({
            "id": "c", "kind": "clock", "target": "canvas", "face": "digital"
        }))));
        assert!(!renders_live(&doc(serde_json::json!({
            "id": "c", "kind": "clock", "target": "canvas"
        }))));
        assert!(!renders_live(&doc(serde_json::json!({
            "id": "l", "kind": "label", "target": "cell_a"
        }))));
    }
}
