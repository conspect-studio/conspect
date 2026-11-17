//! `[cast]`: engine-wide settings of the cast transport ladder (ADR-0114).
//!
//! ```toml
//! [cast]
//! ladder = ["hls"]
//! ```
//!
//! `ladder` names the rungs the engine may offer a receiver on
//! `POST /cast/<token>/ladder`, best first. Only `hls` exists today
//! (ADR-0114 slice 2); `moq`, `webrtc` and `srt` are reserved names that a
//! later slice enables, and are refused until then so that a config never
//! names a rung the build cannot serve. `hls` is the floor of every ladder
//! and must be listed: a receiver always falls back to it.
//!
//! Absent ⇒ [`CastConfig::default`] (`["hls"]`); a default section does not
//! serialize.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The rungs this build can serve.
pub const SERVABLE_RUNGS: &[&str] = &["hls"];
/// Rung names reserved for later slices of ADR-0114.
pub const RESERVED_RUNGS: &[&str] = &["moq", "webrtc", "srt"];

/// See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CastConfig {
    /// The rungs the ladder may offer, best first. Default `["hls"]`.
    #[serde(default = "default_ladder")]
    pub ladder: Vec<String>,
}

fn default_ladder() -> Vec<String> {
    vec!["hls".to_owned()]
}

impl Default for CastConfig {
    fn default() -> Self {
        Self {
            ladder: default_ladder(),
        }
    }
}

impl CastConfig {
    /// A `[cast]` section with this ladder.
    #[must_use]
    pub fn with_ladder(ladder: Vec<String>) -> Self {
        Self { ladder }
    }

    /// Whether this is the default section (not serialized).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the ladder enables `rung`.
    #[must_use]
    pub fn enables(&self, rung: &str) -> bool {
        self.ladder.iter().any(|r| r == rung)
    }

    /// Check the section.
    ///
    /// # Errors
    /// [`ConfigError::Validation`] for an empty list, an unknown, reserved
    /// or repeated rung, or a ladder without `hls`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.ladder.is_empty() {
            return Err(ConfigError::Validation(
                "cast.ladder is empty: it must list at least \"hls\" (the floor every receiver \
                 falls back to)"
                    .to_owned(),
            ));
        }
        for (i, rung) in self.ladder.iter().enumerate() {
            if RESERVED_RUNGS.contains(&rung.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "cast.ladder: the rung {rung:?} is not built yet (ADR-0114); this build \
                     serves {SERVABLE_RUNGS:?}"
                )));
            }
            if !SERVABLE_RUNGS.contains(&rung.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "cast.ladder: unknown rung {rung:?}; this build serves {SERVABLE_RUNGS:?}"
                )));
            }
            if self.ladder.iter().take(i).any(|r| r == rung) {
                return Err(ConfigError::Validation(format!(
                    "cast.ladder lists {rung:?} twice"
                )));
            }
        }
        if !self.enables("hls") {
            return Err(ConfigError::Validation(
                "cast.ladder must list \"hls\" (the floor every receiver falls back to)".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> CastConfig {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn default_is_hls_only_and_valid() {
        let config = CastConfig::default();
        assert_eq!(config.ladder, ["hls"]);
        assert!(config.is_default());
        config.validate().unwrap();
        assert_eq!(parse(""), config);
        assert_eq!(parse("ladder = [\"hls\"]"), config);
    }

    #[test]
    fn refuses_empty_unknown_reserved_repeated_and_missing_hls() {
        for (ladder, needle) in [
            ("[]", "empty"),
            ("[\"hls\", \"rtmp\"]", "unknown rung"),
            ("[\"moq\", \"hls\"]", "not built yet"),
            ("[\"srt\", \"hls\"]", "not built yet"),
            ("[\"hls\", \"hls\"]", "twice"),
        ] {
            let error = parse(&format!("ladder = {ladder}")).validate().unwrap_err();
            assert!(error.to_string().contains(needle), "{ladder}: {error}");
        }
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(toml::from_str::<CastConfig>("ladders = [\"hls\"]").is_err());
    }
}
