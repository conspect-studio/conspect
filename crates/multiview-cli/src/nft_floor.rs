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

/// `{"nftables": [...]}` containing `table inet conspect_floor`, a bounded
/// dynamic `set` (`size`, flags `dynamic` + `timeout`) named each of
/// [`FLOOR_METERS`] with that exact `type`, a `chain` named each of
/// [`FLOOR_CHAINS`], and the shipped verdicts in those chains (see
/// [`verdicts_are_the_shipped_ones`]): `present` means the floor fails
/// closed, not merely that its objects exist.
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
    // A bounded dynamic set: `size` present and `dynamic` + `timeout` flags
    // (without them a full set or an idle source behaves differently).
    let has_set = |name: &str, ty: &str| {
        items.iter().any(|item| {
            item.get("set").is_some_and(|set| {
                let flags: Vec<&str> = set
                    .get("flags")
                    .and_then(serde_json::Value::as_array)
                    .map(|f| f.iter().filter_map(serde_json::Value::as_str).collect())
                    .unwrap_or_default();
                in_table(set, name)
                    && str_of(set, "type").as_deref() == Some(ty)
                    && set
                        .get("size")
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|n| n > 0)
                    && flags.contains(&"dynamic")
                    && flags.contains(&"timeout")
            })
        })
    };
    let has_chain = |name: &str| {
        items
            .iter()
            .any(|item| item.get("chain").is_some_and(|chain| in_table(chain, name)))
    };
    let rules_of = |chain: &str| -> Vec<Vec<String>> {
        items
            .iter()
            .filter_map(|item| item.get("rule"))
            .filter(|rule| {
                str_of(rule, "family").as_deref() == Some("inet")
                    && str_of(rule, "table").as_deref() == Some("conspect_floor")
                    && str_of(rule, "chain").as_deref() == Some(chain)
            })
            .map(|rule| {
                rule.get("expr")
                    .and_then(serde_json::Value::as_array)
                    .map(|exprs| exprs.iter().filter_map(shape).collect())
                    .unwrap_or_default()
            })
            .collect()
    };
    has_table
        && FLOOR_METERS.iter().all(|(name, ty)| has_set(name, ty))
        && FLOOR_CHAINS.iter().all(|name| has_chain(name))
        && verdicts_are_the_shipped_ones(&rules_of)
}

/// The rule verdicts the shipped floor depends on (review PR 349 round 3:
/// `present` means the floor fails closed, not merely that its objects
/// exist). Rates and bursts are the operator's to tune and are not checked;
/// the structure is:
/// * `input` jumps to `udp443_new` for UDP 443 in ct state new and then to
///   `udp443_all` for every UDP 443 packet;
/// * each tier chain is, in order: `update` of its v4 set with a `limit`
///   then `accept`; `update` of its v6 set (keyed on the /64) with a
///   `limit` then `accept`; and a final `drop`, so a packet over its rate,
///   or from a source the full set cannot take, is dropped.
fn verdicts_are_the_shipped_ones(rules_of: &dyn Fn(&str) -> Vec<Vec<String>>) -> bool {
    let input = rules_of("input");
    let jumps = |target: &str, needs_new: bool| {
        input.iter().any(|rule| {
            rule.iter().any(|t| t == "dport443")
                && rule.iter().any(|t| t == &format!("jump:{target}"))
                && (!needs_new || rule.iter().any(|t| t == "ct:new"))
        })
    };
    let tier = |chain: &str, v4: &str, v6: &str| {
        let rules = rules_of(chain);
        let update =
            |set: &str, key: &str| vec![format!("update:@{set}:{key}:limit"), "accept".to_owned()];
        rules.len() == 3
            && rules.first() == Some(&update(v4, "ip"))
            && rules.get(1) == Some(&update(v6, "ip6/64"))
            && rules
                .get(2)
                .is_some_and(|last| last.last().map(String::as_str) == Some("drop"))
    };
    jumps("udp443_new", true)
        && jumps("udp443_all", false)
        && tier("udp443_new", "udp443_new_v4", "udp443_new_v6")
        && tier("udp443_all", "udp443_all_v4", "udp443_all_v6")
}

/// One nft JSON statement reduced to the token the floor check compares
/// (counters and limit values ignored).
fn shape(expr: &serde_json::Value) -> Option<String> {
    if expr.get("accept").is_some() {
        return Some("accept".to_owned());
    }
    if expr.get("drop").is_some() {
        return Some("drop".to_owned());
    }
    if let Some(target) = expr
        .pointer("/jump/target")
        .and_then(serde_json::Value::as_str)
    {
        return Some(format!("jump:{target}"));
    }
    if let Some(m) = expr.get("match") {
        if m.pointer("/left/payload/field")
            .and_then(serde_json::Value::as_str)
            == Some("dport")
            && m.pointer("/left/payload/protocol")
                .and_then(serde_json::Value::as_str)
                == Some("udp")
            && m.get("right").and_then(serde_json::Value::as_u64) == Some(443)
        {
            return Some("dport443".to_owned());
        }
        if m.pointer("/left/ct/key")
            .and_then(serde_json::Value::as_str)
            == Some("state")
        {
            let right = m.get("right")?;
            let has_new = right.as_str() == Some("new")
                || right
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("new")));
            return has_new.then(|| "ct:new".to_owned());
        }
        return Some("match".to_owned());
    }
    if let Some(set) = expr.get("set") {
        if set.get("op").and_then(serde_json::Value::as_str) != Some("update") {
            return Some("set".to_owned());
        }
        let name = set.get("set").and_then(serde_json::Value::as_str)?;
        let elem = set.get("elem")?;
        let key = if elem
            .pointer("/payload/protocol")
            .and_then(serde_json::Value::as_str)
            == Some("ip")
            && elem
                .pointer("/payload/field")
                .and_then(serde_json::Value::as_str)
                == Some("saddr")
        {
            "ip"
        } else if elem
            .get("&")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|a| {
                a.first()
                    .and_then(|p| p.pointer("/payload/protocol"))
                    .and_then(serde_json::Value::as_str)
                    == Some("ip6")
                    && a.get(1).and_then(serde_json::Value::as_str) == Some("ffff:ffff:ffff:ffff::")
            })
        {
            "ip6/64"
        } else {
            "other"
        };
        let limited = set
            .get("stmt")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|stmts| {
                stmts
                    .iter()
                    .any(|st| st.get("limit").is_some_and(|l| l.get("inv").is_none()))
            });
        return Some(format!(
            "update:{name}:{key}:{}",
            if limited { "limit" } else { "nolimit" }
        ));
    }
    if expr.get("counter").is_some() {
        return None;
    }
    Some("other".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `nft -j list table inet conspect_floor` (nft 1.1.3) after loading
    /// the shipped file in a `unshare -Urn` namespace, verbatim except that
    /// the rule objects are omitted (the parser does not read them).
    /// `nft -j list table inet conspect_floor` of the shipped ruleset (nft
    /// 1.1.3, rules included).
    const PRESENT_FIXTURE: &str = r#"{"nftables": [{"metainfo": {"version": "1.1.3", "release_name": "Commodore Bullmoose #4", "json_schema_version": 1}}, {"table": {"family": "inet", "name": "conspect_floor", "handle": 1}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "input", "handle": 1, "type": "filter", "hook": "input", "prio": -5, "policy": "accept"}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "udp443_new", "handle": 2}}, {"chain": {"family": "inet", "table": "conspect_floor", "name": "udp443_all", "handle": 3}}, {"set": {"family": "inet", "name": "udp443_new_v4", "table": "conspect_floor", "type": "ipv4_addr", "handle": 4, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "udp443_new_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 5, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "udp443_all_v4", "table": "conspect_floor", "type": "ipv4_addr", "handle": 6, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"set": {"family": "inet", "name": "udp443_all_v6", "table": "conspect_floor", "type": "ipv6_addr", "handle": 7, "size": 65536, "flags": ["timeout", "dynamic"], "timeout": 120}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "input", "handle": 8, "expr": [{"match": {"op": "==", "left": {"payload": {"protocol": "udp", "field": "dport"}}, "right": 443}}, {"match": {"op": "in", "left": {"ct": {"key": "state"}}, "right": ["new", "untracked"]}}, {"jump": {"target": "udp443_new"}}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "input", "handle": 9, "expr": [{"match": {"op": "==", "left": {"payload": {"protocol": "udp", "field": "dport"}}, "right": 443}}, {"jump": {"target": "udp443_all"}}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "udp443_new", "handle": 10, "expr": [{"set": {"op": "update", "elem": {"payload": {"protocol": "ip", "field": "saddr"}}, "set": "@udp443_new_v4", "stmt": [{"limit": {"rate": 2000, "burst": 1000, "per": "second"}}]}}, {"accept": null}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "udp443_new", "handle": 11, "expr": [{"set": {"op": "update", "elem": {"&": [{"payload": {"protocol": "ip6", "field": "saddr"}}, "ffff:ffff:ffff:ffff::"]}, "set": "@udp443_new_v6", "stmt": [{"limit": {"rate": 2000, "burst": 1000, "per": "second"}}]}}, {"accept": null}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "udp443_new", "handle": 12, "expr": [{"counter": {"packets": 0, "bytes": 0}}, {"drop": null}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "udp443_all", "handle": 13, "expr": [{"set": {"op": "update", "elem": {"payload": {"protocol": "ip", "field": "saddr"}}, "set": "@udp443_all_v4", "stmt": [{"limit": {"rate": 25000, "burst": 10000, "per": "second"}}]}}, {"accept": null}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "udp443_all", "handle": 14, "expr": [{"set": {"op": "update", "elem": {"&": [{"payload": {"protocol": "ip6", "field": "saddr"}}, "ffff:ffff:ffff:ffff::"]}, "set": "@udp443_all_v6", "stmt": [{"limit": {"rate": 25000, "burst": 10000, "per": "second"}}]}}, {"accept": null}]}}, {"rule": {"family": "inet", "table": "conspect_floor", "chain": "udp443_all", "handle": 15, "expr": [{"counter": {"packets": 0, "bytes": 0}}, {"drop": null}]}}]}"#;

    /// nft 1.1.3's stderr for the same command with no such table (observed).
    const MISSING_TABLE: &str =
        "Error: No such file or directory\nlist table inet conspect_floor\n                ^^^^^^^^^^^^^^\n";

    #[test]
    fn the_shipped_floor_listing_is_present() {
        assert_eq!(parse(PRESENT_FIXTURE), NftFloorStatus::Present);
    }

    /// Drop the rule objects for which `drop_rule` is true.
    fn without_rules(drop_rule: impl Fn(&serde_json::Value) -> bool) -> String {
        let mut v: serde_json::Value = serde_json::from_str(PRESENT_FIXTURE).unwrap();
        let items = v["nftables"].as_array_mut().unwrap();
        items.retain(|item| item.get("rule").is_none_or(|r| !drop_rule(r)));
        v.to_string()
    }

    #[test]
    fn a_floor_without_its_verdicts_is_not_present() {
        // Objects all there, but a tier's final drop is gone: fails open.
        let no_drop = without_rules(|r| {
            r["chain"] == "udp443_all"
                && r["expr"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e.get("drop").is_some())
        });
        assert_eq!(parse(&no_drop), NftFloorStatus::Unknown);
        // The new-tier jump is gone.
        let no_jump =
            without_rules(|r| r["chain"] == "input" && r.to_string().contains("udp443_new"));
        assert_eq!(parse(&no_jump), NftFloorStatus::Unknown);
        // The v6 update is gone.
        let no_v6 = without_rules(|r| r.to_string().contains("@udp443_new_v6"));
        assert_eq!(parse(&no_v6), NftFloorStatus::Unknown);
        // No rules at all (objects only).
        assert_eq!(parse(&without_rules(|_| true)), NftFloorStatus::Unknown);
    }

    #[test]
    fn a_set_without_size_or_dynamic_timeout_is_not_present() {
        for (field, value) in [
            ("size", serde_json::Value::Null),
            ("flags", serde_json::json!(["dynamic"])),
        ] {
            let mut v: serde_json::Value = serde_json::from_str(PRESENT_FIXTURE).unwrap();
            for item in v["nftables"].as_array_mut().unwrap() {
                if let Some(set) = item.get_mut("set") {
                    if value.is_null() {
                        set.as_object_mut().unwrap().remove(field);
                    } else {
                        set[field] = value.clone();
                    }
                }
            }
            assert_eq!(parse(&v.to_string()), NftFloorStatus::Unknown, "{field}");
        }
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
