//! Cast media-URL construction (DEV-D2, ADR-M011): the device-reachable HLS
//! URL a session LOADs. Cast devices ignore DHCP-provided DNS and resolve via
//! hardcoded public resolvers, so the base must be an IP literal (or a
//! publicly resolvable name) — never `.local`, never a bare LAN name, never a
//! loopback the device cannot reach.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::DisplayAssign;
use multiview_control::devices::cast::media::{
    split_authority, CastDelivery, CastMediaBase, CastMediaError, CastMediaTarget, HlsSegmentFormat,
};

#[test]
fn accepts_an_ipv4_literal_base() {
    // The one deliberate IPv4 legacy-interop carve-out (conventions §10 /
    // ADR-0042): Cast devices are effectively IPv4-only in practice.
    let base = CastMediaBase::parse("http://192.0.2.7:8080").expect("an IPv4 literal base");
    assert_eq!(base.as_str(), "http://192.0.2.7:8080");
}

#[test]
fn accepts_a_bracketed_ipv6_literal_base() {
    let base = CastMediaBase::parse("http://[2001:db8::7]:8080").expect("an IPv6 literal base");
    assert_eq!(base.as_str(), "http://[2001:db8::7]:8080");
}

#[test]
fn strips_a_single_trailing_slash() {
    let base = CastMediaBase::parse("http://192.0.2.7:8080/").expect("a trailing slash is fine");
    assert_eq!(base.as_str(), "http://192.0.2.7:8080");
}

#[test]
fn rejects_loopback_bases_the_device_cannot_reach() {
    assert!(matches!(
        CastMediaBase::parse("http://[::1]:8080"),
        Err(CastMediaError::Loopback { .. })
    ));
    assert!(matches!(
        CastMediaBase::parse("http://127.0.0.1:8080"),
        Err(CastMediaError::Loopback { .. })
    ));
}

#[test]
fn rejects_mdns_local_names() {
    // `.local` is mDNS: Cast devices resolve via public DNS and never see it.
    assert!(matches!(
        CastMediaBase::parse("http://multiview.local:8080"),
        Err(CastMediaError::UnresolvableHost { .. })
    ));
}

#[test]
fn rejects_single_label_lan_names() {
    // A bare LAN hostname only resolves via the site's DNS — which the device
    // ignores (it uses hardcoded public resolvers).
    assert!(matches!(
        CastMediaBase::parse("http://multiview:8080"),
        Err(CastMediaError::UnresolvableHost { .. })
    ));
}

#[test]
fn accepts_a_public_fqdn_with_the_documented_caveat() {
    // A multi-label name is accepted: it MUST be publicly resolvable (the
    // device resolves via Google's public DNS) — documented caveat.
    let base = CastMediaBase::parse("https://mv.example.com").expect("a public FQDN");
    assert_eq!(base.as_str(), "https://mv.example.com");
}

#[test]
fn rejects_non_http_schemes_and_paths() {
    assert!(matches!(
        CastMediaBase::parse("rtsp://192.0.2.7"),
        Err(CastMediaError::NotHttp { .. })
    ));
    assert!(matches!(
        CastMediaBase::parse("http://192.0.2.7:8080/hls"),
        Err(CastMediaError::HasPath { .. })
    ));
    assert!(CastMediaBase::parse("http://").is_err());
}

#[test]
fn joins_the_mount_route_and_playlist_into_the_media_url() {
    let base = CastMediaBase::parse("http://192.0.2.7:8080").expect("a base");
    assert_eq!(
        base.join("/hls/program", "program.m3u8"),
        "http://192.0.2.7:8080/hls/program/program.m3u8"
    );
}

#[test]
fn delivery_resolves_outputs_and_the_program_default() {
    let mut delivery = CastDelivery::new();
    delivery.insert(
        "out-a",
        CastMediaTarget {
            url: "http://192.0.2.7:8080/hls/out-a/a.m3u8".to_owned(),
            format: HlsSegmentFormat::MpegTs,
        },
    );
    delivery.insert(
        "out-b",
        CastMediaTarget {
            url: "http://192.0.2.7:8080/hls/out-b/b.m3u8".to_owned(),
            format: HlsSegmentFormat::Fmp4,
        },
    );

    // Named output → that rendition (and its segment format rides along).
    let target = delivery.for_output("out-b").expect("out-b resolves");
    assert_eq!(target.format, HlsSegmentFormat::Fmp4);
    assert!(target.url.contains("/hls/out-b/"));
    assert!(delivery.for_output("nope").is_none());

    // `{ program = true }` → the first declared HLS rendition (documented
    // default: every HLS output is a rendition of the program canvas).
    let first = delivery.first().expect("a first rendition");
    assert!(first.url.contains("/hls/out-a/"));

    // DisplayAssign resolution: Output(id) / Program(true) resolve; a wall
    // head is NOT an HLS rendition (ADR-M011: the media path is an existing
    // rendition), so it never resolves.
    assert!(delivery
        .resolve_assign(&DisplayAssign::Output("out-a".to_owned()))
        .is_some());
    assert!(delivery
        .resolve_assign(&DisplayAssign::Program(true))
        .is_some());
    assert!(delivery
        .resolve_assign(&DisplayAssign::WallHead("head-l".to_owned()))
        .is_none());
}

#[test]
fn empty_delivery_resolves_nothing() {
    let delivery = CastDelivery::new();
    assert!(delivery.first().is_none());
    assert!(delivery
        .resolve_assign(&DisplayAssign::Program(true))
        .is_none());
}

#[test]
fn split_authority_handles_v6_v4_and_defaults_the_cast_port() {
    // IPv6 literal (bracketed) — host comes back unbracketed for the dialer.
    assert_eq!(
        split_authority("[2001:db8::20]:8010"),
        Some(("2001:db8::20".to_owned(), 8010))
    );
    // Default CASTV2 port 8009 when none is given (Cast groups advertise
    // non-default ports — always honour an explicit one).
    assert_eq!(
        split_authority("[2001:db8::20]"),
        Some(("2001:db8::20".to_owned(), 8009))
    );
    assert_eq!(
        split_authority("192.0.2.20:32198"),
        Some(("192.0.2.20".to_owned(), 32198))
    );
    assert_eq!(
        split_authority("192.0.2.20"),
        Some(("192.0.2.20".to_owned(), 8009))
    );
    // Garbage ports are rejected, never defaulted.
    assert_eq!(split_authority("192.0.2.20:notaport"), None);
    assert_eq!(split_authority(""), None);
}
