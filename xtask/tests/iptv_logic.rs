//! Offline, deterministic unit tests for the PURE logic of the `soak-iptv`
//! test-source selection tool: the streams×channels JOIN, the quirk classifier,
//! the stratified/quirk-aware deterministic sampler, the container detection /
//! URL parsing, and the offline-fixture catalog + prober seam.
//!
//! These tests touch **no network**: the catalog and prober are injected via
//! their traits using the synthetic in-repo JSON fixtures under
//! `tests/fixtures/` (RFC-2606 `example.*` domains — never real stream URLs).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;

use xtask::iptv::{
    classify_quirks, join_streams_channels, sample_sources, Channel, Container, FixtureCatalog,
    FixtureProber, Plan, ProbeOutcome, Prober, QuirkTag, SourceCatalog, Stream,
};

fn load_fixtures() -> (Vec<Stream>, Vec<Channel>) {
    let streams_raw = include_str!("fixtures/streams.json");
    let channels_raw = include_str!("fixtures/channels.json");
    let streams: Vec<Stream> = serde_json::from_str(streams_raw).expect("parse streams.json");
    let channels: Vec<Channel> = serde_json::from_str(channels_raw).expect("parse channels.json");
    (streams, channels)
}

#[test]
fn parses_synthetic_streams_and_channels() {
    let (streams, channels) = load_fixtures();
    // The fixture has 11 streams and 8 channels.
    assert_eq!(streams.len(), 11);
    assert_eq!(channels.len(), 8);
    // Optional fields default cleanly: the first stream has no UA / referrer / label.
    let first = &streams[0];
    assert_eq!(first.channel.as_deref(), Some("NewsOne.us"));
    assert!(first.user_agent.is_none());
    assert!(first.referrer.is_none());
    assert!(first.label.is_none());
    // A stream carrying a UA + referrer round-trips.
    let with_ua = streams
        .iter()
        .find(|s| s.user_agent.is_some())
        .expect("a stream with a user_agent");
    assert_eq!(with_ua.user_agent.as_deref(), Some("MultiviewProbe/1.0"));
}

#[test]
fn join_attaches_category_and_country_and_drops_orphans() {
    let (streams, channels) = load_fixtures();
    let joined = join_streams_channels(&streams, &channels);
    // The "OrphanNine.zz" stream has no matching channel → it is dropped by the JOIN.
    assert!(
        joined.iter().all(|j| j.channel_id != "OrphanNine.zz"),
        "orphan stream with no channel must be dropped"
    );
    // 11 streams − 1 orphan = 10 joined rows.
    assert_eq!(joined.len(), 10);
    // The news stream carries its channel's category + country + nsfw flag.
    let news = joined
        .iter()
        .find(|j| j.url == "https://stream.example.com/news-one/master.m3u8")
        .expect("news stream present");
    assert!(news.categories.iter().any(|c| c == "news"));
    assert_eq!(news.country.as_deref(), Some("US"));
    assert!(!news.is_nsfw);
    // The adult channel propagates is_nsfw onto its stream.
    let adult = joined
        .iter()
        .find(|j| j.channel_id == "AdultSeven.xx")
        .expect("adult stream present");
    assert!(adult.is_nsfw);
}

#[test]
fn classifier_tags_hls_dash_and_raw_ts_containers() {
    assert_eq!(
        Container::from_url("https://x.example.com/a/master.m3u8"),
        Container::Hls
    );
    assert_eq!(
        Container::from_url("https://x.example.com/a/playlist.m3u8?token=abc"),
        Container::Hls
    );
    assert_eq!(
        Container::from_url("https://x.example.com/a/manifest.mpd"),
        Container::Dash
    );
    assert_eq!(
        Container::from_url("https://x.example.com/a/raw.ts"),
        Container::RawTs
    );
    assert_eq!(
        Container::from_url("rtsp://x.example.com/a/stream"),
        Container::Other
    );
}

#[test]
fn classifier_emits_resilience_quirk_tags() {
    let (streams, channels) = load_fixtures();
    let joined = join_streams_channels(&streams, &channels);

    // Geo-blocked label → GeoBlocked tag, plus RawTs container tag.
    let geo = joined
        .iter()
        .find(|j| j.url == "https://stream.example.org/sports-two/raw.ts")
        .expect("geo-blocked ts stream");
    let tags = classify_quirks(geo);
    assert!(tags.contains(&QuirkTag::GeoBlocked), "tags: {tags:?}");
    assert!(tags.contains(&QuirkTag::ContainerRawTs), "tags: {tags:?}");
    assert!(
        tags.contains(&QuirkTag::Interlaced),
        "576i must be flagged interlaced; tags: {tags:?}"
    );

    // "Not 24/7" label → NotAroundTheClock.
    let part_time = joined
        .iter()
        .find(|j| j.url == "https://stream.example.com/movie-three/master.m3u8")
        .expect("not-24/7 movie stream");
    let tags = classify_quirks(part_time);
    assert!(
        tags.contains(&QuirkTag::NotAroundTheClock),
        "tags: {tags:?}"
    );

    // Non-TLS http:// scheme → NonTls.
    let plain = joined
        .iter()
        .find(|j| {
            j.url
                .starts_with("http://stream.example.net/news-one-backup")
        })
        .expect("http backup stream");
    let tags = classify_quirks(plain);
    assert!(tags.contains(&QuirkTag::NonTls), "tags: {tags:?}");
    assert!(
        tags.contains(&QuirkTag::CustomUserAgent),
        "UA-bearing stream must tag CustomUserAgent; tags: {tags:?}"
    );
    assert!(
        tags.contains(&QuirkTag::CustomReferrer),
        "referrer-bearing stream must tag CustomReferrer; tags: {tags:?}"
    );

    // null quality → OddQuality; 2160p → UltraHigh; 144p → UltraLow.
    let null_q = joined
        .iter()
        .find(|j| j.url == "https://stream.example.com/music-four/master.m3u8")
        .expect("null quality stream");
    assert!(classify_quirks(null_q).contains(&QuirkTag::OddQuality));

    let uhd = joined
        .iter()
        .find(|j| j.url == "https://stream.example.org/sports-two/manifest.mpd")
        .expect("2160p stream");
    let tags = classify_quirks(uhd);
    assert!(tags.contains(&QuirkTag::UltraHighRes), "tags: {tags:?}");
    assert!(tags.contains(&QuirkTag::ContainerDash), "tags: {tags:?}");

    let low = joined
        .iter()
        .find(|j| j.url.starts_with("https://stream.example.com/kids-five/"))
        .expect("144p stream");
    assert!(classify_quirks(low).contains(&QuirkTag::UltraLowRes));
}

#[test]
fn sampler_is_deterministic_for_a_fixed_seed() {
    let (streams, channels) = load_fixtures();
    let joined = join_streams_channels(&streams, &channels);

    let plan = Plan {
        seed: 42,
        oversample: 6,
        ..Plan::default()
    };
    let a = sample_sources(&joined, &plan);
    let b = sample_sources(&joined, &plan);
    let urls_a: Vec<&str> = a.iter().map(|s| s.url.as_str()).collect();
    let urls_b: Vec<&str> = b.iter().map(|s| s.url.as_str()).collect();
    assert_eq!(urls_a, urls_b, "same seed → identical ordered selection");

    // A different seed yields a different ordering (the selection is genuinely
    // randomized over the strata, not a fixed pass-through).
    let plan2 = Plan {
        seed: 7,
        oversample: 6,
        ..Plan::default()
    };
    let c = sample_sources(&joined, &plan2);
    let urls_c: Vec<&str> = c.iter().map(|s| s.url.as_str()).collect();
    assert_ne!(
        urls_a, urls_c,
        "different seed should permute the stratified selection"
    );
}

#[test]
fn sampler_filters_nsfw_and_is_stratified_across_categories() {
    let (streams, channels) = load_fixtures();
    let joined = join_streams_channels(&streams, &channels);

    let plan = Plan {
        seed: 1,
        oversample: 32, // over-sample wide so every stratum is represented
        ..Plan::default()
    };
    let selected = sample_sources(&joined, &plan);

    // NSFW is always filtered out of the candidate set.
    assert!(
        selected.iter().all(|s| !s.is_nsfw),
        "no nsfw source may survive sampling"
    );
    assert!(
        selected.iter().all(|s| s.channel_id != "AdultSeven.xx"),
        "the adult channel must never be selected"
    );

    // Stratified across the category axis: news, sports, movies, music, kids,
    // weather all appear (over-sampling guarantees full coverage here).
    let cats: BTreeSet<String> = selected
        .iter()
        .flat_map(|s| s.categories.iter().cloned())
        .collect();
    for want in ["news", "sports", "movies", "music", "kids", "weather"] {
        assert!(cats.contains(want), "stratum {want} missing from {cats:?}");
    }

    // Every selected source carries its computed quirk tag set.
    assert!(
        selected.iter().any(|s| !s.quirks.is_empty()),
        "quirk tags must be attached to selected sources"
    );
}

#[tokio::test]
async fn fixture_catalog_loads_streams_and_channels_without_network() {
    let catalog = FixtureCatalog::new(
        include_str!("fixtures/streams.json"),
        include_str!("fixtures/channels.json"),
    );
    let (streams, channels) = catalog.fetch().await.expect("fixture fetch");
    assert_eq!(streams.len(), 11);
    assert_eq!(channels.len(), 8);
}

#[tokio::test]
async fn prober_keeps_live_and_retains_some_dead_for_state_machine_testing() {
    // The injected prober marks `blocked.example.com` dead and everything else live.
    let prober = FixtureProber::new(|url| {
        if url.contains("blocked.example.com") {
            ProbeOutcome::Dead
        } else {
            ProbeOutcome::Live
        }
    });
    assert_eq!(
        prober
            .probe("https://stream.example.com/x/master.m3u8", None, None)
            .await,
        ProbeOutcome::Live
    );
    assert_eq!(
        prober
            .probe("https://blocked.example.com/x/master.m3u8", None, None)
            .await,
        ProbeOutcome::Dead
    );
}
