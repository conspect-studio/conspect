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
fn masters() -> &'static Mutex<HashMap<String, (String, String)>> {
    static MASTERS: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
    MASTERS.get_or_init(Default::default)
}

/// Record `master` as the current resolved HLS master of source `id`, resolved
/// from `watch_url`.
pub fn record_master(id: &str, watch_url: &str, master: &str) {
    masters()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.to_owned(), (watch_url.to_owned(), master.to_owned()));
}

/// The current resolved master of source `id`, only when it was resolved from
/// `watch_url`.
#[must_use]
pub fn master(id: &str, watch_url: &str) -> Option<String> {
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
        record_master(id, "https://youtu.be/a", "https://g.test/a.m3u8");
        assert_eq!(
            master(id, "https://youtu.be/a"),
            Some("https://g.test/a.m3u8".to_owned())
        );
        assert_eq!(master(id, "https://youtu.be/b"), None);
        record(id, Some("failed"));
        forget(id);
        assert_eq!(master(id, "https://youtu.be/a"), None);
        assert_eq!(last_error(id), None);
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
