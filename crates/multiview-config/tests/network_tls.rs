use multiview_config::{AcmeChallenge, CertificateAuthority, NetworkConfig, NetworkTlsConfig};

fn sample() -> &'static str {
    r#"schema_version = 1
[canvas]
width = 1920
height = 1080
fps = "30"
[layout]
kind = "grid"
rows = 1
columns = 1
[network]
public_host = "test.example.org"
[network.tls]
ca = "custom"
directory = "https://acme.example.org/directory"
trust_anchor = "/etc/acme/root.pem"
challenge = "dns-01"
domains = ["test.example.org", "*.example.org"]
eab_kid_env = "EAB_KID"
eab_hmac_env = "EAB_HMAC"
state_dir = "/var/lib/conspect/tls"
[network.tls.dns]
provider = "cloudflare"
token_env = "CF_TOKEN"
"#
}

#[test]
fn custom_profile_round_trips_and_rejects_literal_credentials() {
    let network: NetworkConfig = toml::from_str(
        &sample()
            .split_once("[network]\n")
            .unwrap()
            .1
            .replace("[network.tls", "[tls"),
    )
    .unwrap();
    network.validate().unwrap();
    let serialized = toml::to_string(&network).unwrap();
    let restored: NetworkConfig = toml::from_str(&serialized).unwrap();
    assert_eq!(network, restored);
    assert!(!serialized.contains("secret"));
    let mut invalid = network.clone();
    let Some(multiview_config::DnsProviderConfig::Cloudflare { token_env, .. }) =
        invalid.tls.as_mut().expect("tls").dns.as_mut()
    else {
        panic!("cloudflare")
    };
    *token_env = Some("some-literal-secret-123".into());
    assert!(invalid
        .validate()
        .unwrap_err()
        .to_string()
        .contains("network.tls.dns.token_env"));
    if let Some(multiview_config::DnsProviderConfig::Cloudflare { token_env, .. }) =
        invalid.tls.as_mut().expect("tls").dns.as_mut()
    {
        *token_env = Some("Ab3kQ9x2Lm7Zr0Tq5Wv8Yc1Nd4Hs6Jf2Gp9Ke3Ru".into());
    }
    assert!(invalid.validate().is_err());
}

#[test]
fn rejects_unsupported_http01_and_invalid_combinations() {
    assert!(toml::from_str::<NetworkTlsConfig>("ca='custom'\ndirectory='https://localhost/dir'\nchallenge='http-01'\ndomains=['a.example']\nstate_dir='/tmp/state'").is_err());
    let mut tls = NetworkTlsConfig {
        ca: CertificateAuthority::Google,
        staging: false,
        challenge: AcmeChallenge::TlsAlpn01,
        domains: vec!["test.example.org".into()],
        contact_email: None,
        acme_profile: None,
        directory: None,
        trust_anchor: None,
        eab_kid_env: None,
        eab_hmac_env: None,
        state_dir: "/var/lib/tls".into(),
        dns: None,
    };
    assert!(tls
        .validate()
        .unwrap_err()
        .to_string()
        .contains("eab_kid_env"));
    tls.eab_kid_env = Some("EAB_KID".into());
    tls.eab_hmac_env = Some("EAB_HMAC".into());
    tls.validate().unwrap();
    tls.domains = vec!["192.0.2.8".into()];
    assert!(tls.validate().unwrap_err().to_string().contains("domains"));
    tls.ca = CertificateAuthority::Letsencrypt;
    assert!(tls
        .validate()
        .unwrap_err()
        .to_string()
        .contains("acme_profile"));
    tls.ca = CertificateAuthority::Custom;
    tls.domains = vec!["test.example.org".into()];
    tls.directory = Some("https://169.254.169.254/latest/meta-data".into());
    assert!(tls
        .validate()
        .unwrap_err()
        .to_string()
        .contains("directory"));
    tls.directory = Some("https://2130706433/directory".into());
    assert!(tls
        .validate()
        .unwrap_err()
        .to_string()
        .contains("trust_anchor"));
    tls.directory = Some("https://localhost/directory".into());
    tls.trust_anchor = Some("/etc/conspect/pebble-root.pem".into());
    tls.validate().unwrap();
}

const DOCUMENT: &str = r##"
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
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
[network]
public_host = "test.example.org"
[network.tls]
ca = "letsencrypt"
challenge = "dns-01"
domains = ["test.example.org"]
state_dir = "/var/lib/conspect/tls"
[network.tls.dns]
provider = "cloudflare"
token_env = "CF_TOKEN"
[control]
listen = "[::1]:8080"
[control.tls]
mode = "static"
cert_file = "/etc/multiview/tls/fullchain.pem"
key_file = "/etc/multiview/tls/privkey.pem"
"##;

#[test]
fn document_keeps_static_tls_beside_network_tls() {
    let config: multiview_config::MultiviewConfig = toml::from_str(DOCUMENT).unwrap();
    config.validate().unwrap();
    assert!(config.network.is_some());
    let control = config.control.as_ref().expect("control section");
    assert!(matches!(
        control.tls,
        Some(multiview_config::TlsConfig::Static { .. })
    ));
    let restored: multiview_config::MultiviewConfig =
        toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(config.network, restored.network);
    assert_eq!(config.control, restored.control);
}

#[test]
fn document_rejects_a_public_host_missing_from_domains() {
    let raw = DOCUMENT.replace(
        "domains = [\"test.example.org\"]",
        "domains = [\"other.example.org\"]",
    );
    let config: multiview_config::MultiviewConfig = toml::from_str(&raw).unwrap();
    assert!(config
        .validate()
        .unwrap_err()
        .to_string()
        .contains("network.tls.domains"));
}
