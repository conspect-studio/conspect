//! The signed **entitlement** resource model (ADR-0050 §2, brief §2, §6.1).
//!
//! The entitlement is the resource a licence assertion carries: the opaque
//! `tier` (rendered, never computed — brief §1), the licensed vs detected
//! hardware class, the GPU limit, the current [`Lease`], and feature flags. It
//! is pure data; the enforcement ladder ([`crate::ladder`]) is computed from it,
//! never stored on it.

use serde::{Deserialize, Serialize};

use crate::lease::Lease;

/// The hardware class an entitlement is licensed for, and the class actually
/// detected on the machine. A mismatch is a ladder reason (brief §6) — the
/// class is licensed and detected; this crate scores the *match*, it does not
/// gather raw hardware identifiers (brief §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HardwareClass {
    /// A standard single-host deployment.
    Standard,
    /// A datacenter-class host (the licence tier the spec maps to it is opaque).
    Datacenter,
    /// An edge / appliance-class host.
    Edge,
}

/// The opaque commercial tier. This crate **renders** the tier string; it never
/// computes tier semantics or gates features by tier in v1 (brief §1, O7). It is
/// a newtype so it cannot be confused with any other string field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tier(String);

impl Tier {
    /// Wrap an opaque tier identifier supplied by the licence server.
    #[must_use]
    pub fn new(tier: String) -> Self {
        Self(tier)
    }

    /// The opaque tier string, for rendering only.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The GPU allowance carried by the entitlement. Adjacently tagged on `kind`
/// (+ `value` for the cap; conventions §5 — never untagged) so `limited` and
/// `unlimited` parse unambiguously across formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum GpuLimit {
    /// No GPU cap (the over-GPU ladder reason can never fire).
    Unlimited,
    /// At most this many GPUs may be in use before the over-GPU reason fires.
    Limited(u32),
}

impl GpuLimit {
    /// Whether `in_use` GPUs exceed this limit. `Unlimited` is never over; a
    /// `Limited` cap is over only when usage is **strictly greater** than the cap
    /// (usage equal to the cap is within budget).
    #[must_use]
    pub const fn is_over(self, in_use: u32) -> bool {
        match self {
            GpuLimit::Unlimited => false,
            GpuLimit::Limited(count) => in_use > count,
        }
    }
}

/// Boolean entitlement flags. `#[non_exhaustive]` + `Default` so future flags
/// add without breaking the resource shape; defaults are all conservative
/// (no special grants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct EntitlementFlags {
    /// Whether this entitlement is an evaluation/trial grant (drives the
    /// evaluation ladder track, brief §6). Defaults to `false`.
    pub evaluation: bool,
    /// Whether the official heartbeat client is present in this build. A source
    /// build with the client compiled out reports this `false` and the ladder
    /// renders `unlicensed-build` honestly (ADR-0050 §7). Defaults to `true`
    /// (the official build).
    pub heartbeat_present: bool,
}

/// The signed entitlement resource (the thing a licence assertion carries).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Entitlement {
    /// The opaque commercial tier (rendered, never computed).
    pub tier: Tier,
    /// The hardware class this entitlement is licensed for.
    pub licensed_class: HardwareClass,
    /// The hardware class detected on the machine (a mismatch is a ladder reason).
    pub detected_class: HardwareClass,
    /// The GPU allowance.
    pub gpu_limit: GpuLimit,
    /// The current dated lease.
    pub lease: Lease,
    /// Entitlement feature flags.
    pub flags: EntitlementFlags,
}

impl Entitlement {
    /// Assemble an entitlement resource. A constructor is provided because the
    /// type is `#[non_exhaustive]` (it is a versioned wire resource), so it
    /// cannot be built by struct literal outside this crate.
    #[must_use]
    pub fn new(
        tier: Tier,
        licensed_class: HardwareClass,
        detected_class: HardwareClass,
        gpu_limit: GpuLimit,
        lease: Lease,
        flags: EntitlementFlags,
    ) -> Self {
        Self {
            tier,
            licensed_class,
            detected_class,
            gpu_limit,
            lease,
            flags,
        }
    }
}
