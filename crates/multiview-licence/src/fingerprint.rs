//! Machine-identity **fingerprint scoring** (ADR-0050 §6, brief §2.3, §8).
//!
//! Machine identity is a **score over salted component digests**, never raw
//! serials/MACs. This crate scores **digests handed to it** — it does **not**
//! gather hardware identifiers itself (data minimisation, brief §8): each
//! component is already a salted, hashed digest (opaque 32 bytes) by the time it
//! reaches here. The score answers one question: *is this the same machine as
//! before?* At/above [`crate::FINGERPRINT_MATCH_THRESHOLD`] (70) it is the same
//! machine (hardware drift tolerated); below it a re-claim is required. A GPU
//! swap or a NIC change drift the score down but never force a re-bind on their
//! own; a board/CPU change or a wholesale re-platform does.

use serde::{Deserialize, Serialize};

use crate::constants::{FINGERPRINT_MATCH_STRONG, FINGERPRINT_MATCH_THRESHOLD};

/// The length of a salted component digest, in bytes (a 256-bit hash).
pub const DIGEST_LEN: usize = 32;

/// The kind of hardware component a digest summarises. Serialised `snake_case`.
///
/// The kind drives the drift weight: the board/CPU anchor a machine (a change is
/// a strong "new machine" signal); a GPU/NIC/disk change is tolerated drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ComponentKind {
    /// The mainboard / baseboard — the strongest anchor.
    Board,
    /// The CPU — a strong anchor.
    Cpu,
    /// A network interface — benign drift (swapped/added NIC).
    Nic,
    /// A storage device — benign drift.
    Disk,
    /// A GPU — benign drift (a swapped accelerator).
    Gpu,
}

impl ComponentKind {
    /// The score penalty (out of 100) for this component **differing** between
    /// the reference and candidate fingerprints. Board/CPU are heavily weighted
    /// (a change pushes below the 70 threshold on its own); NIC/disk/GPU are
    /// light so single benign swaps stay above it.
    #[must_use]
    pub const fn drift_penalty(self) -> u8 {
        match self {
            ComponentKind::Board | ComponentKind::Cpu => 40,
            ComponentKind::Gpu => 20,
            ComponentKind::Nic | ComponentKind::Disk => 15,
        }
    }
}

/// A single salted, hashed hardware-component digest. The digest is opaque to
/// this crate (it never reverses it to an identifier — brief §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FingerprintComponent {
    /// What kind of component this digest summarises.
    pub kind: ComponentKind,
    /// The salted digest bytes (opaque).
    pub digest: [u8; DIGEST_LEN],
}

impl FingerprintComponent {
    /// Build a component from its kind and salted digest.
    #[must_use]
    pub fn new(kind: ComponentKind, digest: [u8; DIGEST_LEN]) -> Self {
        Self { kind, digest }
    }
}

/// A machine fingerprint: the set of salted component digests. The score is
/// computed against a previously-recorded fingerprint of the same machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Fingerprint {
    /// The component digests. Order is not significant to scoring (matched by
    /// kind + digest).
    pub components: Vec<FingerprintComponent>,
}

impl Fingerprint {
    /// Build a fingerprint from its components.
    #[must_use]
    pub fn from_components(components: Vec<FingerprintComponent>) -> Self {
        Self { components }
    }

    /// The digest recorded for `kind`, if any.
    fn digest_for(&self, kind: ComponentKind) -> Option<&[u8; DIGEST_LEN]> {
        self.components
            .iter()
            .find(|c| c.kind == kind)
            .map(|c| &c.digest)
    }
}

/// The outcome of scoring two fingerprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScoreOutcome {
    /// The continuity score in `0..=100` ([`FINGERPRINT_MATCH_STRONG`] is a
    /// perfect match).
    pub score: u8,
}

impl ScoreOutcome {
    /// Whether the candidate is the **same machine** as the reference: the score
    /// is at or above [`FINGERPRINT_MATCH_THRESHOLD`] (70, inclusive).
    #[must_use]
    pub const fn is_same_machine(self) -> bool {
        self.score >= FINGERPRINT_MATCH_THRESHOLD
    }

    /// Whether a re-bind (fresh claim) is required: the score fell **below** the
    /// threshold. The complement of [`ScoreOutcome::is_same_machine`].
    #[must_use]
    pub const fn rebind_needed(self) -> bool {
        !self.is_same_machine()
    }
}

/// Score `candidate` against `reference` (a previously-recorded fingerprint of
/// the machine). Starts at 100 and subtracts each differing/absent component's
/// [`ComponentKind::drift_penalty`], saturating at 0.
///
/// Identical sets score 100; a single benign drift (GPU/NIC/disk) stays ≥ 70; an
/// anchor change (board/CPU) or wholesale re-platform falls below 70 → re-bind.
#[must_use]
pub fn score_fingerprint(reference: &Fingerprint, candidate: &Fingerprint) -> ScoreOutcome {
    let mut score = FINGERPRINT_MATCH_STRONG;
    for component in &reference.components {
        let matched = candidate
            .digest_for(component.kind)
            .is_some_and(|d| d == &component.digest);
        if !matched {
            score = score.saturating_sub(component.kind.drift_penalty());
        }
    }
    ScoreOutcome { score }
}
