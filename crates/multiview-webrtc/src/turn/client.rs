//! A sans-IO TURN client (RFC 5766 / RFC 8656).
//!
//! The client owns no socket. It is driven cooperatively, mirroring str0m's own
//! sans-IO shape: [`TurnClient::poll_output`] yields the next datagram to send (to
//! the TURN server) or a timeout; [`TurnClient::handle_input`] feeds a received
//! datagram back in. The endpoint's driver task ([`crate::transport`]) owns the
//! UDP socket and shuttles bytes between this client and the wire. Because it is
//! sans-IO it is exhaustively tested in memory against a fake TURN server.
//!
//! ## What it does
//!
//! 1. **Allocate** a relay on the configured TURN server, handling the RFC 5766
//!    §6.1 unauthenticated-first / `401`-challenge / authenticated-retry dance and
//!    learning the relayed transport address (XOR-RELAYED-ADDRESS).
//! 2. **Refresh** the allocation before its lifetime expires.
//! 3. **`CreatePermission`** for each peer the relay must accept traffic from.
//! 4. **Send / Data** indications: wrap an outbound application datagram destined
//!    for a peer into a Send indication, and unwrap an inbound Data indication
//!    back into `(peer, payload)`.
//!
//! The learned relay address is handed to str0m as a relay ICE candidate; relayed
//! media then rides through `wrap_send`/`unwrap_data` beneath str0m's sans-IO core.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::auth::TurnCredential;
use super::message::{Attribute, Class, Method, StunMessage, TransactionId};
use crate::error::TurnError;

/// How long before the allocation's expiry the client proactively refreshes.
const REFRESH_LEAD: Duration = Duration::from_secs(60);

/// The retransmit interval for an in-flight request (RFC 5389 RTO; a fixed value
/// is fine for a self-hosted relay on a low-latency path).
const RETRANSMIT_INTERVAL: Duration = Duration::from_millis(500);

/// The lifecycle of a TURN allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnState {
    /// No allocation yet; the first (unauthenticated) Allocate has not been sent.
    Idle,
    /// An Allocate is in flight (either the initial probe or the authenticated
    /// retry).
    Allocating,
    /// A relay is allocated at `relay`, valid until `expires_at`.
    Allocated {
        /// The relayed transport address the TURN server assigned.
        relay: SocketAddr,
        /// The absolute instant the allocation expires (refresh before this).
        expires_at: Instant,
    },
    /// The allocation failed terminally (e.g. bad credentials).
    Failed,
}

/// An event surfaced from [`TurnClient::handle_input`] worth acting on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnEvent {
    /// The relay was allocated; the endpoint can now register the relay
    /// candidate with str0m.
    Allocated(SocketAddr),
    /// A permission for `peer` was installed.
    PermissionCreated(SocketAddr),
    /// Application data relayed back from `peer`.
    Data(SocketAddr, Vec<u8>),
}

/// What the client wants the driver to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnOutput {
    /// Send `payload` to `destination` (the TURN server).
    Transmit {
        /// The TURN server address the datagram goes to.
        destination: SocketAddr,
        /// The datagram bytes.
        payload: Vec<u8>,
    },
    /// Nothing to send now; wake the client again at this instant.
    Timeout(Instant),
    /// Nothing to do and no timeout pending.
    Idle,
}

/// An in-flight request awaiting a response, keyed by transaction id.
#[derive(Debug, Clone)]
struct Pending {
    transaction_id: TransactionId,
    method: Method,
    /// The exact request bytes, for retransmit.
    bytes: Vec<u8>,
    /// When to retransmit if no response has arrived.
    retransmit_at: Instant,
    /// The peer this request concerns (for `CreatePermission` / `ChannelBind`).
    peer: Option<SocketAddr>,
}

/// A sans-IO TURN client for one allocation on one TURN server.
#[derive(Debug)]
pub struct TurnClient {
    server: SocketAddr,
    credential: TurnCredential,
    state: TurnState,
    /// REALM learned from the server's `401` (or the configured one).
    realm: Option<String>,
    /// NONCE learned from the server's `401`.
    nonce: Option<String>,
    /// The single in-flight request, if any (TURN requests are serialized here —
    /// the client issues one at a time, which is ample for a self-hosted relay).
    pending: Option<Pending>,
    /// A queue of requests waiting to go out (e.g. `CreatePermission` requested
    /// while an Allocate is still in flight).
    queue: Vec<QueuedRequest>,
    permissions: HashSet<SocketAddr>,
    /// Whether the initial unauthenticated Allocate has been emitted.
    started: bool,
    /// The relay address family the next Allocate requests (IPv6-first per
    /// ADR-0042). A server `440 Address Family not Supported` flips this to IPv4
    /// and re-queues the Allocate, so an IPv4-only relay still works.
    request_family: super::message::AddressFamily,
}

#[derive(Debug, Clone, Copy)]
enum QueuedRequest {
    Allocate,
    Refresh,
    CreatePermission(SocketAddr),
}

impl TurnClient {
    /// Build a client for `server` with `credential`. The allocation is not
    /// started until the first [`Self::poll_output`].
    #[must_use]
    pub fn new(server: SocketAddr, credential: TurnCredential) -> Self {
        let realm = credential.realm.clone();
        Self {
            server,
            credential,
            state: TurnState::Idle,
            realm,
            nonce: None,
            pending: None,
            queue: vec![QueuedRequest::Allocate],
            permissions: HashSet::new(),
            started: false,
            // IPv6-first: ask for an IPv6 relay; fall back to IPv4 only on a
            // server 440 (Address Family not Supported).
            request_family: super::message::AddressFamily::Ipv6,
        }
    }

    /// The current allocation state.
    #[must_use]
    pub fn state(&self) -> TurnState {
        self.state.clone()
    }

    /// The TURN server transport address this client allocates against — the
    /// endpoint driver routes datagrams from this address into the client.
    #[must_use]
    pub fn server_addr(&self) -> SocketAddr {
        self.server
    }

    /// The learned relay address, if allocated.
    #[must_use]
    pub fn relay(&self) -> Option<SocketAddr> {
        match self.state {
            TurnState::Allocated { relay, .. } => Some(relay),
            _ => None,
        }
    }

    /// Whether a permission is installed for `peer`.
    #[must_use]
    pub fn has_permission(&self, peer: &SocketAddr) -> bool {
        self.permissions.contains(peer)
    }

    /// Request a permission for `peer` (queued; driven on the next poll).
    pub fn create_permission(&mut self, peer: SocketAddr, _now: Instant) {
        if !self.permissions.contains(&peer) {
            self.queue.push(QueuedRequest::CreatePermission(peer));
        }
    }

    /// Drive the client: emit the next datagram, a timeout, or idle.
    pub fn poll_output(&mut self, now: Instant) -> TurnOutput {
        self.started = true;

        // Retransmit an in-flight request whose timer fired.
        if let Some(pending) = &mut self.pending {
            if now >= pending.retransmit_at {
                pending.retransmit_at = now + RETRANSMIT_INTERVAL;
                return TurnOutput::Transmit {
                    destination: self.server,
                    payload: pending.bytes.clone(),
                };
            }
            // Still waiting on this request; nothing else goes out until it
            // resolves (requests are serialized).
            return TurnOutput::Timeout(pending.retransmit_at);
        }

        // Proactively refresh as the allocation nears expiry. Enqueue here but
        // defer emission to the next poll (return `Timeout(now)` to prompt an
        // immediate re-poll): the driver always sends what `poll_output` yields,
        // so detection and emission stay distinct ticks and a single "the
        // allocation is near expiry" wake never races its own Transmit.
        if let TurnState::Allocated { expires_at, .. } = self.state {
            if now + REFRESH_LEAD >= expires_at
                && !self
                    .queue
                    .iter()
                    .any(|q| matches!(q, QueuedRequest::Refresh))
            {
                self.queue.push(QueuedRequest::Refresh);
                return TurnOutput::Timeout(now);
            }
        }

        // Issue the next queued request.
        if !self.queue.is_empty() {
            let next = self.queue.remove(0);
            return self.emit(next, now);
        }

        match self.state {
            TurnState::Allocated { expires_at, .. } => {
                let wake = expires_at.checked_sub(REFRESH_LEAD).unwrap_or(now).max(now);
                TurnOutput::Timeout(wake)
            }
            _ => TurnOutput::Idle,
        }
    }

    fn emit(&mut self, req: QueuedRequest, now: Instant) -> TurnOutput {
        let (msg, method, peer) = match req {
            QueuedRequest::Allocate => {
                self.state = TurnState::Allocating;
                let mut m = StunMessage::request(Method::Allocate);
                m.push(Attribute::RequestedTransportUdp);
                // IPv6-first relay (ADR-0042): ask the server for the current
                // family (IPv6 until a 440 forces IPv4).
                m.push(Attribute::RequestedAddressFamily(self.request_family));
                m.push(Attribute::Lifetime(600));
                (self.authenticate(m), Method::Allocate, None)
            }
            QueuedRequest::Refresh => {
                let mut m = StunMessage::request(Method::Refresh);
                m.push(Attribute::Lifetime(600));
                (self.authenticate(m), Method::Refresh, None)
            }
            QueuedRequest::CreatePermission(peer) => {
                let mut m = StunMessage::request(Method::CreatePermission);
                m.push(Attribute::XorPeerAddress(peer));
                (self.authenticate(m), Method::CreatePermission, Some(peer))
            }
        };
        let (transaction_id, bytes) = msg;
        self.pending = Some(Pending {
            transaction_id,
            method,
            bytes: bytes.clone(),
            retransmit_at: now + RETRANSMIT_INTERVAL,
            peer,
        });
        TurnOutput::Transmit {
            destination: self.server,
            payload: bytes,
        }
    }

    /// Serialize `msg` with long-term credentials when a realm/nonce are known,
    /// returning `(transaction_id, bytes)`.
    fn authenticate(&self, mut msg: StunMessage) -> (TransactionId, Vec<u8>) {
        let transaction_id = msg.transaction_id();
        match (&self.realm, &self.nonce) {
            (Some(realm), Some(nonce)) => {
                let realm = realm.clone();
                msg.push(Attribute::Username(self.credential.username.clone()));
                msg.push(Attribute::Realm(realm.clone()));
                msg.push(Attribute::Nonce(nonce.clone()));
                let key = super::message::long_term_key(
                    &self.credential.username,
                    &realm,
                    &self.credential.password,
                );
                (transaction_id, msg.to_bytes(Some(&key)))
            }
            // No challenge yet — send unauthenticated to provoke the 401.
            _ => (transaction_id, msg.to_bytes(None)),
        }
    }

    /// Feed a received datagram. Returns any [`TurnEvent`] worth acting on.
    ///
    /// # Errors
    ///
    /// [`TurnError`] if the datagram is malformed, carries an unexpected
    /// transaction id for a request response, or signals a terminal server error.
    pub fn handle_input(
        &mut self,
        datagram: &[u8],
        now: Instant,
    ) -> Result<Option<TurnEvent>, TurnError> {
        // A Data indication is server→client relayed media, not a response.
        if let Ok(msg) = StunMessage::parse(datagram) {
            if msg.class() == Class::Indication && msg.method() == Method::Data {
                if let (Some(peer), Some(data)) = (msg.peer_address(), msg.data()) {
                    return Ok(Some(TurnEvent::Data(peer, data.to_vec())));
                }
                return Ok(None);
            }
            return self.handle_response(&msg, now);
        }
        Err(TurnError::NotStun)
    }

    fn handle_response(
        &mut self,
        msg: &StunMessage,
        now: Instant,
    ) -> Result<Option<TurnEvent>, TurnError> {
        let Some(pending) = self.pending.as_ref() else {
            return Err(TurnError::UnknownTransaction);
        };
        if msg.transaction_id() != pending.transaction_id {
            // A stale/duplicate response for an already-resolved request.
            return Ok(None);
        }
        let method = pending.method;
        let peer = pending.peer;

        match msg.class() {
            Class::Error => {
                let code = msg.error_code().unwrap_or(0);
                // 401 Unauthorized / 438 Stale Nonce: learn realm+nonce and retry.
                if code == 401 || code == 438 {
                    self.realm = msg
                        .realm()
                        .map(str::to_owned)
                        .or_else(|| self.realm.clone());
                    self.nonce = msg.nonce().map(str::to_owned);
                    self.pending = None;
                    // Re-queue the same request, now that we can authenticate.
                    self.requeue(method, peer);
                    return Ok(None);
                }
                // 440 Address Family not Supported (RFC 8656 §7.1): the server
                // cannot give us the requested family (IPv6). Fall back to IPv4 and
                // re-Allocate — IPv6-first, IPv4 as the legacy fallback, never a
                // hard failure on an IPv4-only relay.
                if code == 440
                    && method == Method::Allocate
                    && self.request_family == super::message::AddressFamily::Ipv6
                {
                    self.request_family = super::message::AddressFamily::Ipv4;
                    self.pending = None;
                    self.state = TurnState::Idle;
                    self.requeue(Method::Allocate, None);
                    return Ok(None);
                }
                self.pending = None;
                if method == Method::Allocate {
                    self.state = TurnState::Failed;
                }
                Err(TurnError::ServerError {
                    code,
                    reason: msg.error_reason().unwrap_or("").to_owned(),
                })
            }
            Class::Success => {
                self.pending = None;
                self.on_success(method, peer, msg, now)
            }
            // A request/indication arriving as "input" is not expected.
            _ => Ok(None),
        }
    }

    fn on_success(
        &mut self,
        method: Method,
        peer: Option<SocketAddr>,
        msg: &StunMessage,
        now: Instant,
    ) -> Result<Option<TurnEvent>, TurnError> {
        match method {
            Method::Allocate => {
                let relay = msg
                    .relayed_address()
                    .ok_or(TurnError::MissingAttribute("XOR-RELAYED-ADDRESS"))?;
                let lifetime = msg.lifetime().unwrap_or(600);
                self.state = TurnState::Allocated {
                    relay,
                    expires_at: now + Duration::from_secs(u64::from(lifetime)),
                };
                Ok(Some(TurnEvent::Allocated(relay)))
            }
            Method::Refresh => {
                let lifetime = msg.lifetime().unwrap_or(600);
                if let TurnState::Allocated { relay, .. } = self.state {
                    self.state = TurnState::Allocated {
                        relay,
                        expires_at: now + Duration::from_secs(u64::from(lifetime)),
                    };
                }
                Ok(None)
            }
            Method::CreatePermission => {
                if let Some(peer) = peer {
                    self.permissions.insert(peer);
                    return Ok(Some(TurnEvent::PermissionCreated(peer)));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn requeue(&mut self, method: Method, peer: Option<SocketAddr>) {
        let req = match (method, peer) {
            (Method::Allocate, _) => QueuedRequest::Allocate,
            (Method::Refresh, _) => QueuedRequest::Refresh,
            (Method::CreatePermission, Some(p)) => QueuedRequest::CreatePermission(p),
            _ => return,
        };
        // Retried requests go to the FRONT so auth completes before other work.
        self.queue.insert(0, req);
    }

    /// Wrap an outbound application datagram destined for `peer` into a TURN Send
    /// indication addressed to the TURN server.
    #[must_use]
    pub fn wrap_send(&self, peer: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let mut msg = StunMessage::indication(Method::Send);
        msg.push(Attribute::XorPeerAddress(peer));
        msg.push(Attribute::Data(payload.to_vec()));
        msg.to_bytes(None)
    }

    /// Unwrap an inbound TURN Data indication into `(peer, payload)`.
    ///
    /// Returns `None` if `datagram` is not a Data indication.
    #[must_use]
    pub fn unwrap_data(&self, datagram: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        let msg = StunMessage::parse(datagram).ok()?;
        if msg.class() != Class::Indication || msg.method() != Method::Data {
            return None;
        }
        Some((msg.peer_address()?, msg.data()?.to_vec()))
    }
}
