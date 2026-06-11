//! The NDI® runtime license gate (ADR-0008 §7.5, `docs/io/ndi.md` §7.5).
//!
//! Even in an `ndi`-enabled build, **no NDI source or output may start** until an
//! operator has explicitly accepted the NDI SDK license. This module models that
//! gate as a typestate guard: [`NdiLicense`] can only hold an `Accepted` state
//! when constructed from an explicit acceptance record, and the NDI sink seam
//! ([`super::output::NdiOutput`]) *requires* an accepted guard to construct — so
//! it is structurally impossible to start an NDI sender without acceptance.
//!
//! The acceptance carries **who/when** for the audit log (ADR-0008: the
//! acknowledgement is audited and exported with config as a flag, never a
//! secret). The config-schema wiring (`[system.ndi] accept_license`) and the
//! `PATCH /api/v1/system/settings` surface live in other crates; this gate is the
//! enforcement point those settings feed.

/// Why an NDI operation was refused at the license gate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiLicenseError {
    /// The operator has not accepted the NDI SDK license. NDI I/O stays inert;
    /// any configured NDI output is reported with this status (`ndi_unlicensed`)
    /// and never started.
    NotAccepted,
    /// An acceptance record was supplied but is missing required audit fields
    /// (who/when). The gate refuses incomplete records rather than logging a
    /// half-formed audit entry.
    IncompleteAcceptance {
        /// Human-readable detail of what was missing.
        detail: String,
    },
}

impl std::fmt::Display for NdiLicenseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAccepted => write!(
                f,
                "ndi_unlicensed: the NDI SDK license has not been accepted; NDI \
                 I/O is refused until an operator accepts ({})",
                super::NDI_ATTRIBUTION_URL
            ),
            Self::IncompleteAcceptance { detail } => {
                write!(f, "ndi license acceptance is incomplete: {detail}")
            }
        }
    }
}

impl std::error::Error for NdiLicenseError {}

/// An **audited** record of an operator accepting the NDI SDK license.
///
/// `accepted_by` + `accepted_at` are the audit fields ADR-0008 requires; they are
/// exported with config as a flag (never a secret). `accepted_at` is carried as a
/// free-form timestamp string (e.g. RFC 3339) so this leaf type stays
/// dependency-free; the config layer supplies the canonical value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseAcceptance {
    /// Principal (operator id / name) who accepted, for the audit log.
    pub accepted_by: String,
    /// When acceptance was recorded (free-form timestamp, e.g. RFC 3339).
    pub accepted_at: String,
}

/// The NDI runtime license gate guard.
///
/// Construct it from an [`LicenseAcceptance`] via [`NdiLicense::accept`]. There is
/// **no** public constructor that yields an accepted guard without an acceptance
/// record, so the only way to obtain one is to have an acceptance — which is
/// exactly what gates the [`super::output::NdiOutput`] sink seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdiLicense {
    acceptance: LicenseAcceptance,
}

impl NdiLicense {
    /// Build an accepted license guard from an audited acceptance record.
    ///
    /// Refuses (with [`NdiLicenseError::IncompleteAcceptance`]) if the audit
    /// fields are blank — an accepted gate must always carry who/when.
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
    /// When `accept_license` is `false` the result is `NotAccepted`; NDI stays
    /// inert and the caller reports `ndi_unlicensed` without starting anything.
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
