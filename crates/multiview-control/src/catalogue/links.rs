//! Link tracker (docs/io/iptv-org.md §2.4): for every source carrying
//! `origin.provider = "iptv-org"`, compute a [`LinkStatus`] against the current
//! snapshot, remember the previous one to detect transitions, and describe the
//! change an operator (or `follow = "auto"`) may apply.
//!
//! Pure and synchronous: callers feed it the sources and snapshot; it never does
//! I/O. Realtime events, the health warning and the auto-apply write path live in
//! the callers (`service`/`routes`).

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use multiview_config::{OriginFollow, Source};
use serde::Serialize;

use super::index::{Flag, IndexRow, Snapshot};

/// The provider id this tracker follows.
pub const PROVIDER: &str = "iptv-org";

/// Per-source link state (design §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LinkStatus {
    /// Still present, headers and labels unchanged.
    InSync,
    /// Same `channel@feed`, URL/headers/labels changed.
    Updated,
    /// URL gone; other streams exist for the channel.
    StreamRemoved,
    /// Channel closed (`replaced_by` offered when present).
    ChannelClosed,
    /// Channel added to the blocklist.
    Blocklisted,
    /// No match in the catalogue.
    Unlinked,
    /// Operator changed `url` away from `origin.stream_url`.
    Edited,
}

impl LinkStatus {
    /// Wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::Updated => "updated",
            Self::StreamRemoved => "stream_removed",
            Self::ChannelClosed => "channel_closed",
            Self::Blocklisted => "blocklisted",
            Self::Unlinked => "unlinked",
            Self::Edited => "edited",
        }
    }

    /// Whether the operator should look at this source.
    #[must_use]
    pub const fn needs_attention(self) -> bool {
        !matches!(self, Self::InSync | Self::Unlinked)
    }
}

/// What changed upstream (for **Review changes** and auto-apply).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Change {
    /// New stream URL, when it differs from the source's.
    pub url: Option<String>,
    /// New `User-Agent` (`Some(None)` ⇒ cleared).
    pub user_agent: Option<Option<String>>,
    /// New `Referer`.
    pub referrer: Option<Option<String>>,
    /// New catalogue labels, when they differ from `origin.labels`.
    pub labels: Option<Vec<String>>,
    /// Flags the new row carries that the linked stream did not (`geo_blocked`, …).
    pub added_flags: Vec<Flag>,
    /// The index row key of the new stream.
    pub row: String,
}

/// A ranked replacement candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alternative {
    /// Index row key.
    pub k: String,
    /// URL.
    pub url: String,
    /// Quality.
    pub q: Option<String>,
    /// Flags.
    pub flags: Vec<Flag>,
    /// Feed id.
    pub feed: Option<String>,
    /// Channel id.
    pub ch: Option<String>,
    /// Required `User-Agent`.
    pub user_agent: Option<String>,
    /// Required `Referer`.
    pub referrer: Option<String>,
    /// Catalogue labels (`Geo-blocked`, `Not 24/7`).
    pub labels: Vec<String>,
}

/// One source's link evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkInfo {
    /// Source id.
    pub source_id: String,
    /// Status.
    pub status: LinkStatus,
    /// Linked channel.
    pub channel: Option<String>,
    /// Linked feed.
    pub feed: Option<String>,
    /// Follow mode.
    pub follow: &'static str,
    /// Suggested change (for `updated`).
    pub change: Option<Change>,
    /// Replacement candidates (for `stream_removed` / `channel_closed`).
    pub alternatives: Vec<Alternative>,
    /// Replacement channel id (for `channel_closed`).
    pub replaced_by: Option<String>,
    /// RFC 3339 time of the evaluation that produced this (set by the tracker).
    pub checked_at: Option<String>,
}

/// A status transition to announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// Source id.
    pub source_id: String,
    /// Previous status (`None` on first sight).
    pub previous: Option<LinkStatus>,
    /// New status.
    pub status: LinkStatus,
    /// Linked channel.
    pub channel: Option<String>,
    /// Linked feed.
    pub feed: Option<String>,
    /// Replacement channel id.
    pub replaced_by: Option<String>,
}

fn follow_str(f: OriginFollow) -> &'static str {
    match f {
        OriginFollow::Manual => "manual",
        OriginFollow::Notify => "notify",
        OriginFollow::Auto => "auto",
    }
}

/// The source's media URL (`url` of any URL-bearing kind).
#[must_use]
pub fn source_url(source: &Source) -> Option<String> {
    serde_json::to_value(source)
        .ok()?
        .get("url")?
        .as_str()
        .map(str::to_owned)
}

pub(crate) fn source_headers(source: &Source) -> (Option<String>, Option<String>) {
    let v = serde_json::to_value(source).unwrap_or_default();
    let g = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    (g("user_agent"), g("referrer"))
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn rank(row: &IndexRow) -> (u8, u32, usize) {
    let bad = u8::from(
        row.flags
            .iter()
            .any(|f| matches!(f, Flag::Unsupported | Flag::Closed | Flag::Blocklisted)),
    ) + u8::from(row.flags.contains(&Flag::GeoBlocked));
    let q = row
        .q
        .as_deref()
        .and_then(|q| q.trim_end_matches(['p', 'i']).parse::<u32>().ok())
        .unwrap_or(0);
    (bad, u32::MAX - q, row.flags.len())
}

fn alternative(r: &IndexRow) -> Alternative {
    Alternative {
        k: r.k.clone(),
        url: r.url.clone(),
        q: r.q.clone(),
        flags: r.flags.clone(),
        feed: r.feed.clone(),
        ch: r.ch.clone(),
        user_agent: r.ua.clone(),
        referrer: r.referrer.clone(),
        labels: r.labels(),
    }
}

/// Evaluate one source. `None` when the source is not iptv-org-linked.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn evaluate(source: &Source, snapshot: &Snapshot) -> Option<LinkInfo> {
    let origin = source.origin.as_ref().filter(|o| o.provider == PROVIDER)?;
    let url = source_url(source);
    let mut info = LinkInfo {
        source_id: source.id.clone(),
        status: LinkStatus::Unlinked,
        channel: origin.channel.clone(),
        feed: origin.feed.clone(),
        follow: follow_str(origin.follow),
        change: None,
        alternatives: Vec::new(),
        replaced_by: None,
        checked_at: None,
    };
    if let (Some(linked), Some(now)) = (&origin.stream_url, &url) {
        if linked != now {
            info.status = LinkStatus::Edited;
            return Some(info);
        }
    }
    let lookup_url = origin.stream_url.clone().or(url);
    let in_channel: Vec<&IndexRow> = origin
        .channel
        .as_deref()
        .map(|ch| {
            snapshot
                .rows
                .iter()
                .filter(|r| r.ch.as_deref() == Some(ch))
                .collect()
        })
        .unwrap_or_default();
    if in_channel
        .iter()
        .any(|r| r.flags.contains(&Flag::Blocklisted))
    {
        info.status = LinkStatus::Blocklisted;
        return Some(info);
    }
    if let Some(closed) = in_channel.iter().find(|r| r.flags.contains(&Flag::Closed)) {
        info.status = LinkStatus::ChannelClosed;
        info.replaced_by.clone_from(&closed.replaced_by);
        let mut alts: Vec<&IndexRow> = closed
            .replaced_by
            .as_deref()
            .map(|rep| {
                snapshot
                    .rows
                    .iter()
                    .filter(|r| r.ch.as_deref() == Some(rep))
                    .collect()
            })
            .unwrap_or_default();
        alts.sort_by_key(|r| rank(r));
        info.alternatives = alts.into_iter().take(8).map(alternative).collect();
        return Some(info);
    }
    let same_feed: Vec<&IndexRow> = in_channel
        .iter()
        .copied()
        .filter(|r| origin.feed.is_none() || r.feed == origin.feed)
        .collect();
    let present = lookup_url.as_deref().and_then(|u| {
        snapshot
            .rows
            .iter()
            .find(|r| r.url == u && (origin.channel.is_none() || r.ch == origin.channel))
    });
    if let Some(row) = present {
        let (ua, rf) = source_headers(source);
        let labels_changed = sorted(origin.labels.clone()) != sorted(row.labels());
        let headers_changed = ua != row.ua || rf != row.referrer;
        if labels_changed || headers_changed {
            let linked_flags_geo = origin
                .labels
                .iter()
                .any(|l| l.eq_ignore_ascii_case("geo-blocked"));
            let mut added = Vec::new();
            if row.flags.contains(&Flag::GeoBlocked) && !linked_flags_geo {
                added.push(Flag::GeoBlocked);
            }
            if row.flags.contains(&Flag::Unsupported) {
                added.push(Flag::Unsupported);
            }
            info.status = LinkStatus::Updated;
            info.change = Some(Change {
                url: None,
                user_agent: (ua != row.ua).then(|| row.ua.clone()),
                referrer: (rf != row.referrer).then(|| row.referrer.clone()),
                labels: labels_changed.then(|| row.labels()),
                added_flags: added,
                row: row.k.clone(),
            });
        } else {
            info.status = LinkStatus::InSync;
        }
        return Some(info);
    }
    // URL no longer listed.
    if origin.channel.is_none() {
        info.status = LinkStatus::Unlinked;
        return Some(info);
    }
    let mut candidates: Vec<&IndexRow> = if same_feed.is_empty() {
        in_channel.clone()
    } else {
        same_feed.clone()
    };
    candidates.sort_by_key(|r| rank(r));
    match (candidates.len(), same_feed.len()) {
        (0, _) => info.status = LinkStatus::Unlinked,
        (1, 1) => {
            let Some(row) = candidates.first().copied() else {
                return Some(info);
            };
            let (ua, rf) = source_headers(source);
            let mut added = Vec::new();
            if row.flags.contains(&Flag::GeoBlocked)
                && !origin
                    .labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case("geo-blocked"))
            {
                added.push(Flag::GeoBlocked);
            }
            if row.flags.contains(&Flag::Unsupported) {
                added.push(Flag::Unsupported);
            }
            info.status = LinkStatus::Updated;
            info.change = Some(Change {
                url: Some(row.url.clone()),
                user_agent: (ua != row.ua).then(|| row.ua.clone()),
                referrer: (rf != row.referrer).then(|| row.referrer.clone()),
                labels: (sorted(origin.labels.clone()) != sorted(row.labels()))
                    .then(|| row.labels()),
                added_flags: added,
                row: row.k.clone(),
            });
        }
        _ => {
            info.status = LinkStatus::StreamRemoved;
            info.alternatives = candidates.into_iter().take(8).map(alternative).collect();
        }
    }
    Some(info)
}

/// Apply a [`Change`] to a source document (JSON), also re-pinning `origin`.
///
/// # Errors
///
/// A message when the source does not round-trip.
pub fn apply_change(source: &Source, change: &Change, now_rfc3339: &str) -> Result<Source, String> {
    let mut v = serde_json::to_value(source).map_err(|e| e.to_string())?;
    let obj = v.as_object_mut().ok_or("source is not an object")?;
    if let Some(u) = &change.url {
        obj.insert("url".into(), serde_json::Value::String(u.clone()));
    }
    for (key, val) in [
        ("user_agent", &change.user_agent),
        ("referrer", &change.referrer),
    ] {
        if let Some(new) = val {
            match new {
                Some(s) => obj.insert(key.into(), serde_json::Value::String(s.clone())),
                None => obj.remove(key),
            };
        }
    }
    if let Some(origin) = obj.get_mut("origin").and_then(|o| o.as_object_mut()) {
        if let Some(u) = &change.url {
            origin.insert("stream_url".into(), serde_json::Value::String(u.clone()));
        }
        if let Some(l) = &change.labels {
            origin.insert("labels".into(), serde_json::json!(l));
        }
        origin.insert(
            "linked_at".into(),
            serde_json::Value::String(now_rfc3339.to_owned()),
        );
    }
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Whether `follow = auto` may apply this evaluation unattended: only `updated`,
/// never adding `geo_blocked`/`unsupported`.
#[must_use]
pub fn auto_applicable(info: &LinkInfo) -> Option<&Change> {
    if info.follow != "auto" || info.status != LinkStatus::Updated {
        return None;
    }
    let change = info.change.as_ref()?;
    if change
        .added_flags
        .iter()
        .any(|f| matches!(f, Flag::GeoBlocked | Flag::Unsupported))
    {
        return None;
    }
    Some(change)
}

/// Remembers the last evaluation per source so transitions can be announced.
#[derive(Debug, Default)]
pub struct LinkTracker {
    last: RwLock<HashMap<String, LinkInfo>>,
    warned: std::sync::atomic::AtomicBool,
}

impl LinkTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-evaluate all `sources`; returns the transitions since the previous run
    /// and replaces the remembered state (sources no longer linked are forgotten).
    pub fn recompute(&self, sources: &[Source], snapshot: &Snapshot) -> Vec<Transition> {
        let checked_at = super::store::now_rfc3339();
        let infos: Vec<LinkInfo> = sources
            .iter()
            .filter_map(|s| evaluate(s, snapshot))
            .map(|mut i| {
                i.checked_at = Some(checked_at.clone());
                i
            })
            .collect();
        let mut g = self
            .last
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for info in &infos {
            let prev = g.get(&info.source_id).map(|p| p.status);
            if prev != Some(info.status) && (prev.is_some() || info.status != LinkStatus::InSync) {
                out.push(Transition {
                    source_id: info.source_id.clone(),
                    previous: prev,
                    status: info.status,
                    channel: info.channel.clone(),
                    feed: info.feed.clone(),
                    replaced_by: info.replaced_by.clone(),
                });
            }
        }
        *g = infos
            .into_iter()
            .map(|i| (i.source_id.clone(), i))
            .collect();
        out
    }

    /// Remember that the aggregate health warning is raised.
    pub fn set_warning_latch(&self) {
        self.warned
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Clear the raised-warning latch, returning whether it was set.
    pub fn take_warning_latch(&self) -> bool {
        self.warned.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// The remembered evaluations.
    #[must_use]
    pub fn all(&self) -> Vec<LinkInfo> {
        let g = self
            .last
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v: Vec<LinkInfo> = g.values().cloned().collect();
        v.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        v
    }

    /// Status tally (for `GET status`).
    #[must_use]
    pub fn tally(&self, visible: impl Fn(&str) -> bool) -> BTreeMap<String, u64> {
        let mut m = BTreeMap::new();
        for i in self.all().iter().filter(|i| visible(&i.source_id)) {
            *m.entry(i.status.as_str().to_owned()).or_insert(0) += 1;
        }
        m
    }

    /// Number of sources needing attention.
    #[must_use]
    pub fn attention_count(&self) -> usize {
        self.all()
            .iter()
            .filter(|i| i.status.needs_attention())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::gazetteer::Gazetteer;
    use crate::catalogue::index::build_snapshot;
    use crate::catalogue::model::RawCatalogue;

    fn snapshot(streams: &str, channels: &str, blocklist: &str) -> Snapshot {
        let mut raw = RawCatalogue::default();
        raw.set_file("streams", streams.as_bytes()).unwrap();
        raw.set_file("channels", channels.as_bytes()).unwrap();
        raw.set_file("blocklist", blocklist.as_bytes()).unwrap();
        raw.set_file("feeds", br#"[{"channel":"A.us","id":"SD","is_main":true,"broadcast_area":["c/US"],"languages":["eng"]}]"#).unwrap();
        build_snapshot(&raw, &Gazetteer::default(), "e".into(), "t".into())
    }

    const CH: &str = r#"[{"id":"A.us","name":"A","country":"US","categories":["news"]},{"id":"B.us","name":"B","country":"US"}]"#;

    fn src(url: &str, extra: &str) -> Source {
        serde_json::from_str(&format!(
            r#"{{"id":"s1","kind":"hls","url":"{url}","origin":{{"provider":"iptv-org","channel":"A.us","feed":"SD","stream_url":"https://a/1.m3u8"{extra}}}}}"#
        ))
        .unwrap()
    }

    fn stream(url: &str, extra: &str) -> String {
        stream_q(url, "720p", extra)
    }

    fn stream_q(url: &str, quality: &str, extra: &str) -> String {
        format!(
            r#"{{"channel":"A.us","feed":"SD","title":"A","url":"{url}","quality":"{quality}"{extra}}}"#
        )
    }

    #[test]
    fn in_sync_updated_removed_edited_unlinked() {
        // in_sync
        let snap = snapshot(
            &format!("[{}]", stream("https://a/1.m3u8", r#","labels":[]"#)),
            CH,
            "[]",
        );
        assert_eq!(
            evaluate(&src("https://a/1.m3u8", ""), &snap)
                .unwrap()
                .status,
            LinkStatus::InSync
        );
        // updated: header added upstream for the same URL
        let snap = snapshot(
            &format!(
                "[{}]",
                stream("https://a/1.m3u8", r#","user_agent":"UA/2""#)
            ),
            CH,
            "[]",
        );
        let info = evaluate(&src("https://a/1.m3u8", ""), &snap).unwrap();
        assert_eq!(info.status, LinkStatus::Updated);
        assert_eq!(
            info.change.unwrap().user_agent,
            Some(Some("UA/2".to_owned()))
        );
        // updated: the single stream's URL moved
        let snap = snapshot(&format!("[{}]", stream("https://a/2.m3u8", "")), CH, "[]");
        let info = evaluate(&src("https://a/1.m3u8", ""), &snap).unwrap();
        assert_eq!(info.status, LinkStatus::Updated);
        assert_eq!(
            info.change.as_ref().unwrap().url.as_deref(),
            Some("https://a/2.m3u8")
        );
        // stream_removed: URL gone, several alternatives remain (ranked, best first)
        let two = format!(
            "[{},{}]",
            stream_q("https://a/9.m3u8", "360p", ""),
            stream("https://a/8.m3u8", "")
        );
        let snap = snapshot(&two, CH, "[]");
        let info = evaluate(&src("https://a/1.m3u8", ""), &snap).unwrap();
        assert_eq!(info.status, LinkStatus::StreamRemoved);
        assert_eq!(info.alternatives.len(), 2);
        // edited: operator changed the url
        let snap = snapshot(&format!("[{}]", stream("https://a/1.m3u8", "")), CH, "[]");
        assert_eq!(
            evaluate(&src("https://mine/x.m3u8", ""), &snap)
                .unwrap()
                .status,
            LinkStatus::Edited
        );
        // unlinked: channel not in the catalogue at all
        let snap = snapshot(
            &format!(
                "[{}]",
                stream("https://z/1.m3u8", "").replace("A.us", "B.us")
            ),
            CH,
            "[]",
        );
        assert_eq!(
            evaluate(&src("https://a/1.m3u8", ""), &snap)
                .unwrap()
                .status,
            LinkStatus::Unlinked
        );
        // not linked → not tracked
        let plain: Source =
            serde_json::from_str(r#"{"id":"p","kind":"hls","url":"https://a/1.m3u8"}"#).unwrap();
        assert!(evaluate(&plain, &snap).is_none());
    }

    #[test]
    fn closed_and_blocklisted() {
        let closed = r#"[{"id":"A.us","name":"A","closed":"2025-01-01","replaced_by":"B.us"},{"id":"B.us","name":"B"}]"#;
        let streams = format!(
            "[{},{}]",
            stream("https://a/1.m3u8", ""),
            stream("https://b/1.m3u8", "").replace("A.us", "B.us")
        );
        let snap = snapshot(&streams, closed, "[]");
        let info = evaluate(&src("https://a/1.m3u8", ""), &snap).unwrap();
        assert_eq!(info.status, LinkStatus::ChannelClosed);
        assert_eq!(info.replaced_by.as_deref(), Some("B.us"));
        assert_eq!(
            info.alternatives.len(),
            1,
            "replacement channel's streams are offered"
        );
        let snap = snapshot(
            &format!("[{}]", stream("https://a/1.m3u8", "")),
            CH,
            r#"[{"channel":"A.us","reason":"dmca"}]"#,
        );
        assert_eq!(
            evaluate(&src("https://a/1.m3u8", ""), &snap)
                .unwrap()
                .status,
            LinkStatus::Blocklisted
        );
    }

    #[test]
    fn auto_follow_applies_only_updated_and_never_adds_geo_blocked() {
        let snap = snapshot(&format!("[{}]", stream("https://a/2.m3u8", "")), CH, "[]");
        let auto = src("https://a/1.m3u8", r#","follow":"auto""#);
        let info = evaluate(&auto, &snap).unwrap();
        assert!(auto_applicable(&info).is_some());
        // notify (default) and manual never auto-apply.
        assert!(auto_applicable(&evaluate(&src("https://a/1.m3u8", ""), &snap).unwrap()).is_none());
        assert!(auto_applicable(
            &evaluate(&src("https://a/1.m3u8", r#","follow":"manual""#), &snap).unwrap()
        )
        .is_none());
        // A change that would add geo_blocked is refused even under auto.
        let geo = snapshot(
            &format!(
                "[{}]",
                stream("https://a/2.m3u8", r#","labels":["Geo-blocked"]"#)
            ),
            CH,
            "[]",
        );
        let info = evaluate(&auto, &geo).unwrap();
        assert_eq!(info.status, LinkStatus::Updated);
        assert!(
            auto_applicable(&info).is_none(),
            "must not add geo_blocked unattended"
        );
        // stream_removed is never auto-applied (no channel switching).
        let two = format!(
            "[{},{}]",
            stream("https://a/8.m3u8", ""),
            stream("https://a/9.m3u8", "")
        );
        let info = evaluate(&auto, &snapshot(&two, CH, "[]")).unwrap();
        assert_eq!(info.status, LinkStatus::StreamRemoved);
        assert!(auto_applicable(&info).is_none());
    }

    #[test]
    fn apply_change_rewrites_url_headers_and_repins_origin() {
        let snap = snapshot(
            &format!(
                "[{}]",
                stream("https://a/2.m3u8", r#","user_agent":"UA/9""#)
            ),
            CH,
            "[]",
        );
        let s = src("https://a/1.m3u8", "");
        let info = evaluate(&s, &snap).unwrap();
        let next = apply_change(&s, info.change.as_ref().unwrap(), "2026-11-07T00:00:00Z").unwrap();
        next.validate().unwrap();
        assert_eq!(source_url(&next).as_deref(), Some("https://a/2.m3u8"));
        assert_eq!(
            evaluate(&next, &snap).unwrap().status,
            LinkStatus::InSync,
            "settles to in_sync after apply"
        );
        let o = next.origin.unwrap();
        assert_eq!(o.stream_url.as_deref(), Some("https://a/2.m3u8"));
        assert_eq!(o.linked_at.as_deref(), Some("2026-11-07T00:00:00Z"));
    }

    #[test]
    fn tracker_reports_transitions_once() {
        let t = LinkTracker::new();
        let s = src("https://a/1.m3u8", "");
        let good = snapshot(&format!("[{}]", stream("https://a/1.m3u8", "")), CH, "[]");
        assert!(
            t.recompute(std::slice::from_ref(&s), &good).is_empty(),
            "first in_sync is silent"
        );
        let moved = snapshot(&format!("[{}]", stream("https://a/2.m3u8", "")), CH, "[]");
        let tr = t.recompute(std::slice::from_ref(&s), &moved);
        assert_eq!(tr.len(), 1);
        assert_eq!(
            (tr[0].previous, tr[0].status),
            (Some(LinkStatus::InSync), LinkStatus::Updated)
        );
        assert!(
            t.recompute(std::slice::from_ref(&s), &moved).is_empty(),
            "no repeat while unchanged"
        );
        assert_eq!(t.attention_count(), 1);
        let tr = t.recompute(std::slice::from_ref(&s), &good);
        assert_eq!(tr[0].status, LinkStatus::InSync);
        assert_eq!(t.attention_count(), 0);
    }
}

/// Wire shapes of `GET .../links` (the web contract's `LinksResponse`).
pub mod wire {
    use serde::Serialize;

    use super::{LinkInfo, LinkStatus};
    use crate::catalogue::index::{Flag, IndexRow, Snapshot};

    /// URL/headers/labels triple compared in "Review changes".
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct LinkTarget {
        /// Stream URL.
        pub url: String,
        /// `User-Agent`.
        pub user_agent: Option<String>,
        /// `Referer`.
        pub referrer: Option<String>,
        /// Labels.
        pub labels: Vec<String>,
    }

    /// A ranked alternative stream.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct LinkAlternative {
        /// URL.
        pub url: String,
        /// `User-Agent`.
        pub user_agent: Option<String>,
        /// `Referer`.
        pub referrer: Option<String>,
        /// Labels.
        pub labels: Vec<String>,
        /// Row key.
        pub k: String,
        /// Channel id.
        pub ch: Option<String>,
        /// Feed id.
        pub feed: Option<String>,
        /// Quality.
        pub q: Option<String>,
        /// Flags.
        pub flags: Vec<Flag>,
        /// Latest cached probe verdict for this URL, if one exists.
        pub verdict: Option<crate::catalogue::probe::Verdict>,
    }

    /// One link as the web reads it.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    pub struct WireLink {
        /// Source id.
        pub source_id: String,
        /// Status.
        pub status: LinkStatus,
        /// Evaluation time.
        pub checked_at: Option<String>,
        /// What upstream now says (for `updated`).
        pub upstream: Option<LinkTarget>,
        /// Replacement channel id.
        pub replaced_by: Option<String>,
        /// Ranked alternatives.
        pub alternatives: Vec<LinkAlternative>,
    }

    fn target(row: &IndexRow) -> LinkTarget {
        LinkTarget {
            url: row.url.clone(),
            user_agent: row.ua.clone(),
            referrer: row.referrer.clone(),
            labels: row.labels(),
        }
    }

    /// Convert an evaluation; `verdict_of` looks up a cached probe verdict for an alternative.
    #[must_use]
    pub fn to_wire(
        info: &LinkInfo,
        snapshot: Option<&Snapshot>,
        verdict_of: &dyn Fn(&LinkAlternative) -> Option<crate::catalogue::probe::Verdict>,
    ) -> WireLink {
        let upstream = info
            .change
            .as_ref()
            .and_then(|c| snapshot?.row(&c.row))
            .map(target);
        WireLink {
            source_id: info.source_id.clone(),
            status: info.status,
            checked_at: info.checked_at.clone(),
            upstream,
            replaced_by: info.replaced_by.clone(),
            alternatives: info
                .alternatives
                .iter()
                .map(|a| {
                    let mut alt = LinkAlternative {
                        url: a.url.clone(),
                        user_agent: a.user_agent.clone(),
                        referrer: a.referrer.clone(),
                        labels: a.labels.clone(),
                        k: a.k.clone(),
                        ch: a.ch.clone(),
                        feed: a.feed.clone(),
                        q: a.q.clone(),
                        flags: a.flags.clone(),
                        verdict: None,
                    };
                    alt.verdict = verdict_of(&alt);
                    alt
                })
                .collect(),
        }
    }
}
