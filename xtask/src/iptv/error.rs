//! Error taxonomy for the iptv soak-source selection tool.

use std::path::PathBuf;

use thiserror::Error;

/// Failure modes of fetching, parsing, selecting, and emitting the soak set.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IptvError {
    /// `streams.json` could not be parsed.
    #[error("parsing streams.json: {0}")]
    ParseStreams(#[source] serde_json::Error),
    /// `channels.json` could not be parsed.
    #[error("parsing channels.json: {0}")]
    ParseChannels(#[source] serde_json::Error),
    /// `blocklist.json` could not be parsed.
    #[error("parsing blocklist.json: {0}")]
    ParseBlocklist(#[source] serde_json::Error),
    /// The selection produced an empty set (no live source survived) — refuse
    /// to emit an empty manifest silently.
    #[error("no live sources survived selection (oversample={oversample})")]
    NoLiveSources {
        /// The over-sample target that was attempted.
        oversample: usize,
    },
    /// The manifest could not be serialized.
    #[error("serializing the manifest: {0}")]
    Serialize(#[source] serde_json::Error),
    /// The output directory could not be created.
    #[error("creating output directory {path}: {source}")]
    CreateDir {
        /// The directory whose creation failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest file could not be written.
    #[error("writing manifest {path}: {source}")]
    Write {
        /// The file path that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The live HTTP fetch failed (only reachable under the `net` feature).
    #[error("fetching {what} from {url}: {message}")]
    Http {
        /// Which catalog was being fetched (streams/channels).
        what: &'static str,
        /// The URL being fetched.
        url: String,
        /// A human-readable description of the transport failure.
        message: String,
    },
}
