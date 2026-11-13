//! The nft UDP-443 floor probe (ADR-0107 design note, "Slice 3 as built,
//! part B"; `deploy/nftables/conspect-udp443-floor.nft`).
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
/// (design note, "Slice 3 as built, part B").
pub const NFT_FLOOR_REREAD_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The meters `deploy/nftables/conspect-udp443-floor.nft` creates, by their
/// exact names. nft's JSON listing reports each `meter` as a `set` with
/// `dynamic`/`timeout` flags in `table inet conspect_floor`.
pub const FLOOR_METERS: [&str; 4] = [
    "conspect_udp443_new_v4",
    "conspect_udp443_new_v6",
    "conspect_udp443_all_v4",
    "conspect_udp443_all_v6",
];

/// The floor status this probe reports (mirrors
/// `multiview_control::system::NftFloorStatus`; kept as its own type here so
/// this module has no dependency on `multiview-control`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NftFloorStatus {
    /// Valid nft JSON listing `table inet conspect_floor` with every one of
    /// [`FLOOR_METERS`] in it.
    Present,
    /// nft's own error for a table that does not exist (see
    /// [`is_missing_table_error`]), or the literal word `absent`.
    Absent,
    /// Anything else: a missing, unreadable or empty file, unparsable
    /// content, another error, or a listing without the exact table and
    /// meters (a partial or renamed floor is not reported as present).
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
    if trimmed == "absent" || is_missing_table_error(trimmed) {
        return NftFloorStatus::Absent;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return NftFloorStatus::Unknown;
    };
    if lists_the_shipped_floor(&value) {
        NftFloorStatus::Present
    } else {
        NftFloorStatus::Unknown
    }
}

/// nft 1.1.3's exact output for `nft -j list table inet conspect_floor`
/// when the table does not exist (observed, exit status 1):
///
/// ```text
/// Error: No such file or directory
/// list table inet conspect_floor
///                 ^^^^^^^^^^^^^^
/// ```
///
/// Matched as: the first line is exactly `Error: No such file or
/// directory`, and the second line is exactly the echoed command.
fn is_missing_table_error(content: &str) -> bool {
    let mut lines = content.lines().map(str::trim_end);
    lines.next() == Some("Error: No such file or directory")
        && lines.next().map(str::trim) == Some("list table inet conspect_floor")
}

/// `{"nftables": [...]}` containing `table inet conspect_floor` and a `set`
/// named each of [`FLOOR_METERS`] in that table.
fn lists_the_shipped_floor(value: &serde_json::Value) -> bool {
    let Some(items) = value.get("nftables").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let str_of = |v: &serde_json::Value, key: &str| -> Option<String> {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let has_table = items.iter().any(|item| {
        item.get("table").is_some_and(|t| {
            str_of(t, "family").as_deref() == Some("inet")
                && str_of(t, "name").as_deref() == Some("conspect_floor")
        })
    });
    let has_meter = |name: &str| {
        items.iter().any(|item| {
            item.get("set").is_some_and(|set| {
                str_of(set, "family").as_deref() == Some("inet")
                    && str_of(set, "table").as_deref() == Some("conspect_floor")
                    && str_of(set, "name").as_deref() == Some(name)
            })
        })
    };
    has_table && FLOOR_METERS.iter().all(|name| has_meter(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `nft -j list table inet conspect_floor` (nft 1.1.3) after loading
    /// the shipped file in a `unshare -Urn` namespace, verbatim up to the
    /// rules (rule objects omitted: the parser does not read them).
    const PRESENT_FIXTURE: &str = r#"{"nftables": [{"metainfo": {"version": "1.1.3", "release_name": "Commodore Bullmoose #4", "json_schema_version": 1}}, {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "input", "handle": 1, "type": "filter", "hook": "input", "prio": -5, "policy": "accept"}}, {"set": {"family": "inet", "name": "conspect_udp443_new_v4", "table": "conspect_floor", "type": "ipv4_addr", "handle": 2, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "conspect_udp443_new_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 4, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "conspect_udp443_all_v4", "table": "conspect_floor", "type": "ipv4_addr", "handle": 6, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "conspect_udp443_all_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 8, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}]}"#;

    /// nft 1.1.3's stderr for the same command with no such table (observed).
    const MISSING_TABLE: &str =
        "Error: No such file or directory\nlist table inet conspect_floor\n                ^^^^^^^^^^^^^^\n";

    #[test]
    fn the_shipped_floor_listing_is_present() {
        assert_eq!(parse(PRESENT_FIXTURE), NftFloorStatus::Present);
    }

    #[test]
    fn nfts_exact_missing_table_error_is_absent() {
        assert_eq!(parse(MISSING_TABLE), NftFloorStatus::Absent);
    }

    #[test]
    fn the_literal_fallback_word_is_absent() {
        assert_eq!(parse("absent"), NftFloorStatus::Absent);
        assert_eq!(parse("absent\n"), NftFloorStatus::Absent);
    }

    #[test]
    fn other_errors_and_lookalikes_are_unknown() {
        // A different error, the same error for another object, the error
        // text embedded in other output, and words that merely contain it.
        for text in [
            "Error: Operation not permitted\nlist table inet conspect_floor\n",
            "Error: No such file or directory\nlist table inet other_table\n",
            "Error: No such file or directory\n",
            "netlink: Error: No such file or directory\nlist table inet conspect_floor\n",
            "warning\nError: No such file or directory\nlist table inet conspect_floor\n",
            "absent!",
            "not absent",
            "/bin/sh: 1: nft: not found",
        ] {
            assert_eq!(parse(text), NftFloorStatus::Unknown, "{text:?}");
        }
    }

    #[test]
    fn empty_or_unparsable_content_is_unknown() {
        assert_eq!(parse(""), NftFloorStatus::Unknown);
        assert_eq!(parse("   \n"), NftFloorStatus::Unknown);
        assert_eq!(parse("not json at all {"), NftFloorStatus::Unknown);
        assert_eq!(parse(r#"{"other": []}"#), NftFloorStatus::Unknown);
    }

    #[test]
    fn a_missing_file_is_unknown() {
        assert_eq!(
            probe(Path::new("/nonexistent/path/does/not/exist.json")),
            NftFloorStatus::Unknown
        );
    }

    #[test]
    fn each_meter_is_required_by_its_exact_name() {
        for missing in FLOOR_METERS {
            // Dropped entirely.
            let dropped = PRESENT_FIXTURE.replace(&format!("\"{missing}\""), "\"x\"");
            assert_eq!(parse(&dropped), NftFloorStatus::Unknown, "{missing} absent");
            // Renamed to a lookalike (prefix match must not pass).
            let renamed =
                PRESENT_FIXTURE.replace(&format!("\"{missing}\""), &format!("\"{missing}_old\""));
            assert_eq!(
                parse(&renamed),
                NftFloorStatus::Unknown,
                "{missing} renamed"
            );
        }
        // The previous (single-tier) meter names are not the shipped floor.
        let old = r#"{"nftables": [
            {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}},
            {"set": {"family": "inet", "name": "conspect_udp443_new", "table": "conspect_floor", "type": "ipv4_addr"}},
            {"set": {"family": "inet", "name": "conspect_udp443_new_v6", "table": "conspect_floor", "type": "ipv6_addr"}}
        ]}"#;
        assert_eq!(parse(old), NftFloorStatus::Unknown);
    }

    #[test]
    fn the_table_must_be_listed_and_meters_must_be_in_it() {
        let no_table = PRESENT_FIXTURE.replace(
            r#"{"table": {"family": "inet", "name": "conspect_floor", "handle": 1}}, "#,
            "",
        );
        assert_ne!(no_table, PRESENT_FIXTURE);
        assert_eq!(parse(&no_table), NftFloorStatus::Unknown);
        let wrong_family = PRESENT_FIXTURE.replace(r#""family": "inet""#, r#""family": "ip""#);
        assert_eq!(parse(&wrong_family), NftFloorStatus::Unknown);
        let wrong_table =
            PRESENT_FIXTURE.replace(r#""table": "conspect_floor""#, r#""table": "other_table""#);
        assert_eq!(parse(&wrong_table), NftFloorStatus::Unknown);
    }
}
