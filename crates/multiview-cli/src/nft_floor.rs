//! The nft UDP-443 floor probe (ADR-0107 design note sections 10, 10.1 and
//! 11; `deploy/nftables/conspect-udp443-floor.nft`).
//!
//! The engine runs unprivileged (no `CAP_NET_ADMIN`) and cannot list the
//! nftables ruleset itself. The shipped systemd unit's `ExecStartPre=+`
//! (root) runs `nft -j list table inet conspect_floor` and writes the result
//! to `network.udp.nft_floor_status_file` (default
//! `/run/conspect/nft-floor.json`); this module is a **pure file parser**
//! over that file — a cheap read on the front-door runtime, never on the
//! program runtime (invariant #1), re-read every
//! [`NFT_FLOOR_REREAD_INTERVAL`].

use std::path::Path;
use std::time::Duration;

/// How often the front-door runtime re-reads the nft-floor status file
/// (design note section 3: "Re-read every 10 minutes on the front-door
/// runtime").
pub const NFT_FLOOR_REREAD_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The floor status this probe reports (mirrors
/// `multiview_control::system::NftFloorStatus`; kept as its own type here so
/// this module has no dependency on `multiview-control`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NftFloorStatus {
    /// The expected meter/set (`conspect_udp443_new`, or `_v6`) was found in
    /// `table inet conspect_floor`.
    Present,
    /// The status file said the table is absent (nft's own
    /// "No such file or directory" error text), or explicitly says `absent`.
    Absent,
    /// The file is missing, unreadable, or its content does not parse.
    Unknown,
}

/// Read and parse `path` (see module docs). Never panics: any I/O or parse
/// failure is [`NftFloorStatus::Unknown`].
#[must_use]
pub fn probe(path: &Path) -> NftFloorStatus {
    match std::fs::read_to_string(path) {
        Ok(content) => parse(&content),
        Err(_) => NftFloorStatus::Unknown,
    }
}

/// Parse the nft-floor status file's content (see [`probe`]). Exposed for
/// fixture-based unit tests.
#[must_use]
pub fn parse(content: &str) -> NftFloorStatus {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return NftFloorStatus::Unknown;
    }
    // The literal fallback the unit writes when it chooses not to run nft at
    // all, and nft's own error text for a table that was never installed
    // (or was removed) — both mean "absent", not "unknown": the probe ran,
    // and it found nothing.
    if trimmed == "absent" || trimmed.contains("No such file or directory") {
        return NftFloorStatus::Absent;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return NftFloorStatus::Unknown;
    };
    if table_has_conspect_floor_meter(&value) {
        NftFloorStatus::Present
    } else {
        NftFloorStatus::Absent
    }
}

/// Walk `nft -j list table ...`'s `{"nftables": [...]}` array for a `set`
/// (nft's JSON reports a meter as a `set` with `dynamic`/`timeout` flags)
/// named `conspect_udp443_new` (or any name starting with it, covering the
/// shipped file's separate v4/v6 meters) inside `table inet conspect_floor`.
fn table_has_conspect_floor_meter(value: &serde_json::Value) -> bool {
    let Some(items) = value.get("nftables").and_then(serde_json::Value::as_array) else {
        return false;
    };
    items.iter().any(|item| {
        let Some(set) = item.get("set") else {
            return false;
        };
        let family = set.get("family").and_then(serde_json::Value::as_str);
        let table = set.get("table").and_then(serde_json::Value::as_str);
        let name = set.get("name").and_then(serde_json::Value::as_str);
        family == Some("inet")
            && table == Some("conspect_floor")
            && name.is_some_and(|n| n.starts_with("conspect_udp443_new"))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const PRESENT_FIXTURE: &str = r#"{"nftables": [
        {"metainfo": {"version": "1.1.3"}},
        {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}},
        {"chain": {"family": "inet", "table": "conspect_floor", "name": "input", "handle": 1, "type": "filter", "hook": "input", "prio": -5, "policy": "accept"}},
        {"set": {"family": "inet", "name": "conspect_udp443_new", "table": "conspect_floor", "type": "ipv4_addr", "handle": 3, "size": 65536, "flags": ["timeout", "dynamic"]}},
        {"set": {"family": "inet", "name": "conspect_udp443_new_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 5, "size": 65536, "flags": ["timeout", "dynamic"]}}
    ]}"#;

    #[test]
    fn a_table_with_the_named_meter_is_present() {
        assert_eq!(parse(PRESENT_FIXTURE), NftFloorStatus::Present);
    }

    #[test]
    fn nfts_missing_table_error_text_is_absent() {
        let text = "Error: No such file or directory\nlist table inet conspect_floor\n                ^^^^^^^^^^^^^^\n";
        assert_eq!(parse(text), NftFloorStatus::Absent);
    }

    #[test]
    fn the_literal_fallback_word_is_absent() {
        assert_eq!(parse("absent"), NftFloorStatus::Absent);
        assert_eq!(parse("absent\n"), NftFloorStatus::Absent);
    }

    #[test]
    fn empty_or_unparsable_content_is_unknown() {
        assert_eq!(parse(""), NftFloorStatus::Unknown);
        assert_eq!(parse("   \n"), NftFloorStatus::Unknown);
        assert_eq!(parse("not json at all {"), NftFloorStatus::Unknown);
    }

    #[test]
    fn a_missing_file_is_unknown() {
        assert_eq!(
            probe(Path::new("/nonexistent/path/does/not/exist.json")),
            NftFloorStatus::Unknown
        );
    }

    #[test]
    fn a_wellformed_table_without_the_named_meter_is_absent() {
        // Valid nft JSON, but describes some other table (or the floor's
        // table exists with no meter of ours, e.g. mid-edit): honest
        // "absent", not "present" by table-name alone.
        let json = r#"{"nftables": [
            {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}},
            {"chain": {"family": "inet", "table": "conspect_floor", "name": "input", "handle": 1}}
        ]}"#;
        assert_eq!(parse(json), NftFloorStatus::Absent);
    }

    #[test]
    fn a_meter_in_a_different_table_or_family_does_not_count() {
        let json = r#"{"nftables": [
            {"set": {"family": "ip", "name": "conspect_udp443_new", "table": "conspect_floor", "type": "ipv4_addr"}},
            {"set": {"family": "inet", "name": "conspect_udp443_new", "table": "other_table", "type": "ipv4_addr"}}
        ]}"#;
        assert_eq!(parse(json), NftFloorStatus::Absent);
    }
}
