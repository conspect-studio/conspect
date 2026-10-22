//! The pure per-section structural diff between two configuration documents
//! (ADR-W020): the unit the config-file watcher turns into apply actions.
//!
//! Exhaustive over every `MultiviewConfig` section: sources and overlays (each
//! by id — added/changed/removed; ADR-W024 round 7 added the per-id overlay
//! delta the file-watch overlay apply consumes), canvas (pinned signal vs
//! cosmetic axes), layout+cells, and the restart-only sections (`outputs`,
//! `probes`, `audio`, `control`, `placement`, `salvos`, `tally_profiles`,
//! `walls`, `devices`, `sync_groups`, `routing`, `schema_version`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ConfigDiff, MultiviewConfig, OverlayChange, SourceChange};

/// The baseline two-source, two-cell document every test perturbs.
const BASE_DOC: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "solid"
color = "#103050"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[outputs]]
kind = "hls"
path = "/tmp/diff-base.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn base() -> MultiviewConfig {
    let config = MultiviewConfig::load_from_toml(BASE_DOC).expect("parse base doc");
    config.validate().expect("base doc validates");
    config
}

/// Parse a perturbed document built by string-editing the base TOML.
fn parsed(doc: &str) -> MultiviewConfig {
    let config = MultiviewConfig::load_from_toml(doc).expect("parse perturbed doc");
    config.validate().expect("perturbed doc validates");
    config
}

#[test]
fn identical_documents_diff_empty() {
    let diff = ConfigDiff::between(&base(), &base());
    assert!(
        diff.is_empty(),
        "identical documents must diff empty: {diff:?}"
    );
    assert!(diff.sources.is_empty());
    assert!(!diff.canvas_signal_changed);
    assert!(!diff.canvas_cosmetic_changed);
    assert!(!diff.layout_changed);
    assert!(diff.changed_sections.is_empty());
}

#[test]
fn an_added_source_is_reported_added_with_its_document() {
    let next = parsed(&format!(
        "{BASE_DOC}[[sources]]\nid = \"in_c\"\nkind = \"bars\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(!diff.is_empty());
    assert_eq!(diff.sources.len(), 1);
    match &diff.sources[0] {
        SourceChange::Added(source) => assert_eq!(source.id, "in_c"),
        other => panic!("expected Added(in_c), got {other:?}"),
    }
    // A pure source add touches nothing else.
    assert!(!diff.layout_changed && !diff.canvas_signal_changed);
    assert!(diff.changed_sections.is_empty());
}

#[test]
fn a_changed_source_carries_previous_and_next() {
    let next = parsed(&BASE_DOC.replace("#103050", "#f0f0f0"));
    let diff = ConfigDiff::between(&base(), &next);
    assert_eq!(diff.sources.len(), 1);
    match &diff.sources[0] {
        SourceChange::Changed { previous, next } => {
            assert_eq!(previous.id, "in_a");
            assert_eq!(next.id, "in_a");
            assert_ne!(previous, next, "the changed pair differs");
        }
        other => panic!("expected Changed(in_a), got {other:?}"),
    }
}

/// Drop the `in_b` source and rebind `cell_b` onto `in_a` so the perturbed
/// document still validates (a cell may not bind an undeclared source).
fn without_in_b(doc: &str) -> String {
    doc.replace("[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n", "")
        .replace(
            "[[cells]]\nid = \"cell_b\"\narea = \"b\"\n[cells.source]\ninput_id = \"in_b\"",
            "[[cells]]\nid = \"cell_b\"\narea = \"b\"\n[cells.source]\ninput_id = \"in_a\"",
        )
}

#[test]
fn a_removed_source_is_reported_by_id() {
    let next = parsed(&without_in_b(BASE_DOC));
    let diff = ConfigDiff::between(&base(), &next);
    assert_eq!(diff.sources.len(), 1);
    match &diff.sources[0] {
        SourceChange::Removed(id) => assert_eq!(id, "in_b"),
        other => panic!("expected Removed(in_b), got {other:?}"),
    }
}

#[test]
fn add_change_and_remove_report_together_deterministically() {
    // in_a changed, in_b removed, in_c added — Added/Changed ride `next`
    // declaration order, then Removed in `running` declaration order.
    let next = parsed(&format!(
        "{}[[sources]]\nid = \"in_c\"\nkind = \"bars\"\n",
        without_in_b(&BASE_DOC.replace("#103050", "#f0f0f0"))
    ));
    let diff = ConfigDiff::between(&base(), &next);
    let kinds: Vec<&str> = diff
        .sources
        .iter()
        .map(|c| match c {
            SourceChange::Added(s) => {
                assert_eq!(s.id, "in_c");
                "added"
            }
            SourceChange::Changed { next, .. } => {
                assert_eq!(next.id, "in_a");
                "changed"
            }
            SourceChange::Removed(id) => {
                assert_eq!(id, "in_b");
                "removed"
            }
        })
        .collect();
    assert_eq!(kinds, vec!["changed", "added", "removed"]);
}

// ADR-W024 round 7 — overlay changes are reported per-id in `diff.overlays`
// (baseline-derived, the shape the file-watch overlay apply consumes so its
// `UpsertOverlay`/`RemoveOverlay` delta is STABLE across shed retries), AND
// `overlays` still appears in `changed_sections` for uniform reporting.

/// A base document carrying one overlay, so a perturbation can change/remove it.
const BASE_WITH_OVERLAY: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "solid"
color = "#103050"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[overlays]]
id = "ov1"
kind = "clock"
target = "canvas"
[[outputs]]
kind = "hls"
path = "/tmp/diff-base.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn base_with_overlay() -> MultiviewConfig {
    parsed(BASE_WITH_OVERLAY)
}

#[test]
fn an_added_overlay_is_reported_added_with_its_document() {
    // base() has no overlays; next adds ov1.
    let diff = ConfigDiff::between(&base(), &base_with_overlay());
    assert_eq!(diff.overlays.len(), 1, "exactly one overlay change");
    match &diff.overlays[0] {
        OverlayChange::Added(overlay) => assert_eq!(overlay.id, "ov1"),
        other => panic!("expected Added(ov1), got {other:?}"),
    }
    assert!(
        diff.changed_sections.contains("overlays"),
        "overlays must also report in changed_sections for uniform accounting"
    );
}

#[test]
fn a_changed_overlay_carries_the_next_document() {
    // Re-target the overlay: a per-id change, not an add/remove.
    let next = parsed(&BASE_WITH_OVERLAY.replace("target = \"canvas\"", "target = \"cell_a\""));
    let diff = ConfigDiff::between(&base_with_overlay(), &next);
    assert_eq!(diff.overlays.len(), 1);
    match &diff.overlays[0] {
        OverlayChange::Changed(overlay) => assert_eq!(overlay.id, "ov1"),
        other => panic!("expected Changed(ov1), got {other:?}"),
    }
}

#[test]
fn a_removed_overlay_is_reported_by_id() {
    // base_with_overlay → base() drops ov1.
    let diff = ConfigDiff::between(&base_with_overlay(), &base());
    assert_eq!(diff.overlays.len(), 1);
    match &diff.overlays[0] {
        OverlayChange::Removed(id) => assert_eq!(id, "ov1"),
        other => panic!("expected Removed(ov1), got {other:?}"),
    }
}

#[test]
fn overlay_add_change_and_remove_report_together_deterministically() {
    // running: ov_keep (clock), ov_drop (label). next: ov_keep CHANGED,
    // ov_drop REMOVED, ov_new ADDED. Added/Changed ride `next` order, then
    // Removed in `running` order — exactly the source-path contract.
    let two_overlays = "[[overlays]]\nid = \"ov_keep\"\nkind = \"clock\"\ntarget = \"canvas\"\n[[overlays]]\nid = \"ov_drop\"\nkind = \"label\"\ntarget = \"canvas\"\ntext = \"x\"\n";
    let running = parsed(&format!("{BASE_DOC}{two_overlays}"));
    let next_overlays = "[[overlays]]\nid = \"ov_keep\"\nkind = \"clock\"\ntarget = \"cell_a\"\n[[overlays]]\nid = \"ov_new\"\nkind = \"label\"\ntarget = \"canvas\"\ntext = \"y\"\n";
    let next = parsed(&format!("{BASE_DOC}{next_overlays}"));
    let diff = ConfigDiff::between(&running, &next);
    let kinds: Vec<&str> = diff
        .overlays
        .iter()
        .map(|c| match c {
            OverlayChange::Added(o) => {
                assert_eq!(o.id, "ov_new");
                "added"
            }
            OverlayChange::Changed(o) => {
                assert_eq!(o.id, "ov_keep");
                "changed"
            }
            OverlayChange::Removed(id) => {
                assert_eq!(id, "ov_drop");
                "removed"
            }
        })
        .collect();
    assert_eq!(kinds, vec!["changed", "added", "removed"]);
}

// Task #130 — a pure REORDER of overlays/sources with identical content and
// equal z-order is a real delta. The id-keyed Added/Changed/Removed taxonomy is
// position-blind, so a reorder of the same id-set with identical documents
// produces an EMPTY id-keyed list — but DECLARATION ORDER is the equal-`z`
// draw-order tie-break (the overlay stack sorts `sort_by_key(|l| l.z)`, a STABLE
// sort, so equal-z overlays blend in insertion order;
// `multiview-overlay/src/layer.rs:267` + its `stack_z_sort_is_stable_for_equal_z`
// test pin this). A config-file-watch reorder must therefore re-apply draw order,
// not vanish as a silent lost update. The reorder is a separate signal from the
// per-id change lists (it is a whole-section draw-order resync, not a per-id
// Upsert/Remove command) and, being a pure z/draw-order reorder, is Class-1
// (hot/seamless, ADR-W024 / inv #11) — NEVER reported restart-only.

#[test]
fn an_equal_z_overlay_reorder_is_a_real_delta() {
    // running: ov1 then ov2, both z=0 (equal-z tie-break = declaration order).
    // next: ov2 then ov1 — same ids, identical per-overlay documents, equal z,
    // only the order swapped. The id-keyed list is EMPTY (no Added/Changed/
    // Removed), but the draw order changed, so it MUST report a reorder delta.
    let ov1 = "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n";
    let ov2 =
        "[[overlays]]\nid = \"ov2\"\nkind = \"label\"\ntarget = \"canvas\"\ntext = \"x\"\nz = 0\n";
    let running = parsed(&format!("{BASE_DOC}{ov1}{ov2}"));
    let next = parsed(&format!("{BASE_DOC}{ov2}{ov1}"));

    let diff = ConfigDiff::between(&running, &next);

    assert!(
        diff.overlays.is_empty(),
        "a pure reorder of identical equal-z overlays produces NO id-keyed \
         Added/Changed/Removed; got {:?}",
        diff.overlays
    );
    assert!(
        diff.overlays_reordered,
        "an equal-z overlay reorder MUST report overlays_reordered — declaration \
         order is the equal-z draw-order tie-break, so a file-watch reorder is a \
         silent lost update unless the diff flags it"
    );
    assert!(
        !diff.is_empty(),
        "the document genuinely changed (draw order), so the diff is NOT empty"
    );
}

#[test]
fn an_equal_z_overlay_reorder_is_classified_class_1_not_canvas_or_layout() {
    // A pure z/draw-order reorder is Class-1 (hot/seamless): the order delta is
    // carried by `overlays_reordered`, the per-id list stays empty, and it must
    // NOT masquerade as a Class-2 canvas reset or a layout change. The
    // order-sensitive `overlays` changed-section entry DOES fire (it is the
    // watcher's trigger to reseed the overlay store to the new declaration order)
    // — but that is the only section it touches, never `canvas`/`layout`/`cells`.
    let ov1 = "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n";
    let ov2 =
        "[[overlays]]\nid = \"ov2\"\nkind = \"label\"\ntarget = \"canvas\"\ntext = \"x\"\nz = 0\n";
    let running = parsed(&format!("{BASE_DOC}{ov1}{ov2}"));
    let next = parsed(&format!("{BASE_DOC}{ov2}{ov1}"));

    let diff = ConfigDiff::between(&running, &next);

    assert!(
        diff.overlays_reordered,
        "precondition: the reorder is detected"
    );
    assert!(
        diff.overlays.is_empty(),
        "a pure reorder carries no per-id content delta; got {:?}",
        diff.overlays
    );
    // Class-1: no Class-2 canvas reset, no layout/cells change.
    assert!(
        !diff.canvas_signal_changed && !diff.canvas_cosmetic_changed && !diff.layout_changed,
        "a pure overlay reorder is not a canvas/layout change"
    );
    // The ONLY section it touches is `overlays` (the store-reseed trigger).
    assert_eq!(
        diff.changed_sections.iter().copied().collect::<Vec<_>>(),
        vec!["overlays"],
        "a pure overlay reorder touches only the overlays section (store reseed), \
         nothing restart-class; got {:?}",
        diff.changed_sections
    );
}

#[test]
fn an_equal_z_source_reorder_is_a_real_delta() {
    // running: in_a then in_b. next: in_b then in_a — same ids, identical
    // per-source documents, only the declaration order swapped. The id-keyed
    // list is EMPTY, but source declaration order is observable (the software
    // build's `enumerate()`-indexed test pattern, run.rs), so it MUST report a
    // reorder delta — the same position-blindness the overlay path had.
    let in_a = "[[sources]]\nid = \"in_a\"\nkind = \"solid\"\ncolor = \"#103050\"\n";
    let in_b = "[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n";
    let cells =
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_a\"\n[[cells]]\nid = \"cell_b\"\narea = \"b\"\n[cells.source]\ninput_id = \"in_b\"\n";
    // A self-contained header (no sources/cells) so we control source order.
    let header = "schema_version = 1\n[canvas]\nwidth = 64\nheight = 64\nfps = \"25/1\"\npixel_format = \"nv12\"\nbackground = \"#101014\"\n[canvas.color]\nprofile = \"sdr-bt709-limited\"\n[layout]\nkind = \"grid\"\ncolumns = [\"1fr\", \"1fr\"]\nrows = [\"1fr\"]\nareas = [\"a b\"]\n";
    let outputs =
        "[[outputs]]\nkind = \"hls\"\npath = \"/tmp/diff-base.m3u8\"\ncodec = \"mpeg2video\"\nsegment_ms = 1000\n";
    let running = parsed(&format!("{header}{in_a}{in_b}{cells}{outputs}"));
    let next = parsed(&format!("{header}{in_b}{in_a}{cells}{outputs}"));

    let diff = ConfigDiff::between(&running, &next);

    assert!(
        diff.sources.is_empty(),
        "a pure reorder of identical sources produces NO id-keyed \
         Added/Changed/Removed; got {:?}",
        diff.sources
    );
    assert!(
        diff.sources_reordered,
        "a source reorder MUST report sources_reordered — the id-keyed diff is \
         position-blind, so a file-watch reorder is otherwise a silent lost update"
    );
    assert!(
        !diff.is_empty(),
        "the document genuinely changed (source order)"
    );
    assert!(
        !diff.changed_sections.contains("sources"),
        "sources is never a `changed_sections` name (it has its own list); a \
         reorder must not invent one — got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_reorder_with_a_real_content_change_reports_both_signals() {
    // running: ov1(z=0), ov2(z=0). next: ov2(z=0), ov1(z=0, RETARGETED). The
    // order swapped AND ov1's document changed — the per-id Changed AND the
    // reorder are independent facts; both must surface.
    let ov1 = "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n";
    let ov1_changed = "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"cell_a\"\nz = 0\n";
    let ov2 =
        "[[overlays]]\nid = \"ov2\"\nkind = \"label\"\ntarget = \"canvas\"\ntext = \"x\"\nz = 0\n";
    let running = parsed(&format!("{BASE_DOC}{ov1}{ov2}"));
    let next = parsed(&format!("{BASE_DOC}{ov2}{ov1_changed}"));

    let diff = ConfigDiff::between(&running, &next);

    assert_eq!(diff.overlays.len(), 1, "exactly the one changed overlay");
    match &diff.overlays[0] {
        OverlayChange::Changed(o) => assert_eq!(o.id, "ov1"),
        other => panic!("expected Changed(ov1), got {other:?}"),
    }
    assert!(
        diff.overlays_reordered,
        "the surviving id-set was also reordered — an independent signal"
    );
}

#[test]
fn a_z_change_alone_is_a_per_id_change_not_a_reorder() {
    // Bumping ov1's z (same declaration order) is a per-id Changed, NOT a
    // reorder: the id-keyed list captures it and the reorder flag stays false.
    let ov1 = "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 0\n";
    let ov1_z = "[[overlays]]\nid = \"ov1\"\nkind = \"clock\"\ntarget = \"canvas\"\nz = 5\n";
    let ov2 =
        "[[overlays]]\nid = \"ov2\"\nkind = \"label\"\ntarget = \"canvas\"\ntext = \"x\"\nz = 0\n";
    let running = parsed(&format!("{BASE_DOC}{ov1}{ov2}"));
    let next = parsed(&format!("{BASE_DOC}{ov1_z}{ov2}"));

    let diff = ConfigDiff::between(&running, &next);

    assert_eq!(diff.overlays.len(), 1);
    match &diff.overlays[0] {
        OverlayChange::Changed(o) => assert_eq!(o.id, "ov1"),
        other => panic!("expected Changed(ov1), got {other:?}"),
    }
    assert!(
        !diff.overlays_reordered,
        "the surviving id sequence is unchanged (ov1, ov2 in both) — a z bump is \
         a content change, not a reorder"
    );
}

#[test]
fn an_added_overlay_is_not_a_reorder() {
    // Appending an overlay extends the id sequence; it is an Add, not a reorder
    // of the surviving set. A reorder is strictly a permutation of the COMMON
    // ids — adds/removes are reported by the id-keyed list, never as a reorder.
    let diff = ConfigDiff::between(&base(), &base_with_overlay());
    assert_eq!(diff.overlays.len(), 1, "the add is reported");
    assert!(
        !diff.overlays_reordered,
        "an add is not a reorder of the common id-set"
    );
    let reverse = ConfigDiff::between(&base_with_overlay(), &base());
    assert_eq!(reverse.overlays.len(), 1, "the remove is reported");
    assert!(
        !reverse.overlays_reordered,
        "a remove is not a reorder of the common id-set"
    );
}

#[test]
fn identical_documents_report_no_reorder() {
    let diff = ConfigDiff::between(&base_with_overlay(), &base_with_overlay());
    assert!(diff.is_empty());
    assert!(!diff.overlays_reordered && !diff.sources_reordered);
}

#[test]
fn canvas_geometry_change_is_a_signal_change() {
    let next = parsed(&BASE_DOC.replace("width = 64", "width = 128"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.canvas_signal_changed, "geometry is the pinned signal");
    assert!(!diff.canvas_cosmetic_changed);
}

#[test]
fn canvas_cadence_change_is_a_signal_change() {
    let next = parsed(&BASE_DOC.replace("fps = \"25/1\"", "fps = \"30/1\""));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.canvas_signal_changed, "cadence is the pinned signal");
}

#[test]
fn an_equivalent_non_reduced_cadence_is_not_a_canvas_change() {
    // 50/2 == 25/1 by value (Rational cross-multiplies): the SAME pinned
    // signal, so this must NOT report a canvas change (ADR-W019 MINOR-3).
    let next = parsed(&BASE_DOC.replace("fps = \"25/1\"", "fps = \"50/2\""));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        !diff.canvas_signal_changed,
        "an equivalent non-reduced cadence is the same signal"
    );
    assert!(
        !diff.canvas_cosmetic_changed,
        "cadence spelling is not a cosmetic change either"
    );
}

#[test]
fn canvas_background_change_is_cosmetic_not_signal() {
    let next = parsed(&BASE_DOC.replace("#101014", "#000000"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(!diff.canvas_signal_changed);
    assert!(
        diff.canvas_cosmetic_changed,
        "background is a cosmetic axis"
    );
}

#[test]
fn a_cell_rebinding_is_a_layout_change() {
    // cell_a re-bound from in_a to in_b: the layout/cells section changed.
    let next = parsed(&BASE_DOC.replace(
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_a\"",
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_b\"",
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.layout_changed, "a cell rebinding changes the layout");
    assert!(!diff.canvas_signal_changed);
    assert!(diff.sources.is_empty());
}

#[test]
fn a_layout_strategy_change_is_a_layout_change() {
    let next = parsed(&BASE_DOC.replace(
        "columns = [\"1fr\", \"1fr\"]",
        "columns = [\"2fr\", \"1fr\"]",
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.layout_changed, "grid tracks changed");
}

/// Every restart-only section reports under its authored TOML name.
#[test]
fn each_restart_only_section_is_reported_by_name() {
    let cases: Vec<(&str, String)> = vec![
        (
            "outputs",
            BASE_DOC.replace("segment_ms = 1000", "segment_ms = 2000"),
        ),
        (
            "overlays",
            format!("{BASE_DOC}[[overlays]]\nid = \"clk\"\nkind = \"clock\"\ntarget = \"canvas\"\n"),
        ),
        (
            "probes",
            format!(
                "{BASE_DOC}[[probes]]\nid = \"black_a\"\ncell = \"cell_a\"\nkind = \"black\"\nluma_threshold = 16\n"
            ),
        ),
        (
            "audio",
            format!("{BASE_DOC}[audio]\nsample_rate_hz = 48000\n"),
        ),
        (
            "control",
            format!("{BASE_DOC}[control]\nlisten = \"[::1]:8080\"\n"),
        ),
        (
            "salvos",
            format!(
                "{BASE_DOC}[[salvos]]\nid = \"s1\"\n[[salvos.sources]]\ncell = \"cell_a\"\ninput_id = \"in_b\"\n"
            ),
        ),
        (
            "tally_profiles",
            format!("{BASE_DOC}[[tally_profiles]]\nid = \"tsl\"\n"),
        ),
        (
            "devices",
            format!(
                "{BASE_DOC}[[devices]]\nid = \"box1\"\ndriver = \"zowietek\"\naddress = \"http://[2001:db8::10]\"\n"
            ),
        ),
        (
            "sync_groups",
            format!(
                "{BASE_DOC}[[devices]]\nid = \"box1\"\ndriver = \"zowietek\"\naddress = \"http://[2001:db8::10]\"\n\n[[sync_groups]]\nid = \"g1\"\ntarget_skew_ms = 50\n[[sync_groups.members]]\ndevice = \"box1\"\n"
            ),
        ),
    ];
    for (section, doc) in cases {
        let next = parsed(&doc);
        let diff = ConfigDiff::between(&base(), &next);
        assert!(
            diff.changed_sections.contains(section),
            "a {section} change must report section {section:?}; got {:?}",
            diff.changed_sections
        );
    }
    // sync_groups perturbation necessarily adds a device too; pin the pair.
    let with_group = parsed(&format!(
        "{BASE_DOC}[[devices]]\nid = \"box1\"\ndriver = \"zowietek\"\naddress = \"http://[2001:db8::10]\"\n\n[[sync_groups]]\nid = \"g1\"\ntarget_skew_ms = 50\n[[sync_groups.members]]\ndevice = \"box1\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &with_group);
    assert!(diff.changed_sections.contains("devices"));
    assert!(diff.changed_sections.contains("sync_groups"));
}

#[test]
fn a_schema_version_change_is_reported() {
    let next = parsed(&BASE_DOC.replace("schema_version = 1", "schema_version = 2"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("schema_version"),
        "got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_placement_change_is_reported() {
    let next = parsed(&format!("{BASE_DOC}[placement]\nreserve_headroom = 0.25\n"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("placement"),
        "got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_walls_change_is_reported() {
    let next = parsed(&format!(
        "{BASE_DOC}[[walls]]\nname = \"wall1\"\ncols = 1\nrows = 1\n[[walls.heads]]\nid = \"h1\"\nwidth = 64\nheight = 64\nfps = \"25/1\"\nlayout = \"grid\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("walls"),
        "got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_routing_change_is_reported() {
    // Consistent with the authored cell binding (a both-populated document
    // must agree); the explicit table's PRESENCE is the change.
    let next = parsed(&format!(
        "{BASE_DOC}\n[[routing.video]]\ncell = \"cell_a\"\n[routing.video.source]\ninput_id = \"in_a\"\nkind = {{ kind = \"video\" }}\n"
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("routing"),
        "got {:?}",
        diff.changed_sections
    );
}
