//! The fetch + liveness-probe seam.
//!
//! All network access is hidden behind two async traits — [`SourceCatalog`]
//! (fetch the iptv-org `streams.json` + `channels.json`) and [`Prober`]
//! (probe a single stream URL for liveness, replaying its required UA/referrer).
//! The offline tests inject [`FixtureCatalog`] + [`FixtureProber`] (synthetic
//! in-repo JSON, no sockets); the live `soak-iptv` run injects the real
//! HTTP-backed implementations behind the off-by-default `net` feature.

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

use crate::iptv::error::IptvError;
use crate::iptv::model::{Channel, Stream};

/// The outcome of probing a single stream URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutcome {
    /// The origin responded with a readable playlist/manifest.
    Live,
    /// The origin did not respond, errored, or was geo/region-blocked.
    Dead,
}

/// A boxed, `Send` future — the object-safe return type for the async seam.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The parsed iptv-org catalog: the stream rows and the channel rows.
pub type Catalog = (Vec<Stream>, Vec<Channel>);

/// Fetches the raw iptv-org catalog (`streams.json` + `channels.json`).
pub trait SourceCatalog {
    /// Fetch and parse the streams + channels catalogs.
    fn fetch(&self) -> BoxFuture<'_, Result<Catalog, IptvError>>;
}

/// Probes a single stream URL for liveness, replaying any required headers.
pub trait Prober {
    /// Probe `url`, sending `user_agent` / `referrer` when present. Must never
    /// fail the run — a network error resolves to [`ProbeOutcome::Dead`].
    fn probe<'a>(
        &'a self,
        url: &'a str,
        user_agent: Option<&'a str>,
        referrer: Option<&'a str>,
    ) -> BoxFuture<'a, ProbeOutcome>;
}

/// An in-memory catalog backed by JSON strings (the offline test seam).
pub struct FixtureCatalog {
    streams_json: String,
    channels_json: String,
}

impl FixtureCatalog {
    /// Build a fixture catalog from raw `streams.json` + `channels.json` text.
    #[must_use]
    pub fn new(streams_json: &str, channels_json: &str) -> Self {
        Self {
            streams_json: streams_json.to_owned(),
            channels_json: channels_json.to_owned(),
        }
    }
}

impl SourceCatalog for FixtureCatalog {
    fn fetch(&self) -> BoxFuture<'_, Result<Catalog, IptvError>> {
        Box::pin(async move {
            let streams: Vec<Stream> =
                serde_json::from_str(&self.streams_json).map_err(IptvError::ParseStreams)?;
            let channels: Vec<Channel> =
                serde_json::from_str(&self.channels_json).map_err(IptvError::ParseChannels)?;
            Ok((streams, channels))
        })
    }
}

/// A prober driven by a pure closure `url -> ProbeOutcome` (the offline seam).
pub struct FixtureProber<F> {
    decide: F,
}

impl<F> FixtureProber<F>
where
    F: Fn(&str) -> ProbeOutcome + Sync,
{
    /// Build a fixture prober from a decision closure.
    pub fn new(decide: F) -> Self {
        Self { decide }
    }
}

impl<F> Prober for FixtureProber<F>
where
    F: Fn(&str) -> ProbeOutcome + Sync,
{
    fn probe<'a>(
        &'a self,
        url: &'a str,
        _user_agent: Option<&'a str>,
        _referrer: Option<&'a str>,
    ) -> BoxFuture<'a, ProbeOutcome> {
        let outcome = (self.decide)(url);
        Box::pin(async move { outcome })
    }
}
