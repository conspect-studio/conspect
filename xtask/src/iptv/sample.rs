//! The deterministic, stratified, quirk-aware sampler.
//!
//! Given the joined catalog and a [`Plan`] (seed + over-sample target), produce
//! an ordered candidate set that (a) filters NSFW, (b) is stratified across the
//! category axis so every kind of channel is represented, and (c) is
//! reproducible: the same seed + the same input yields byte-identical output.
//!
//! Determinism comes from a tiny in-crate `SplitMix64` PRNG seeded from the
//! [`Plan::seed`] — no external RNG crate, no system entropy, so the offline
//! tests are exact. Over-sampling is deliberate: the live liveness probe will
//! discard the dead/geo-blocked majority, so we draw more than we ultimately
//! keep.

use crate::iptv::classify::{classify_quirks, Container, QuirkTag};
use crate::iptv::join::JoinedStream;

/// The category strata the sampler balances across (the "axis: category" of the
/// soak set). A joined stream is bucketed into the first stratum whose slug its
/// channel carries; anything else lands in [`Stratum::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stratum {
    News,
    Sports,
    Movies,
    Music,
    Kids,
    Weather,
    Other,
}

impl Stratum {
    /// The fixed stratum order (round-robin draws follow this).
    const ALL: [Stratum; 7] = [
        Stratum::News,
        Stratum::Sports,
        Stratum::Movies,
        Stratum::Music,
        Stratum::Kids,
        Stratum::Weather,
        Stratum::Other,
    ];

    fn of(categories: &[String]) -> Self {
        for c in categories {
            match c.to_ascii_lowercase().as_str() {
                "news" => return Stratum::News,
                "sports" | "sport" => return Stratum::Sports,
                "movies" | "movie" => return Stratum::Movies,
                "music" => return Stratum::Music,
                "kids" | "children" => return Stratum::Kids,
                "weather" => return Stratum::Weather,
                _ => {}
            }
        }
        Stratum::Other
    }
}

/// A sampled candidate source, enriched with its computed quirk tags.
#[derive(Debug, Clone)]
pub struct SelectedSource {
    /// The channel id.
    pub channel_id: String,
    /// The playable URL.
    pub url: String,
    /// The declared quality, if any.
    pub quality: Option<String>,
    /// A `User-Agent` the origin requires, if any (replayed on probe/fetch).
    pub user_agent: Option<String>,
    /// A `Referer` the origin requires, if any (replayed on probe/fetch).
    pub referrer: Option<String>,
    /// The channel's category slugs.
    pub categories: Vec<String>,
    /// The channel's country code, if any.
    pub country: Option<String>,
    /// Whether the channel is NSFW (always `false` post-sampling).
    pub is_nsfw: bool,
    /// The detected delivery container.
    pub container: Container,
    /// The computed quirk-tag set.
    pub quirks: Vec<QuirkTag>,
}

impl SelectedSource {
    /// Build a [`SelectedSource`] from a joined stream, computing its quirks.
    #[must_use]
    pub fn from_joined(j: &JoinedStream) -> Self {
        let quirks: Vec<QuirkTag> = classify_quirks(j).into_iter().collect();
        Self {
            channel_id: j.channel_id.clone(),
            url: j.url.clone(),
            quality: j.quality.clone(),
            user_agent: j.user_agent.clone(),
            referrer: j.referrer.clone(),
            categories: j.categories.clone(),
            country: j.country.clone(),
            is_nsfw: j.is_nsfw,
            container: Container::from_url(&j.url),
            quirks,
        }
    }
}

/// Selection plan: the seed (reproducibility), the over-sample target (how many
/// candidates to draw before probing), and the post-probe keep caps.
#[derive(Debug, Clone)]
pub struct Plan {
    /// PRNG seed — fixes the stratified shuffle for reproducible runs/tests.
    pub seed: u64,
    /// How many candidates to draw across the strata before liveness probing.
    pub oversample: usize,
    /// After probing, the maximum number of LIVE sources to keep.
    pub keep_live: usize,
    /// After probing, the number of DEAD/geo sources to deliberately retain
    /// (so the `LIVE -> STALE -> RECONNECTING -> NO_SIGNAL` state machine is
    /// exercised).
    pub keep_dead: usize,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            seed: 0,
            oversample: 64,
            keep_live: 24,
            keep_dead: 4,
        }
    }
}

/// A minimal deterministic `SplitMix64` PRNG (public-domain algorithm).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Fisher–Yates shuffle of `items` driven by `rng` (in place).
///
/// Index arithmetic is done in `u64` via `try_from` (no `as` casts). A bucket
/// that somehow exceeds `u64::MAX` entries — impossible for a stream catalog —
/// would simply be left unshuffled rather than panic.
fn shuffle<T>(items: &mut [T], rng: &mut SplitMix64) {
    let len = items.len();
    if len <= 1 {
        return;
    }
    let mut i = len - 1;
    while i > 0 {
        // Unbiased-enough index in [0, i] for a dev sampling tool.
        let Ok(i_u64) = u64::try_from(i) else {
            break;
        };
        let span = i_u64.saturating_add(1);
        let draw = rng.next_u64() % span;
        // `draw < span <= i+1 <= len`, so it always fits back into `usize`.
        let Ok(j) = usize::try_from(draw) else {
            break;
        };
        items.swap(i, j);
        i -= 1;
    }
}

/// Draw a stratified, quirk-aware, NSFW-filtered candidate set.
///
/// The result is deterministic for a fixed `(catalog, plan.seed)` and is capped
/// at `plan.oversample` entries, drawn round-robin across the category strata so
/// no single stratum dominates.
#[must_use]
pub fn sample_sources(joined: &[JoinedStream], plan: &Plan) -> Vec<SelectedSource> {
    // Filter NSFW up front — it must never reach the candidate set.
    let mut buckets: Vec<Vec<SelectedSource>> = Stratum::ALL.iter().map(|_| Vec::new()).collect();
    for j in joined {
        if j.is_nsfw {
            continue;
        }
        let stratum = Stratum::of(&j.categories);
        // `position` is safe: `stratum` is always one of `Stratum::ALL`.
        if let Some(idx) = Stratum::ALL.iter().position(|s| *s == stratum) {
            if let Some(bucket) = buckets.get_mut(idx) {
                bucket.push(SelectedSource::from_joined(j));
            }
        }
    }

    // Shuffle each stratum deterministically. Each bucket gets its own RNG
    // stream derived from the seed + stratum index so re-ordering one bucket
    // never perturbs another.
    for (idx, bucket) in buckets.iter_mut().enumerate() {
        // `idx` is bounded by `Stratum::ALL.len()` (== 7), so this never fails.
        let idx_u64 = u64::try_from(idx).unwrap_or(0);
        let mut rng = SplitMix64::new(plan.seed.wrapping_add(idx_u64.wrapping_mul(0x1000_0001)));
        shuffle(bucket, &mut rng);
    }

    // Round-robin draw across strata until we hit the over-sample target or
    // exhaust every bucket. The visitation order of the strata is itself
    // seed-shuffled each pass, so the seed genuinely permutes the output even
    // when every stratum holds a single entry (no fixed stratum-priority bias).
    let mut out: Vec<SelectedSource> = Vec::new();
    let mut cursors = vec![0usize; buckets.len()];
    // A dedicated RNG stream for the visitation order (distinct from the
    // per-bucket streams above so neither perturbs the other).
    let mut order_rng = SplitMix64::new(plan.seed ^ 0xA5A5_5A5A_DEAD_BEEF);
    loop {
        if out.len() >= plan.oversample {
            break;
        }
        let mut order: Vec<usize> = (0..buckets.len()).collect();
        shuffle(&mut order, &mut order_rng);
        let mut drew_any = false;
        for idx in order {
            if out.len() >= plan.oversample {
                break;
            }
            let Some(bucket) = buckets.get(idx) else {
                continue;
            };
            let Some(cursor) = cursors.get_mut(idx) else {
                continue;
            };
            if let Some(item) = bucket.get(*cursor) {
                out.push(item.clone());
                *cursor += 1;
                drew_any = true;
            }
        }
        if !drew_any {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_is_stable() {
        let mut a = SplitMix64::new(123);
        let mut b = SplitMix64::new(123);
        for _ in 0..16 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut v: Vec<u32> = (0..50).collect();
        let mut rng = SplitMix64::new(7);
        shuffle(&mut v, &mut rng);
        v.sort_unstable();
        let expected: Vec<u32> = (0..50).collect();
        assert_eq!(v, expected);
    }
}
