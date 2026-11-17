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

/// The off-clock stinger roll for these media-player mailboxes (ADR-0112
/// §5): `Cue` (to the in-point) then `Play`, submitted to the player's
/// bounded two-class transport mailbox from the roll worker, never from the
/// output clock. `None` without media players or when the worker cannot
/// start (logged; a stinger then still takes on schedule).
#[must_use]
pub fn player_roll<S: std::hash::BuildHasher + Send + 'static>(
    mailboxes: std::collections::HashMap<String, Arc<crate::player::TransportMailbox>, S>,
) -> Option<multiview_engine::player_roll::PlayerRollSender> {
    if mailboxes.is_empty() {
        return None;
    }
    let ids: Vec<String> = mailboxes.keys().cloned().collect();
    let spawned = multiview_engine::player_roll::PlayerRollSender::spawn(ids, move |player| {
        if let Some(mailbox) = mailboxes.get(player) {
            mailbox.submit(crate::player::TransportVerb::Cue { frame: None });
            mailbox.submit(crate::player::TransportVerb::Play);
        }
    });
    match spawned {
        Ok(roll) => Some(roll),
        Err(error) => {
            tracing::warn!(%error, "stinger player roll worker could not start; stingers will not roll their player");
            None
        }
    }
}

/// The M/E value machine at boot: the starting buses and the persisted
/// transition selection (ADR-0112 §4), applied before the clock starts so the
/// first AUTO already uses it.
///
/// # Errors
/// A persisted selection the machine refuses (a validated document never
/// carries one: config validation applies the same rule).
pub fn boot_machine(
    me: &MixEffectsConfig,
    program: SourceRef,
    preview: SourceRef,
) -> Result<SwitcherState, PipelineError> {
    let mut machine = SwitcherState::new(program, preview, vec![]);
    if let Some(transition) = &me.transition {
        if !transition.is_rendered() {
            return Err(PipelineError::Engine(
                multiview_engine::switcher_control::REFUSE_UNSUPPORTED_TRANSITION.into(),
            ));
        }
        machine
            .set_transition(transition.clone())
            .map_err(|e| PipelineError::Engine(format!("switcher transition: {e}")))?;
    }
    Ok(machine)
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

/// The `keyer` demand items of the M/E's upstream keyers (ADR-0113 §4, K3):
/// one lease per fill and key, labelled with the keyer object id
/// (`<me>/<id>`).
#[must_use]
pub fn usk_demand_items(me: &MixEffectsConfig) -> Vec<SetItem> {
    let mut items = Vec::with_capacity(me.keyers.len().saturating_mul(2));
    for usk in &me.keyers {
        let label: Arc<str> = Arc::from(format!("{}/{}", me.id, usk.id).as_str());
        for source in std::iter::once(&usk.fill).chain(usk.key.as_ref()) {
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
    me: Option<&MixEffectsConfig>,
    stores: &std::collections::HashMap<
        String,
        Arc<multiview_framestore::TileStore<multiview_compositor::pipeline::Nv12Image>>,
        S,
    >,
    canvas: (u32, u32),
    budget: f32,
) -> Result<keyer::KeyerRuntime, String> {
    let mut defs = keyers
        .iter()
        .map(|dsk| keyer::KeyerDef::from_config(dsk, stores, canvas.0, canvas.1))
        .collect::<Result<Vec<_>, _>>()?;
    // K3: the M/E's upstream keyers, in declared order (key index 1..).
    if let Some(me) = me {
        for usk in &me.keyers {
            defs.push(keyer::KeyerDef::from_usk(
                usk,
                &me.id,
                &me.program,
                stores,
                canvas.0,
                canvas.1,
            )?);
        }
    }
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
    let (control, mut driver) = SwitcherControl::new(boot_machine(&me, program, preview)?);
    // Shares the already-allocated stores; the driver only samples `is_primed`.
    driver.set_prime_stores(pipeline.preview_stores());
    // ADR-0112 §5: a stinger AUTO cues then plays its media player through
    // the player's bounded transport mailbox, from an off-clock worker.
    if let Some(roll) = player_roll(pipeline.player_mailboxes()) {
        driver.set_player_roll(roll);
    }
    if let Some(inputs) = inputs {
        // Installed before the clock starts; validation guaranteed that the
        // starting inputs are patched to program_source/preview_source, so the
        // boot lease and the scene pair below name the resolved sources.
        driver.set_inputs(inputs);
    }
    // ADR-0113 / K2a: keyers are admitted before the clock starts; a missing
    // store or an over-budget document fails the boot. K4: the runtime is
    // installed even with no DSK declared, so `keyer_set` can define the
    // first one at run time (an empty runtime is a no-op per tick).
    let runtime = admit_keyers(
        pipeline.switcher_keyers(),
        Some(&me),
        &pipeline.preview_stores(),
        pipeline.canvas_size(),
        pipeline.keyer_budget(),
    )
    .map_err(PipelineError::Engine)?;
    driver.set_keyers(runtime);
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
    // K4: the demand manager may already exist (it is built before the
    // attach on the run path); hand its `keyer` set to the switcher so a
    // committed `keyer_set` republishes the fill/key leases.
    if let Some(set) = pipeline.keyer_demand() {
        control.set_keyer_demand(set.clone());
    }
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

    /// ADR-0112 §5: the roll submits Cue then Play to the named player's
    /// mailbox, off the caller's thread, and ignores an unknown player.
    #[test]
    fn the_stinger_roll_cues_then_plays_the_named_player() {
        let mailbox = Arc::new(crate::player::TransportMailbox::new());
        let other = Arc::new(crate::player::TransportMailbox::new());
        let roll = player_roll(std::collections::HashMap::from([
            ("mp1".to_owned(), Arc::clone(&mailbox)),
            ("mp2".to_owned(), Arc::clone(&other)),
        ]))
        .expect("a worker");
        assert!(roll.roll(&SourceRef::new("mp1")));
        assert!(!roll.roll(&SourceRef::new("ghost")));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut verbs = Vec::new();
        while verbs.len() < 2 && std::time::Instant::now() < deadline {
            verbs.extend(mailbox.drain());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            verbs,
            vec![
                crate::player::TransportVerb::Cue { frame: None },
                crate::player::TransportVerb::Play
            ]
        );
        assert_eq!(other.drain(), Vec::new());
        assert!(player_roll(std::collections::HashMap::new()).is_none());
    }

    /// ADR-0112 §4 restart truth: a persisted selection boots the machine
    /// with it; none keeps the engine default; an unrendered one is refused.
    #[test]
    fn a_persisted_transition_boots_the_machine_with_it() {
        let text = r##"
id = "me1"
program = "main"
program_source = "vt1"
preview_source = "abc-news"
[transition]
kind = "wipe"
pattern = "diagonal"
rate = 20
softness = 0.05
reverse = true
border_width = 0.01
border_color = "#00ff00"
"##;
        let me: MixEffectsConfig = toml::from_str(text).unwrap();
        let me: MixEffectsConfig = toml::from_str(&toml::to_string(&me).unwrap()).unwrap();
        let machine = boot_machine(&me, SourceRef::new("vt1"), SourceRef::new("abc-news")).unwrap();
        assert_eq!(Some(machine.transition()), me.transition.as_ref());
        let mut legacy = me.clone();
        legacy.transition = None;
        let machine =
            boot_machine(&legacy, SourceRef::new("vt1"), SourceRef::new("abc-news")).unwrap();
        assert_eq!(
            machine.transition(),
            &multiview_core::switcher::TransitionDescriptor::Cut
        );
        let mut unrendered = me;
        unrendered.transition = Some(
            serde_json::from_value(serde_json::json!({
                "kind": "wipe", "pattern": "box", "rate": 10, "flip_flop": true
            }))
            .unwrap(),
        );
        assert!(boot_machine(
            &unrendered,
            SourceRef::new("vt1"),
            SourceRef::new("abc-news")
        )
        .is_err());
    }
}
