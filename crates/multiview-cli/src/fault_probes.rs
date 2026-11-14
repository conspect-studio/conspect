//! The per-source content-fault probe construction shared by Program's fault
//! detector (`pipeline::FaultDetector`) and a routed layout's
//! (`aux_overlays::AuxFaultDetector`), so a source declared with an operator
//! probe (threshold, zone, dwell, severity, latch) is judged the same way on
//! both (feature `overlay`).
//!
//! Everything here is pure construction from config, run once per source; no
//! I/O, no lock, never on the output clock.

use std::collections::HashMap;

use multiview_core::alarm::{AlarmId, AlarmKind, AlarmScope, PerceivedSeverity};
use multiview_core::layout::Layout;
use multiview_core::time::MediaTime;
use multiview_engine::{
    black_config_from_kind, freeze_config_from_kind, AlarmHysteresis, AlarmStateMachine,
    BlackConfig, BlackProbe, FreezeConfig, FreezeProbe,
};

/// The CLI freeze probe's per-sample tolerance: real encoded sources
/// re-quantize a frozen picture at every GOP boundary (see
/// `pipeline::FaultDetector`), so the CLI widens the engine default.
pub const FREEZE_TOLERANCE: u8 = 6;

/// An operator-declared probe override for one source, resolved from
/// `config.probes` (M10): the config [`Probe`](multiview_config::probe::Probe)
/// the engine builds a config-derived analyser + X.733 lifecycle from, per
/// fault class. A `None` entry means "no declared probe for this class — use
/// the hardcoded default".
///
/// The first declared probe of each kind for a source wins (duplicate probe
/// ids are already rejected by config validation; two probes of the same kind
/// on one source is an unusual config and deterministically resolves to the
/// first).
#[derive(Debug, Default, Clone)]
pub struct SourceProbeOverride {
    /// The declared black probe (its `luma_threshold` + `zone` + dwell + severity).
    pub black: Option<multiview_config::probe::Probe>,
    /// The declared freeze probe (its `difference_threshold` + `zone` + dwell + …).
    pub freeze: Option<multiview_config::probe::Probe>,
    /// The declared silence probe (its dwell + severity, and its `level_dbfs`,
    /// which becomes this source's silence detection floor).
    pub silence: Option<multiview_config::probe::Probe>,
}

/// Per source id: its declared probe overrides.
pub type SourceProbes = HashMap<String, SourceProbeOverride>;

/// Resolve `config.probes` into per-**source** overrides by mapping each
/// probe's watched **cell** to its bound source via the solved `layout` (M10).
///
/// The config carries cell ids only on the raw cells, not the solved
/// `Layout`; the desugared run binds cell id == source id for the common
/// single-source-per-cell case, so a probe's `cell` resolves against a layout
/// cell whose bound source equals it, falling back to treating the `cell` as
/// the source id directly. Runs once at run start, off the hot loop.
#[must_use]
pub fn resolve_source_probes(
    layout: &Layout,
    probes: &[multiview_config::probe::Probe],
) -> SourceProbes {
    use multiview_config::probe::ProbeKind;
    let mut out = SourceProbes::new();
    for probe in probes {
        let source = layout
            .cells
            .iter()
            .filter_map(|c| c.source.clone())
            .find(|s| s == &probe.cell)
            .unwrap_or_else(|| probe.cell.clone());
        let entry = out.entry(source).or_default();
        match probe.kind {
            ProbeKind::Black { .. } if entry.black.is_none() => {
                entry.black = Some(probe.clone());
            }
            ProbeKind::Freeze { .. } if entry.freeze.is_none() => {
                entry.freeze = Some(probe.clone());
            }
            ProbeKind::Silence { .. } if entry.silence.is_none() => {
                entry.silence = Some(probe.clone());
            }
            // A loudness probe (or a duplicate of an already-captured kind, or a
            // future kind) does not override one of the three content-fault badge
            // classes; the loudness alarm rides the meter path elsewhere.
            _ => {}
        }
    }
    out
}

/// The default (undeclared) analysers and dwell windows: black/silence raise
/// after ~0.5 s and freeze after ~2 s of the condition; all clear after
/// ~0.3 s of its absence. On the media timeline, never wall-clock.
#[derive(Debug, Clone, Copy)]
pub struct DefaultProbes {
    /// The shared default black analyser.
    pub black: BlackProbe,
    /// The shared default freeze analyser (CLI tolerance).
    pub freeze: FreezeProbe,
    /// Default black dwell.
    pub black_dwell: AlarmHysteresis,
    /// Default freeze dwell.
    pub freeze_dwell: AlarmHysteresis,
    /// Default silence dwell.
    pub silence_dwell: AlarmHysteresis,
}

impl Default for DefaultProbes {
    fn default() -> Self {
        let dwell = |num: i64, den: i64| -> MediaTime {
            MediaTime::from_nanos(num.saturating_mul(1_000_000_000) / den.max(1))
        };
        let down = dwell(3, 10); // 0.3 s
        Self {
            black: BlackProbe::new(BlackConfig::default()),
            freeze: FreezeProbe::new(FreezeConfig::default().with_tolerance(FREEZE_TOLERANCE)),
            black_dwell: AlarmHysteresis::new(dwell(1, 2), down), // 0.5 s up
            freeze_dwell: AlarmHysteresis::new(dwell(2, 1), down), // 2 s up
            silence_dwell: AlarmHysteresis::new(dwell(1, 2), down), // 0.5 s up
        }
    }
}

/// The source's declared **black** analyser (threshold + zone), if any.
#[must_use]
pub fn declared_black(ov: &SourceProbeOverride) -> Option<BlackProbe> {
    ov.black
        .as_ref()
        .and_then(|p| black_config_from_kind(&p.kind))
        .map(BlackProbe::new)
}

/// The source's declared **freeze** analyser (change threshold + zone), with
/// the CLI's wider per-sample tolerance kept (the engine mapper keeps the
/// default tolerance).
#[must_use]
pub fn declared_freeze(ov: &SourceProbeOverride) -> Option<FreezeProbe> {
    ov.freeze
        .as_ref()
        .and_then(|p| freeze_config_from_kind(&p.kind))
        .map(|cfg| FreezeProbe::new(cfg.with_tolerance(FREEZE_TOLERANCE)))
}

/// One fault class's dwell machine for source `id`: the declared probe's
/// full X.733 lifecycle (id/scope/severity/dwell/latch) via
/// [`AlarmStateMachine::from_probe`], else the hardcoded default machine.
#[must_use]
pub fn fault_machine(
    id: &str,
    declared: Option<&multiview_config::probe::Probe>,
    kind: AlarmKind,
    severity: PerceivedSeverity,
    hysteresis: AlarmHysteresis,
) -> AlarmStateMachine {
    match declared {
        Some(probe) => AlarmStateMachine::from_probe(probe),
        None => AlarmStateMachine::new(
            AlarmId::new(format!("{id}:{kind:?}")),
            kind,
            AlarmScope::Probe { id: id.to_owned() },
            severity,
            hysteresis,
        ),
    }
}
