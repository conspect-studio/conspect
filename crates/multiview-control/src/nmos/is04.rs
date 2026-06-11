//! AMWA **NMOS IS-04** ("Discovery & Registration") resource model.
//!
//! IS-04 describes an IP media facility as a graph of **resources** — a Node
//! hosts Devices; Devices expose Senders and Receivers; Senders originate from
//! Sources/Flows (broadcast-multiviewer brief §6/§8). Multiview publishes its
//! inputs as Receivers and its program/preview outputs as Senders so an NMOS
//! Registry (and the broader facility) can discover and connect it.
//!
//! This module is the **pure JSON model** plus the **registration protocol**
//! shapes — no sockets. The handlers (in [`super::router`](crate::nmos)) serve
//! these over HTTP and the registration client (gated `nmos`) posts them to a
//! registry; both are exhaustively testable with `tower::oneshot`.
//!
//! Every resource carries the IS-04 common fields: a `id` (UUID string), a
//! `version` (the TAI `<seconds>:<nanoseconds>` string that orders updates), a
//! human `label`, a `description`, and free-form `tags`. The model keeps the
//! version as an opaque string — Multiview stamps it from its own clock at the
//! boundary, never from an input PTS (invariant #1).
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The IS-04 fields common to every resource.
///
/// Flattened into each resource type with `#[serde(flatten)]` so the wire JSON
/// is a flat object (as IS-04 defines), not a nested `core` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResourceCore {
    /// The resource UUID (a stable identifier, opaque string here).
    pub id: String,
    /// The TAI version `<seconds>:<nanoseconds>` ordering updates to this
    /// resource (a newer version supersedes an older one).
    pub version: String,
    /// A human-readable label.
    pub label: String,
    /// A longer human-readable description.
    #[serde(default)]
    pub description: String,
    /// Free-form tags (each key maps to a list of string values, per IS-04).
    #[serde(default)]
    pub tags: BTreeMap<String, Vec<String>>,
}

impl ResourceCore {
    /// Build a minimally-populated core with the given id/version/label.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            label: label.into(),
            description: String::new(),
            tags: BTreeMap::new(),
        }
    }
}

/// An IS-04 **Node**: the top-level host (a running Multiview instance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Node {
    /// The common IS-04 fields.
    #[serde(flatten)]
    pub core: ResourceCore,
    /// The HTTP(S) hrefs the node is reachable at (its API base URLs).
    #[serde(default)]
    pub href: String,
    /// The hostname the node advertises.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

/// An IS-04 **Device**: a logical grouping of senders/receivers on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Device {
    /// The common IS-04 fields.
    #[serde(flatten)]
    pub core: ResourceCore,
    /// The id of the node hosting this device.
    pub node_id: String,
    /// The device type URN (e.g. `urn:x-nmos:device:generic`).
    #[serde(rename = "type")]
    pub device_type: String,
    /// The sender ids this device exposes.
    #[serde(default)]
    pub senders: Vec<String>,
    /// The receiver ids this device exposes.
    #[serde(default)]
    pub receivers: Vec<String>,
}

/// The media format of a sender/receiver (the IS-04 `format` URN family).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MediaFormat {
    /// Uncompressed/compressed **video** (`urn:x-nmos:format:video`).
    Video,
    /// **Audio** (`urn:x-nmos:format:audio`).
    Audio,
    /// **Ancillary data** (`urn:x-nmos:format:data`).
    Data,
}

impl MediaFormat {
    /// The IS-04 format URN for this media format.
    #[must_use]
    pub const fn urn(self) -> &'static str {
        match self {
            Self::Video => "urn:x-nmos:format:video",
            Self::Audio => "urn:x-nmos:format:audio",
            Self::Data => "urn:x-nmos:format:data",
        }
    }
}

/// An IS-04 **Sender**: an egress flow (a Multiview program/preview output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Sender {
    /// The common IS-04 fields.
    #[serde(flatten)]
    pub core: ResourceCore,
    /// The id of the device this sender belongs to.
    pub device_id: String,
    /// The id of the flow this sender originates (opaque here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    /// The transport URN (e.g. `urn:x-nmos:transport:rtp.mcast`).
    pub transport: String,
    /// The URL the sender's transport file (SDP) is served at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_href: Option<String>,
}

/// An IS-04 **Receiver**: an ingress flow (a Multiview input).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Receiver {
    /// The common IS-04 fields.
    #[serde(flatten)]
    pub core: ResourceCore,
    /// The id of the device this receiver belongs to.
    pub device_id: String,
    /// The media format this receiver consumes.
    pub format: MediaFormat,
    /// The transport URN.
    pub transport: String,
    /// The id of the sender this receiver is subscribed to, if connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribed_sender: Option<String>,
}

/// The IS-04 resource type discriminator used by the registration protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResourceType {
    /// A node resource.
    Node,
    /// A device resource.
    Device,
    /// A sender resource.
    Sender,
    /// A receiver resource.
    Receiver,
}

/// An IS-04 **registration request**: the `{type, data}` body posted to a
/// Registry's `POST /x-nmos/registration/v1.3/resource`.
///
/// The `data` is the resource JSON for the declared `type`. Modelled internally
/// tagged on `type` (never `untagged`) so it round-trips robustly. The payload
/// is the raw resource JSON value so this one type carries any resource kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Registration {
    /// The kind of resource being registered.
    #[serde(rename = "type")]
    pub resource_type: ResourceType,
    /// The resource document (its IS-04 JSON).
    #[cfg_attr(feature = "openapi", schema(value_type = Object))]
    pub data: serde_json::Value,
}

impl Registration {
    /// Build a node registration request.
    #[must_use]
    pub fn node(node: &Node) -> Self {
        Self {
            resource_type: ResourceType::Node,
            data: serde_json::to_value(node).unwrap_or(serde_json::Value::Null),
        }
    }

    /// Build a sender registration request.
    #[must_use]
    pub fn sender(sender: &Sender) -> Self {
        Self {
            resource_type: ResourceType::Sender,
            data: serde_json::to_value(sender).unwrap_or(serde_json::Value::Null),
        }
    }

    /// Build a receiver registration request.
    #[must_use]
    pub fn receiver(receiver: &Receiver) -> Self {
        Self {
            resource_type: ResourceType::Receiver,
            data: serde_json::to_value(receiver).unwrap_or(serde_json::Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        Device, MediaFormat, Node, Receiver, Registration, ResourceCore, ResourceType, Sender,
    };

    fn core(id: &str) -> ResourceCore {
        ResourceCore::new(id, "1700000000:0", format!("label-{id}"))
    }

    #[test]
    fn node_serialises_with_flattened_core_fields() {
        let node = Node {
            core: core("node-1"),
            href: "http://multiview.local/".to_owned(),
            hostname: Some("multiview.local".to_owned()),
        };
        let json = serde_json::to_value(&node).unwrap();
        // The core fields are flat (not nested under `core`), as IS-04 requires.
        assert_eq!(json["id"], "node-1");
        assert_eq!(json["version"], "1700000000:0");
        assert_eq!(json["label"], "label-node-1");
        assert!(json.get("core").is_none(), "core must be flattened");
        let back: Node = serde_json::from_value(json).unwrap();
        assert_eq!(back, node);
    }

    #[test]
    fn device_round_trips_with_sender_and_receiver_lists() {
        let device = Device {
            core: core("dev-1"),
            node_id: "node-1".to_owned(),
            device_type: "urn:x-nmos:device:generic".to_owned(),
            senders: vec!["snd-1".to_owned()],
            receivers: vec!["rcv-1".to_owned(), "rcv-2".to_owned()],
        };
        let json = serde_json::to_value(&device).unwrap();
        assert_eq!(json["node_id"], "node-1");
        assert_eq!(json["type"], "urn:x-nmos:device:generic");
        let back: Device = serde_json::from_value(json).unwrap();
        assert_eq!(back, device);
    }

    #[test]
    fn receiver_carries_its_media_format_and_round_trips() {
        let rcv = Receiver {
            core: core("rcv-1"),
            device_id: "dev-1".to_owned(),
            format: MediaFormat::Video,
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            subscribed_sender: None,
        };
        let json = serde_json::to_value(&rcv).unwrap();
        assert_eq!(json["format"], "video");
        // An unconnected receiver omits the subscribed sender.
        assert!(json.get("subscribed_sender").is_none());
        let back: Receiver = serde_json::from_value(json).unwrap();
        assert_eq!(back, rcv);
    }

    #[test]
    fn media_format_urns_match_the_is04_vocabulary() {
        assert_eq!(MediaFormat::Video.urn(), "urn:x-nmos:format:video");
        assert_eq!(MediaFormat::Audio.urn(), "urn:x-nmos:format:audio");
        assert_eq!(MediaFormat::Data.urn(), "urn:x-nmos:format:data");
    }

    #[test]
    fn registration_wraps_a_resource_as_typed_data() {
        let sender = Sender {
            core: core("snd-1"),
            device_id: "dev-1".to_owned(),
            flow_id: Some("flow-1".to_owned()),
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            manifest_href: Some("http://multiview.local/snd-1.sdp".to_owned()),
        };
        let reg = Registration::sender(&sender);
        let json = serde_json::to_value(&reg).unwrap();
        assert_eq!(json["type"], "sender");
        assert_eq!(json["data"]["id"], "snd-1");
        let back: Registration = serde_json::from_value(json).unwrap();
        assert_eq!(back.resource_type, ResourceType::Sender);
        // The wrapped resource round-trips back to the original Sender.
        let unwrapped: Sender = serde_json::from_value(back.data).unwrap();
        assert_eq!(unwrapped, sender);
    }
}
