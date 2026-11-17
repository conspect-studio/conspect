//! Declarative M/E first slice (ADR-M012). The remaining keyer, aux and
//! composition domains remain in `extra` for lossless forward migration; they
//! are not reported as active by the runtime until their implementations land.
//!
//! Downstream keyers (ADR-0113, `docs/architecture/switcher-keyers.md` §2,
//! slice K2): `[[switcher.downstream_keyers]]` is typed, validated and
//! admitted against `switcher.keyer_budget` at definition.
//!
//! Numbered mixer inputs (ADR-0110, `docs/architecture/output-routing.md`
//! §12.2): an M/E may declare `[[switcher.mix_effects.inputs]]`; PGM/PVW then
//! select input numbers and the patch decides which source is behind each.

use std::collections::HashSet;

use multiview_core::switcher::TransitionDescriptor;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Highest mixer input number and the maximum count of declared inputs.
pub const MAX_MIXER_INPUTS: u16 = 64;
/// Prefix of the engine's mixer-input reference spelling (`in:<n>`).
pub const INPUT_REF_PREFIX: &str = "in:";
/// Maximum operator label length, in characters.
pub const MAX_MIXER_INPUT_LABEL: usize = 64;

/// One mix/effects stage inside a program. The first runtime admits exactly one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixEffectsConfig {
    /// Unique M/E id, currently `me1`.
    pub id: String,
    /// Parent program, currently `main` or the sole declared program.
    pub program: String,
    /// Source id shown on PGM when starting (a declared source). With
    /// `inputs` declared it must equal the source patched to `program_input`.
    pub program_source: String,
    /// Source id armed on PVW when starting (a declared source). With
    /// `inputs` declared it must equal the source patched to `preview_input`.
    pub preview_source: String,
    /// Swap previous PGM onto PVW after a completed take.
    #[serde(default = "default_flip_flop")]
    pub flip_flop: bool,
    /// Numbered mixer inputs (§12.2). Empty: PGM/PVW select sources by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<MixerInput>,
    /// Starting PGM input number; required exactly when `inputs` is non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_input: Option<u16>,
    /// Starting PVW input number; required exactly when `inputs` is non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_input: Option<u16>,
    /// The selected transition the next AUTO uses (ADR-0112 §4), re-applied
    /// at boot. Absent means the engine default (an AUTO is a MIX). Written
    /// back by the control plane after the engine applied a selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<TransitionDescriptor>,
}

/// One numbered mixer input slot `me1.in<N>` and its patched video source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixerInput {
    /// Input number, 1..=64, unique within the M/E.
    pub number: u16,
    /// Operator label shown on the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The patched video essence; absent means unpatched (black, silent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<MixerInputVideo>,
}

impl MixerInput {
    /// The source id patched to this input, if any.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        match &self.video {
            Some(MixerInputVideo::Source(id)) => Some(id.as_str()),
            None => None,
        }
    }
}

/// What feeds a mixer input's video slot. Only configured sources for now;
/// layouts and buses arrive with #307 (they are refused by deserialization).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum MixerInputVideo {
    /// A declared source id: `video = { source = "abc-news" }`.
    Source(String),
}

/// Hard limit on downstream keyers per program (ADR-0113 §9).
pub const MAX_DOWNSTREAM_KEYERS: usize = 2;
/// Smallest fly width and height, in canvas pixels (ADR-0113 §4).
pub const MIN_KEYER_FLY: u32 = 16;
/// Largest DSK AUTO rate, in frames (ADR-0113 §5).
pub const MAX_KEYER_RATE: u32 = 300;
/// Default DSK AUTO rate, in frames.
pub const DEFAULT_KEYER_RATE: u32 = 15;
/// Largest admitted `max_source` side (a device texture bound shared by the
/// wgpu baseline; a source above it can never be uploaded).
pub const MAX_KEYER_SOURCE_DIM: u32 = 8192;
/// Fold weight of a DSK (ADR-0113 §9: a DSK is folded once per tick).
pub const DSK_FOLD: f64 = 1.0;
/// Upload weight `u` of ADR-0113 §9, per canvas of source pixels.
///
/// **Uncalibrated placeholder.** K1 owes the RTX 4060 figure (design note
/// §11.1); until then `1.0` makes the upload of a canvas-size picture cost the
/// same as one canvas fold, which is the conservative CPU-fallback reading.
pub const KEYER_UPLOAD_WEIGHT: f64 = 1.0;
/// Default `switcher.keyer_budget`: the cost of exactly one full-canvas DSK
/// with a canvas-size fill **and** key, so the default admits one DSK of any
/// key type and refuses a second full-canvas one (ADR-0113 §9, until the
/// mda001 protection-bound run records a measured budget).
/// Equals `DSK_FOLD + 2 × KEYER_UPLOAD_WEIGHT` (pinned by a unit test).
pub const DEFAULT_KEYER_BUDGET: f32 = 3.0;

/// A rectangle in pixels (`fly` in canvas pixels, `mask` in fill-source
/// pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyerRect {
    /// Left column.
    pub x: u32,
    /// Top row.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

impl KeyerRect {
    /// Whether this rectangle lies inside a `w`×`h` area (u64 arithmetic).
    #[must_use]
    pub fn fits_in(&self, w: u32, h: u32) -> bool {
        u64::from(self.x) + u64::from(self.w) <= u64::from(w)
            && u64::from(self.y) + u64::from(self.h) <= u64::from(h)
    }
}

/// A picture size in pixels (`max_source`, ADR-0113 §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyerSize {
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// How a DSK derives its matte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyerType {
    /// Linear key (the fill's own alpha, or a key source's luma).
    #[default]
    Linear,
    /// Luma key with `clip` and `gain`.
    Luma,
    /// Chroma key (slice K7, ADR-C007 §3) on the fill's own colour: `color`
    /// is the reference, `clip`/`gain` shape the edge on the linear-RGB
    /// distance. A key source is refused.
    Chroma,
}

/// What a keyer does when a source reaches its loss trigger (ADR-R011 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyerLossPolicy {
    /// Take the keyer off air; it stays off until an operator puts it on.
    #[default]
    DropToOffAir,
    /// Keep drawing the frozen last-good pair and report `stale`.
    HoldLastGood,
}

/// The source state at or past which a keyer's loss policy fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum KeyerLossTrigger {
    /// The first non-live read past the hold window.
    Stale,
    /// The source is reconnecting (the default).
    #[default]
    Reconnecting,
    /// The source has no signal.
    NoSignal,
}

fn default_keyer_rate() -> u32 {
    DEFAULT_KEYER_RATE
}

fn default_gain() -> f32 {
    1.0
}

#[allow(clippy::trivially_copy_pass_by_ref)]
// reason: serde's `skip_serializing_if` passes a reference.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One downstream keyer, `[[switcher.downstream_keyers]]` (ADR-0113 §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
// reason: four independent operator flags of the config document.
pub struct DownstreamKeyerConfig {
    /// Unique id within the program; the object id is `<program>/<id>`.
    pub id: String,
    /// Parent program (a declared program).
    pub program: String,
    /// Fill source id (a declared source; mixer inputs are refused).
    pub fill: String,
    /// Optional key source id; absent means a self key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Matte derivation.
    #[serde(rename = "type", default)]
    pub key_type: KeyerType,
    /// Luma clip on normalized Y, `[0, 1]`. Chroma: the linear-RGB distance
    /// below which the fill is keyed out fully, `[0, 1]`.
    #[serde(default)]
    pub clip: f32,
    /// Luma or chroma gain (soft-edge slope), finite and `>= 0`.
    #[serde(default = "default_gain")]
    pub gain: f32,
    /// Chroma reference colour, `#RRGGBB` / `#RGB` R'G'B' code values of the
    /// fill's own transfer (e.g. `#00b140`). Required by `type = "chroma"`,
    /// refused by every other type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Invert the matte.
    #[serde(default, skip_serializing_if = "is_false")]
    pub invert: bool,
    /// Mask in fill-source pixels; absent means no mask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<KeyerRect>,
    /// Destination rectangle in canvas pixels; absent means the full canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fly: Option<KeyerRect>,
    /// Admitted source envelope; absent means the canvas size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_source: Option<KeyerSize>,
    /// AUTO rate in frames, `1..=300`.
    #[serde(default = "default_keyer_rate")]
    pub rate: u32,
    /// Tied to the next background transition.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tie: bool,
    /// Goes on at boot through the fresh gate (the station bug).
    #[serde(default, skip_serializing_if = "is_false")]
    pub on_at_start: bool,
    /// Order among DSKs (ascending = drawn first), unique per program.
    #[serde(default)]
    pub z: i32,
    /// Loss policy.
    #[serde(default)]
    pub loss: KeyerLossPolicy,
    /// Loss trigger.
    #[serde(default)]
    pub loss_trigger: KeyerLossTrigger,
    /// Shaped (premultiplied) fills: parsed only to be refused by name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub premultiplied: bool,
}

/// The admission cost of one keyer in canvas folds (ADR-0113 §9):
/// `fold × dst_area / canvas_area + u × (fill_px + key_px) / canvas_px`,
/// where the source areas are the declared `max_source` envelope. Shared by
/// config validation and the engine's admission. A zero canvas costs
/// infinity (never admitted).
#[must_use]
pub fn keyer_cost(
    fold: f64,
    dst: (u32, u32),
    source: (u32, u32),
    has_key: bool,
    canvas: (u32, u32),
) -> f64 {
    let canvas_px = f64::from(canvas.0) * f64::from(canvas.1);
    if canvas_px <= 0.0 {
        return f64::INFINITY;
    }
    let dst_px = f64::from(dst.0) * f64::from(dst.1);
    let source_px = f64::from(source.0) * f64::from(source.1);
    let sources = if has_key { 2.0 } else { 1.0 };
    fold * dst_px / canvas_px + KEYER_UPLOAD_WEIGHT * sources * source_px / canvas_px
}

/// Slack for comparing a summed cost with the budget, so the exact default
/// (one full-canvas DSK) is never refused by rounding.
const BUDGET_EPSILON: f64 = 1e-9;

impl DownstreamKeyerConfig {
    /// The keyer's destination rectangle, defaulting to the full canvas.
    #[must_use]
    pub fn fly_or_canvas(&self, canvas_w: u32, canvas_h: u32) -> KeyerRect {
        self.fly.unwrap_or(KeyerRect {
            x: 0,
            y: 0,
            w: canvas_w,
            h: canvas_h,
        })
    }

    /// The admitted source envelope, defaulting to the canvas size.
    #[must_use]
    pub fn max_source_or_canvas(&self, canvas_w: u32, canvas_h: u32) -> KeyerSize {
        self.max_source.unwrap_or(KeyerSize {
            w: canvas_w,
            h: canvas_h,
        })
    }

    /// This keyer's admission cost on a `canvas_w`×`canvas_h` canvas.
    #[must_use]
    pub fn cost(&self, canvas_w: u32, canvas_h: u32) -> f64 {
        let fly = self.fly_or_canvas(canvas_w, canvas_h);
        let source = self.max_source_or_canvas(canvas_w, canvas_h);
        keyer_cost(
            DSK_FOLD,
            (fly.w, fly.h),
            (source.w, source.h),
            self.key.is_some(),
            (canvas_w, canvas_h),
        )
    }

    /// Validate one definition on its own (the K4 `keyer_set` admission):
    /// every field check of [`SwitcherConfig::validate_keyers`] except the
    /// cross-keyer ones (unique id and z, the budget), which the caller
    /// checks against the live set.
    ///
    /// # Errors
    /// A bad reference (`in:<n>` included), a refused-by-name feature or a
    /// geometry out of bounds.
    pub fn validate_definition(
        &self,
        sources: &[&str],
        canvas_w: u32,
        canvas_h: u32,
    ) -> Result<(), ConfigError> {
        self.validate(sources, &[self.program.as_str()], canvas_w, canvas_h)
    }

    #[allow(clippy::too_many_lines)]
    // reason: one flat list of independent field checks, in document order.
    fn validate(
        &self,
        sources: &[&str],
        programs: &[&str],
        canvas_w: u32,
        canvas_h: u32,
    ) -> Result<(), ConfigError> {
        let err = |detail: String| {
            Err(ConfigError::Validation(format!(
                "downstream keyer {:?} {detail}",
                self.id
            )))
        };
        if self.id.trim().is_empty() || self.id.contains('/') {
            return err("needs a non-empty id without '/'".into());
        }
        if !programs.contains(&self.program.as_str()) {
            return err(format!("references undeclared program {:?}", self.program));
        }
        if self.key_type == KeyerType::Chroma {
            if self.key.is_some() {
                return err(
                    "type = \"chroma\" keys the fill's own colour; a separate key source is \
                     refused (chroma-key-source)"
                        .into(),
                );
            }
            match self.color.as_deref() {
                None => {
                    return err(
                        "type = \"chroma\" needs color = \"#RRGGBB\" (chroma-color-required)"
                            .into(),
                    )
                }
                Some(c) if crate::parse_hex_color(c).is_none() => {
                    return err(format!(
                        "color {c:?} is not a #RGB / #RRGGBB hex color (chroma-color)"
                    ));
                }
                Some(_) => {}
            }
        } else if self.color.is_some() {
            return err(
                "color applies only to type = \"chroma\" (chroma-color-without-chroma)".into(),
            );
        }
        if self.premultiplied {
            return err(
                "premultiplied = true is not supported yet (shaped fills need un-premultiply, ADR-0113 §3)"
                    .into(),
            );
        }
        for (role, id) in [
            ("fill", Some(self.fill.as_str())),
            ("key", self.key.as_deref()),
        ] {
            let Some(id) = id else { continue };
            if id.starts_with(INPUT_REF_PREFIX) {
                return err(format!(
                    "{role} {id:?} names a mixer input; keyer fill and key name sources only"
                ));
            }
            if !sources.contains(&id) {
                return err(format!("{role} references undeclared source {id:?}"));
            }
        }
        if self.rate == 0 || self.rate > MAX_KEYER_RATE {
            return err(format!(
                "rate {} is outside 1..={MAX_KEYER_RATE}",
                self.rate
            ));
        }
        if !self.clip.is_finite() || !(0.0..=1.0).contains(&self.clip) {
            return err(format!("clip {} is outside [0, 1]", self.clip));
        }
        if !self.gain.is_finite() || self.gain < 0.0 {
            return err(format!("gain {} must be finite and >= 0", self.gain));
        }
        if let Some(fly) = self.fly {
            let even = [fly.x, fly.y, fly.w, fly.h].iter().all(|v| v % 2 == 0);
            if !even {
                return err("fly x, y, w and h must be even".into());
            }
            if fly.w < MIN_KEYER_FLY || fly.h < MIN_KEYER_FLY {
                return err(format!(
                    "fly must be at least {MIN_KEYER_FLY}x{MIN_KEYER_FLY}"
                ));
            }
            if !fly.fits_in(canvas_w, canvas_h) {
                return err(format!("fly lies outside the {canvas_w}x{canvas_h} canvas"));
            }
        }
        let source = self.max_source_or_canvas(canvas_w, canvas_h);
        if source.w == 0
            || source.h == 0
            || source.w % 2 != 0
            || source.h % 2 != 0
            || source.w > MAX_KEYER_SOURCE_DIM
            || source.h > MAX_KEYER_SOURCE_DIM
        {
            return err(format!(
                "max_source must be even and within 2..={MAX_KEYER_SOURCE_DIM} per side"
            ));
        }
        if let Some(mask) = self.mask {
            if mask.w == 0 || mask.h == 0 || !mask.fits_in(source.w, source.h) {
                return err(format!(
                    "mask must be non-empty and inside max_source {}x{}",
                    source.w, source.h
                ));
            }
        }
        Ok(())
    }
}

fn default_flip_flop() -> bool {
    true
}

/// A declared `[switcher]` root. An absent block preserves legacy behaviour.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SwitcherConfig {
    /// M/E stages. Empty means no switcher runtime or preview bus exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mix_effects: Vec<MixEffectsConfig>,
    /// Downstream keyers of the program (ADR-0113), at most
    /// [`MAX_DOWNSTREAM_KEYERS`]. Empty: no DSK runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downstream_keyers: Vec<DownstreamKeyerConfig>,
    /// The keyer cost budget in canvas folds (ADR-0113 §9). `None`: the
    /// [`DEFAULT_KEYER_BUDGET`], which admits exactly one full-canvas DSK.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyer_budget: Option<f32>,
    /// Future keyer/aux/composition declarations survive round trips; no runtime
    /// advertises those domains as executable until explicitly implemented.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl MixEffectsConfig {
    /// The source patched to input `number`, if that input exists and is patched.
    #[must_use]
    pub fn input_source(&self, number: u16) -> Option<&str> {
        self.inputs
            .iter()
            .find(|input| input.number == number)
            .and_then(MixerInput::source)
    }

    /// A persisted selection must be one the engine accepts and renders: the
    /// same rule as `SwitcherVerb::SetTransition` (ADR-0112); a stinger must
    /// name a configured media player (§5).
    fn validate_transition(&self, players: &[&str]) -> Result<(), ConfigError> {
        let Some(transition) = &self.transition else {
            return Ok(());
        };
        if !transition.is_valid() {
            return Err(ConfigError::Validation(format!(
                "switcher M/E {:?} transition has an out-of-range parameter \
                 (dip rate >= 2 and switch point in (0, 1); wipe numbers in [0, 1]; \
                 stinger trigger_point + mix_rate <= clip_duration and every count <= 1000)",
                self.id
            )));
        }
        if !transition.is_rendered() {
            return Err(ConfigError::Validation(format!(
                "switcher M/E {:?} transition is unsupported: this engine renders cut, mix, \
                 dip to a colour, wipe without flip_flop or border_fill_source, and stinger",
                self.id
            )));
        }
        if let TransitionDescriptor::Stinger { player, .. } = transition {
            if !players.contains(&player.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "switcher M/E {:?} stinger names {:?}, which is not a configured media player",
                    self.id,
                    player.as_str()
                )));
            }
        }
        Ok(())
    }

    fn validate_inputs(&self, sources: &[&str]) -> Result<(), ConfigError> {
        let err = |detail: String| {
            Err(ConfigError::Validation(format!(
                "switcher M/E {:?} {detail}",
                self.id
            )))
        };
        if self.inputs.is_empty() {
            if self.program_input.is_some() || self.preview_input.is_some() {
                return err("declares program_input/preview_input without any inputs".into());
            }
            return Ok(());
        }
        // Source ids may contain ':' (only non-empty + unique is enforced), and
        // the engine spells a mixer-input reference `in:<n>`. With inputs
        // declared a source id with that prefix would be ambiguous: refuse it.
        if let Some(clash) = sources.iter().find(|id| id.starts_with(INPUT_REF_PREFIX)) {
            return err(format!(
                "declares inputs, so no source id may start with {INPUT_REF_PREFIX:?} (found {clash:?})"
            ));
        }
        if self.inputs.len() > usize::from(MAX_MIXER_INPUTS) {
            return err(format!("declares more than {MAX_MIXER_INPUTS} inputs"));
        }
        let mut seen = HashSet::with_capacity(self.inputs.len());
        for input in &self.inputs {
            if input.number == 0 || input.number > MAX_MIXER_INPUTS {
                return err(format!(
                    "input number {} is outside 1..={MAX_MIXER_INPUTS}",
                    input.number
                ));
            }
            if !seen.insert(input.number) {
                return err(format!("declares input {} twice", input.number));
            }
            if let Some(label) = &input.label {
                if label.chars().count() > MAX_MIXER_INPUT_LABEL
                    || label.chars().any(char::is_control)
                {
                    return err(format!(
                        "input {} label must be at most {MAX_MIXER_INPUT_LABEL} characters without control characters",
                        input.number
                    ));
                }
            }
            if let Some(source) = input.source() {
                if !sources.contains(&source) {
                    return err(format!(
                        "input {} is patched to undeclared source {source:?}",
                        input.number
                    ));
                }
            }
        }
        let (Some(program_input), Some(preview_input)) = (self.program_input, self.preview_input)
        else {
            return err("declares inputs, so program_input and preview_input are required".into());
        };
        if program_input == preview_input {
            return err("program_input and preview_input must differ".into());
        }
        for (bus, number, configured) in [
            ("program_input", program_input, &self.program_source),
            ("preview_input", preview_input, &self.preview_source),
        ] {
            if !seen.contains(&number) {
                return err(format!("{bus} {number} is not a declared input"));
            }
            let Some(patched) = self.input_source(number) else {
                return err(format!("{bus} {number} is not patched to a source"));
            };
            if patched != configured {
                return err(format!(
                    "{bus} {number} is patched to {patched:?} but the bus source is {configured:?}"
                ));
            }
        }
        Ok(())
    }
}

impl SwitcherConfig {
    /// Validate references and the first-slice N=1 admission boundary.
    /// `players` are the configured media-player ids a stinger may name.
    ///
    /// # Errors
    /// Invalid M/E names, missing sources/programs/players, or unsupported
    /// N>1 fail closed instead of silently selecting another scene or program.
    pub fn validate(
        &self,
        sources: &[&str],
        programs: &[&str],
        players: &[&str],
    ) -> Result<(), ConfigError> {
        if self.mix_effects.len() > 1 {
            return Err(ConfigError::Validation(
                "first-slice switcher supports exactly one M/E; multi-M/E is not active".into(),
            ));
        }
        for me in &self.mix_effects {
            if me.id != "me1" {
                return Err(ConfigError::Validation(format!(
                    "switcher M/E {:?} is unsupported; expected me1",
                    me.id
                )));
            }
            if !programs.contains(&me.program.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "switcher M/E {:?} references undeclared program {:?}",
                    me.id, me.program
                )));
            }
            for (bus, source) in [
                ("program_source", &me.program_source),
                ("preview_source", &me.preview_source),
            ] {
                if !sources.contains(&source.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "switcher M/E {:?} {bus} references undeclared source {:?}",
                        me.id, source
                    )));
                }
            }
            me.validate_inputs(sources)?;
            me.validate_transition(players)?;
        }
        Ok(())
    }

    /// Validate the downstream keyers against the declared sources, programs
    /// and the canvas, and admit their summed cost against
    /// `keyer_budget` (ADR-0113 §9, reserved at definition).
    ///
    /// # Errors
    /// A hard limit, a bad reference, a refused-by-name feature
    /// (`premultiplied`, a chroma key with a key source), a geometry out of bounds or a document over budget.
    pub fn validate_keyers(
        &self,
        sources: &[&str],
        programs: &[&str],
        canvas_w: u32,
        canvas_h: u32,
    ) -> Result<(), ConfigError> {
        if self.downstream_keyers.is_empty() {
            return Ok(());
        }
        if self.downstream_keyers.len() > MAX_DOWNSTREAM_KEYERS {
            return Err(ConfigError::Validation(format!(
                "a program has at most {MAX_DOWNSTREAM_KEYERS} downstream keyers"
            )));
        }
        let mut ids = HashSet::with_capacity(self.downstream_keyers.len());
        let mut zs = HashSet::with_capacity(self.downstream_keyers.len());
        for dsk in &self.downstream_keyers {
            dsk.validate(sources, programs, canvas_w, canvas_h)?;
            // Review F3: a DSK is keyed on its program's M/E drive; without
            // that M/E boot would install nothing (no `on_at_start`, leases
            // held for nothing, Clean refused for nothing). Refuse it.
            if !self.mix_effects.iter().any(|me| me.program == dsk.program) {
                return Err(ConfigError::Validation(format!(
                    "downstream keyer {:?} needs a [[switcher.mix_effects]] on program {:?} \
                     (dsk-requires-mix-effects)",
                    dsk.id, dsk.program
                )));
            }
            if !ids.insert((dsk.program.as_str(), dsk.id.as_str())) {
                return Err(ConfigError::Validation(format!(
                    "downstream keyer {:?} is declared twice",
                    dsk.id
                )));
            }
            if !zs.insert((dsk.program.as_str(), dsk.z)) {
                return Err(ConfigError::Validation(format!(
                    "downstream keyer {:?} shares z = {} with another keyer",
                    dsk.id, dsk.z
                )));
            }
        }
        let budget = self.keyer_budget.unwrap_or(DEFAULT_KEYER_BUDGET);
        if !budget.is_finite() || budget < 0.0 {
            return Err(ConfigError::Validation(format!(
                "switcher.keyer_budget {budget} must be finite and >= 0"
            )));
        }
        let total: f64 = self
            .downstream_keyers
            .iter()
            .map(|dsk| dsk.cost(canvas_w, canvas_h))
            .sum();
        if total > f64::from(budget) + BUDGET_EPSILON {
            return Err(ConfigError::Validation(format!(
                "downstream keyers cost {total:.3} canvas folds, over switcher.keyer_budget {budget}"
            )));
        }
        Ok(())
    }

    /// Whether this document defines at least one downstream keyer.
    #[must_use]
    pub fn defines_dsk(&self) -> bool {
        !self.downstream_keyers.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SOURCES: &[&str] = &["abc-news", "vt1", "bars"];
    const PLAYERS: &[&str] = &["mp1"];

    fn input(number: u16, source: Option<&str>) -> MixerInput {
        MixerInput {
            number,
            label: None,
            video: source.map(|s| MixerInputVideo::Source(s.into())),
        }
    }

    fn me() -> MixEffectsConfig {
        MixEffectsConfig {
            id: "me1".into(),
            program: "main".into(),
            program_source: "abc-news".into(),
            preview_source: "vt1".into(),
            flip_flop: true,
            inputs: vec![
                input(1, Some("abc-news")),
                input(2, Some("vt1")),
                input(3, None),
            ],
            program_input: Some(1),
            preview_input: Some(2),
            transition: None,
        }
    }

    fn check(me: MixEffectsConfig) -> Result<(), ConfigError> {
        SwitcherConfig {
            mix_effects: vec![me],
            downstream_keyers: vec![],
            keyer_budget: None,
            extra: serde_json::Map::new(),
        }
        .validate(SOURCES, &["main"], PLAYERS)
    }

    fn refused(me: MixEffectsConfig, needle: &str) {
        let error = check(me).expect_err("must be refused").to_string();
        assert!(error.contains(needle), "{error:?} lacks {needle:?}");
    }

    #[test]
    fn declared_inputs_validate() {
        check(me()).unwrap();
    }

    #[test]
    fn absent_inputs_keep_legacy_rules_and_refuse_input_numbers() {
        let mut legacy = me();
        legacy.inputs.clear();
        legacy.program_input = None;
        legacy.preview_input = None;
        check(legacy.clone()).unwrap();
        legacy.program_input = Some(1);
        refused(legacy, "without any inputs");
    }

    #[test]
    fn input_numbers_are_bounded_and_unique() {
        let mut zero = me();
        zero.inputs.push(input(0, None));
        refused(zero, "outside 1..=64");
        let mut high = me();
        high.inputs.push(input(65, None));
        refused(high, "outside 1..=64");
        let mut dup = me();
        dup.inputs.push(input(2, None));
        refused(dup, "input 2 twice");
        let mut many = me();
        many.inputs = (1..=65).map(|n| input(n, None)).collect();
        refused(many, "more than 64 inputs");
    }

    #[test]
    fn patched_sources_must_be_declared() {
        let mut bad = me();
        bad.inputs[2] = input(3, Some("ghost"));
        refused(bad, "undeclared source \"ghost\"");
    }

    #[test]
    fn labels_are_bounded_without_control_characters() {
        let mut long = me();
        long.inputs[0].label = Some("x".repeat(65));
        refused(long, "label");
        let mut control = me();
        control.inputs[0].label = Some("a\nb".into());
        refused(control, "label");
        let mut ok = me();
        ok.inputs[0].label = Some("x".repeat(64));
        check(ok).unwrap();
    }

    #[test]
    fn bus_inputs_are_required_declared_patched_distinct_and_consistent() {
        let mut missing = me();
        missing.preview_input = None;
        refused(missing, "required");
        let mut same = me();
        same.preview_input = Some(1);
        refused(same, "must differ");
        let mut undeclared = me();
        undeclared.preview_input = Some(9);
        refused(undeclared, "not a declared input");
        let mut unpatched = me();
        unpatched.preview_input = Some(3);
        refused(unpatched, "not patched");
        let mut mismatch = me();
        mismatch.program_source = "bars".into();
        refused(mismatch, "is patched to \"abc-news\"");
    }

    #[test]
    fn input_reference_prefix_is_reserved_when_inputs_are_declared() {
        let error = SwitcherConfig {
            mix_effects: vec![me()],
            downstream_keyers: vec![],
            keyer_budget: None,
            extra: serde_json::Map::new(),
        }
        .validate(&["abc-news", "vt1", "in:1"], &["main"], &[])
        .expect_err("ambiguous id")
        .to_string();
        assert!(error.contains("in:"), "{error}");
    }

    #[test]
    fn toml_round_trip_preserves_inputs() {
        let text = r#"
id = "me1"
program = "main"
program_source = "abc-news"
preview_source = "vt1"
program_input = 1
preview_input = 2
[[inputs]]
number = 1
label = "Source 1"
video = { source = "abc-news" }
[[inputs]]
number = 2
video = { source = "vt1" }
[[inputs]]
number = 3
"#;
        let parsed: MixEffectsConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.inputs.len(), 3);
        assert_eq!(parsed.input_source(1), Some("abc-news"));
        assert_eq!(parsed.inputs[0].label.as_deref(), Some("Source 1"));
        assert_eq!(parsed.input_source(3), None);
        let again: MixEffectsConfig = toml::from_str(&toml::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
        // Layout/bus patches are a later slice: refused at parse time.
        let layout = text.replace("video = { source = \"vt1\" }", "video = { layout = \"x\" }");
        assert!(toml::from_str::<MixEffectsConfig>(&layout).is_err());
        // Without inputs nothing new is serialized (byte-compatible documents).
        let mut legacy = parsed;
        legacy.inputs.clear();
        legacy.program_input = None;
        legacy.preview_input = None;
        let out = toml::to_string(&legacy).unwrap();
        assert!(!out.contains("input"), "{out}");
    }

    #[test]
    fn a_selected_transition_round_trips_through_toml() {
        let mut selected = me();
        selected.transition = Some(
            serde_json::from_value(serde_json::json!({
                "kind": "wipe", "pattern": "circle", "rate": 25, "softness": 0.1,
                "border_width": 0.02, "border_color": "#ff8000", "reverse": true,
                "position_xy": [0.25, 0.75]
            }))
            .unwrap(),
        );
        check(selected.clone()).unwrap();
        let text = toml::to_string(&selected).unwrap();
        let again: MixEffectsConfig = toml::from_str(&text).unwrap();
        assert_eq!(again, selected, "{text}");
        let mut dip = me();
        dip.transition = Some(
            serde_json::from_value(serde_json::json!({
                "kind": "dip", "rate": 30, "color": "#ffffff",
                "switch_point": {"num": 1, "den": 3}
            }))
            .unwrap(),
        );
        let text = toml::to_string(&dip).unwrap();
        assert_eq!(toml::from_str::<MixEffectsConfig>(&text).unwrap(), dip);
        // No selection serializes nothing (byte-compatible documents).
        assert!(!toml::to_string(&me()).unwrap().contains("transition"));
    }

    #[test]
    fn an_invalid_or_unrendered_transition_is_refused() {
        for (value, needle) in [
            (
                serde_json::json!({"kind": "dip", "rate": 1}),
                "out-of-range",
            ),
            (
                serde_json::json!({"kind": "dip", "rate": 10, "switch_point": {"num": 1, "den": 1}}),
                "out-of-range",
            ),
            (
                serde_json::json!({"kind": "wipe", "pattern": "box", "rate": 10, "softness": 1.5}),
                "out-of-range",
            ),
            (
                serde_json::json!({"kind": "wipe", "pattern": "box", "rate": 10, "flip_flop": true}),
                "unsupported",
            ),
            (
                serde_json::json!({"kind": "wipe", "pattern": "box", "rate": 10, "border_fill_source": "vt1"}),
                "unsupported",
            ),
            (
                serde_json::json!({"kind": "dip", "rate": 10, "dip_source": "vt1"}),
                "unsupported",
            ),
            (
                serde_json::json!({"kind": "stinger", "player": "mp1", "pre_roll": 1,
                    "clip_duration": 30, "trigger_point": 25, "mix_rate": 10}),
                "out-of-range",
            ),
            (
                serde_json::json!({"kind": "stinger", "player": "mp1", "pre_roll": 1,
                    "clip_duration": 1001, "trigger_point": 5, "mix_rate": 10}),
                "out-of-range",
            ),
        ] {
            let mut bad = me();
            bad.transition = Some(serde_json::from_value(value).unwrap());
            refused(bad, needle);
        }
        // An unknown kind or field never parses.
        assert!(toml::from_str::<MixEffectsConfig>(
            "id = \"me1\"\nprogram = \"main\"\nprogram_source = \"a\"\npreview_source = \"b\"\n[transition]\nkind = \"spin\"\n"
        )
        .is_err());
    }

    /// ADR-0112 §5: a persisted stinger loads when it names a configured
    /// media player and round-trips; an unknown player is refused.
    #[test]
    fn a_persisted_stinger_must_name_a_configured_media_player() {
        let stinger = |player: &str| {
            let mut me = me();
            me.transition = Some(
                serde_json::from_value(serde_json::json!({
                    "kind": "stinger", "player": player, "pre_roll": 3,
                    "clip_duration": 40, "trigger_point": 10, "mix_rate": 1
                }))
                .unwrap(),
            );
            me
        };
        check(stinger("mp1")).unwrap();
        let text = toml::to_string(&stinger("mp1")).unwrap();
        assert_eq!(
            toml::from_str::<MixEffectsConfig>(&text).unwrap(),
            stinger("mp1")
        );
        refused(stinger("vt1"), "not a configured media player");
        refused(stinger("ghost"), "not a configured media player");
    }

    /// MED-P3-4: a persisted selection with an unknown field, a field of
    /// another variant or an unknown nested field is refused at load.
    #[test]
    fn a_persisted_transition_with_an_unknown_field_is_refused_at_load() {
        let head = "id = \"me1\"\nprogram = \"main\"\nprogram_source = \"a\"\npreview_source = \"b\"\n[transition]\n";
        for body in [
            "kind = \"mix\"\nrate = 10\nreverse = true\n",
            "kind = \"cut\"\nrate = 10\n",
            "kind = \"dip\"\nrate = 10\npattern = \"box\"\n",
            "kind = \"wipe\"\npattern = \"box\"\nrate = 10\ncolor = \"#ffffff\"\n",
            "kind = \"dip\"\nrate = 10\nswitch_point = { num = 1, den = 2, x = 0 }\n",
        ] {
            let error = toml::from_str::<MixEffectsConfig>(&format!("{head}{body}"));
            assert!(error.is_err(), "{body}");
        }
        // The known shapes still load.
        assert!(toml::from_str::<MixEffectsConfig>(&format!(
            "{head}kind = \"dip\"\nrate = 10\nswitch_point = {{ num = 1, den = 3 }}\n"
        ))
        .is_ok());
    }

    // ---- Downstream keyers (ADR-0113, K2) ----

    const CANVAS: (u32, u32) = (1920, 1080);

    fn dsk(id: &str, z: i32) -> DownstreamKeyerConfig {
        DownstreamKeyerConfig {
            id: id.into(),
            program: "main".into(),
            fill: "bars".into(),
            key: None,
            key_type: KeyerType::Linear,
            clip: 0.0,
            gain: 1.0,
            color: None,
            invert: false,
            mask: None,
            fly: None,
            max_source: None,
            rate: DEFAULT_KEYER_RATE,
            tie: false,
            on_at_start: false,
            z,
            loss: KeyerLossPolicy::DropToOffAir,
            loss_trigger: KeyerLossTrigger::Reconnecting,
            premultiplied: false,
        }
    }

    fn keyers(list: Vec<DownstreamKeyerConfig>, budget: Option<f32>) -> Result<(), ConfigError> {
        SwitcherConfig {
            mix_effects: vec![me()],
            downstream_keyers: list,
            keyer_budget: budget,
            extra: serde_json::Map::new(),
        }
        .validate_keyers(SOURCES, &["main"], CANVAS.0, CANVAS.1)
    }

    fn keyer_refused(list: Vec<DownstreamKeyerConfig>, needle: &str) {
        let error = keyers(list, None).expect_err("must be refused").to_string();
        assert!(error.contains(needle), "{error:?} lacks {needle:?}");
    }

    #[test]
    fn default_budget_is_one_full_canvas_dsk_with_fill_and_key() {
        let mut one = dsk("dsk1", 1);
        one.key = Some("vt1".into());
        let cost = one.cost(CANVAS.0, CANVAS.1);
        assert!(
            (cost - f64::from(DEFAULT_KEYER_BUDGET)).abs() < 1e-9,
            "{cost}"
        );
        assert!(
            (DSK_FOLD + 2.0 * KEYER_UPLOAD_WEIGHT - f64::from(DEFAULT_KEYER_BUDGET)).abs() < 1e-9
        );
        keyers(vec![one], None).unwrap();
        keyers(vec![dsk("dsk1", 1)], None).unwrap();
        keyer_refused(
            vec![dsk("dsk1", 1), dsk("dsk2", 2)],
            "over switcher.keyer_budget",
        );
        // An explicit larger budget admits two.
        keyers(vec![dsk("dsk1", 1), dsk("dsk2", 2)], Some(4.0)).unwrap();
    }

    #[test]
    fn hard_limits_and_uniqueness() {
        let three = vec![dsk("a", 1), dsk("b", 2), dsk("c", 3)];
        assert!(keyers(three, Some(100.0))
            .unwrap_err()
            .to_string()
            .contains("at most 2"));
        let dup_id = keyers(vec![dsk("a", 1), dsk("a", 2)], Some(100.0)).unwrap_err();
        assert!(dup_id.to_string().contains("twice"), "{dup_id}");
        let dup_z = keyers(vec![dsk("a", 1), dsk("b", 1)], Some(100.0)).unwrap_err();
        assert!(dup_z.to_string().contains("z = 1"), "{dup_z}");
    }

    #[test]
    fn references_are_declared_sources_never_inputs() {
        let mut ghost = dsk("dsk1", 1);
        ghost.fill = "ghost".into();
        keyer_refused(vec![ghost], "undeclared source \"ghost\"");
        let mut input = dsk("dsk1", 1);
        input.key = Some("in:1".into());
        keyer_refused(vec![input], "mixer input");
        let mut program = dsk("dsk1", 1);
        program.program = "other".into();
        keyer_refused(vec![program], "undeclared program");
    }

    #[test]
    fn premultiplied_is_refused_by_name() {
        let mut shaped = dsk("dsk1", 1);
        shaped.premultiplied = true;
        keyer_refused(vec![shaped], "premultiplied");
    }

    fn chroma(color: Option<&str>) -> DownstreamKeyerConfig {
        let mut k = dsk("dsk1", 1);
        k.key_type = KeyerType::Chroma;
        k.color = color.map(str::to_owned);
        k.clip = 0.05;
        k.gain = 8.0;
        k
    }

    #[test]
    fn a_chroma_key_with_a_reference_colour_is_accepted() {
        keyers(vec![chroma(Some("#00b140"))], None).unwrap();
        keyers(vec![chroma(Some("#0f0"))], None).unwrap();
        let mut masked = chroma(Some("#00FF00"));
        masked.invert = true;
        masked.mask = Some(KeyerRect {
            x: 0,
            y: 0,
            w: 960,
            h: 540,
        });
        keyers(vec![masked.clone()], None).unwrap();
        // The K4 per-definition check is the same rule.
        masked
            .validate_definition(SOURCES, CANVAS.0, CANVAS.1)
            .unwrap();
        // Same cost as a self linear key (no key source, ADR-0113 §9).
        assert!(
            (masked.cost(CANVAS.0, CANVAS.1) - dsk("x", 1).cost(CANVAS.0, CANVAS.1)).abs() < 1e-12
        );
    }

    #[test]
    fn a_chroma_key_is_refused_with_typed_messages() {
        keyer_refused(vec![chroma(None)], "chroma-color-required");
        keyer_refused(vec![chroma(Some("green"))], "chroma-color");
        keyer_refused(vec![chroma(Some("#00b14"))], "chroma-color");
        let mut sourced = chroma(Some("#00b140"));
        sourced.key = Some("vt1".into());
        keyer_refused(vec![sourced], "chroma-key-source");
        let mut stray = dsk("dsk1", 1);
        stray.color = Some("#00b140".into());
        keyer_refused(vec![stray], "chroma-color-without-chroma");
        for (clip, gain, needle) in [
            (f32::NAN, 1.0, "clip"),
            (-0.1, 1.0, "clip"),
            (1.5, 1.0, "clip"),
            (0.1, f32::INFINITY, "gain"),
            (0.1, -1.0, "gain"),
            (0.1, f32::NAN, "gain"),
        ] {
            let mut k = chroma(Some("#00b140"));
            k.clip = clip;
            k.gain = gain;
            keyer_refused(vec![k], needle);
        }
    }

    #[test]
    fn a_chroma_key_round_trips_through_toml() {
        let doc = "id = \"dsk1\"\nprogram = \"main\"\nfill = \"bars\"\ntype = \"chroma\"\n\
                   color = \"#00b140\"\nclip = 0.05\ngain = 8.0\n";
        let k: DownstreamKeyerConfig = toml::from_str(doc).unwrap();
        assert_eq!(k.key_type, KeyerType::Chroma);
        assert_eq!(k.color.as_deref(), Some("#00b140"));
        let back: DownstreamKeyerConfig = toml::from_str(&toml::to_string(&k).unwrap()).unwrap();
        assert_eq!(back, k);
        keyers(vec![k], None).unwrap();
    }

    #[test]
    fn rate_clip_and_gain_are_bounded() {
        for (rate, ok) in [(0, false), (1, true), (300, true), (301, false)] {
            let mut k = dsk("dsk1", 1);
            k.rate = rate;
            assert_eq!(keyers(vec![k], None).is_ok(), ok, "rate {rate}");
        }
        let mut clip = dsk("dsk1", 1);
        clip.clip = 1.5;
        keyer_refused(vec![clip], "clip");
        let mut gain = dsk("dsk1", 1);
        gain.gain = f32::NAN;
        keyer_refused(vec![gain], "gain");
        let mut negative = dsk("dsk1", 1);
        negative.gain = -1.0;
        keyer_refused(vec![negative], "gain");
    }

    #[test]
    fn fly_is_even_large_enough_and_inside_the_canvas() {
        let rect = |x, y, w, h| Some(KeyerRect { x, y, w, h });
        let mut odd = dsk("dsk1", 1);
        odd.fly = rect(1, 0, 64, 64);
        keyer_refused(vec![odd], "even");
        let mut small = dsk("dsk1", 1);
        small.fly = rect(0, 0, 14, 64);
        keyer_refused(vec![small], "at least 16x16");
        let mut outside = dsk("dsk1", 1);
        outside.fly = rect(1900, 0, 64, 64);
        keyer_refused(vec![outside], "outside");
        let mut ok = dsk("dsk1", 1);
        ok.fly = rect(1280, 60, 576, 324);
        keyers(vec![ok], None).unwrap();
    }

    #[test]
    fn mask_lies_inside_max_source() {
        let mut k = dsk("dsk1", 1);
        k.max_source = Some(KeyerSize { w: 640, h: 360 });
        k.mask = Some(KeyerRect {
            x: 600,
            y: 0,
            w: 64,
            h: 64,
        });
        keyer_refused(vec![k.clone()], "mask");
        k.mask = Some(KeyerRect {
            x: 0,
            y: 0,
            w: 640,
            h: 360,
        });
        keyers(vec![k], None).unwrap();
    }

    #[test]
    fn toml_round_trip_and_refused_unknown_fields() {
        let text = r#"
[[mix_effects]]
id = "me1"
program = "main"
program_source = "abc-news"
preview_source = "vt1"
[[downstream_keyers]]
id = "dsk1"
program = "main"
fill = "bars"
key = "vt1"
type = "luma"
clip = 0.1
gain = 4.0
fly = { x = 1280, y = 60, w = 576, h = 324 }
rate = 20
tie = true
loss = "hold_last_good"
loss_trigger = "stale"
z = 1
[future_aux]
kept = true
"#;
        let parsed: SwitcherConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.downstream_keyers.len(), 1);
        let k = &parsed.downstream_keyers[0];
        assert_eq!(k.key_type, KeyerType::Luma);
        assert_eq!(k.loss, KeyerLossPolicy::HoldLastGood);
        assert_eq!(k.loss_trigger, KeyerLossTrigger::Stale);
        assert!(
            !parsed.extra.contains_key("downstream_keyers"),
            "typed, not extra"
        );
        assert!(
            parsed.extra.contains_key("future_aux"),
            "legacy extra survives"
        );
        let again: SwitcherConfig = toml::from_str(&toml::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(again, parsed);
        let unknown = text.replace("z = 1", "z = 1\nborder = 2");
        assert!(toml::from_str::<SwitcherConfig>(&unknown).is_err());
        // Defaults: linear, rate 15, drop_to_off_air on reconnecting.
        let minimal: DownstreamKeyerConfig =
            toml::from_str("id = \"d\"\nprogram = \"main\"\nfill = \"bars\"\n").unwrap();
        assert_eq!(minimal.key_type, KeyerType::Linear);
        assert_eq!(minimal.rate, DEFAULT_KEYER_RATE);
        assert_eq!(minimal.loss, KeyerLossPolicy::DropToOffAir);
        assert_eq!(minimal.loss_trigger, KeyerLossTrigger::Reconnecting);
        // No keyers: nothing new is serialized.
        let mut legacy = parsed;
        legacy.downstream_keyers.clear();
        let out = toml::to_string(&legacy).unwrap();
        assert!(
            !out.contains("downstream_keyers") && !out.contains("keyer_budget"),
            "{out}"
        );
    }
}
