//! Offline end-to-end test of the `soak-iptv` orchestration: fetch (fixture
//! catalog) → join → classify → stratified sample → probe (fixture prober) →
//! filter (nsfw + blocklist) → emit a quirk-tagged manifest + summary.
//!
//! No network: catalog + prober are injected. The manifest is written to a
//! `tempdir`, never the repo, and asserted to be quirk-tagged.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use xtask::iptv::{
    select_sources, Blocklist, FixtureCatalog, FixtureProber, Plan, ProbeOutcome, ABC_NEWS_URL,
};

fn plan() -> Plan {
    Plan {
        seed: 99,
        oversample: 32,
        keep_live: 6,
        keep_dead: 2,
    }
}

#[tokio::test]
async fn select_always_includes_abc_news_and_marks_it_live() {
    let catalog = FixtureCatalog::new(
        include_str!("fixtures/streams.json"),
        include_str!("fixtures/channels.json"),
    );
    // Prober: abc-news + everything except blocked.example.com is live.
    let prober = FixtureProber::new(|url| {
        if url.contains("blocked.example.com") {
            ProbeOutcome::Dead
        } else {
            ProbeOutcome::Live
        }
    });
    let blocklist = Blocklist::empty();
    let manifest = select_sources(&catalog, &prober, &blocklist, &plan())
        .await
        .expect("selection succeeds");

    // The ABC-News WebVTT-class stream is ALWAYS present.
    let abc = manifest
        .sources
        .iter()
        .find(|s| s.url == ABC_NEWS_URL)
        .expect("abc-news must always be included");
    assert_eq!(abc.probe, ProbeOutcome::Live);
    assert!(
        abc.quirks
            .iter()
            .any(|q| matches!(q, xtask::iptv::QuirkTag::PinnedSubtitleClass)),
        "the abc-news pin must be tagged as the subtitle/WebVTT resilience class"
    );
}

#[tokio::test]
async fn select_filters_blocklisted_domains() {
    let catalog = FixtureCatalog::new(
        include_str!("fixtures/streams.json"),
        include_str!("fixtures/channels.json"),
    );
    let prober = FixtureProber::new(|_url| ProbeOutcome::Live);
    let blocklist =
        Blocklist::from_json(include_str!("fixtures/blocklist.json")).expect("parse blocklist");
    let manifest = select_sources(&catalog, &prober, &blocklist, &plan())
        .await
        .expect("selection succeeds");

    assert!(
        manifest
            .sources
            .iter()
            .all(|s| !s.url.contains("blocked.example.com")),
        "blocklisted domains must be filtered before they ever reach the manifest"
    );
    // NSFW also never appears.
    assert!(manifest.sources.iter().all(|s| !s.is_nsfw));
}

#[tokio::test]
async fn select_retains_some_dead_sources_for_state_machine_testing() {
    let catalog = FixtureCatalog::new(
        include_str!("fixtures/streams.json"),
        include_str!("fixtures/channels.json"),
    );
    // Mark TWO distinct hosts dead so dead-retention has candidates.
    let prober = FixtureProber::new(|url| {
        if url.contains("blocked.example.com") || url.contains("stream.example.org") {
            ProbeOutcome::Dead
        } else {
            ProbeOutcome::Live
        }
    });
    let blocklist = Blocklist::empty();
    let manifest = select_sources(&catalog, &prober, &blocklist, &plan())
        .await
        .expect("selection succeeds");

    let dead = manifest
        .sources
        .iter()
        .filter(|s| s.probe == ProbeOutcome::Dead)
        .count();
    let live = manifest
        .sources
        .iter()
        .filter(|s| s.probe == ProbeOutcome::Live)
        .count();
    assert!(
        dead >= 1,
        "at least one dead source is deliberately retained for state-machine testing; dead={dead}"
    );
    assert!(live >= 1, "live sources must dominate; live={live}");
    // Never more dead than the configured cap.
    assert!(dead <= plan().keep_dead);
}

#[tokio::test]
async fn manifest_serializes_quirk_tagged_to_a_tempfile_never_the_repo() {
    let catalog = FixtureCatalog::new(
        include_str!("fixtures/streams.json"),
        include_str!("fixtures/channels.json"),
    );
    let prober = FixtureProber::new(|_url| ProbeOutcome::Live);
    let blocklist = Blocklist::empty();
    let manifest = select_sources(&catalog, &prober, &blocklist, &plan())
        .await
        .expect("selection succeeds");

    let json = manifest.to_pretty_json().expect("serialize manifest");
    // The manifest is quirk-tagged + carries the resolve-time note.
    assert!(
        json.contains("\"quirks\""),
        "manifest must carry quirk tags"
    );
    assert!(
        json.contains("resolved_at") || json.contains("resolved"),
        "manifest must record that URLs are resolved live each run"
    );

    // Write to a tempdir under the OS temp root — NEVER the repo working tree.
    let dir = std::env::temp_dir().join(format!("mv-iptv-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir temp");
    let out = dir.join("iptv-sources.json");
    manifest.write_to(&out).expect("write manifest");
    let read_back = std::fs::read_to_string(&out).expect("read back");
    assert!(read_back.contains(ABC_NEWS_URL));
    std::fs::remove_dir_all(&dir).ok();

    // The summary table is human-readable + names the quirk axes.
    let summary = manifest.summary_table();
    assert!(summary.contains("LIVE") || summary.contains("live"));
    assert!(summary.contains("quirk") || summary.contains("Quirk"));
}
