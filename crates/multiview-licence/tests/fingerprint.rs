//! Fingerprint-scoring tests (ADR-0050 §6 / brief §2.3, §8): machine identity is
//! a SCORE over salted component digests. ≥70 is the same machine (drift
//! tolerated); below 70 forces a re-claim. The crate scores DIGESTS handed to
//! it — it never gathers raw serials/MACs (data minimisation, §8).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use multiview_licence::fingerprint::{
    score_fingerprint, ComponentKind, Fingerprint, FingerprintComponent,
};
use multiview_licence::{FINGERPRINT_MATCH_STRONG, FINGERPRINT_MATCH_THRESHOLD};

/// A salted digest is opaque bytes to this crate. Build a component from a kind
/// and a digest tag (the test supplies distinct digests for "changed").
fn comp(kind: ComponentKind, tag: u8) -> FingerprintComponent {
    FingerprintComponent::new(kind, [tag; 32])
}

/// The canonical four-component baseline fingerprint (board, CPU, NIC, disk).
fn baseline() -> Fingerprint {
    Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 3),
        comp(ComponentKind::Disk, 4),
    ])
}

#[test]
fn constants_are_exact() {
    assert_eq!(FINGERPRINT_MATCH_STRONG, 100);
    assert_eq!(FINGERPRINT_MATCH_THRESHOLD, 70);
}

#[test]
fn identical_fingerprint_scores_one_hundred() {
    let a = baseline();
    let b = baseline();
    let outcome = score_fingerprint(&a, &b);
    assert_eq!(outcome.score, 100);
    assert!(outcome.is_same_machine());
    assert!(!outcome.rebind_needed());
}

#[test]
fn gpu_swap_drops_twenty_still_same_machine() {
    // A swapped GPU costs -20; 100-20 = 80 ≥ 70 → still the same machine.
    let a = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 3),
        comp(ComponentKind::Disk, 4),
        comp(ComponentKind::Gpu, 5),
    ]);
    let mut b_components = vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 3),
        comp(ComponentKind::Disk, 4),
        comp(ComponentKind::Gpu, 99), // different GPU digest
    ];
    let b = Fingerprint::from_components(std::mem::take(&mut b_components));
    let outcome = score_fingerprint(&a, &b);
    assert_eq!(outcome.score, 80);
    assert!(outcome.is_same_machine());
    assert!(!outcome.rebind_needed());
}

#[test]
fn nic_change_drops_fifteen_still_same_machine() {
    // A changed NIC costs -15; 100-15 = 85 ≥ 70 → still the same machine.
    let a = baseline();
    let b = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 99), // different NIC digest
        comp(ComponentKind::Disk, 4),
    ]);
    let outcome = score_fingerprint(&a, &b);
    assert_eq!(outcome.score, 85);
    assert!(outcome.is_same_machine());
    assert!(!outcome.rebind_needed());
}

#[test]
fn gpu_swap_and_nic_change_drops_below_threshold_rebind() {
    // GPU swap (-20) + NIC change (-15) = 100-35 = 65 < 70 → rebind needed.
    let a = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 3),
        comp(ComponentKind::Disk, 4),
        comp(ComponentKind::Gpu, 5),
    ]);
    let b = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 99), // NIC changed
        comp(ComponentKind::Disk, 4),
        comp(ComponentKind::Gpu, 99), // GPU swapped
    ]);
    let outcome = score_fingerprint(&a, &b);
    assert_eq!(outcome.score, 65);
    assert!(!outcome.is_same_machine());
    assert!(outcome.rebind_needed());
}

#[test]
fn threshold_seventy_is_inclusive_same_machine() {
    // A score of exactly 70 is still the SAME machine (≥ 70 per §2.3).
    // Construct it: change a NIC (-15) and a disk (-15) = 100-30 = 70.
    let a = baseline();
    let b = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 1),
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 99),
        comp(ComponentKind::Disk, 99),
    ]);
    let outcome = score_fingerprint(&a, &b);
    assert_eq!(outcome.score, 70);
    assert!(outcome.is_same_machine());
    assert!(!outcome.rebind_needed());
}

#[test]
fn board_or_cpu_change_is_heavily_penalised() {
    // The board is the anchor — changing it is a strong signal of a NEW machine.
    let a = baseline();
    let b = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 99), // board changed
        comp(ComponentKind::Cpu, 2),
        comp(ComponentKind::Nic, 3),
        comp(ComponentKind::Disk, 4),
    ]);
    let outcome = score_fingerprint(&a, &b);
    assert!(
        outcome.score < FINGERPRINT_MATCH_THRESHOLD,
        "a board swap should fall below the threshold (was {})",
        outcome.score
    );
    assert!(outcome.rebind_needed());
}

#[test]
fn score_never_underflows_below_zero() {
    // A wholesale re-platform: nothing matches. Score must clamp at 0, not wrap.
    let a = baseline();
    let b = Fingerprint::from_components(vec![
        comp(ComponentKind::Board, 50),
        comp(ComponentKind::Cpu, 51),
        comp(ComponentKind::Nic, 52),
        comp(ComponentKind::Disk, 53),
    ]);
    let outcome = score_fingerprint(&a, &b);
    assert_eq!(outcome.score, 0);
    assert!(outcome.rebind_needed());
}
