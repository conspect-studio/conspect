//! The nft UDP-443 floor probe (ADR-0107 design note, "Slice 3 as built,
//! part B"; `deploy/nftables/conspect-udp443-floor.nft`).
//!
//! The engine runs unprivileged (no `CAP_NET_ADMIN`) and cannot list the
//! nftables ruleset itself. The shipped systemd unit's `ExecStartPre=+`
//! (root) runs `nft -j list table inet conspect_floor` and writes the result
//! to `network.udp.nft_floor_status_file` (default
//! `/run/conspect-floor/nft-floor.json`, a root-owned directory the service
//! user cannot modify); this module is a **pure file parser** over that
//! file — a cheap read on the front-door runtime, never on the program
//! runtime (invariant #1), re-read every [`NFT_FLOOR_REREAD_INTERVAL`].
//!
//! Trust model: the file is a *report*, never an enforcement input. [`probe`]
//! only believes a regular file (no symlink) owned by root and not writable
//! by group or other; anything else is [`NftFloorStatus::Unknown`]. That
//! stops the unprivileged service user (or any other non-root user) from
//! forging `present`; a process running as root can still forge it.

use std::path::Path;
use std::time::Duration;

/// How often the front-door runtime re-reads the nft-floor status file
/// (design note, "Slice 3 as built, part B").
pub const NFT_FLOOR_REREAD_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The named dynamic sets `deploy/nftables/conspect-udp443-floor.nft`
/// declares, by their exact names, with the exact nft key `type` each must
/// carry (IPv6 sets hold the source /64 as an `ipv6_addr`).
pub const FLOOR_METERS: [(&str, &str); 4] = [
    ("udp443_new_v4", "ipv4_addr"),
    ("udp443_new_v6", "ipv6_addr"),
    ("udp443_all_v4", "ipv4_addr"),
    ("udp443_all_v6", "ipv6_addr"),
];

/// The per-tier sub-chains of the shipped floor, by exact name. Each one
/// meters a packet against its tier's sets and ends in a verdict, so every
/// UDP 443 packet is charged to exactly one tier.
pub const FLOOR_CHAINS: [&str; 2] = ["udp443_new", "udp443_all"];

/// The owner uid [`probe`] requires of the status file in production: root.
pub const STATUS_FILE_OWNER_UID: u32 = 0;

/// The floor status this probe reports (mirrors
/// `multiview_control::system::NftFloorStatus`; kept as its own type here so
/// this module has no dependency on `multiview-control`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NftFloorStatus {
    /// Valid nft JSON listing `table inet conspect_floor` with every one of
    /// [`FLOOR_METERS`] (name and type) and [`FLOOR_CHAINS`] in it.
    Present,
    /// nft's own error for a table that does not exist (see
    /// [`is_missing_table_error`]), or the literal word `absent`.
    Absent,
    /// Anything else: a missing, unreadable or empty file, a file that is
    /// not trusted (see [`probe`]), unparsable content, another error, or a
    /// listing without the exact table, sets and chains (a partial or
    /// renamed floor is not reported as present).
    Unknown,
}

/// Read and parse `path` (see module docs), trusting it only when it is a
/// regular file (not a symlink) owned by [`STATUS_FILE_OWNER_UID`] (root)
/// and not writable by group or other. Never panics: any I/O, trust or
/// parse failure is [`NftFloorStatus::Unknown`].
#[must_use]
pub fn probe(path: &Path) -> NftFloorStatus {
    probe_owned_by(path, STATUS_FILE_OWNER_UID)
}

/// [`probe`] with the required owner uid as a parameter (tests run as a
/// non-root uid; production always passes [`STATUS_FILE_OWNER_UID`]).
#[must_use]
pub fn probe_owned_by(path: &Path, owner_uid: u32) -> NftFloorStatus {
    read_trusted(path, owner_uid).map_or(NftFloorStatus::Unknown, |content| parse(&content))
}

/// The content of `path` if it passes the trust checks, else `None`.
/// `lstat` first (a symlink is rejected, never followed), then the opened
/// file's own metadata must be the same inode with the same checks, so a
/// swap between the two calls cannot slip a different file in.
#[cfg(unix)]
fn read_trusted(path: &Path, owner_uid: u32) -> Option<String> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;

    let trusted = |meta: &std::fs::Metadata| {
        meta.file_type().is_file() && meta.uid() == owner_uid && meta.mode() & 0o022 == 0
    };
    let link = std::fs::symlink_metadata(path).ok()?;
    if !trusted(&link) {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    if !trusted(&opened) || opened.dev() != link.dev() || opened.ino() != link.ino() {
        return None;
    }
    let mut content = String::new();
    file.read_to_string(&mut content).ok()?;
    Some(content)
}

/// Non-unix builds have no nft and no ownership model: never trusted.
#[cfg(not(unix))]
fn read_trusted(_path: &Path, _owner_uid: u32) -> Option<String> {
    None
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

/// `{"nftables": [...]}` containing `table inet conspect_floor`, a `set`
/// named each of [`FLOOR_METERS`] with that exact `type`, and a `chain`
/// named each of [`FLOOR_CHAINS`], all in that table.
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
    let in_table = |v: &serde_json::Value, name: &str| {
        str_of(v, "family").as_deref() == Some("inet")
            && str_of(v, "table").as_deref() == Some("conspect_floor")
            && str_of(v, "name").as_deref() == Some(name)
    };
    let has_set = |name: &str, ty: &str| {
        items.iter().any(|item| {
            item.get("set").is_some_and(|set| {
                in_table(set, name) && str_of(set, "type").as_deref() == Some(ty)
            })
        })
    };
    let has_chain = |name: &str| {
        items
            .iter()
            .any(|item| item.get("chain").is_some_and(|chain| in_table(chain, name)))
    };
    has_table
        && FLOOR_METERS.iter().all(|(name, ty)| has_set(name, ty))
        && FLOOR_CHAINS.iter().all(|name| has_chain(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `nft -j list table inet conspect_floor` (nft 1.1.3) after loading
    /// the shipped file in a `unshare -Urn` namespace, verbatim except that
    /// the rule objects are omitted (the parser does not read them).
    const PRESENT_FIXTURE: &str = r#"{"nftables": [{"metainfo": {"version": "1.1.3", "release_name": "Commodore Bullmoose #4", "json_schema_version": 1}}, {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "input", "handle": 1, "type": "filter", "hook": "input", "prio": -5, "policy": "accept"}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "udp443_new", "handle": 2}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "udp443_all", "handle": 3}}, {"set": {"family": "inet", "name": "udp443_new_v4", "table": "conspect_floor", "type": "ipv4_addr", "handle": 4, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "udp443_new_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 5, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "udp443_all_v4", "table": "conspect_floor", "type": "ipv4_addr", "handle": 6, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "udp443_all_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 7, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}]}"#;

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

    /// The current (test) uid and a status file it owns, mode 0644.
    #[cfg(unix)]
    fn owned_status_file(dir: &Path, content: &str) -> (std::path::PathBuf, u32) {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let path = dir.join("nft-floor.json");
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        (path, uid)
    }

    #[cfg(unix)]
    #[test]
    fn a_trusted_file_owned_by_the_expected_uid_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let (path, uid) = owned_status_file(dir.path(), PRESENT_FIXTURE);
        assert_eq!(probe_owned_by(&path, uid), NftFloorStatus::Present);
        std::fs::write(&path, MISSING_TABLE).unwrap();
        assert_eq!(probe_owned_by(&path, uid), NftFloorStatus::Absent);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_not_owned_by_the_expected_uid_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let (path, uid) = owned_status_file(dir.path(), PRESENT_FIXTURE);
        assert_eq!(
            probe_owned_by(&path, uid.wrapping_add(1)),
            NftFloorStatus::Unknown
        );
        if uid != STATUS_FILE_OWNER_UID {
            // Production: a file the (non-root) service user wrote itself.
            assert_eq!(probe(&path), NftFloorStatus::Unknown);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_group_or_world_writable_file_is_unknown() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (path, uid) = owned_status_file(dir.path(), PRESENT_FIXTURE);
        for mode in [0o666, 0o646, 0o664, 0o620, 0o602] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                probe_owned_by(&path, uid),
                NftFloorStatus::Unknown,
                "mode {mode:o}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_unknown_even_to_a_trusted_file() {
        let dir = tempfile::tempdir().unwrap();
        let (target, uid) = owned_status_file(dir.path(), PRESENT_FIXTURE);
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(probe_owned_by(&target, uid), NftFloorStatus::Present);
        assert_eq!(probe_owned_by(&link, uid), NftFloorStatus::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_is_unknown() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        assert_eq!(probe_owned_by(dir.path(), uid), NftFloorStatus::Unknown);
    }

    #[test]
    fn each_meter_is_required_by_its_exact_name() {
        for (missing, _) in FLOOR_METERS {
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
        // Each set must carry its exact key type.
        for (name, ty) in FLOOR_METERS {
            let other = if ty == "ipv4_addr" {
                "ipv6_addr"
            } else {
                "ipv4_addr"
            };
            let retyped = PRESENT_FIXTURE.replace(
                &format!("\"name\": \"{name}\", \"table\": \"conspect_floor\", \"type\": \"{ty}\""),
                &format!(
                    "\"name\": \"{name}\", \"table\": \"conspect_floor\", \"type\": \"{other}\""
                ),
            );
            assert_ne!(retyped, PRESENT_FIXTURE, "{name}");
            assert_eq!(parse(&retyped), NftFloorStatus::Unknown, "{name} retyped");
        }
        // Each per-tier chain is required by its exact name.
        for chain in FLOOR_CHAINS {
            let renamed = PRESENT_FIXTURE.replace(
                &format!("\"name\": \"{chain}\", \"handle\""),
                &format!("\"name\": \"{chain}_x\", \"handle\""),
            );
            assert_ne!(renamed, PRESENT_FIXTURE, "{chain}");
            assert_eq!(parse(&renamed), NftFloorStatus::Unknown, "{chain} renamed");
        }
        // The previous meter-based floors (round 1's four meters, the
        // single-tier one) are not the shipped floor.
        let round1 = r#"{"nftables": [
            {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}},
            {"set": {"family": "inet", "name": "conspect_udp443_new_v4", "table": "conspect_floor", "type": "ipv4_addr"}},
            {"set": {"family": "inet", "name": "conspect_udp443_new_v6", "table": "conspect_floor", "type": "ipv6_addr"}},
            {"set": {"family": "inet", "name": "conspect_udp443_all_v4", "table": "conspect_floor", "type": "ipv4_addr"}},
            {"set": {"family": "inet", "name": "conspect_udp443_all_v6", "table": "conspect_floor", "type": "ipv6_addr"}}
        ]}"#;
        assert_eq!(parse(round1), NftFloorStatus::Unknown);
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
