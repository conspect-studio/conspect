//! Declarative M/E first slice (ADR-M012). The remaining keyer, aux and
//! composition domains remain in `extra` for lossless forward migration; they
//! are not reported as active by the runtime until their implementations land.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// One mix/effects stage inside a program. The first runtime admits exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixEffectsConfig {
    /// Unique M/E id, currently `me1`.
    pub id: String,
    /// Parent program, currently `main` or the sole declared program.
    pub program: String,
    /// Source id shown on PGM when starting (a declared source).
    pub program_source: String,
    /// Source id armed on PVW when starting (a declared source).
    pub preview_source: String,
    /// Swap previous PGM onto PVW after a completed take.
    #[serde(default = "default_flip_flop")]
    pub flip_flop: bool,
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
        }
        Ok(())
    }
}
