//! The pure per-section structural diff between two configuration documents
//! (ADR-W020).
//!
//! [`ConfigDiff::between`] compares a **running** (currently-applied) document
//! against a **next** (just-loaded) one and reports, per section, exactly what
//! changed — the unit the config-file watcher turns into apply actions: source
//! changes ride the live `UpsertSource`/`RemoveSource` machinery, a
//! layout/cells change rides the apply-layout path, a pinned-canvas change is
//! Class-2 (restart), and every other changed section is named for the
//! reseed + requires-restart surface.
//!
//! Pure and total: no I/O, no clocks, no allocation beyond the reported
//! changes. Comparison is over the typed schema (`PartialEq`), so semantic
//! equality holds where the schema defines it — e.g. an [`Fps`] cadence
//! compares **by value** (`50/2 == 25/1`), never by spelling.

use std::collections::BTreeSet;

use crate::layout_doc::LayoutCanvas;
use crate::schema::{Overlay, Source};
use crate::MultiviewConfig;

/// One source-level change between the running and next documents, keyed by
/// the source's stable `id`.
///
/// Deliberately **exhaustive** (no `#[non_exhaustive]`): added/changed/removed
/// is the complete taxonomy of an id-keyed list diff by construction, and the
/// watcher's apply plan must handle every case — an exhaustive `match` is the
/// compiler-enforced proof.
/// The `Source` payloads are boxed: a source document is large (~330 B) and a
/// `Removed` id is a thin `String`, so boxing keeps the enum small on the
/// stack (and is exactly the shape `Command::UpsertSource` carries).
#[derive(Debug, Clone, PartialEq)]
pub enum SourceChange {
    /// A source id present in `next` but not in `running`.
    Added(Box<Source>),
    /// A source id present in both whose document differs.
    Changed {
        /// The running document's version (e.g. for synthetic→decoded kind
        /// transitions, which stop the stale producer).
        previous: Box<Source>,
        /// The next document's version (the one to apply).
        next: Box<Source>,
    },
    /// A source id present in `running` but not in `next`.
    Removed(String),
}

/// One overlay-level change between the running and next documents, keyed by the
/// overlay's stable `id` (ADR-W024 round 7).
///
/// The file-watch overlay apply mirrors the source path: it derives the delta
/// from THIS list (computed against the stable file baseline by
/// [`ConfigDiff::between`]), NOT from the mutated control store — a store-derived
/// delta vanishes on a shed retry (the store is reseeded to `next` before the
/// retry resolves) and the change is silently lost. Exhaustive (no
/// `#[non_exhaustive]`): added/changed/removed is the complete id-keyed taxonomy.
/// The `Overlay` payloads are boxed (an overlay document is large; a `Removed`
/// id is a thin `String`) — exactly the shape `Command::UpsertOverlay` carries.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayChange {
    /// An overlay id present in `next` but not in `running`.
    Added(Box<Overlay>),
    /// An overlay id present in both whose document differs (the `next` version
    /// is the one to apply via `UpsertOverlay`).
    Changed(Box<Overlay>),
    /// An overlay id present in `running` but not in `next` (apply via
    /// `RemoveOverlay`).
    Removed(String),
}

/// The per-section structural diff between two configuration documents.
///
/// Produced by [`ConfigDiff::between`]; an all-default value (see
/// [`ConfigDiff::is_empty`]) means the documents are identical.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
// The boolean fields are INDEPENDENT, simultaneously-true facts about one diff
// (a single edit can be e.g. both `layout_changed` AND `canvas_cosmetic_changed`,
// or both `overlays_reordered` AND carry per-id overlay changes), not the states
// of a single state machine — so the `struct_excessive_bools` "use a state
// machine / two-variant enum" refactor does not apply (it would misrepresent
// orthogonal flags as mutually-exclusive states). Task #130 added two of them.
#[allow(clippy::struct_excessive_bools)]
pub struct ConfigDiff {
    /// Source changes by id: `Added`/`Changed` in `next` declaration order,
    /// then `Removed` in `running` declaration order (deterministic — and the
    /// apply order a layout rebinding to a just-added source needs).
    pub sources: Vec<SourceChange>,
    /// Overlay changes by id (ADR-W024 round 7): `Added`/`Changed` in `next`
    /// declaration order, then `Removed` in `running` declaration order. The
    /// file-watch overlay apply derives its `UpsertOverlay`/`RemoveOverlay`
    /// commands from THIS (baseline-derived, stable across shed retries), never
    /// from the mutated control store. `overlays` is ALSO in `changed_sections`
    /// when the overlay `Vec` differs (so reporting/restart accounting stays
    /// uniform AND the watcher reseeds the store on a pure reorder). A pure
    /// reorder leaves THIS per-id list empty — the order delta surfaces via
    /// [`overlays_reordered`](Self::overlays_reordered) (Class-1).
    pub overlays: Vec<OverlayChange>,
    /// The surviving (present-in-both) source ids appear in a **different
    /// relative order** between `running` and `next` — a pure permutation of the
    /// common id-set (adds/removes are reported by [`sources`](Self::sources), not
    /// here). A reorder is invisible to the id-keyed list, yet source declaration
    /// order is observable: it sets the cold-start test-pattern palette index
    /// (`enumerate()` over the boot file's sources). Unlike the overlay reorder,
    /// this has **no live engine seam** (cells bind by `input_id`; the live source
    /// stores are id-keyed, so order is not even representable in the
    /// store/`active.toml`) — so it is a **Class-2 / restart-pending** delta: the
    /// watcher reports `sources` restart-pending (it takes effect on the next
    /// restart, which re-reads the boot file's order), never silently dropped and
    /// never via a (non-existent) live `ReorderSources` command. Task #130.
    pub sources_reordered: bool,
    /// The surviving (present-in-both) overlay ids appear in a **different
    /// relative order** between `running` and `next` — a pure permutation of the
    /// common id-set. Declaration order is the equal-`z` draw-order tie-break (the
    /// overlay stack's `sort_by_key(|l| l.z)` is **stable**, so equal-z overlays
    /// blend in insertion order), so a reorder genuinely changes the composited
    /// output — yet the per-id [`overlays`](Self::overlays) list stays empty for
    /// it. A pure z/draw-order reorder is **Class-1** (hot/seamless, ADR-W024 /
    /// inv #11), distinct from a Class-2 canvas reset: this flag is the honest
    /// classification signal (the order-sensitive `overlays` `changed_sections`
    /// entry still fires so the watcher reseeds the store to the new order).
    /// Task #130.
    pub overlays_reordered: bool,
    /// The **pinned signal** (width / height / cadence-by-value) changed — a
    /// Class-2 change (ADR-R004): never hot-appliable.
    pub canvas_signal_changed: bool,
    /// A non-pinned canvas axis (pixel format, background, colour profile)
    /// changed — not live-rendered either, so restart-only, but distinct from
    /// the pinned signal for honest reporting.
    pub canvas_cosmetic_changed: bool,
    /// The layout strategy or the cells changed (a live apply-layout candidate
    /// when the pinned signal did not change).
    pub layout_changed: bool,
    /// Every other changed section, by its authored TOML name (`outputs`,
    /// `overlays`, `probes`, `audio`, `control`, `placement`, `salvos`,
    /// `tally_profiles`, `walls`, `devices`, `sync_groups`, `routing`,
    /// `discovery`, `timing`, `webrtc`, `schema_version`). Sorted + deduplicated.
    pub changed_sections: BTreeSet<&'static str>,
}

impl ConfigDiff {
    /// Compute the structural diff from the `running` document to `next`.
    // The length is forced by the deliberate, load-bearing exhaustive destructure
    // of every `MultiviewConfig` field (the review-m1 compile-time guard documented
    // below): the body grows by two binds per field, and splitting it would only
    // hide the guard behind a helper. The logic stays a flat, readable per-field
    // comparison, so the line count is inherent rather than accidental complexity.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn between(running: &MultiviewConfig, next: &MultiviewConfig) -> Self {
        // EXHAUSTIVE destructure (no `..`): adding a field to `MultiviewConfig`
        // is a compile error HERE until this diff accounts for it — a new
        // section can never silently fall through the watcher (review m1).
        // `#[non_exhaustive]` does not apply within the defining crate.
        let MultiviewConfig {
            schema_version: running_schema_version,
            canvas: running_canvas,
            layout: running_layout,
            sources: _,
            cells: running_cells,
            overlays: running_overlays,
            outputs: running_outputs,
            probes: running_probes,
            tally_profiles: running_tally_profiles,
            salvos: running_salvos,
            walls: running_walls,
            devices: running_devices,
            sync_groups: running_sync_groups,
            control: running_control,
            api: running_api,
            placement: running_placement,
            audio: running_audio,
            routing: running_routing,
            discovery: running_discovery,
            timing: running_timing,
            webrtc: running_webrtc,
            system: running_system,
            media_library: running_media_library,
            media_players: running_media_players,
        } = running;
        let MultiviewConfig {
            schema_version: next_schema_version,
            canvas: next_canvas,
            layout: next_layout,
            sources: _,
            cells: next_cells,
            overlays: next_overlays,
            outputs: next_outputs,
            probes: next_probes,
            tally_profiles: next_tally_profiles,
            salvos: next_salvos,
            walls: next_walls,
            devices: next_devices,
            sync_groups: next_sync_groups,
            control: next_control,
            api: next_api,
            placement: next_placement,
            audio: next_audio,
            routing: next_routing,
            discovery: next_discovery,
            timing: next_timing,
            webrtc: next_webrtc,
            system: next_system,
            media_library: next_media_library,
            media_players: next_media_players,
        } = next;

        // Canvas: the pinned signal is geometry + cadence BY VALUE (the
        // LayoutCanvas PartialEq cross-multiplies Fps), exactly the ADR-W019
        // Class-1 gate's comparison. Everything else on the canvas is
        // cosmetic (and still restart-only).
        let running_signal = LayoutCanvas::new(
            running_canvas.width,
            running_canvas.height,
            running_canvas.fps,
        );
        let next_signal = LayoutCanvas::new(next_canvas.width, next_canvas.height, next_canvas.fps);
        let canvas_signal_changed = running_signal != next_signal;
        let canvas_cosmetic_changed = !canvas_signal_changed && running_canvas != next_canvas;

        // Media library + player channels (ADR-0057): a restart-class section.
        // The live load/cue/play/vamp verbs are command-bus actions (ADR-0097),
        // not a document-diff apply, so a change to the declared set is a
        // controlled reset, never a silent hot-swap.
        let media_changed = running_media_library != next_media_library
            || running_media_players != next_media_players;
        let mut changed_sections = BTreeSet::new();
        let sectioned: [(&'static str, bool); 19] = [
            (
                "schema_version",
                running_schema_version != next_schema_version,
            ),
            ("outputs", running_outputs != next_outputs),
            // Order-sensitive (`Vec` compares by position): a pure overlay
            // REORDER sets this so the file-watch overlay apply (`apply_overlay_
            // changes`) still reseeds the store to the new declaration order — the
            // equal-`z` draw-order tie-break (task #130). The CLASSIFICATION of a
            // pure reorder is carried by `overlays_reordered` (Class-1) — with a
            // live overlay capability the watcher leaves nothing restart-pending
            // (the empty per-id delta lands trivially); a no-capability run is
            // honestly restart-only (no live overlay seam at all).
            ("overlays", running_overlays != next_overlays),
            ("probes", running_probes != next_probes),
            ("audio", running_audio != next_audio),
            ("control", running_control != next_control),
            // Config-declared API keys (ADR-W026). A changed section: keys are
            // registered at startup, and a live re-registration path (file-watch
            // diff → register/revoke/set_principal, RT010 generation bump) is a
            // follow-up — so a change here is surfaced (never silently dropped),
            // classified restart-pending by the apply layer until that path lands.
            ("api", running_api != next_api),
            ("placement", running_placement != next_placement),
            ("salvos", running_salvos != next_salvos),
            (
                "tally_profiles",
                running_tally_profiles != next_tally_profiles,
            ),
            ("walls", running_walls != next_walls),
            ("devices", running_devices != next_devices),
            ("sync_groups", running_sync_groups != next_sync_groups),
            ("routing", running_routing != next_routing),
            ("discovery", running_discovery != next_discovery),
            ("timing", running_timing != next_timing),
            ("webrtc", running_webrtc != next_webrtc),
            // A change to `[system.ndi]` license acceptance is restart-class: full
            // live revocation/acceptance propagation to running NDI I/O at the next
            // safe boundary (ADR-0008 §7.5) is a separate follow-up, so surface it
            // as a changed section (controlled reset) rather than claiming hot-apply.
            ("system", running_system != next_system),
            ("media", media_changed),
        ];
        for (name, changed) in sectioned {
            if changed {
                changed_sections.insert(name);
            }
        }

        let mut diff = Self {
            sources: diff_sources(running, next),
            overlays: diff_overlays(running, next),
            // A pure permutation of the present-in-both id-set is invisible to the
            // id-keyed lists above (task #130) — yet declaration order is the
            // equal-`z` overlay draw-order tie-break and the software test
            // pattern's source index. Compare the COMMON-id subsequence in each
            // document's declaration order: adds/removes are excluded (so an
            // add/remove alone is never a reorder), a genuine permutation of the
            // survivors always differs. Orthogonal to `Changed`: a reorder that
            // also edits a survivor's document reports both.
            sources_reordered: common_ids_reordered(
                running.sources.iter().map(|s| &s.id),
                next.sources.iter().map(|s| &s.id),
            ),
            overlays_reordered: common_ids_reordered(
                running.overlays.iter().map(|o| &o.id),
                next.overlays.iter().map(|o| &o.id),
            ),
            canvas_signal_changed,
            canvas_cosmetic_changed,
            layout_changed: running_layout != next_layout || running_cells != next_cells,
            changed_sections,
        };
        // Defence in depth behind the compile-time destructure guard above.
        backstop_unrecognized(&mut diff, running == next);
        diff
    }

    /// Whether the two documents were structurally identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && self.overlays.is_empty()
            && !self.sources_reordered
            && !self.overlays_reordered
            && !self.canvas_signal_changed
            && !self.canvas_cosmetic_changed
            && !self.layout_changed
            && self.changed_sections.is_empty()
    }
}

/// The review-m1 backstop: if the per-section comparison produced an EMPTY
/// diff for two documents that are NOT equal (`documents_equal == false`),
/// surface a named `"unrecognized"` restart section instead of letting the
/// change vanish silently. Normally unreachable — [`ConfigDiff::between`]
/// destructures `MultiviewConfig` exhaustively, so a new field is a compile
/// error there — this guards a future partial-compare regression at runtime.
fn backstop_unrecognized(diff: &mut ConfigDiff, documents_equal: bool) {
    if diff.is_empty() && !documents_equal {
        diff.changed_sections.insert("unrecognized");
    }
}

/// Diff the source lists by id: `Added`/`Changed` in `next` declaration order,
/// then `Removed` in `running` declaration order.
fn diff_sources(running: &MultiviewConfig, next: &MultiviewConfig) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    for source in &next.sources {
        match running.sources.iter().find(|r| r.id == source.id) {
            None => changes.push(SourceChange::Added(Box::new(source.clone()))),
            Some(previous) if previous != source => changes.push(SourceChange::Changed {
                previous: Box::new(previous.clone()),
                next: Box::new(source.clone()),
            }),
            Some(_) => {}
        }
    }
    for source in &running.sources {
        if !next.sources.iter().any(|n| n.id == source.id) {
            changes.push(SourceChange::Removed(source.id.clone()));
        }
    }
    changes
}

/// Diff the overlay lists by id (ADR-W024 round 7), the same shape as
/// [`diff_sources`]: `Added`/`Changed` in `next` declaration order, then
/// `Removed` in `running` declaration order. Computed against the file baseline
/// so the file-watch overlay apply re-derives a STABLE delta on every shed
/// retry (a store-derived delta vanishes once the store is reseeded to `next`).
fn diff_overlays(running: &MultiviewConfig, next: &MultiviewConfig) -> Vec<OverlayChange> {
    let mut changes = Vec::new();
    for overlay in &next.overlays {
        match running.overlays.iter().find(|r| r.id == overlay.id) {
            None => changes.push(OverlayChange::Added(Box::new(overlay.clone()))),
            Some(previous) if previous != overlay => {
                changes.push(OverlayChange::Changed(Box::new(overlay.clone())));
            }
            Some(_) => {}
        }
    }
    for overlay in &running.overlays {
        if !next.overlays.iter().any(|n| n.id == overlay.id) {
            changes.push(OverlayChange::Removed(overlay.id.clone()));
        }
    }
    changes
}

/// Whether the ids **present in both** lists appear in a different relative
/// order between `running` and `next` — a pure permutation of the common id-set
/// that the id-keyed diff is blind to (task #130).
///
/// The comparison restricts each side to its present-in-both subsequence (in
/// declaration order), then compares those two sequences:
/// * Adds/removes are excluded, so an add or a remove **alone** is never a
///   reorder (the surviving sequence is unchanged) — they are already reported by
///   the id-keyed [`SourceChange`]/[`OverlayChange`] lists.
/// * A genuine swap of two common ids changes the surviving sequence, so it is
///   reported even when every common document is otherwise identical.
/// * It is **orthogonal to `Changed`**: editing a survivor's document does not
///   move it in the id-sequence, and reordering survivors does not depend on
///   their documents — so a combined edit-and-reorder reports both signals.
///
/// The two legs classify differently (by what order affects at runtime): for
/// overlays it is the equal-`z` draw-order tie-break (stable sort), a **live
/// Class-1** re-blend via `ReorderOverlays`; for sources it is the cold-start
/// `enumerate()` test-pattern index with no live seam (cells bind by `input_id`),
/// a **Class-2 restart-pending** delta. Either way the reorder is detected (the
/// caller routes it to the right classification) — never a silent lost update.
fn common_ids_reordered<'a>(
    running: impl Iterator<Item = &'a String>,
    next: impl Iterator<Item = &'a String>,
) -> bool {
    // Collect once so each side can be scanned against the other's membership.
    let running: Vec<&String> = running.collect();
    let next_ids: Vec<&String> = next.collect();
    // The common-id subsequence in each document's own declaration order.
    let running_common: Vec<&String> = running
        .iter()
        .copied()
        .filter(|id| next_ids.contains(id))
        .collect();
    let next_common: Vec<&String> = next_ids
        .iter()
        .copied()
        .filter(|id| running.contains(id))
        .collect();
    running_common != next_common
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{backstop_unrecognized, common_ids_reordered, ConfigDiff};

    /// Drive [`common_ids_reordered`] directly over id sequences (task #130). The
    /// helper compares the present-in-BOTH subsequence of each side in its own
    /// order: a permutation of the common ids is a reorder; an add/remove alone is
    /// not; ids unique to one side are ignored.
    fn reordered(running: &[&str], next: &[&str]) -> bool {
        let r: Vec<String> = running.iter().map(|s| (*s).to_owned()).collect();
        let n: Vec<String> = next.iter().map(|s| (*s).to_owned()).collect();
        common_ids_reordered(r.iter(), n.iter())
    }

    #[test]
    fn common_ids_reordered_detects_a_pure_swap() {
        assert!(reordered(&["a", "b"], &["b", "a"]), "a swap is a reorder");
        assert!(
            reordered(&["a", "b", "c"], &["c", "b", "a"]),
            "a 3-way permutation is a reorder"
        );
    }

    #[test]
    fn common_ids_reordered_ignores_same_order() {
        assert!(!reordered(&["a", "b", "c"], &["a", "b", "c"]), "identical");
        assert!(!reordered(&[], &[]), "both empty");
        assert!(!reordered(&["a"], &["a"]), "single element, same");
    }

    #[test]
    fn common_ids_reordered_excludes_adds_and_removes() {
        // Pure add (b appended) — the common subsequence [a] is unchanged.
        assert!(
            !reordered(&["a"], &["a", "b"]),
            "an add alone is not a reorder"
        );
        // Pure remove (b dropped) — the common subsequence [a] is unchanged.
        assert!(
            !reordered(&["a", "b"], &["a"]),
            "a remove alone is not a reorder"
        );
        // An add that lands BETWEEN two common ids that keep their relative order
        // is still not a reorder: common subsequence [a, c] vs [a, c].
        assert!(
            !reordered(&["a", "c"], &["a", "b", "c"]),
            "an insertion that preserves the common order is not a reorder"
        );
    }

    #[test]
    fn common_ids_reordered_detects_a_swap_amid_adds_and_removes() {
        // a,b survive but swap; x removed; y added — the common subsequence is
        // [a, b] vs [b, a], a genuine reorder despite the add/remove churn.
        assert!(
            reordered(&["x", "a", "b"], &["b", "a", "y"]),
            "a survivor swap under add/remove churn is a reorder"
        );
    }

    #[test]
    fn common_ids_reordered_ignores_fully_disjoint_sets() {
        // No common ids ⇒ no common subsequence to permute ⇒ never a reorder.
        assert!(
            !reordered(&["a", "b"], &["c", "d"]),
            "fully disjoint id-sets share no order to change"
        );
    }

    /// REVIEW m1 backstop: if the per-section comparison ever reports an
    /// EMPTY diff for two documents that are NOT equal (a future field missed
    /// by a partial compare — normally impossible: `between` destructures the
    /// config exhaustively, so a new field is a compile error), the backstop
    /// must surface a named "unrecognized" restart section rather than let
    /// the change vanish silently.
    #[test]
    fn an_empty_diff_for_unequal_documents_reports_unrecognized() {
        let mut diff = ConfigDiff::default();
        assert!(diff.is_empty());
        backstop_unrecognized(&mut diff, false);
        assert!(
            diff.changed_sections.contains("unrecognized"),
            "unequal documents with an empty diff must surface an unrecognized section"
        );
        assert!(!diff.is_empty());
    }

    /// The backstop never fires for genuinely identical documents, and never
    /// touches a non-empty diff.
    #[test]
    fn the_backstop_is_inert_for_equal_documents_and_nonempty_diffs() {
        let mut diff = ConfigDiff::default();
        backstop_unrecognized(&mut diff, true);
        assert!(diff.is_empty(), "equal documents stay an empty diff");

        let mut nonempty = ConfigDiff::default();
        nonempty.changed_sections.insert("outputs");
        backstop_unrecognized(&mut nonempty, false);
        assert!(
            !nonempty.changed_sections.contains("unrecognized"),
            "a non-empty diff already carries the change; no backstop entry"
        );
    }
}
