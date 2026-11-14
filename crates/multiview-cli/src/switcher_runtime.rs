//! First-slice Program/Preview launcher wiring.
//!
//! One place builds the real M/E frame-boundary handle, its bounded sustained
//! Preview demand-lease owner and the genuinely separate preview monitor tap, so
//! the production run path (`run_pipeline_until_ctrl_c`) and the CLI integration
//! test attach byte-identical wiring. Nothing here runs on the output clock: the
//! driver rides the engine frame hook and every lease is control-plane state.
//!
//! Honesty rules this module enforces:
//! - an absent `[switcher]` block attaches nothing (legacy one-canvas path);
//! - the configured preview source takes a bounded warm-on-arm lease at boot, so
//!   a cold PVW bus primes without operator action;
//! - a boot lease refusal is reported, never fatal — Program, the output clock
//!   and every other output keep running, and the PVW monitor stays explicitly
//!   unavailable rather than silently copying Program;
//! - the advertised capability is published only by the engine after both real
//!   buses composited a frame (in the hook), never here.

use std::sync::Arc;

use multiview_config::{DownstreamKeyerConfig, MixEffectsConfig};
use multiview_control::source_demand::{SwitcherPreviewLease, SwitcherPreviewLeases};
use multiview_control::SharedSourceDemand;
use multiview_core::switcher::SourceRef;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{ScenePair, SwitcherControl};
use multiview_engine::switcher_inputs::MixerInputs;
use multiview_engine::{keyer, SetItem};

use crate::pipeline::{Pipeline, PipelineError};
use crate::preview::ProgramSlot;

/// Scene generation this first slice authors. One immutable pair is installed;
/// re-authoring a second generation is a later slice.
pub const FIRST_SLICE_SCENE_VERSION: u64 = 1;

/// A live first-slice M/E attached to one pipeline.
pub struct FirstSliceSwitcher {
    /// Frame-boundary command handle; installed into the control plane only
    /// after this run's real provider starts publishing.
    pub control: SwitcherControl,
    /// Bounded sustained PVW lease owner shared with the REST routes, so a
    /// replacement lease is held before the previous PVW lease is released.
    pub leases: Arc<SwitcherPreviewLeases>,
    /// The separate PVW monitor tap; never Program's canvas.
    pub preview_slot: ProgramSlot,
    /// M/E identity echoed on correlated outcomes.
    pub me_id: String,
    /// Parent program identity (authorization scope + outcomes).
    pub program: String,
    /// Honest, non-fatal boot note when the configured preview source could not
    /// take its warm-on-arm lease. `None` when the lease was taken or when the
    /// run has no demand provider at all.
    pub boot_warning: Option<String>,
}

/// Build the engine's mixer-input table from the M/E declaration, or `None`
/// when the M/E declares no inputs (today's source-select behaviour).
///
/// # Errors
/// A table the engine cannot hold (a validated document never produces one).
pub fn mixer_inputs(me: &MixEffectsConfig) -> Result<Option<MixerInputs>, String> {
    if me.inputs.is_empty() {
        return Ok(None);
    }
    MixerInputs::new(me.inputs.iter().map(|input| {
        (
            input.number,
            input.label.clone(),
            input.source().map(SourceRef::new),
        )
    }))
    .map(Some)
}

/// The `keyer` demand items (ADR-0113 §4): one lease per fill and key,
/// labelled with the keyer object id (`<program>/<id>`).
#[must_use]
pub fn keyer_demand_items(keyers: &[DownstreamKeyerConfig]) -> Vec<SetItem> {
    let mut items = Vec::with_capacity(keyers.len().saturating_mul(2));
    for dsk in keyers {
        let label: Arc<str> = Arc::from(format!("{}/{}", dsk.program, dsk.id).as_str());
        for source in std::iter::once(&dsk.fill).chain(dsk.key.as_ref()) {
            items.push(SetItem {
                id: Arc::from(source.as_str()),
                label: Arc::clone(&label),
                size: None,
            });
        }
    }
    items
}

/// Resolve the declared downstream keyers against the run's stores and
/// admit them against the budget (ADR-0113 §9, reserved at definition).
///
/// # Errors
/// A fill or key without a frame store in this run, or a document over the
/// budget (validation refuses that first). Both are boot errors: a DSK is
/// never silently dropped.
pub fn admit_keyers<S: std::hash::BuildHasher>(
    keyers: &[DownstreamKeyerConfig],
    stores: &std::collections::HashMap<
        String,
        Arc<multiview_framestore::TileStore<multiview_compositor::pipeline::Nv12Image>>,
        S,
    >,
    canvas: (u32, u32),
    budget: f32,
) -> Result<keyer::KeyerRuntime, String> {
    let defs = keyers
        .iter()
        .map(|dsk| keyer::KeyerDef::from_config(dsk, stores, canvas.0, canvas.1))
        .collect::<Result<Vec<_>, _>>()?;
    keyer::admit(defs, canvas.0, canvas.1, budget)
}

/// Attach the declared first-slice M/E, if this document declares one.
///
/// `demand` is this run's control-plane demand provider (the same one the
/// control plane serves). When it is present the configured preview source takes
/// a bounded warm-on-arm lease **before** the engine starts, so a cold PVW bus
/// primes on its own. A lease refusal is reported via
/// [`FirstSliceSwitcher::boot_warning`] and is never fatal.
///
/// # Errors
///
/// Returns a typed error when a declared M/E cannot provide two real buses
/// (e.g. the pipeline was built without a usable program scene) — never a
/// silent single-bus fallback.
pub fn attach_first_slice(
    pipeline: &mut Pipeline,
    demand: Option<SharedSourceDemand>,
) -> Result<Option<FirstSliceSwitcher>, PipelineError> {
    let Some(me) = pipeline.switcher_me().cloned() else {
        // No declared M/E: the legacy one-canvas run path stays byte-identical.
        return Ok(None);
    };
    let preview_slot = crate::preview::program_slot();
    let inputs = mixer_inputs(&me).map_err(PipelineError::Engine)?;
    let (program, preview) = match (&inputs, me.program_input, me.preview_input) {
        // Numbered inputs (ADR-0110 §12.2): the machine selects input refs.
        (Some(_), Some(program), Some(preview)) => {
            (SourceRef::input(program), SourceRef::input(preview))
        }
        _ => (
            SourceRef::new(&me.program_source),
            SourceRef::new(&me.preview_source),
        ),
    };
    let (control, mut driver) = SwitcherControl::new(SwitcherState::new(program, preview, vec![]));
    // Shares the already-allocated stores; the driver only samples `is_primed`.
    driver.set_prime_stores(pipeline.preview_stores());
    if let Some(inputs) = inputs {
        // Installed before the clock starts; validation guaranteed that the
        // starting inputs are patched to program_source/preview_source, so the
        // boot lease and the scene pair below name the resolved sources.
        driver.set_inputs(inputs);
    }
    if !pipeline.switcher_keyers().is_empty() {
        // ADR-0113 / K2a: keyers are admitted before the clock starts; a
        // missing store or an over-budget document fails the boot.
        let runtime = admit_keyers(
            pipeline.switcher_keyers(),
            &pipeline.preview_stores(),
            pipeline.canvas_size(),
            pipeline.keyer_budget(),
        )
        .map_err(PipelineError::Engine)?;
        driver.set_keyers(runtime);
    }
    control
        .set_scenes(ScenePair {
            version: FIRST_SLICE_SCENE_VERSION,
            program: pipeline.switcher_program_layout(),
            preview: pipeline.switcher_preview_layout(&me.preview_source),
        })
        .map_err(PipelineError::Engine)?;
    let leases = Arc::new(SwitcherPreviewLeases::default());
    let boot_warning = match demand {
        Some(demand) => {
            match demand.switcher_preview_lease(&me.preview_source, "switcher:me1:pvw:boot") {
                Some(lease) => {
                    leases.seed(SwitcherPreviewLease::new(lease));
                    None
                }
                None => Some(format!(
                    "switcher preview source {:?} could not take a warm-on-arm lease at boot; \
                 Preview stays explicitly unavailable until an operator selects it \
                 (Program and every other output are unaffected)",
                    me.preview_source
                )),
            }
        }
        None => None,
    };
    pipeline.attach_switcher(control.clone(), driver, Arc::clone(&preview_slot))?;
    Ok(Some(FirstSliceSwitcher {
        control,
        leases,
        preview_slot,
        me_id: me.id.clone(),
        program: me.program.clone(),
        boot_warning,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// #341 restart truth: a persisted document (round-tripped through TOML,
    /// as the accepted state is) boots the engine with the same patch.
    #[test]
    fn a_persisted_input_table_boots_the_engine_with_the_same_patch() {
        let text = r#"
id = "me1"
program = "main"
program_source = "vt1"
preview_source = "abc-news"
program_input = 2
preview_input = 1
[[inputs]]
number = 1
label = "Source 1"
video = { source = "abc-news" }
[[inputs]]
number = 2
video = { source = "vt1" }
[[inputs]]
number = 5
"#;
        let me: MixEffectsConfig = toml::from_str(text).unwrap();
        let me: MixEffectsConfig = toml::from_str(&toml::to_string(&me).unwrap()).unwrap();
        let table = mixer_inputs(&me).unwrap().expect("inputs declared");
        let readback = table.readback(0, 0);
        let rows: Vec<_> = readback
            .inputs
            .iter()
            .map(|i| (i.number, i.source.as_ref().map(|s| s.as_str().to_owned())))
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, Some("abc-news".to_owned())),
                (2, Some("vt1".to_owned())),
                (5, None),
            ]
        );
        assert_eq!(table.resolve(&SourceRef::input(2)).as_str(), "vt1");
        let mut legacy = me;
        legacy.inputs.clear();
        assert!(mixer_inputs(&legacy).unwrap().is_none());
    }
}
