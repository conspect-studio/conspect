//! The crate error taxonomy (per-crate `Error` enum via `thiserror`,
//! conventions §9).
//!
//! Every fallible operation in this crate returns one of these — there is no
//! `unwrap`/`expect`/`panic` in non-test code (CLAUDE.md guardrail #1). Note
//! that **none** of these variants can stop output: this crate computes data
//! and verifies signatures; it has no engine handle and no process control
//! (the never-off-air invariant, ADR-0050 §5).

/// Errors raised by the entitlement plane's pure-data + verification surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LicenceError {
    /// The supplied verifying key was not a well-formed Ed25519 public key
    /// (wrong length or invalid point encoding).
    #[error("malformed Ed25519 verifying key")]
    MalformedKey,

    /// The signature bytes were not a well-formed Ed25519 signature (wrong
    /// length). Distinct from [`LicenceError::BadSignature`] so a transport
    /// framing bug is not confused with a genuine verification failure.
    #[error("malformed Ed25519 signature")]
    MalformedSignature,

    /// The signature did not verify against the pinned public key — a tampered
    /// payload, the wrong signer, or a forged assertion. The lease is rejected.
    #[error("Ed25519 signature verification failed")]
    BadSignature,

    /// A CBOR encode/decode fault on the lease/binding file or the
    /// `<host>.challenge` export (CONSPECT-1). Carries the underlying message so
    /// a malformed dropped file is logged, never panicked on
    /// (bad-inputs-are-the-purpose).
    #[error("CBOR codec error: {0}")]
    Cbor(String),
}
