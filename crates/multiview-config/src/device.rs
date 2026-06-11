//! Managed-device configuration (config-as-code): the durable desired state
//! for operator-adopted hardware (ADR-M008; managed-devices brief §2, §7.3).
//!
//! A [`Device`] carries **desired state only** — driver, address, write-only
//! credential ref, desired mode, offline-alarm severity, reconnect policy,
//! and display assignment. Runtime state (online state, firmware,
//! temperature, current streams, achieved skew, discovered-but-unadopted
//! inventory, ad-hoc cast sessions) has **no representation in this model**,
//! so a config export can only ever emit what was authored; credentials only
//! ever appear as the [`DeviceAuth::secret_ref`] string, never a resolved
//! secret. Addresses are IPv6-first (ADR-0042): bracketed IPv6 literals lead
//! every example (`http://[fd00:db8::42]`); an IPv4 device address is
//! legacy-interop only.

use std::fmt;

use multiview_core::alarm::PerceivedSeverity;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The reconnect backoff ceiling (one hour) — a larger `max_ms` is a typo,
/// not a policy.
const MAX_RECONNECT_MS: u32 = 3_600_000;

/// The compiled-in device-driver families (ADR-M008 §2.3 — the YAGNI guard).
///
/// A **closed** enum: a new device family is a new variant + driver module +
/// ADR, never a plugin. `#[non_exhaustive]` so adding a family is not a
/// breaking change downstream (matches must carry a wildcard arm); an unknown
/// driver token in a document is rejected at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeviceDriver {
    /// ZowieBox-class network encoder/decoder appliances, managed over the
    /// vendor-published HTTP API (ADR-M009). Addressed directly: requires
    /// [`Device::address`].
    Zowietek,
    /// Our own display nodes. A node authenticates by enrolled keypair
    /// identity and resolves the controller itself, so an authored
    /// [`Device::address`] is optional.
    Displaynode,
    /// Cast media targets. Addressed directly: requires [`Device::address`].
    /// Ad-hoc cast sessions are runtime state and never appear in config.
    Cast,
}

impl DeviceDriver {
    /// Whether this driver requires an authored [`Device::address`].
    ///
    /// `zowietek` and `cast` devices are reached at a fixed address;
    /// `displaynode` binds by enrolled identity (the node finds the
    /// controller), so its address is optional.
    #[must_use]
    pub const fn requires_address(self) -> bool {
        match self {
            Self::Zowietek | Self::Cast => true,
            Self::Displaynode => false,
        }
    }

    /// The driver's serde **wire token** (`"zowietek"` / `"displaynode"` /
    /// `"cast"`) — the exact string the `#[serde(rename_all = "snake_case")]`
    /// derive emits.
    ///
    /// This is the single source of truth for the driver string: the realtime
    /// device events (ADR-RT007) construct their `driver` field **only** from
    /// this method, never from a
    /// hand-typed literal, so a future renamed/added variant cannot drift the
    /// event wire form from the config wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zowietek => "zowietek",
            Self::Displaynode => "displaynode",
            Self::Cast => "cast",
        }
    }
}

impl fmt::Display for DeviceDriver {
    /// Write the driver's config token (`zowietek` / `displaynode` / `cast`) —
    /// the same string as [`DeviceDriver::as_str`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Write-only credential pointer for a device (never plaintext).
///
/// Carries only the **reference** into the secret store (e.g.
/// `op://Site/foyer-decoder/credentials`). The resolved secret never enters
/// the config model, so an export can only ever emit this ref string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DeviceAuth {
    /// A secret reference (e.g. `op://Site/foyer-decoder/credentials`).
    pub secret_ref: String,
}

/// Supervised-reconnect backoff bounds for an unreachable device — the same
/// shape inputs use: exponential backoff from `initial_ms` capped at `max_ms`,
/// with jitter and a circuit breaker (managed-devices brief §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReconnectPolicy {
    /// First-retry delay in milliseconds (`>= 1`; `0` would busy-loop).
    pub initial_ms: u32,
    /// Backoff ceiling in milliseconds (`initial_ms..=3_600_000`).
    pub max_ms: u32,
}

impl ReconnectPolicy {
    /// Validate the backoff bounds: `initial_ms >= 1`,
    /// `initial_ms <= max_ms <= 3_600_000` (one hour).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the out-of-range field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.initial_ms == 0 {
            return Err(ConfigError::Validation(
                "reconnect initial_ms must be >= 1 (0 would retry without backoff)".to_owned(),
            ));
        }
        if self.max_ms < self.initial_ms {
            return Err(ConfigError::Validation(format!(
                "reconnect max_ms ({}) must be >= initial_ms ({})",
                self.max_ms, self.initial_ms
            )));
        }
        if self.max_ms > MAX_RECONNECT_MS {
            return Err(ConfigError::Validation(format!(
                "reconnect max_ms ({}) exceeds the {MAX_RECONNECT_MS} ms (one hour) ceiling",
                self.max_ms
            )));
        }
        Ok(())
    }
}

/// What a display-capable device presents.
///
/// Serialized **externally tagged** — the single map key is the tag, matching
/// the brief's authored shape: `{ program = true }`,
/// `{ output = "out-main" }`, `{ wall_head = "head-l" }`. A one-key table is
/// robust across TOML and JSON; never `untagged` (ADR-0010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DisplayAssign {
    /// Present the program canvas. The payload must be `true` — validation
    /// rejects `{ program = false }`; omit the `display` block to leave the
    /// device unassigned instead.
    Program(bool),
    /// Present a declared output, referenced by its stable output id
    /// ([`crate::schema::Output::id`]).
    Output(String),
    /// Present one head of a declared video wall, referenced by head id
    /// ([`crate::wall::HeadConfig::id`]). Allowed for **any** display-capable
    /// device, including Tier-C vendor decoders (a decoder's HDMI out counts
    /// as a display head — ADR-M009 facet (c)); a sync group containing such
    /// a member simply reports the weakest-member tier, it is never
    /// over-claimed.
    WallHead(String),
}

/// The display facet of a device: what it is assigned to present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DeviceDisplay {
    /// The assignment (program / output ref / wall-head ref). Reference
    /// resolution against the document's outputs and wall heads is enforced
    /// by [`crate::MultiviewConfig::validate`].
    pub assign: DisplayAssign,
}

/// A managed device: operator-adopted hardware as declarative desired state
/// (ADR-M008). Applying a config performs idempotent adoption + convergence;
/// the registry the control plane keeps at runtime is seeded from these
/// entries exactly as sources/outputs are today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Device {
    /// Stable device id (referenced by `[[sync_groups]]` members and by
    /// device-projected sources/outputs via `device_ref`).
    pub id: String,
    /// Human-friendly display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The compiled-in driver family managing this device.
    pub driver: DeviceDriver,
    /// Management address, IPv6-first (e.g. `http://[fd00:db8::42]`).
    /// Required for `zowietek`/`cast`; optional for `displaynode`, whose
    /// enrolled keypair identity locates the controller instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The desired converged work mode (e.g. `"decoder"`). Valid values are
    /// driver-specific; the driver re-converges the device onto this mode
    /// whenever an adopt or reconnect brings it `ONLINE`. Absent ⇒ the device
    /// keeps its current mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_mode: Option<String>,
    /// The X.733 severity of the alarm raised when the device stays offline
    /// beyond the dwell, written as the lowercase device token (`"major"`;
    /// the core `PascalCase` form `"Major"` is tolerated on input). Absent ⇒
    /// no offline alarm; inactive (`"cleared"`) and `"indeterminate"`
    /// severities are rejected — an authored offline alarm must be definite.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "severity_token"
    )]
    pub alarm_on_offline: Option<PerceivedSeverity>,
    /// Write-only credentials (export emits the ref string only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<DeviceAuth>,
    /// Supervised-reconnect backoff bounds. Absent ⇒ the driver's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<ReconnectPolicy>,
    /// The display facet: what a display-capable device presents. Absent ⇒
    /// the device has no display assignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DeviceDisplay>,
}

impl Device {
    /// Validate this device's per-item semantics — the same checks
    /// [`crate::MultiviewConfig::validate`] applies per device: non-empty id,
    /// the driver's address requirement, non-empty optional strings, an
    /// active offline-alarm severity, sane reconnect bounds, and a
    /// well-formed display assignment. Document-level rules (id uniqueness,
    /// output/wall-head reference resolution, sync-group membership) remain
    /// on the document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a device has an empty id".to_owned(),
            ));
        }
        match &self.address {
            Some(address) => {
                if address.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "device {:?} declares an empty address",
                        self.id
                    )));
                }
            }
            None => {
                if self.driver.requires_address() {
                    return Err(ConfigError::Validation(format!(
                        "device {:?}: driver {} requires an address (e.g. \
                         \"http://[fd00:db8::42]\"); only displaynode binds by enrolled \
                         identity without one",
                        self.id, self.driver
                    )));
                }
            }
        }
        if let Some(mode) = &self.desired_mode {
            if mode.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "device {:?} declares an empty desired_mode (omit the field to keep the \
                     device's current mode)",
                    self.id
                )));
            }
        }
        if let Some(severity) = self.alarm_on_offline {
            if !severity.is_active() || severity == PerceivedSeverity::Indeterminate {
                return Err(ConfigError::Validation(format!(
                    "device {:?}: alarm_on_offline must be a definite active severity (warning \
                     / minor / major / critical); omit the field to disable the offline alarm",
                    self.id
                )));
            }
        }
        if let Some(auth) = &self.auth {
            if auth.secret_ref.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "device {:?} declares an empty auth secret_ref",
                    self.id
                )));
            }
        }
        if let Some(reconnect) = &self.reconnect {
            reconnect
                .validate()
                .map_err(|e| ConfigError::Validation(format!("device {:?}: {e}", self.id)))?;
        }
        if let Some(display) = &self.display {
            match &display.assign {
                DisplayAssign::Program(enabled) => {
                    if !enabled {
                        return Err(ConfigError::Validation(format!(
                            "device {:?}: display assign {{ program = false }} is meaningless — \
                             omit the display block to leave the device unassigned",
                            self.id
                        )));
                    }
                }
                DisplayAssign::Output(output) => {
                    if output.is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "device {:?} display assignment references an empty output id",
                            self.id
                        )));
                    }
                }
                DisplayAssign::WallHead(head) => {
                    if head.is_empty() {
                        return Err(ConfigError::Validation(format!(
                            "device {:?} display assignment references an empty wall head id",
                            self.id
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Field-level serde for [`Device::alarm_on_offline`]: the X.733
/// [`PerceivedSeverity`] written as the device-domain lowercase token
/// (`"major"`, the managed-devices brief's vocabulary), tolerating the core
/// type's own `PascalCase` form (`"Major"`, as probe `severity` is authored)
/// on input.
mod severity_token {
    use multiview_core::alarm::PerceivedSeverity;
    use serde::de::{Error as DeError, Unexpected};
    use serde::ser::Error as SerError;
    use serde::{Deserialize, Deserializer, Serializer};

    /// The lowercase device tokens, in the X.733 ascending-urgency order.
    const TOKENS: [&str; 6] = [
        "cleared",
        "indeterminate",
        "warning",
        "minor",
        "major",
        "critical",
    ];

    /// The lowercase token for a severity, or `None` for a variant this map
    /// does not know (`PerceivedSeverity` is `#[non_exhaustive]` in
    /// `multiview-core`, so a variant added there has no device token until
    /// this map learns it).
    const fn token(severity: PerceivedSeverity) -> Option<&'static str> {
        match severity {
            PerceivedSeverity::Cleared => Some("cleared"),
            PerceivedSeverity::Indeterminate => Some("indeterminate"),
            PerceivedSeverity::Warning => Some("warning"),
            PerceivedSeverity::Minor => Some("minor"),
            PerceivedSeverity::Major => Some("major"),
            PerceivedSeverity::Critical => Some("critical"),
            _ => None,
        }
    }

    /// Serialize the severity as its lowercase token (or nothing for `None`;
    /// in practice the field's `skip_serializing_if` elides that case).
    // serde's `with = "module"` contract calls this with the field by
    // reference (`&Option<PerceivedSeverity>`); the derive fixes the
    // signature, so the `Option<&T>` / by-value shapes the lints ask for
    // cannot be used here.
    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<PerceivedSeverity>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_none(),
            Some(severity) => {
                let token = token(*severity).ok_or_else(|| {
                    S::Error::custom(format!(
                        "severity {severity:?} has no device alarm_on_offline token"
                    ))
                })?;
                serializer.serialize_str(token)
            }
        }
    }

    /// Deserialize a severity token case-insensitively (`"major"` or
    /// `"Major"`), rejecting anything outside the X.733 vocabulary.
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PerceivedSeverity>, D::Error> {
        let text = String::deserialize(deserializer)?;
        let severity = match text.to_ascii_lowercase().as_str() {
            "cleared" => PerceivedSeverity::Cleared,
            "indeterminate" => PerceivedSeverity::Indeterminate,
            "warning" => PerceivedSeverity::Warning,
            "minor" => PerceivedSeverity::Minor,
            "major" => PerceivedSeverity::Major,
            "critical" => PerceivedSeverity::Critical,
            _ => {
                return Err(D::Error::invalid_value(
                    Unexpected::Str(&text),
                    &format!("one of {TOKENS:?}").as_str(),
                ));
            }
        };
        Ok(Some(severity))
    }
}
