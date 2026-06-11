//! `soak-iptv` — a quirk-tagged, liveness-probed test-source selection tool.
//!
//! This module builds a systematically *adversarial* set of REAL test sources
//! from the iptv-org public catalog so the Multiview ingest path is exercised
//! against the ingest-resilience edge cases that bite in production (the
//! pinned ABC News `WebVTT` class, geo-blocked feeds, part-time channels,
//! header-gated origins, non-TLS origins, and resolution extremes). It:
//!
//! 1. fetches iptv-org `streams.json` + `channels.json`,
//! 2. JOINs streams→channels to attach category/country/nsfw,
//! 3. draws a **deterministic, seed-stratified, quirk-aware** over-sample,
//! 4. PROBES liveness (replaying each source's required `user_agent`/`referrer`),
//!    keeping the first *K* live and deliberately retaining a few dead/geo
//!    sources so the `LIVE -> STALE -> RECONNECTING -> NO_SIGNAL` tile state
//!    machine is tested,
//! 5. FILTERS NSFW + a local blocklist,
//! 6. emits a quirk-tagged manifest (to a gitignored path) + a summary table,
//!    ALWAYS including the pinned [`ABC_NEWS_URL`].
//!
//! Real stream URLs are **never** committed — they rot and may be NSFW/DMCA-
//! adjacent. They are resolved live each run; the manifest is written to a
//! gitignored output path (e.g. `.multiview-build/`).
//!
//! # Offline vs networked
//! Everything except the actual fetch/probe is pure and unit-tested offline via
//! synthetic in-repo fixtures: the [`join_streams_channels`] JOIN, the
//! [`classify_quirks`] classifier, the [`Container`] detection, the
//! [`sample_sources`] sampler, and the full [`select_sources`] pipeline (driven
//! by [`FixtureCatalog`] + [`FixtureProber`]). The live HTTP catalog + prober
//! ([`HttpCatalog`] / [`HttpProber`]) sit behind the off-by-default `net`
//! feature and are the only network-touching code.

mod classify;
mod error;
mod fetch;
mod join;
mod model;
mod run;
mod sample;

#[cfg(feature = "net")]
mod http;

pub use classify::{classify_quirks, Container, QuirkTag};
pub use error::IptvError;
pub use fetch::{Catalog, FixtureCatalog, FixtureProber, ProbeOutcome, Prober, SourceCatalog};
pub use join::{join_streams_channels, JoinedStream};
pub use model::{Channel, Stream};
pub use run::{select_sources, Blocklist, Manifest, ManifestSource, ABC_NEWS_URL};
pub use sample::{sample_sources, Plan, SelectedSource};

#[cfg(feature = "net")]
pub use http::{HttpCatalog, HttpProber};
