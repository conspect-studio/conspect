//! Declarative M/E first slice (ADR-M012). The remaining keyer, aux and
//! composition domains remain in `extra` for lossless forward migration; they
//! are not reported as active by the runtime until their implementations land.
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

fn default_flip_flop() -> bool {
    true
}

/// A declared `[switcher]` root. An absent block preserves legacy behaviour.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SwitcherConfig {
    /// M/E stages. Empty means no switcher runtime or preview bus exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mix_effects: Vec<MixEffectsConfig>,
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
    /// same rule as `SwitcherVerb::SetTransition` (ADR-0112).
    fn validate_transition(&self) -> Result<(), ConfigError> {
        let Some(transition) = &self.transition else {
            return Ok(());
        };
        if !transition.is_valid() {
            return Err(ConfigError::Validation(format!(
                "switcher M/E {:?} transition has an out-of-range parameter \
                 (dip rate >= 2 and switch point in (0, 1); wipe numbers in [0, 1])",
                self.id
            )));
        }
        if !transition.is_rendered() {
            return Err(ConfigError::Validation(format!(
                "switcher M/E {:?} transition is unsupported: this engine renders cut, mix, \
                 dip to a colour and wipe without flip_flop or border_fill_source",
                self.id
            )));
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
    ///
    /// # Errors
    /// Invalid M/E names, missing sources/programs, or unsupported N>1 fail
    /// closed instead of silently selecting another scene or program.
    pub fn validate(&self, sources: &[&str], programs: &[&str]) -> Result<(), ConfigError> {
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
            me.validate_transition()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const SOURCES: &[&str] = &["abc-news", "vt1", "bars"];

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
            extra: serde_json::Map::new(),
        }
        .validate(SOURCES, &["main"])
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
            extra: serde_json::Map::new(),
        }
        .validate(&["abc-news", "vt1", "in:1"], &["main"])
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
                    "clip_duration": 30, "trigger_point": 5, "mix_rate": 10}),
                "unsupported",
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
}
