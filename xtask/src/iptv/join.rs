//! The streams×channels JOIN.
//!
//! `streams.json` rows reference a channel by id; `channels.json` carries the
//! category/country/nsfw metadata. The JOIN attaches that metadata to each
//! stream, dropping streams whose channel id has no match (orphans) — those
//! cannot be stratified by category/country and are not useful soak material.

use std::collections::HashMap;

use crate::iptv::model::{Channel, Stream};

/// A stream enriched with its channel's category/country/nsfw metadata.
#[derive(Debug, Clone)]
pub struct JoinedStream {
    /// The channel id (after a successful join, always present).
    pub channel_id: String,
    /// The playable URL.
    pub url: String,
    /// The declared quality string, if any.
    pub quality: Option<String>,
    /// A `User-Agent` the origin requires, if any.
    pub user_agent: Option<String>,
    /// A `Referer` the origin requires, if any.
    pub referrer: Option<String>,
    /// The free-text resilience label, if any.
    pub label: Option<String>,
    /// The channel's category slugs.
    pub categories: Vec<String>,
    /// The channel's country code, if any.
    pub country: Option<String>,
    /// Whether the channel is flagged NSFW.
    pub is_nsfw: bool,
}

/// Join `streams` against `channels` on `stream.channel == channel.id`.
///
/// Streams with no `channel` id, or whose id has no matching channel, are
/// dropped. The output order follows the input `streams` order (stable), which
/// keeps the downstream sampler reproducible for a fixed input + seed.
#[must_use]
pub fn join_streams_channels(streams: &[Stream], channels: &[Channel]) -> Vec<JoinedStream> {
    let by_id: HashMap<&str, &Channel> = channels.iter().map(|c| (c.id.as_str(), c)).collect();

    let mut out = Vec::new();
    for s in streams {
        let Some(channel_id) = s.channel.as_deref() else {
            continue;
        };
        let Some(channel) = by_id.get(channel_id) else {
            continue;
        };
        out.push(JoinedStream {
            channel_id: channel_id.to_owned(),
            url: s.url.clone(),
            quality: s.quality.clone(),
            user_agent: s.user_agent.clone(),
            referrer: s.referrer.clone(),
            label: s.label.clone(),
            categories: channel.categories.clone(),
            country: channel.country.clone(),
            is_nsfw: channel.is_nsfw,
        });
    }
    out
}
