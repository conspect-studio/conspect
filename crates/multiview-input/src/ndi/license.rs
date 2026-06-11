//! The NDI® runtime license gate for the **ingest** side (ADR-0008 §7.5,
//! `docs/io/ndi.md` §7.5).
//!
//! Mirrors the gate on the output side (`multiview-output::ndi::license`): even in
//! an `ndi`-enabled build, **no NDI source may start receiving** until an operator
//! has explicitly accepted the NDI SDK license. The acceptance carries who/when
//! for the audit log (ADR-0008: the acknowledgement is audited and exported with
//! config as a flag, never a secret). `multiview-input` cannot depend on
//! `multiview-output` (it is a sibling leaf crate, not an upstream), so the gate is
//! restated here as the enforcement point the `[system.ndi] accept_license`
//! setting feeds for an NDI *source*.

/// Why an NDI ingest operation was refused at the license gate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NdiLicenseError {
    /// The operator has not accepted the NDI SDK license. NDI ingest stays inert;
    /// any configured NDI source is reported with this status (`ndi_unlicensed`)
    /// and never started — its tile degrades (`LIVE→…→NO_SIGNAL`), never hangs.
    #[error("ndi_unlicensed: the NDI SDK license has not been accepted; NDI ingest is refused")]
    NotAccepted,
    /// An acceptance record was supplied but is missing required audit fields
    /// (who/when). The gate refuses incomplete records rather than logging a
    /// half-formed audit entry.
    #[error("ndi license acceptance is incomplete: {detail}")]
    IncompleteAcceptance {
        /// Human-readable detail of what was missing.
        detail: String,
    },
}

/// An **audited** record of an operator accepting the NDI SDK license.
///
/// `accepted_by` + `accepted_at` are the audit fields ADR-0008 requires; they are
/// exported with config as a flag (never a secret). `accepted_at` is a free-form
/// timestamp string (e.g. RFC 3339) so this leaf type stays dependency-free; the
/// config layer supplies the canonical value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseAcceptance {
    /// Principal (operator id / name) who accepted, for the audit log.
    pub accepted_by: String,
    /// When acceptance was recorded (free-form timestamp, e.g. RFC 3339).
    pub accepted_at: String,
}

/// The NDI ingest license gate guard.
///
/// There is **no** public constructor that yields an accepted guard without an
/// acceptance record, so obtaining one requires an acceptance — which is exactly
/// what gates an NDI source from starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdiLicense {
    acceptance: LicenseAcceptance,
}

impl NdiLicense {
    /// Build an accepted license guard from an audited acceptance record.
    ///
    /// # Errors
    /// [`NdiLicenseError::IncompleteAcceptance`] when `accepted_by` or
    /// `accepted_at` is empty.
    pub fn accept(acceptance: LicenseAcceptance) -> Result<Self, NdiLicenseError> {
        if acceptance.accepted_by.trim().is_empty() {
            return Err(NdiLicenseError::IncompleteAcceptance {
                detail: "accepted_by is empty".to_owned(),
            });
        }
        if acceptance.accepted_at.trim().is_empty() {
            return Err(NdiLicenseError::IncompleteAcceptance {
                detail: "accepted_at is empty".to_owned(),
            });
        }
        Ok(Self { acceptance })
    }

    /// Evaluate an `accept_license` flag (as it arrives from `[system.ndi]`) into
    /// either an accepted guard or the typed [`NdiLicenseError::NotAccepted`]
    /// refusal — the single decision point the config/API setting feeds.
    ///
    /// # Errors
    /// [`NdiLicenseError::NotAccepted`] when `accept_license` is `false`;
    /// [`NdiLicenseError::IncompleteAcceptance`] when accepted but the audit
    /// fields are blank.
    pub fn from_setting(
        accept_license: bool,
        acceptance: LicenseAcceptance,
    ) -> Result<Self, NdiLicenseError> {
        if !accept_license {
            return Err(NdiLicenseError::NotAccepted);
        }
        Self::accept(acceptance)
    }

    /// The audited acceptance record (who/when), for the audit log / export.
    #[must_use]
    pub fn acceptance(&self) -> &LicenseAcceptance {
        &self.acceptance
    }
}
