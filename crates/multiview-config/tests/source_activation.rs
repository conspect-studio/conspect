//! `sources[].activation`: default `always_on`, the legacy `demand.always_on`
//! alias, and `[iptv] guide_refresh_minutes` validation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use multiview_config::{IptvConfig, Source, SourceActivation};

fn source(json: serde_json::Value) -> Source {
    serde_json::from_value(json).expect("source")
}

#[test]
fn an_existing_source_document_is_always_on() {
    let s = source(serde_json::json!({"id": "a", "kind": "bars"}));
    assert_eq!(s.effective_activation(), SourceActivation::AlwaysOn);
    assert!(!s.is_on_demand());
    // Round-trips unchanged: no activation key is written for the default.
    let back = serde_json::to_value(&s).unwrap();
    assert!(back.get("activation").is_none(), "{back}");
}

#[test]
fn on_demand_is_explicit_and_round_trips() {
    let s = source(serde_json::json!({"id": "a", "kind": "bars", "activation": "on_demand"}));
    assert!(s.is_on_demand());
    let back = serde_json::to_value(&s).unwrap();
    assert_eq!(back["activation"], "on_demand");
    let toml = toml::to_string(&s).unwrap();
    let again: Source = toml::from_str(&toml).unwrap();
    assert_eq!(again.effective_activation(), SourceActivation::OnDemand);
}

#[test]
fn the_legacy_demand_block_still_loads_and_is_not_written_back() {
    let pinned =
        source(serde_json::json!({"id": "a", "kind": "bars", "demand": {"always_on": true}}));
    assert_eq!(pinned.effective_activation(), SourceActivation::AlwaysOn);
    let lazy =
        source(serde_json::json!({"id": "a", "kind": "bars", "demand": {"always_on": false}}));
    assert_eq!(lazy.effective_activation(), SourceActivation::OnDemand);
    let journal =
        source(serde_json::json!({"id": "a", "kind": "bars", "demand": {"journal_events": true}}));
    assert_eq!(journal.effective_activation(), SourceActivation::AlwaysOn);
    assert!(serde_json::to_value(&lazy).unwrap().get("demand").is_none());
    // An explicit activation wins over the alias.
    let both = source(serde_json::json!({
        "id": "a", "kind": "bars", "activation": "always_on", "demand": {"always_on": false}
    }));
    assert_eq!(both.effective_activation(), SourceActivation::AlwaysOn);
}

#[test]
fn guide_refresh_minutes_defaults_to_thirty_and_is_validated() {
    let config = IptvConfig::default();
    assert_eq!(config.guide_refresh_minutes(), 30);
    config.validate(None, None).unwrap();
    for bad in [0_u32, 4, 1441] {
        let config: IptvConfig =
            serde_json::from_value(serde_json::json!({"guide_refresh_minutes": bad})).unwrap();
        assert!(config.validate(None, None).is_err(), "{bad} accepted");
    }
    let ok: IptvConfig =
        serde_json::from_value(serde_json::json!({"guide_refresh_minutes": 5})).unwrap();
    ok.validate(None, None).unwrap();
    assert_eq!(ok.guide_refresh_minutes(), 5);
}

#[test]
fn the_removed_guide_idle_setting_is_rejected_loudly() {
    let parsed: Result<IptvConfig, _> =
        serde_json::from_value(serde_json::json!({"guide_idle_secs": 60}));
    assert!(parsed.is_err());
}
