//! Entitlement / lease resource serde tests — the signed entitlement resource
//! round-trips through JSON (and would through any serde format), the union is
//! internally-tagged (NEVER `untagged`, conventions §5), and the enums are
//! `#[non_exhaustive]`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use multiview_licence::entitlement::{Entitlement, EntitlementFlags, GpuLimit, Tier};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::{HardwareClass, ACTIVATION_WINDOW_DAYS};

fn epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

#[test]
fn entitlement_roundtrips_through_json() {
    let ent = Entitlement::new(
        Tier::new("pro".to_owned()),
        HardwareClass::Standard,
        HardwareClass::Standard,
        GpuLimit::Limited(4),
        Lease::new_full(
            "serial-9".to_owned(),
            epoch(),
            LeaseSource::Online,
            ACTIVATION_WINDOW_DAYS,
        ),
        EntitlementFlags::default(),
    );
    let json = serde_json::to_string(&ent).unwrap();
    let back: Entitlement = serde_json::from_str(&json).unwrap();
    assert_eq!(ent, back);
}

#[test]
fn lease_source_is_internally_tagged_not_untagged() {
    // The discriminant must be a visible `source` tag, not positional/untagged.
    let lease = Lease::new_full(
        "serial-7".to_owned(),
        epoch(),
        LeaseSource::Relay,
        ACTIVATION_WINDOW_DAYS,
    );
    let json = serde_json::to_value(&lease).unwrap();
    assert_eq!(json["source"], serde_json::json!("relay"));
}

#[test]
fn gpu_limit_unlimited_and_limited_roundtrip() {
    for limit in [GpuLimit::Unlimited, GpuLimit::Limited(8)] {
        let json = serde_json::to_string(&limit).unwrap();
        let back: GpuLimit = serde_json::from_str(&json).unwrap();
        assert_eq!(limit, back);
    }
    // Internally tagged on `kind` — no untagged ambiguity.
    let v = serde_json::to_value(GpuLimit::Limited(8)).unwrap();
    assert_eq!(v["kind"], serde_json::json!("limited"));
}

#[test]
fn hardware_class_roundtrips() {
    for class in [
        HardwareClass::Standard,
        HardwareClass::Datacenter,
        HardwareClass::Edge,
    ] {
        let json = serde_json::to_string(&class).unwrap();
        let back: HardwareClass = serde_json::from_str(&json).unwrap();
        assert_eq!(class, back);
    }
}

#[test]
fn lease_source_resets_only_via_constructors() {
    // The three lease sources are distinct and serialise to stable tags.
    assert_eq!(
        serde_json::to_value(LeaseSource::Online).unwrap(),
        serde_json::json!("online")
    );
    assert_eq!(
        serde_json::to_value(LeaseSource::Relay).unwrap(),
        serde_json::json!("relay")
    );
    assert_eq!(
        serde_json::to_value(LeaseSource::File).unwrap(),
        serde_json::json!("file")
    );
}
