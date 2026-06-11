//! AMWA **NMOS** IP-facility discovery, connection, channel-mapping, and
//! authorization for the control plane.
//!
//! Multiview speaks the NMOS suite so it appears as a first-class node in an IP
//! broadcast facility (broadcast-multiviewer brief §6/§8):
//!
//! * [`is04`] — **IS-04** Discovery & Registration: the node/device/sender/
//!   receiver resource model + the registration request shape.
//! * [`is05`] — **IS-05** Connection Management: staged/active transport params +
//!   a minimal SDP transport-file parse for ST 2110 binding.
//! * [`is08`] — **IS-08** Audio Channel Mapping: the output→input channel map.
//! * [`is10`] — **IS-10** Authorization: the OAuth 2.0 / JWT claims model that
//!   extends the crate's API-key auth.
//!
//! This module adds the [`NmosRegistry`] (the control-plane store of Multiview's own
//! NMOS resources + each receiver's connection state) and the **NMOS Node API**
//! axum handlers ([`nmos_router`]) that serve those resources and accept IS-05
//! connection `PATCH`es. The handlers are pure HTTP over the in-memory registry
//! and are exhaustively testable with `tower::oneshot` — **no sockets**.
//!
//! ## Isolation (invariant #10)
//!
//! The NMOS registry is control-plane state only. Serving a resource or staging a
//! connection never touches the engine's data plane and never back-pressures it.
//! Connecting a 2110 receiver changes which essence the engine *samples*; it
//! cannot pace the output clock (invariant #1).
//!
//! ## Gated transport (`nmos` feature)
//!
//! mDNS/DNS-SD registry discovery and the live ST 2110 receiver bind (joining a
//! multicast group on a real NIC) live behind the off-by-default `nmos` feature
//! (`transport`). With the feature off, the full NMOS **model + Node API** is
//! still compiled, served, and tested; only the live network discovery/bind is
//! absent.
use std::sync::Mutex;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};

use crate::auth::{Action, Principal};
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

pub mod is04;
pub mod is05;
pub mod is08;
pub mod is10;

#[cfg(feature = "nmos")]
pub mod transport;

use is04::{Device, Node, Receiver, Sender};
use is05::{ConnectionRequest, ConnectionState};

/// The control-plane registry of Multiview's own NMOS resources + connection state.
///
/// Holds the node Multiview advertises, its devices, the senders (program/preview
/// egress) and receivers (inputs) it exposes, and one IS-05 [`ConnectionState`]
/// per receiver. Control-plane state only; never on the engine's data plane
/// (invariant #10). Guarded by a single `Mutex` the engine never holds.
#[derive(Debug, Default)]
pub struct NmosRegistry {
    inner: Mutex<RegistryInner>,
}

/// The registry's inner, lock-guarded data.
#[derive(Debug, Default)]
struct RegistryInner {
    node: Option<Node>,
    devices: Vec<Device>,
    senders: Vec<Sender>,
    receivers: Vec<Receiver>,
    /// One connection state per receiver id (IS-05 `single/receivers/{id}`).
    connections: std::collections::HashMap<String, ConnectionState>,
}

impl NmosRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner data, recovering from a poisoned lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Set the node resource Multiview advertises.
    pub fn set_node(&self, node: Node) {
        self.lock().node = Some(node);
    }

    /// The node resource, if one has been set.
    #[must_use]
    pub fn node(&self) -> Option<Node> {
        self.lock().node.clone()
    }

    /// Register a device.
    pub fn add_device(&self, device: Device) {
        self.lock().devices.push(device);
    }

    /// Register a sender (egress).
    pub fn add_sender(&self, sender: Sender) {
        self.lock().senders.push(sender);
    }

    /// Register a receiver (ingress); seeds its empty connection state.
    pub fn add_receiver(&self, receiver: Receiver) {
        let mut guard = self.lock();
        guard
            .connections
            .entry(receiver.core.id.clone())
            .or_default();
        guard.receivers.push(receiver);
    }

    /// All devices.
    #[must_use]
    pub fn devices(&self) -> Vec<Device> {
        self.lock().devices.clone()
    }

    /// All senders.
    #[must_use]
    pub fn senders(&self) -> Vec<Sender> {
        self.lock().senders.clone()
    }

    /// All receivers.
    #[must_use]
    pub fn receivers(&self) -> Vec<Receiver> {
        self.lock().receivers.clone()
    }

    /// The connection state of one receiver, if it exists.
    #[must_use]
    pub fn connection(&self, receiver_id: &str) -> Option<ConnectionState> {
        self.lock().connections.get(receiver_id).cloned()
    }

    /// Stage **and**, for an immediate activation, apply an IS-05 connection
    /// request on a receiver. Returns the resulting connection state.
    ///
    /// # Errors
    ///
    /// [`ControlError::NotFound`] if no receiver has that id.
    pub fn stage_connection(
        &self,
        receiver_id: &str,
        request: ConnectionRequest,
    ) -> ControlResult<ConnectionState> {
        let mut guard = self.lock();
        let state =
            guard
                .connections
                .get_mut(receiver_id)
                .ok_or_else(|| ControlError::NotFound {
                    kind: NMOS_RECEIVER_KIND,
                    id: receiver_id.to_owned(),
                })?;
        state.stage(request);
        state.activate_if_immediate();
        Ok(state.clone())
    }
}

/// The resource-collection name for an NMOS receiver, used in not-found errors.
pub const NMOS_RECEIVER_KIND: &str = "nmos_receiver";

/// `GET /x-nmos/node/v1.3/self` — the node resource (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/x-nmos/node/v1.3/self",
        tag = "nmos",
        responses(
            (status = 200, description = "The Multiview NMOS node resource.", body = crate::nmos::is04::Node),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 404, description = "No node resource configured.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_self(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Node>> {
    principal.role.require(Action::Read)?;
    state
        .nmos
        .node()
        .map(Json)
        .ok_or_else(|| ControlError::NotFound {
            kind: "nmos_node",
            id: "self".to_owned(),
        })
}

/// `GET /x-nmos/node/v1.3/devices` — the device resources (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/x-nmos/node/v1.3/devices",
        tag = "nmos",
        responses(
            (status = 200, description = "All NMOS devices.", body = [crate::nmos::is04::Device]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_devices(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Device>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.nmos.devices()))
}

/// `GET /x-nmos/node/v1.3/senders` — the sender resources (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/x-nmos/node/v1.3/senders",
        tag = "nmos",
        responses(
            (status = 200, description = "All NMOS senders (egress).", body = [crate::nmos::is04::Sender]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_senders(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Sender>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.nmos.senders()))
}

/// `GET /x-nmos/node/v1.3/receivers` — the receiver resources (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/x-nmos/node/v1.3/receivers",
        tag = "nmos",
        responses(
            (status = 200, description = "All NMOS receivers (ingress).", body = [crate::nmos::is04::Receiver]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_receivers(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Receiver>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.nmos.receivers()))
}

/// `GET /x-nmos/connection/v1.1/single/receivers/{id}/active` — the receiver's
/// active IS-05 connection state (role: read).
pub(crate) async fn get_active(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<ConnectionState>> {
    principal.role.require(Action::Read)?;
    state
        .nmos
        .connection(&id)
        .map(Json)
        .ok_or(ControlError::NotFound {
            kind: NMOS_RECEIVER_KIND,
            id,
        })
}

/// `PATCH /x-nmos/connection/v1.1/single/receivers/{id}/staged` — stage (and,
/// for an immediate activation, apply) a connection on a receiver (role: write).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        patch,
        path = "/x-nmos/connection/v1.1/single/receivers/{id}/staged",
        tag = "nmos",
        params(("id" = String, Path, description = "NMOS receiver id.")),
        request_body = crate::nmos::is05::ConnectionRequest,
        responses(
            (status = 200, description = "The resulting connection state.", body = crate::nmos::is05::ConnectionState),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No receiver with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn patch_staged(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(request): Json<ConnectionRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let connection = state.nmos.stage_connection(&id, request)?;
    Ok((StatusCode::OK, Json(connection)).into_response())
}

/// Build the NMOS Node API router under `/x-nmos`.
///
/// Carries [`AppState`] so it is ready to merge into the main control router.
pub fn nmos_router() -> Router<AppState> {
    Router::new()
        .route("/x-nmos/node/v1.3/self", get(get_self))
        .route("/x-nmos/node/v1.3/devices", get(list_devices))
        .route("/x-nmos/node/v1.3/senders", get(list_senders))
        .route("/x-nmos/node/v1.3/receivers", get(list_receivers))
        .route(
            "/x-nmos/connection/v1.1/single/receivers/{id}/active",
            get(get_active),
        )
        .route(
            "/x-nmos/connection/v1.1/single/receivers/{id}/staged",
            patch(patch_staged),
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::is04::{Device, MediaFormat, Node, Receiver, ResourceCore, Sender};
    use super::is05::{Activation, ConnectionRequest, TransportParams};
    use super::NmosRegistry;

    fn core(id: &str) -> ResourceCore {
        ResourceCore::new(id, "1700000000:0", id)
    }

    fn seeded() -> NmosRegistry {
        let reg = NmosRegistry::new();
        reg.set_node(Node {
            core: core("node-1"),
            href: "http://multiview.local/".to_owned(),
            hostname: None,
        });
        reg.add_device(Device {
            core: core("dev-1"),
            node_id: "node-1".to_owned(),
            device_type: "urn:x-nmos:device:generic".to_owned(),
            senders: vec!["snd-1".to_owned()],
            receivers: vec!["rcv-1".to_owned()],
        });
        reg.add_sender(Sender {
            core: core("snd-1"),
            device_id: "dev-1".to_owned(),
            flow_id: None,
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            manifest_href: None,
        });
        reg.add_receiver(Receiver {
            core: core("rcv-1"),
            device_id: "dev-1".to_owned(),
            format: MediaFormat::Video,
            transport: "urn:x-nmos:transport:rtp.mcast".to_owned(),
            subscribed_sender: None,
        });
        reg
    }

    #[test]
    fn registry_seeds_an_empty_connection_for_each_receiver() {
        let reg = seeded();
        let conn = reg.connection("rcv-1").expect("receiver connection exists");
        assert!(conn.active.is_empty());
        assert!(conn.staged.is_none());
    }

    #[test]
    fn staging_an_immediate_connection_activates_it() {
        let reg = seeded();
        let request = ConnectionRequest {
            master_enable: Some(true),
            activation: Activation::immediate(),
            transport_params: vec![TransportParams {
                destination_ip: Some("239.0.0.1".to_owned()),
                destination_port: Some(5004),
                source_ip: None,
                rtp_enabled: Some(true),
            }],
            sender_id: Some("snd-1".to_owned()),
            transport_file: None,
        };
        let state = reg.stage_connection("rcv-1", request).unwrap();
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].destination_port, Some(5004));
        assert!(state.master_enable);
        assert!(state.staged.is_none());
    }

    #[test]
    fn staging_on_an_unknown_receiver_is_not_found() {
        let reg = seeded();
        let request = ConnectionRequest {
            master_enable: None,
            activation: Activation::immediate(),
            transport_params: vec![],
            sender_id: None,
            transport_file: None,
        };
        let err = reg.stage_connection("ghost", request).unwrap_err();
        assert!(matches!(err, crate::error::ControlError::NotFound { .. }));
    }

    #[test]
    fn registry_lists_devices_senders_receivers() {
        let reg = seeded();
        assert_eq!(reg.devices().len(), 1);
        assert_eq!(reg.senders().len(), 1);
        assert_eq!(reg.receivers().len(), 1);
        assert_eq!(reg.node().unwrap().core.id, "node-1");
    }
}
