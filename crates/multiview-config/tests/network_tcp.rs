//! Slice 4 (issue #290): the `[network]` TCP-door keys. Old shapes keep
//! loading; every new key is validated.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{MultiviewConfig, NetworkConfig, NetworkMode, TrustedProxy};

fn net(text: &str) -> NetworkConfig {
    toml::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn rejects(text: &str, needle: &str) {
    let err = net(text).validate().expect_err(text).to_string();
    assert!(err.contains(needle), "{text}: {err}");
}

#[test]
fn old_shapes_load_unchanged_and_mean_no_tcp() {
    for old in ["", "https_port = 443", "https_port = 443\nworkers = 3"] {
        let n = net(old);
        n.validate().unwrap();
        assert_eq!(n.mode, None, "absent mode = no TCP listener");
        assert_eq!(n.effective_redirect_port(), None);
        assert!(n.ice_tcp_enabled());
    }
}

#[test]
fn desired_json_with_only_https_port_round_trips() {
    let n: NetworkConfig = serde_json::from_str(r#"{"https_port":443}"#).unwrap();
    n.validate().unwrap();
    let back = serde_json::to_string(&n).unwrap();
    assert!(!back.contains("mode"), "{back}");
    let again: NetworkConfig = serde_json::from_str(&back).unwrap();
    assert_eq!(n, again);
}

#[test]
fn direct_mode_defaults() {
    let n = net("mode = \"direct\"\nhttps_port = 443\npublic_host = \"mda001.example.net\"");
    n.validate().unwrap();
    assert_eq!(n.mode, Some(NetworkMode::Direct));
    assert_eq!(n.effective_tcp_port(), Some(443));
    assert_eq!(n.effective_redirect_port(), Some(80));
    assert_eq!(n.bind_ip().unwrap(), std::net::Ipv6Addr::UNSPECIFIED);
    let staged = net(
        "mode = \"direct\"\nhttps_port = 443\ntcp_port = 8443\nhttp_redirect_port = 0\nbind = \"[::1]\"\n[ice_tcp]\nenabled = false",
    );
    staged.validate().unwrap();
    assert_eq!(staged.effective_tcp_port(), Some(8443));
    assert_eq!(staged.effective_redirect_port(), None);
    assert_eq!(staged.bind_ip().unwrap(), std::net::Ipv6Addr::LOCALHOST);
    assert!(!staged.ice_tcp_enabled());
}

#[test]
fn behind_proxy_mode() {
    let n = net(
        "mode = \"behind_proxy\"\nhttps_port = 443\ntrusted_proxies = [\"::1\", \"10.0.0.0/8\", \"2001:db8::/32\"]",
    );
    n.validate().unwrap();
    assert_eq!(
        n.effective_redirect_port(),
        None,
        "no redirect behind a proxy"
    );
    assert_eq!(
        n.proxy_listen_addr().unwrap(),
        "[::1]:8443".parse().unwrap()
    );
    let list = n.trusted_proxy_list().unwrap();
    assert!(list[1].contains("10.1.2.3".parse().unwrap()));
    assert!(list[1].contains("::ffff:10.1.2.3".parse().unwrap()));
    assert!(!list[1].contains("11.0.0.1".parse().unwrap()));
    assert!(list[2].contains("2001:db8:ffff::1".parse().unwrap()));
    assert!(!list[2].contains("2001:db9::1".parse().unwrap()));
    assert!(list[0].contains("::1".parse().unwrap()));
    rejects(
        "mode = \"behind_proxy\"\nhttps_port = 443",
        "trusted_proxies",
    );
}

#[test]
fn each_new_key_is_validated() {
    rejects("bind = \"0.0.0.0\"", "IPv6-first");
    rejects("bind = \"127.0.0.1\"", "IPv6-first");
    rejects("bind = \"::ffff:127.0.0.1\"", "IPv6-first");
    rejects("bind = \"localhost\"", "network.bind");
    rejects("trusted_proxies = [\"proxy.example\"]", "trusted_proxies");
    rejects("trusted_proxies = [\"10.0.0.0/33\"]", "trusted_proxies");
    rejects("trusted_proxies = [\"::/129\"]", "trusted_proxies");
    rejects("proxy_listen = \"0.0.0.0:8443\"", "0.0.0.0");
    rejects("proxy_listen = \"[::1]\"", "proxy_listen");
    rejects("proxy_listen = \"[::1]:0\"", "proxy_listen");
    rejects("tcp_port = 0", "tcp_port");
    rejects("mode = \"direct\"\nhttp_redirect_port = 0", "https_port");
    rejects("mode = \"direct\"\nhttps_port = 443", "public_host");
    rejects(
        "mode = \"direct\"\nhttps_port = 443\nhttp_redirect_port = 443\npublic_host = \"a.example.org\"",
        "differ",
    );
    assert!(toml::from_str::<NetworkConfig>("mode = \"tunnel\"").is_err());
    assert!(toml::from_str::<NetworkConfig>("[ice_tcp]\nbogus = 1").is_err());
    assert!(TrustedProxy::parse("2001:db8::1").is_ok());
}

#[test]
fn public_host_alone_is_valid_now() {
    net("public_host = \"a.example.org\"").validate().unwrap();
    net("public_host = \"2001:db8::10\"").validate().unwrap();
    rejects("public_host = \"Bad Host\"", "public_host");
}

#[test]
fn the_document_validation_surfaces_the_new_rules() {
    let doc = |network: &str| {
        format!(
            r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[outputs]]
id = "out-a"
kind = "hls"
path = "/srv/hls/out-a/a.m3u8"
codec = "mpeg2video"
segment_ms = 2000

[network]
{network}
"##
        )
    };
    let ok = MultiviewConfig::load_from_toml(&doc(
        "https_port = 443\nmode = \"direct\"\npublic_host = \"a.example.org\"",
    ))
    .unwrap();
    ok.validate().unwrap();
    let bad =
        MultiviewConfig::load_from_toml(&doc("https_port = 443\nbind = \"0.0.0.0\"")).unwrap();
    assert!(bad
        .validate()
        .unwrap_err()
        .to_string()
        .contains("IPv6-first"));
}

/// Slice 6 (#292): the cutover keys. `cert_file`/`key_file` (the door's own
/// static certificate) go together, are absolute and exclude `[network.tls]`;
/// `acme_webroot` is absolute and needs the redirect listener.
#[test]
fn slice6_cutover_keys() {
    let ok = "https_port = 443\nmode = \"direct\"\npublic_host = \"h.example\"\n\
              cert_file = \"/etc/letsencrypt/live/h.example/fullchain.pem\"\n\
              key_file = \"/etc/letsencrypt/live/h.example/privkey.pem\"\n\
              acme_webroot = \"/var/lib/conspect/acme-webroot\"";
    let n = net(ok);
    n.validate().unwrap();
    assert!(matches!(
        n.static_site_tls(),
        Some(multiview_config::TlsConfig::Static { .. })
    ));
    let back: NetworkConfig = serde_json::from_str(&serde_json::to_string(&n).unwrap()).unwrap();
    assert_eq!(n, back);
    rejects("https_port = 443\ncert_file = \"/c.pem\"", "go together");
    rejects(
        "https_port = 443\ncert_file = \"c.pem\"\nkey_file = \"/k.pem\"",
        "absolute",
    );
    rejects(
        "https_port = 443\nmode = \"direct\"\npublic_host = \"h.example\"\nacme_webroot = \"rel\"",
        "absolute",
    );
    rejects(
        "https_port = 443\nmode = \"direct\"\npublic_host = \"h.example\"\nhttp_redirect_port = 0\nacme_webroot = \"/w\"",
        "redirect",
    );
    rejects("https_port = 443\nacme_webroot = \"/w\"", "redirect");
    rejects(
        "https_port = 443\npublic_host = \"h.example\"\ncert_file = \"/c\"\nkey_file = \"/k\"\n\
         [tls]\nca = \"letsencrypt\"\nchallenge = \"tls-alpn-01\"\ndomains = [\"h.example\"]\nstate_dir = \"/var/lib/conspect/tls\"",
        "exclusive",
    );
    // A config with none of the new keys is unchanged.
    assert!(net("https_port = 443").static_site_tls().is_none());
}

#[test]
fn control_ui_dir_must_be_absolute() {
    let base = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[outputs]]
kind = "hls"
path = "/tmp/ui-dir-test.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[control]
listen = "[::1]:8080"
"##;
    let good =
        MultiviewConfig::load_from_toml(&format!("{base}ui_dir = \"/var/www/conspect-ui/abc\"\n"))
            .unwrap();
    good.validate().unwrap();
    let bad = MultiviewConfig::load_from_toml(&format!("{base}ui_dir = \"dist\"\n"));
    let err = match bad {
        Ok(c) => c.validate().unwrap_err().to_string(),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("ui_dir"), "{err}");
}
