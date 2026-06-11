//! Scanout-affinity discovery tests (DEV-B2, ADR-0044 §3 / display-out.md §3).
//!
//! Prove the pure connector → owning-GPU mapping over an **injected** mock
//! [`ScanoutProbe`] — no DRM, no hardware, GPU-less CI:
//!
//! - a connected connector resolves to the [`DeviceId`] of the card node that
//!   owns it (the scanout framebuffer must live on the connector-owning GPU);
//! - an unknown connector name resolves to `None` (never a fabricated owner);
//! - a single-GPU host maps every connector to that one device (trivial
//!   satisfaction — the machinery still models it);
//! - the inventory lists exactly the connected, EDID-bearing outputs the probe
//!   reported, and the locality set a display sink declares is exactly the
//!   owning devices of its connectors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_hal::load::{DeviceId, Vendor};
use multiview_hal::scanout::{
    CardNode, ConnectionStatus, Connector, ConnectorId, ScanoutInventory, ScanoutProbe,
};

fn amd(stable: &str, index: u32) -> DeviceId {
    DeviceId::new(Vendor::Amd, stable, index)
}

fn nv(stable: &str, index: u32) -> DeviceId {
    DeviceId::new(Vendor::Nvidia, stable, index)
}

/// A probe double returning a fixed, already-reconciled inventory (the
/// `_devices` set is irrelevant to a pre-built double).
struct FixedProbe(ScanoutInventory);
impl ScanoutProbe for FixedProbe {
    fn enumerate(&self, _devices: &[DeviceId]) -> ScanoutInventory {
        self.0.clone()
    }
}

/// A two-GPU host: card0 owns DP-1 (connected, EDID), card1 owns HDMI-A-1
/// (connected, no EDID).
fn two_gpu_inventory() -> ScanoutInventory {
    let card0 = CardNode::new(
        "card0",
        amd("0000:00:01.0", 0),
        vec![Connector::new(
            ConnectorId::new("DP-1"),
            ConnectionStatus::Connected,
            true,
        )],
    );
    let card1 = CardNode::new(
        "card1",
        nv("GPU-UUID-1", 1),
        vec![Connector::new(
            ConnectorId::new("HDMI-A-1"),
            ConnectionStatus::Connected,
            false,
        )],
    );
    ScanoutInventory::new(vec![card0, card1])
}

#[test]
fn connector_resolves_to_owning_gpu() {
    let probe = FixedProbe(two_gpu_inventory());
    let inv = probe.enumerate(&[]);
    assert_eq!(
        inv.owning_device(&ConnectorId::new("DP-1")),
        Some(&amd("0000:00:01.0", 0)),
        "DP-1 lives on card0's GPU"
    );
    assert_eq!(
        inv.owning_device(&ConnectorId::new("HDMI-A-1")),
        Some(&nv("GPU-UUID-1", 1)),
        "HDMI-A-1 lives on card1's GPU"
    );
}

#[test]
fn unknown_connector_has_no_owner() {
    let inv = two_gpu_inventory();
    assert_eq!(
        inv.owning_device(&ConnectorId::new("DP-99")),
        None,
        "an unlisted connector never fabricates an owning GPU"
    );
}

#[test]
fn single_gpu_host_maps_every_connector_to_the_one_device() {
    // The display-node / thin-client tier: one card, two heads. Both connectors
    // must resolve to the single device — trivially satisfied, but modelled.
    let only = amd("0000:00:01.0", 0);
    let card = CardNode::new(
        "card0",
        only.clone(),
        vec![
            Connector::new(ConnectorId::new("DP-1"), ConnectionStatus::Connected, false),
            Connector::new(ConnectorId::new("DP-2"), ConnectionStatus::Connected, true),
        ],
    );
    let inv = ScanoutInventory::new(vec![card]);
    assert_eq!(inv.owning_device(&ConnectorId::new("DP-1")), Some(&only));
    assert_eq!(inv.owning_device(&ConnectorId::new("DP-2")), Some(&only));
}

#[test]
fn locality_for_connectors_collects_the_owning_devices() {
    // A display sink that drives DP-1 and HDMI-A-1 (one per GPU) must declare
    // BOTH owning GPUs as its scanout locality — the composite that feeds it can
    // only legally live on a GPU that owns one of those connectors.
    let inv = two_gpu_inventory();
    let want = inv.locality_for(&[ConnectorId::new("DP-1"), ConnectorId::new("HDMI-A-1")]);
    assert!(want.contains(&amd("0000:00:01.0", 0)));
    assert!(want.contains(&nv("GPU-UUID-1", 1)));
    assert_eq!(want.len(), 2, "exactly the two owning GPUs, deduplicated");
}

#[test]
fn locality_dedupes_two_connectors_on_one_gpu() {
    // Two heads on the same card -> one locality device, not two.
    let only = amd("0000:00:01.0", 0);
    let card = CardNode::new(
        "card0",
        only.clone(),
        vec![
            Connector::new(ConnectorId::new("DP-1"), ConnectionStatus::Connected, true),
            Connector::new(ConnectorId::new("DP-2"), ConnectionStatus::Connected, true),
        ],
    );
    let inv = ScanoutInventory::new(vec![card]);
    let want = inv.locality_for(&[ConnectorId::new("DP-1"), ConnectorId::new("DP-2")]);
    assert_eq!(want, vec![only], "same-card heads collapse to one locality");
}

#[test]
fn connected_outputs_lists_only_connected_connectors() {
    // A disconnected connector is reported by the probe but is not a scanout
    // target; `connected_connectors` filters to the lit outputs.
    let card = CardNode::new(
        "card0",
        amd("0000:00:01.0", 0),
        vec![
            Connector::new(ConnectorId::new("DP-1"), ConnectionStatus::Connected, true),
            Connector::new(
                ConnectorId::new("DP-2"),
                ConnectionStatus::Disconnected,
                false,
            ),
            Connector::new(ConnectorId::new("DP-3"), ConnectionStatus::Unknown, false),
        ],
    );
    let inv = ScanoutInventory::new(vec![card]);
    let connected: Vec<&ConnectorId> = inv.connected_connectors().collect();
    assert_eq!(connected.len(), 1, "only the one Connected output");
    assert_eq!(connected[0], &ConnectorId::new("DP-1"));
}

#[test]
fn disconnected_connector_still_has_a_known_owner() {
    // Owner mapping is by card membership, independent of hotplug state: a
    // momentarily-disconnected connector still belongs to its card's GPU (so a
    // re-plug does not need a fresh device lookup).
    let card = CardNode::new(
        "card0",
        amd("0000:00:01.0", 0),
        vec![Connector::new(
            ConnectorId::new("DP-2"),
            ConnectionStatus::Disconnected,
            false,
        )],
    );
    let inv = ScanoutInventory::new(vec![card]);
    assert_eq!(
        inv.owning_device(&ConnectorId::new("DP-2")),
        Some(&amd("0000:00:01.0", 0))
    );
}

#[test]
fn empty_inventory_owns_nothing() {
    // A host with no DRM cards (or a feature-off CI build, where the real probe
    // returns an empty inventory) maps every connector to None — cleanly, never
    // a panic.
    let inv = ScanoutInventory::new(vec![]);
    assert_eq!(inv.owning_device(&ConnectorId::new("DP-1")), None);
    assert_eq!(inv.connected_connectors().count(), 0);
    assert!(inv.locality_for(&[ConnectorId::new("DP-1")]).is_empty());
}
