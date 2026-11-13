//! Mixer patch persistence (ADR-0110 §12.8, #341; the #329 lesson): the
//! engine's live input table is written into the accepted state only after it
//! was actually on air, under the config-mutation lock, never spliced from the
//! boot file. Off the output clock: the relay wakes on the switcher's bounded
//! drop-oldest outcome ring and reads the change-only readback; a lagged ring
//! is harmless because the readback (not the event) is the source of truth.

use multiview_config::{MixerInputVideo, MultiviewConfig};
use multiview_engine::isolation::RecvError;
use multiview_engine::switcher_control::{SwitcherControl, SwitcherSnapshot};
use multiview_engine::switcher_inputs::InputsReadback;

use crate::{AppState, ControlError, ControlResult};

/// Start the persistence relay for this run's switcher, if any.
pub(crate) fn spawn_persist_relay(state: &AppState) {
    let Some(control) = state.switcher.clone() else {
        return;
    };
    if state.boot_model.is_none() || control.inputs().is_none() {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("mixer patch: no async runtime; applied patches are not persisted");
        return;
    };
    let state = state.clone();
    runtime.spawn(Box::pin(persist_relay(state, control)));
}

async fn persist_relay(state: AppState, control: SwitcherControl) {
    let mut outcomes = control.outcomes();
    // The boot table is the accepted one; only later versions are persisted.
    let mut persisted = control.inputs().map_or(0, |table| table.version);
    loop {
        match outcomes.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
        let Some(table) = control.inputs() else {
            continue;
        };
        if table.version == persisted {
            continue;
        }
        let snapshot = control.snapshot();
        match persist_inputs(&state, &table, snapshot.as_deref()).await {
            Ok(()) => persisted = table.version,
            Err(error) => {
                tracing::warn!(%error, version = table.version, "mixer patch: applied patch not persisted");
            }
        }
    }
}

/// Write `table` as `switcher.mix_effects[0].inputs` of the accepted state.
///
/// PGM/PVW selections are not persisted (they never were); the starting
/// `program_input`/`preview_input` keep their numbers and
/// `program_source`/`preview_source` follow the sources now behind them (the
/// one placeholder Program cell is re-bound with it, so start-up admission
/// keeps holding). A starting input that is now unpatched moves to the input
/// the engine currently has on that bus.
///
/// # Errors
/// The accepted document cannot be read, validated or committed.
pub async fn persist_inputs(
    state: &AppState,
    table: &InputsReadback,
    snapshot: Option<&SwitcherSnapshot>,
) -> ControlResult<()> {
    if state.boot_model.is_none() {
        return Ok(());
    }
    let _mutation = state.lock_config_mutation().await;
    let mut config: MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))?;
    let Some(me) = config
        .switcher
        .as_mut()
        .and_then(|switcher| switcher.mix_effects.first_mut())
    else {
        return Ok(());
    };
    if me.inputs.is_empty() {
        return Ok(());
    }
    for input in &mut me.inputs {
        if let Some(live) = table.inputs.iter().find(|live| live.number == input.number) {
            input.video = live
                .source
                .as_ref()
                .map(|source| MixerInputVideo::Source(source.as_str().to_owned()));
        }
    }
    let live_program = snapshot.and_then(|s| s.program_input);
    let live_preview = snapshot.and_then(|s| s.preview_input);
    if me.program_input.and_then(|n| me.input_source(n)).is_none() {
        me.program_input = live_program;
    }
    if me.preview_input.and_then(|n| me.input_source(n)).is_none()
        || me.preview_input == me.program_input
    {
        me.preview_input = live_preview.filter(|n| Some(*n) != me.program_input);
    }
    let program_source = me
        .program_input
        .and_then(|n| me.input_source(n))
        .map(str::to_owned);
    let preview_source = me
        .preview_input
        .and_then(|n| me.input_source(n))
        .map(str::to_owned);
    let (Some(program_source), Some(preview_source)) = (program_source, preview_source) else {
        return Err(ControlError::Validation(
            "the live patch leaves no patched starting input for Program or Preview".into(),
        ));
    };
    let previous_program = std::mem::replace(&mut me.program_source, program_source.clone());
    me.preview_source = preview_source;
    if previous_program != program_source {
        rebind_program_cell(&mut config, &previous_program, &program_source);
    }
    config
        .validate()
        .map_err(|e| ControlError::Validation(e.to_string()))?;
    let layouts = crate::boot_model::layout_library(state)?;
    crate::boot_model::commit_desired(state, config, layouts).await
}

/// The first slice's one Program cell is a placeholder the switcher hook
/// rebinds on its first tick; admission requires it to name `program_source`.
fn rebind_program_cell(config: &mut MultiviewConfig, from: &str, to: &str) {
    let rebind = |cells: &mut [multiview_config::Cell]| {
        for cell in cells {
            if cell.source.input_id.as_deref() == Some(from) {
                cell.source.input_id = Some(to.to_owned());
            }
        }
    };
    rebind(&mut config.cells);
    for program in &mut config.programs {
        if let multiview_config::ProgramKind::Multiview { cells, .. } = &mut program.kind {
            rebind(cells);
        }
    }
}

/// Map an engine error string to its RFC 9457 `type` suffix.
#[must_use]
pub fn refusal_kind(error: &str) -> &'static str {
    for kind in [
        "on-air-protected",
        "unknown-slot",
        "unknown-source",
        "not-on-an-input",
    ] {
        if error.starts_with(kind) {
            return kind;
        }
    }
    if error.contains("fresh frame") {
        "feed-not-fresh"
    } else {
        "refused"
    }
}
