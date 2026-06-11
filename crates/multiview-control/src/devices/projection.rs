//! Device stream-binding projections (ADR-M009): the **source-candidate** and
//! **output-target** facets a managed device exposes into the media graph.
//!
//! A device is never itself a Source or an Output; its driver *projects* it
//! into the existing graph through these facets, and binding a projection
//! creates an ordinary managed Source/Output carrying a `device_ref`. In this
//! slice there is **no live driver** (DEV-A4/A5 own the typed clients), so the
//! projection endpoints return the **declared/configured** candidates honestly
//! — an empty list for a device whose driver has not enumerated anything yet —
//! never fabricated live telemetry. Where a vendor does not document its served
//! URL paths, the candidate's URL is operator-suppliable and flagged
//! `unverified`, never guessed (ADR-M009).

use serde::{Deserialize, Serialize};

/// One stream a device serves or can push, bindable as an ordinary managed
/// Source with a `device_ref` (ADR-M009 facet (a)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct SourceCandidate {
    /// A stable id for this candidate within the device (driver-assigned).
    pub id: String,
    /// The transport kind the bound Source would use (`rtsp` / `srt` / `ndi`).
    pub kind: String,
    /// The served URL, IPv6-first where known. `None` when the vendor does not
    /// document its mount path — the operator supplies it and it is flagged
    /// [`unverified`](SourceCandidate::unverified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `true` when the URL is unverified (operator-suppliable, undocumented by
    /// the vendor) — never silently guessed (ADR-M009).
    pub unverified: bool,
}

/// One decode-side entry point on a device, bindable as an ordinary managed
/// Output with a `device_ref` (ADR-M009 facet (b)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct OutputTarget {
    /// A stable id for this target within the device (driver-assigned).
    pub id: String,
    /// The transport kind the device decodes (`rtsp` / `srt` / `rtmp` / `ndi`).
    pub kind: String,
    /// A human-friendly label for the decode slot, if the driver reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
