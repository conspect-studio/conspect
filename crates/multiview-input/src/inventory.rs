//! Container-agnostic [`StreamInventory`] **merge** (RT-2, ADR-0034 §3).
//!
//! RT-1 established one discovery surface — the general libav demux path builds a
//! [`StreamInventory`] of every elementary stream. RT-2 enriches that base for the
//! **two native paths that carry richer per-stream metadata than libav surfaces**:
//!
//! * **MPEG-TS / SRT** — the PMT's descriptor loops carry the per-elementary-stream
//!   language + accessibility role libav's TS metadata frequently misses, plus the
//!   authoritative SCTE-35 PIDs. [`merge_ts`] overlays that PMT signalling onto the
//!   libav base and reconciles SCTE-35 so the PID neither double-lists nor misses.
//! * **HLS** — the master playlist's AUDIO + SUBTITLES alternate renditions, which
//!   libav (opening the URL as a single program) never surfaces. [`merge_hls`]
//!   folds those renditions onto the libav base.
//!
//! Both produce **one unified [`StreamInventory`]** so a consumer gets the same
//! typed surface regardless of container. Pure, libav-free data logic.

use multiview_core::stream::{Bcp47, StreamDescriptor, StreamInventory, StreamKind};

use crate::hls::MasterPlaylist;
use crate::mpegts::inventory::{pmt_inventory, reconcile_scte35};
use crate::mpegts::pmt::Pmt;

/// Merge a TS/SRT source's PMT into the general-demux [`StreamInventory`] `base`
/// (RT-2), yielding **one unified inventory**.
///
/// The PMT is authoritative for an MPEG-TS program's per-elementary-stream
/// language + role + SCTE-35 PIDs, which libav's TS metadata frequently misses:
///
/// * audio / subtitle rows in `base` that **lack** a language gain the PMT's
///   descriptor language (and an accessibility role title hint), matched by
///   kind + ordinal within kind (the two surfaces enumerate one program's streams
///   in the same wire order);
/// * SCTE-35 is reconciled to **exactly one** `Data(Scte35)` per PID (see
///   [`reconcile_scte35`]): the PMT's hard PID-keyed rows supersede any soft
///   general-demux SCTE row, and a PSI PID missing from `base` is added.
///
/// `base`'s decoded geometry / channel-layout detail is preserved; only the
/// missing language / role is overlaid. If the PMT cannot be folded (a malformed
/// descriptor loop) the SCTE reconciliation still runs against an empty PSI set,
/// so `base` is returned unchanged rather than failing.
#[must_use]
pub fn merge_ts(base: StreamInventory, pmt: &Pmt) -> StreamInventory {
    let Ok(pmt_inv) = pmt_inventory(pmt) else {
        return reconcile_scte35(base, &[]);
    };

    let mut merged = base;
    overlay_language_and_role(&mut merged, &pmt_inv, StreamKind::is_audio);
    overlay_language_and_role(&mut merged, &pmt_inv, StreamKind::is_subtitle);

    // Reconcile SCTE-35 against the PMT's authoritative PID set so the unified
    // inventory carries exactly one Data(Scte35) per PID.
    reconcile_scte35(merged, &pmt.scte35_pids())
}

/// Merge an HLS master playlist's AUDIO + SUBTITLES renditions into the
/// general-demux [`StreamInventory`] `base` (RT-2), yielding **one unified
/// inventory**.
///
/// libav opens an HLS URL as a single program and never surfaces the master's
/// separate AUDIO / SUBTITLES renditions, so these are **appended** to the base
/// (the base typically carries the variant's in-band video + muxed audio).
/// Each rendition is hard-keyed by `group_id + name` (a non-empty name is
/// synthesised when the playlist omits one) so the ids survive a rendition
/// reorder; an HLS master carries no SCTE-35, so no reconciliation is needed.
#[must_use]
pub fn merge_hls(base: StreamInventory, master: &MasterPlaylist) -> StreamInventory {
    let mut merged = base;
    merged.streams.extend(master.stream_inventory().streams);
    merged
}

/// Overlay the PMT-derived language + role onto `base` rows of one kind family
/// that lack a language, matching by ordinal within the kind family.
fn overlay_language_and_role(
    base: &mut StreamInventory,
    pmt_inv: &StreamInventory,
    predicate: fn(StreamKind) -> bool,
) {
    let enriched: Vec<&StreamDescriptor> = pmt_inv.by_kind(predicate).collect();
    let mut ordinal = 0usize;
    for row in base.streams.iter_mut().filter(|s| predicate(s.kind)) {
        if let Some(src) = enriched.get(ordinal) {
            if row.language.is_none() {
                row.language.clone_from(&src.language);
            }
            if row.title.is_none() {
                row.title.clone_from(&src.title);
            }
            // Carry the PMT's default flag when the base did not set one.
            if !row.default {
                row.default = src.default;
            }
        }
        ordinal = ordinal.saturating_add(1);
    }
}

/// Re-validate a raw language string onto a [`Bcp47`] (lenient: a bad tag → `None`).
///
/// Kept here so callers that hold a raw language string (rather than a parsed
/// descriptor) can fold it onto the same surface; mirrors the libav-path lenience.
#[must_use]
pub fn parse_language(raw: &str) -> Option<Bcp47> {
    Bcp47::parse(raw).ok()
}
