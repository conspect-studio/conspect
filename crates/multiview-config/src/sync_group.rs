//! Sync-group configuration (config-as-code): named presentation-sync groups
//! over managed devices (ADR-M008 + ADR-M010; managed-devices brief §8).
//!
//! A [`SyncGroup`] declares which devices present together and how tightly:
//! the achieved tier is computed at runtime as the **weakest member's** tier
//! and is never over-claimed; each member carries an `offset_ms` presentation
//! trim with AES67-link-offset semantics applied to video — **uniformity
//! across members is the goal, not smallness**. `target_skew_ms` is the
//! drift-alarm threshold: a member drifting beyond it past a dwell raises a
//! `degraded-sync` warning on the Alarms surface. Achieved skew is runtime
//! state and has no representation here, so it is never exported.

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};

/// The inclusive ceiling (ten seconds) for `target_skew_ms` and per-member
/// `offset_ms` — beyond it the value is a typo, not a sync policy (Tier-C
/// devices drift by at most ±100–500 ms; link offsets run 100–300 ms).
const MAX_SKEW_MS: u32 = 10_000;

/// How a sync group claims its achieved tier.
///
/// `#[non_exhaustive]`: further modes (e.g. operator-pinned tiers) arrive
/// with their own ADR; downstream matches carry a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SyncGroupMode {
    /// Claim the **weakest member's** achieved tier, computed at runtime and
    /// displayed immediately — never over-claimed (ADR-M010). The default.
    #[default]
    Auto,
}

/// One device's membership in a sync group, with its presentation trim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SyncMember {
    /// The managed device id (must reference a declared `[[devices]]` entry;
    /// resolution is enforced by [`crate::MultiviewConfig::validate`]).
    pub device: String,
    /// Additional presentation delay for this member in milliseconds
    /// (`0..=10_000`). AES67-link-offset semantics applied to video: the trim
    /// equalizes members' end-to-end delay, so uniformity — not smallness —
    /// is what matters. Applying a change is Class-1 (members trim their
    /// presentation buffers at a frame boundary; the engine's output cadence
    /// is untouched). Defaults to `0`.
    #[serde(default)]
    pub offset_ms: u32,
}

/// A named presentation-sync group over managed devices — a first-class
/// versioned resource (ADR-M008), seeded into the control plane from config
/// exactly as devices are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SyncGroup {
    /// Stable sync-group id (unique within the document).
    pub id: String,
    /// How the group claims its achieved tier. Defaults to
    /// [`SyncGroupMode::Auto`] (weakest member).
    #[serde(default)]
    pub mode: SyncGroupMode,
    /// The drift-alarm threshold in milliseconds (`1..=10_000`): a member
    /// drifting beyond it past a dwell raises a `degraded-sync` warning.
    pub target_skew_ms: u32,
    /// The member devices with their per-member trims. A device may belong
    /// to **at most one** group (enforced by
    /// [`crate::MultiviewConfig::validate`]); a group must have at least one
    /// member. `cast` devices cannot be members: Cast is Tier D — seconds of
    /// receiver-side buffering with no sync surface, never a sync
    /// participant (ADR-M011).
    pub members: Vec<SyncMember>,
}

impl SyncGroup {
    /// Validate this group's per-item semantics — the same checks
    /// [`crate::MultiviewConfig::validate`] applies per group: non-empty id,
    /// `target_skew_ms` in `1..=10_000`, at least one member, no member
    /// listed twice, member offsets in `0..=10_000`. Document-level rules
    /// (group-id uniqueness, member device resolution, one-group-per-device)
    /// remain on the document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a sync group has an empty id".to_owned(),
            ));
        }
        if self.target_skew_ms == 0 {
            return Err(ConfigError::Validation(format!(
                "sync group {:?}: target_skew_ms must be >= 1 (zero skew is not measurable, \
                 let alone achievable)",
                self.id
            )));
        }
        if self.target_skew_ms > MAX_SKEW_MS {
            return Err(ConfigError::Validation(format!(
                "sync group {:?}: target_skew_ms ({}) exceeds the {MAX_SKEW_MS} ms ceiling",
                self.id, self.target_skew_ms
            )));
        }
        if self.members.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sync group {:?} has no members",
                self.id
            )));
        }
        let mut seen: std::collections::HashSet<&str> =
            std::collections::HashSet::with_capacity(self.members.len());
        for member in &self.members {
            if member.device.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "sync group {:?} has a member with an empty device id",
                    self.id
                )));
            }
            if !seen.insert(member.device.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "sync group {:?} lists device {:?} more than once",
                    self.id, member.device
                )));
            }
            if member.offset_ms > MAX_SKEW_MS {
                return Err(ConfigError::Validation(format!(
                    "sync group {:?}: member {:?} offset_ms ({}) exceeds the {MAX_SKEW_MS} ms \
                     ceiling",
                    self.id, member.device, member.offset_ms
                )));
            }
        }
        Ok(())
    }
}
