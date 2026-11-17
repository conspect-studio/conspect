//! Process-wide **last resolve status** registry for `youtube` sources (ADR-0015).
//!
//! When a `youtube` source's `yt-dlp` resolve fails, the ingest thread already
//! logs the honest reason via `tracing` (`YoutubeError::operator_message`), but
//! nothing surfaces it to the operator-facing API/UI — the tile's lifecycle
//! state alone (`NO_SIGNAL`) cannot say *why*. This module mirrors the
//! `av_sync` registry pattern: a tiny process-wide map keyed by source id,
//! written lock-free by the ingest thread after every resolve attempt and read
//! by the control-plane snapshot fold (off the hot loop, invariant #10).
//!
//! A successful resolve clears the stored reason (the source is healthy again);
//! a failed resolve stores the latest [`YoutubeError::operator_message`] text.
//! Only the latest attempt is kept — this is observational status, not an
//! event log.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

fn registry() -> &'static Mutex<HashMap<String, String>> {
    static LAST_ERROR: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    LAST_ERROR.get_or_init(Default::default)
}

/// Record the outcome of a `youtube` source's resolve attempt for `id`:
/// `Some(reason)` stores the operator-facing failure text (already run through
/// [`multiview_input::youtube::YoutubeError::operator_message`] by the caller);
/// `None` (a successful resolve) clears any previously stored reason.
pub fn record(id: &str, reason: Option<&str>) {
    let mut map = registry().lock().unwrap_or_else(PoisonError::into_inner);
    match reason {
        Some(reason) => {
            map.insert(id.to_owned(), reason.to_owned());
        }
        None => {
            map.remove(id);
        }
    }
}

/// The last resolve-failure reason recorded for source `id`, or `None` when the
/// source has no recorded failure (never resolved, or last resolved OK).
#[must_use]
pub fn last_error(id: &str) -> Option<String> {
    let map = registry().lock().unwrap_or_else(PoisonError::into_inner);
    map.get(id).cloned()
}

/// Every source id with a currently-recorded resolve-failure reason, for the
/// status-API fold ([`crate::control::fold_tile_states`]).
#[must_use]
pub fn last_errors() -> HashMap<String, String> {
    registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The latest resolved HLS master per source id, with the watch URL it was
/// resolved from (issue #420): the video rail writes it after every successful
/// resolve (inline or the proactive re-resolution loop), and the audio rail
/// reads it so audio opens the SAME master as video without a second `yt-dlp`
/// run. Keyed by the watch URL too, so an edit to a new URL never hands audio
/// the previous stream's master.
fn masters() -> &'static Mutex<HashMap<String, (String, ResolvedMaster)>> {
    static MASTERS: OnceLock<Mutex<HashMap<String, (String, ResolvedMaster)>>> = OnceLock::new();
    MASTERS.get_or_init(Default::default)
}

/// A resolved `YouTube` HLS master and the address family every request for it
/// must use (issue #498): googlevideo signs the master for the client address
/// that resolved it and refuses segments fetched from the other family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMaster {
    /// The `*.googlevideo.com` HLS master URL.
    pub url: String,
    /// The family pin for the master, its variants and its segments.
    pub pin: crate::hls_fetch::IpPin,
}

impl ResolvedMaster {
    /// The master and pin of one resolve: the family the URL is signed for,
    /// else the family the resolve was forced to, else no pin.
    #[must_use]
    pub fn from_resolved(resolved: &multiview_input::youtube::ResolvedHls) -> Self {
        Self {
            url: multiview_input::youtube::ingest_url(resolved).to_owned(),
            pin: crate::hls_fetch::IpPin::from_media(resolved.media_family()),
        }
    }
}

/// Record `master` as the current resolved HLS master of source `id`, resolved
/// from `watch_url`.
pub fn record_master(id: &str, watch_url: &str, master: &ResolvedMaster) {
    masters()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.to_owned(), (watch_url.to_owned(), master.clone()));
}

/// The current resolved master of source `id`, only when it was resolved from
/// `watch_url`.
#[must_use]
pub fn master(id: &str, watch_url: &str) -> Option<ResolvedMaster> {
    masters()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(id)
        .filter(|(watch, _)| watch == watch_url)
        .map(|(_, master)| master.clone())
}

/// Forget every record of source `id` (it was removed).
pub fn forget(id: &str) {
    masters()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(id);
    record(id, None);
}

/// The run's explicit `yt-dlp` path (`MULTIVIEW_YOUTUBE_YT_DLP_PATH`, else
/// `[system.youtube] yt_dlp_path`), set when the pipeline is built so a source
/// added live and the audio rail resolve with the same binary as startup.
fn resolver_path_slot() -> &'static Mutex<Option<String>> {
    static PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PATH.get_or_init(Default::default)
}

/// Set the run's explicit `yt-dlp` path (`None` resolves `yt-dlp` on `PATH`).
pub fn set_resolver_path(path: Option<String>) {
    *resolver_path_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = path;
}

/// The run's explicit `yt-dlp` path, if one is configured.
#[must_use]
pub fn resolver_path() -> Option<String> {
    resolver_path_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The run's default resolve address family (`[system.youtube]
/// address_family`, issue #498), for sources without their own.
fn resolver_family_slot() -> &'static Mutex<multiview_input::youtube::ResolveFamily> {
    static FAMILY: OnceLock<Mutex<multiview_input::youtube::ResolveFamily>> = OnceLock::new();
    FAMILY.get_or_init(Default::default)
}

/// Set the run's default resolve address family.
pub fn set_resolver_family(family: multiview_input::youtube::ResolveFamily) {
    *resolver_family_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = family;
}

/// The run's default resolve address family (`auto` unless configured).
#[must_use]
pub fn resolver_family() -> multiview_input::youtube::ResolveFamily {
    *resolver_family_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The resolve family of one source: its own setting, else the run default.
#[must_use]
pub fn effective_family(
    own: Option<multiview_input::youtube::ResolveFamily>,
) -> multiview_input::youtube::ResolveFamily {
    own.unwrap_or_else(resolver_family)
}

/// Map the config spelling onto the resolver's family.
#[must_use]
pub fn family_from_config(
    family: multiview_config::YoutubeAddressFamily,
) -> multiview_input::youtube::ResolveFamily {
    use multiview_config::YoutubeAddressFamily as Config;
    use multiview_input::youtube::ResolveFamily as Resolve;
    match family {
        Config::Auto => Resolve::Auto,
        Config::V4 => Resolve::V4,
        Config::V6 => Resolve::V6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_read_a_failure_reason() {
        let id = "youtube-status-test-record-read";
        record(id, Some("not a live stream (status: upcoming)"));
        assert_eq!(
            last_error(id),
            Some("not a live stream (status: upcoming)".to_owned())
        );
    }

    #[test]
    fn a_successful_resolve_clears_the_stored_reason() {
        let id = "youtube-status-test-clear";
        record(id, Some("yt-dlp not found on the server"));
        assert!(last_error(id).is_some());
        record(id, None);
        assert_eq!(last_error(id), None);
    }

    #[test]
    fn a_master_is_only_handed_out_for_the_watch_url_it_came_from() {
        let id = "youtube-status-test-master";
        let a = ResolvedMaster {
            url: "https://g.test/a.m3u8".to_owned(),
            pin: crate::hls_fetch::IpPin::V4,
        };
        record_master(id, "https://youtu.be/a", &a);
        assert_eq!(master(id, "https://youtu.be/a"), Some(a));
        assert_eq!(master(id, "https://youtu.be/b"), None);
        record(id, Some("failed"));
        forget(id);
        assert_eq!(master(id, "https://youtu.be/a"), None);
        assert_eq!(last_error(id), None);
    }

    #[test]
    fn a_resolved_master_carries_the_family_its_url_is_signed_for() {
        use multiview_input::youtube::{LiveStatus, ResolveFamily, ResolvedHls};
        let v4 = ResolvedHls::new(
            "https://m.googlevideo.com/api/manifest/hls_variant/ip/103.135.99.52/x.m3u8",
            LiveStatus::Live,
            None,
        );
        assert_eq!(
            ResolvedMaster::from_resolved(&v4).pin,
            crate::hls_fetch::IpPin::V4
        );
        let v6 = ResolvedHls::new(
            "https://m.googlevideo.com/api/manifest/hls_variant/ip/2404%3A79c0%3A%3A1/x.m3u8",
            LiveStatus::Live,
            None,
        )
        .with_resolved_over(ResolveFamily::V4);
        assert_eq!(
            ResolvedMaster::from_resolved(&v6).pin,
            crate::hls_fetch::IpPin::V6,
            "the signed address wins over the resolve family"
        );
        let unsigned = ResolvedHls::new("https://h/x.m3u8", LiveStatus::Live, None);
        assert_eq!(
            ResolvedMaster::from_resolved(&unsigned).pin,
            crate::hls_fetch::IpPin::Any
        );
        assert_eq!(
            ResolvedMaster::from_resolved(&unsigned.with_resolved_over(ResolveFamily::V6)).pin,
            crate::hls_fetch::IpPin::V6
        );
    }

    #[test]
    fn a_source_family_overrides_the_run_default() {
        use multiview_input::youtube::ResolveFamily;
        assert_eq!(effective_family(Some(ResolveFamily::V6)), ResolveFamily::V6);
        assert_eq!(
            family_from_config(multiview_config::YoutubeAddressFamily::V4),
            ResolveFamily::V4
        );
        assert_eq!(
            family_from_config(multiview_config::YoutubeAddressFamily::default()),
            ResolveFamily::Auto
        );
    }

    #[test]
    fn unknown_source_has_no_recorded_reason() {
        assert_eq!(last_error("youtube-status-test-unknown-source"), None);
    }

    #[test]
    fn last_errors_includes_only_currently_failing_sources() {
        let failing = "youtube-status-test-bulk-failing";
        let healthy = "youtube-status-test-bulk-healthy";
        record(failing, Some("yt-dlp not found on the server"));
        record(healthy, Some("transient"));
        record(healthy, None);

        let all = last_errors();
        assert_eq!(
            all.get(failing).map(String::as_str),
            Some("yt-dlp not found on the server")
        );
        assert!(!all.contains_key(healthy));
    }
}
