//! An in-process fake TURN server for the sans-IO client tests. Not a real
//! socket server — it consumes a request datagram and returns the reply
//! datagram, modelling the RFC 5766 long-term-credential challenge, Allocate,
//! `CreatePermission`, Refresh, and Data indications.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc,
    clippy::unused_self,
    unreachable_pub,
    dead_code
)]

use std::collections::HashSet;
use std::net::SocketAddr;

use multiview_webrtc::turn::message::{long_term_key, Attribute, Class, Method, StunMessage};

const REALM: &str = "example.org";

/// A minimal fake TURN server driven entirely in memory.
pub struct FakeTurnServer {
    addr: SocketAddr,
    relay: SocketAddr,
    username: String,
    password: String,
    realm: String,
    nonce: String,
    permissions: HashSet<SocketAddr>,
    saw_auth_allocate: bool,
    refreshes: u32,
}

impl FakeTurnServer {
    /// Construct a fake server that allocates `relay` for the given long-term
    /// credential.
    #[must_use]
    pub fn new(
        addr: SocketAddr,
        relay: SocketAddr,
        username: &str,
        password: &str,
        realm: &str,
    ) -> Self {
        Self {
            addr,
            relay,
            username: username.to_owned(),
            password: password.to_owned(),
            realm: realm.to_owned(),
            nonce: "nonce-abc-123".to_owned(),
            permissions: HashSet::new(),
            saw_auth_allocate: false,
            refreshes: 0,
        }
    }

    fn key(&self) -> Vec<u8> {
        long_term_key(&self.username, &self.realm, &self.password)
    }

    /// Handle a request datagram, returning the reply (or `None` for an
    /// indication, which gets no response).
    #[must_use]
    pub fn handle(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
        let msg = StunMessage::parse(datagram).expect("client sends valid STUN");
        match (msg.class(), msg.method()) {
            (Class::Request, Method::Allocate) => Some(self.handle_allocate(&msg)),
            (Class::Request, Method::Refresh) => Some(self.handle_refresh(&msg)),
            (Class::Request, Method::CreatePermission) => Some(self.handle_create_permission(&msg)),
            (Class::Request, Method::ChannelBind) => Some(self.handle_channel_bind(&msg)),
            (Class::Indication, Method::Send) => {
                // Send indications are not answered; record the peer permission
                // requirement only.
                None
            }
            _ => None,
        }
    }

    fn challenge_or_verify(&mut self, msg: &StunMessage) -> Result<(), Vec<u8>> {
        // No credentials → 401 with REALM + NONCE.
        let has_integrity = msg
            .attributes()
            .iter()
            .any(|a| matches!(a, Attribute::Username(_)));
        if !has_integrity {
            let mut reply =
                StunMessage::with_transaction(Class::Error, msg.method(), msg.transaction_id());
            reply.push(Attribute::ErrorCode {
                code: 401,
                reason: "Unauthorized".to_owned(),
            });
            reply.push(Attribute::Realm(self.realm.clone()));
            reply.push(Attribute::Nonce(self.nonce.clone()));
            return Err(reply.to_bytes(None));
        }
        // Verify MESSAGE-INTEGRITY with the long-term key.
        if !msg.verify_integrity(&self.key()) {
            let mut reply =
                StunMessage::with_transaction(Class::Error, msg.method(), msg.transaction_id());
            reply.push(Attribute::ErrorCode {
                code: 401,
                reason: "Bad MESSAGE-INTEGRITY".to_owned(),
            });
            reply.push(Attribute::Realm(self.realm.clone()));
            reply.push(Attribute::Nonce(self.nonce.clone()));
            return Err(reply.to_bytes(None));
        }
        Ok(())
    }

    fn handle_allocate(&mut self, msg: &StunMessage) -> Vec<u8> {
        if let Err(err) = self.challenge_or_verify(msg) {
            return err;
        }
        self.saw_auth_allocate = true;
        let mut reply =
            StunMessage::with_transaction(Class::Success, Method::Allocate, msg.transaction_id());
        reply.push(Attribute::XorRelayedAddress(self.relay));
        reply.push(Attribute::Lifetime(600));
        reply.push(Attribute::Username(self.username.clone()));
        reply.push(Attribute::Realm(self.realm.clone()));
        reply.push(Attribute::Nonce(self.nonce.clone()));
        reply.to_bytes(Some(&self.key()))
    }

    fn handle_refresh(&mut self, msg: &StunMessage) -> Vec<u8> {
        if let Err(err) = self.challenge_or_verify(msg) {
            return err;
        }
        self.refreshes += 1;
        let lifetime = msg.lifetime().unwrap_or(600);
        let mut reply =
            StunMessage::with_transaction(Class::Success, Method::Refresh, msg.transaction_id());
        reply.push(Attribute::Lifetime(lifetime));
        reply.to_bytes(Some(&self.key()))
    }

    fn handle_create_permission(&mut self, msg: &StunMessage) -> Vec<u8> {
        if let Err(err) = self.challenge_or_verify(msg) {
            return err;
        }
        if let Some(peer) = msg.peer_address() {
            self.permissions.insert(peer);
        }
        let reply = StunMessage::with_transaction(
            Class::Success,
            Method::CreatePermission,
            msg.transaction_id(),
        );
        reply.to_bytes(Some(&self.key()))
    }

    fn handle_channel_bind(&mut self, msg: &StunMessage) -> Vec<u8> {
        if let Err(err) = self.challenge_or_verify(msg) {
            return err;
        }
        if let Some(peer) = msg.peer_address() {
            self.permissions.insert(peer);
        }
        let reply = StunMessage::with_transaction(
            Class::Success,
            Method::ChannelBind,
            msg.transaction_id(),
        );
        reply.to_bytes(Some(&self.key()))
    }

    /// Build a Data indication carrying `payload` from `peer` (server→client).
    #[must_use]
    pub fn make_data_indication(&self, peer: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let mut msg = StunMessage::indication(Method::Data);
        msg.push(Attribute::XorPeerAddress(peer));
        msg.push(Attribute::Data(payload.to_vec()));
        msg.to_bytes(None)
    }

    #[must_use]
    pub fn saw_authenticated_allocate(&self) -> bool {
        self.saw_auth_allocate
    }

    #[must_use]
    pub fn has_permission(&self, peer: &SocketAddr) -> bool {
        self.permissions.contains(peer)
    }

    #[must_use]
    pub fn refresh_count(&self) -> u32 {
        self.refreshes
    }
}
