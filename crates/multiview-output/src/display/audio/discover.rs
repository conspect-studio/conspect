//! Pure discovery policy: which ALSA card / ELD entry serves a KMS connector
//! (DEV-B4 / display-out §5).
//!
//! There is no kernel API that maps a DRM connector name to the ALSA PCM whose
//! audio rides it, so the feature-gated backend scans the system and applies
//! the **policy pinned here** (pure functions, CI-tested without hardware):
//!
//! - **Pi (vc4-hdmi)**: each HDMI port is its own ALSA card —
//!   [`vc4_card_candidates`] maps `HDMI-A-{n}` to the `vc4hdmi{n-1}` card id
//!   (plus the bare `vc4hdmi` id single-port models use).
//! - **x86 HDA**: one card exposes several `eld#D.P` pin entries;
//!   [`pick_eld_entry`] selects the first entry with a *valid* parsed ELD (the
//!   lit pipe), falling back to the first entry so a boot that raced the
//!   monitor keeps polling and lights up on hotplug. The chosen entry's
//!   ordinal doubles as the `hdmi:CARD=…,DEV=…` device index — the same
//!   pin-ordering assumption alsa-lib's `hdmi:` config layer makes. With
//!   several simultaneously-lit heads on one card this ordinal heuristic is
//!   ambiguous; the backend logs the choice so the field case is diagnosable.
//!   (Hardware-validated on the t630 HDA leg.)

use super::eld::EldCapability;

/// Choose which ELD entry of a card serves the audio path: the first entry
/// whose ELD parsed to a valid capability (a lit pipe), else the first entry
/// (so the sink keeps polling a dark pipe and lights on hotplug), else
/// [`None`] when the card exposes no ELD entries at all.
#[must_use]
pub fn pick_eld_entry(entries: &[Option<EldCapability>]) -> Option<usize> {
    if let Some(lit) = entries.iter().position(Option::is_some) {
        return Some(lit);
    }
    if entries.is_empty() {
        None
    } else {
        Some(0)
    }
}

/// The vc4-hdmi ALSA card ids that can serve a KMS connector, in preference
/// order. `HDMI-A-1` → `vc4hdmi0` (and the bare `vc4hdmi` id used when the
/// model exposes a single port); `HDMI-A-{n}` → `vc4hdmi{n-1}`. Non-HDMI
/// connectors (and malformed names) have no vc4 mapping.
#[must_use]
pub fn vc4_card_candidates(connector: &str) -> Vec<String> {
    let Some(index) = connector
        .strip_prefix("HDMI-A-")
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return Vec::new();
    };
    let Some(port) = index.checked_sub(1) else {
        return Vec::new();
    };
    let mut candidates = vec![format!("vc4hdmi{port}")];
    if port == 0 {
        candidates.push("vc4hdmi".to_owned());
    }
    candidates
}
