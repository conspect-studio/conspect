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

use multiview_control::source_demand::{SwitcherPreviewLease, SwitcherPreviewLeases};
use multiview_control::SharedSourceDemand;
use multiview_core::switcher::SourceRef;
use multiview_engine::switcher::SwitcherState;
use multiview_engine::switcher_control::{ScenePair, SwitcherControl};

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
    let (control, mut driver) = SwitcherControl::new(SwitcherState::new(
        SourceRef::new(&me.program_source),
        SourceRef::new(&me.preview_source),
        vec![],
    ));
    // Shares the already-allocated stores; the driver only samples `is_primed`.
    driver.set_prime_stores(pipeline.preview_stores());
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
