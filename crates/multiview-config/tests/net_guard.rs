//! SSRF dial-guard unit tests (SEC-02/SEC-04, CWE-918/CWE-20) for the shared
//! `multiview_config::device::net_guard` address screen.
//!
//! The guard is the single source of truth both `Device::validate` (config
//! load) and the control-plane dial sites (device poller + Cast session actor)
//! consult before an outbound connection. These tests pin its behaviour on the
//! **RESOLVED** IP (so a DNS-rebind answer is caught, not just a literal):
//!
//! * The **default** ([`DialPolicy::default`] / [`DialPolicy::allow_lan`]) is
//!   **non-breaking** — a self-hosted LAN appliance must keep reaching its
//!   devices out of the box, so every *allowlistable* range (RFC 1918 private,
//!   IPv6 ULA, RFC 6598 carrier-NAT) is dialable by default. The
//!   **never-legitimate** ranges (loopback, link-local incl. the cloud-metadata
//!   IP `169.254.169.254`, unspecified, multicast, broadcast, and their
//!   IPv4-mapped forms) are **always** blocked — no default and no operator
//!   allowlist can re-enable them. This closes the dangerous DNS-rebind targets
//!   (metadata/loopback) unconditionally.
//! * An operator [`DialPolicy::from_cidrs`] allowlist **tightens** the dial: a
//!   private/ULA target is then reachable only if it falls inside one of the
//!   operator CIDRs (locking the dial to the real device subnet closes the
//!   authenticated internal-port-scan vector, SEC-04).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::net::IpAddr;

use multiview_config::device::net_guard::{
    resolve_and_screen, screen_ip, screen_resolved, DialPolicy, HostResolver,
};

/// Parse an IP literal for a test table.
fn ip(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|_| panic!("`{text}` is an IP literal"))
}

/// The never-legitimate ranges — no default and no operator allowlist can ever
/// re-enable a dial to these. This is the unconditional SSRF floor.
const NEVER_LEGIT: &[&str] = &[
    "127.0.0.1",
    "127.10.20.30",
    "::1",
    "169.254.169.254", // cloud-metadata IMDS
    "169.254.42.1",    // IPv4 link-local
    "fe80::1",         // IPv6 link-local
    "0.0.0.0",
    "::",
    "239.0.0.9",              // IPv4 multicast
    "ff02::1",                // IPv6 multicast
    "255.255.255.255",        // IPv4 broadcast
    "::ffff:127.0.0.1",       // IPv4-mapped loopback bypass
    "::ffff:169.254.169.254", // IPv4-mapped IMDS bypass
];

/// The allowlistable LAN ranges — dialable by DEFAULT (non-breaking), and
/// tightenable behind an operator [`DialPolicy::from_cidrs`] allowlist.
const LAN: &[&str] = &[
    "10.0.0.8",
    "172.16.0.8",
    "192.168.0.8",
    "100.64.0.8", // CGNAT (RFC 6598)
    "fd00::8",
    "fd00:db8::42",
];

/// Globally-routable addresses (and the documentation ranges the test suite
/// uses as public stand-ins) a default policy must accept.
const PUBLIC: &[&str] = &[
    "198.51.100.8", // TEST-NET-2 (public stand-in)
    "203.0.113.8",  // TEST-NET-3
    "1.1.1.1",
    "8.8.8.8",
    "2001:db8::8", // documentation (public stand-in)
    "2606:4700:4700::1111",
];

#[test]
fn default_policy_blocks_never_legit_but_allows_lan_and_public() {
    // The non-breaking default: a LAN appliance keeps dialling its devices, but
    // the dangerous never-legitimate targets stay blocked out of the box.
    let policy = DialPolicy::default();

    for text in NEVER_LEGIT {
        assert!(
            screen_ip(ip(text), &policy).is_err(),
            "the default must ALWAYS block never-legitimate {text}"
        );
    }
    for text in LAN {
        screen_ip(ip(text), &policy)
            .unwrap_or_else(|err| panic!("the default must allow LAN device {text}: {err}"));
    }
    for text in PUBLIC {
        screen_ip(ip(text), &policy)
            .unwrap_or_else(|err| panic!("the default must allow public {text}: {err}"));
    }
}

#[test]
fn screen_resolved_is_fail_closed_over_a_mixed_answer() {
    let policy = DialPolicy::default();
    // A single never-legitimate IP among otherwise-public answers rejects the
    // whole set (a DNS answer cannot smuggle in a loopback/metadata target).
    assert!(
        screen_resolved([ip("198.51.100.8"), ip("169.254.169.254")], &policy).is_err(),
        "a mixed answer with one metadata IP must be rejected"
    );
    screen_resolved([ip("198.51.100.8"), ip("203.0.113.8")], &policy)
        .expect("an all-public answer is accepted");
}

#[test]
fn operator_allowlist_tightens_dialing_to_the_listed_cidrs() {
    // With an allowlist set, an allowlistable range is reachable ONLY inside one
    // of the operator CIDRs — the recommended hardening that closes the
    // authenticated internal-dial (SEC-04).
    let policy = DialPolicy::from_cidrs(["192.168.0.0/16", "fd00:db8::/32"])
        .expect("valid operator dial allowlist");

    // Allowlisted LAN ranges remain reachable.
    screen_ip(ip("192.168.0.8"), &policy).expect("allowlisted RFC1918 host reachable");
    screen_ip(ip("fd00:db8::42"), &policy).expect("allowlisted ULA host reachable");

    // A private range OUTSIDE the allowlist is now denied (SEC-04 tightening).
    assert!(
        screen_ip(ip("10.0.0.8"), &policy).is_err(),
        "a private range outside the allowlist is denied once an allowlist is set"
    );
    assert!(
        screen_ip(ip("fd00::8"), &policy).is_err(),
        "a ULA outside the allowlist is denied once an allowlist is set"
    );
}

#[test]
fn never_legitimate_ranges_are_not_allowlistable() {
    // Even if an operator allowlists the link-local / loopback blocks, the
    // cloud-metadata IP and loopback stay hard-blocked.
    let policy = DialPolicy::from_cidrs(["169.254.0.0/16", "127.0.0.0/8", "::1/128"])
        .expect("valid (if misguided) allowlist");
    for text in ["169.254.169.254", "127.0.0.1", "::1"] {
        assert!(
            screen_ip(ip(text), &policy).is_err(),
            "{text} is never-legitimate and cannot be allowlisted"
        );
    }
}

/// A scripted resolver: maps a host name to the A/AAAA answer under test.
struct FakeResolver(HashMap<String, Vec<IpAddr>>);

impl HostResolver for FakeResolver {
    fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        self.0.get(host).cloned().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no answer for {host}"),
            )
        })
    }
}

#[test]
fn resolve_and_screen_rejects_a_rebind_to_a_never_legit_target() {
    let resolver = FakeResolver(HashMap::from([
        // A public NAME that answers with a metadata / loopback address — the
        // dangerous DNS-rebind SSRF the resolved-IP check exists to defeat. The
        // default blocks these unconditionally (no allowlist involved).
        (
            "rebind-imds.example.test".to_owned(),
            vec![ip("169.254.169.254")],
        ),
        ("rebind-loopback.example.test".to_owned(), vec![ip("::1")]),
        (
            "rebind-mixed.example.test".to_owned(),
            vec![ip("198.51.100.8"), ip("127.0.0.1")],
        ),
        // A genuinely public host.
        ("real.example.test".to_owned(), vec![ip("198.51.100.8")]),
    ]));
    let policy = DialPolicy::default();

    for host in [
        "rebind-imds.example.test",
        "rebind-loopback.example.test",
        "rebind-mixed.example.test",
    ] {
        assert!(
            resolve_and_screen(host, &policy, &resolver).is_err(),
            "a rebind answer for {host} pointing at a never-legit target must be rejected"
        );
    }

    let vetted = resolve_and_screen("real.example.test", &policy, &resolver)
        .expect("a public host resolves and screens clean");
    assert_eq!(
        vetted,
        vec![ip("198.51.100.8")],
        "returns the vetted answer"
    );
}

#[test]
fn a_set_allowlist_still_catches_a_rebind_to_a_non_allowlisted_private_target() {
    // SEC-04 hardening: once an operator locks the dial to their device subnet,
    // a public name that rebinds to a DIFFERENT private range is caught on the
    // resolved IP — not just literals.
    let resolver = FakeResolver(HashMap::from([(
        "rebind-lan.example.test".to_owned(),
        vec![ip("10.9.9.9")],
    )]));
    let policy = DialPolicy::from_cidrs(["192.168.0.0/16"]).expect("valid allowlist");
    assert!(
        resolve_and_screen("rebind-lan.example.test", &policy, &resolver).is_err(),
        "a rebind to a private range outside the operator allowlist must be rejected"
    );
}
