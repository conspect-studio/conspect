//! `[catalogues.iptv_org]` parse / validate / round-trip.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use multiview_config::{CataloguesConfig, IptvOrgConfig};

#[test]
fn defaults_enabled_and_six_hours() {
    let c: IptvOrgConfig = toml::from_str("").expect("parse");
    assert!(c.enabled);
    assert_eq!(c.sync_interval().as_secs(), 6 * 3600);
    c.validate().expect("valid");
}

#[test]
fn round_trips_and_rejects_short_intervals() {
    let doc = "[iptv_org]\nenabled = false\ninterval = \"30m\"\nprobe_linked_every = \"60m\"\n";
    let c: CataloguesConfig = toml::from_str(doc).expect("parse");
    let i = c.iptv_org.as_ref().expect("iptv");
    assert!(!i.enabled);
    assert_eq!(i.sync_interval().as_secs(), 1800);
    let back: CataloguesConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
    assert_eq!(back, c);
    let bad: IptvOrgConfig = toml::from_str("interval = \"5m\"").unwrap();
    assert!(bad.validate().unwrap_err().to_string().contains("interval"));
    let bad: IptvOrgConfig = toml::from_str("probe_linked_every = \"1m\"").unwrap();
    assert!(bad.validate().is_err());
    assert!(toml::from_str::<IptvOrgConfig>("bogus = 1").is_err());
}
