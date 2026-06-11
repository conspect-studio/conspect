//! Live router transport bindings (off-by-default `router` feature).
//!
//! The SW-P-08 and Ember+ **codecs** ([`super::swp08`] / [`super::ember`]) are
//! pure and always compiled; this module is the thin, feature-gated seam that
//! carries a framed message over a real byte stream (TCP to a router, or a
//! serial line). It is **compile-only** in this environment — there is no router
//! to connect to — so it deliberately holds no live client, only the typed
//! transport contract and the framing direction the codecs plug into.
//!
//! Keeping the socket here (and out of the default build) preserves the pure,
//! CI-green baseline: nothing in this module is reachable unless a deployment
//! opts into `router`.
use super::ember::{EmberError, GlowNode};
use super::swp08::{SwP08Error, SwP08Message};

/// The transport a router driver speaks over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouterLink {
    /// A TCP connection to the router's control port.
    Tcp,
    /// An RS-422/485 serial line.
    Serial,
}

/// A framed message ready to write to the wire, paired with the link it targets.
///
/// This is the unit a live driver would `write_all` once a socket exists; here
/// it exists only to type the boundary so the gated path compiles and the codec
/// seam is exercised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundFrame {
    /// The link this frame is destined for.
    pub link: RouterLink,
    /// The framed bytes (already DLE/STX- or S101-encoded by the codec).
    pub bytes: Vec<u8>,
}

/// Encode a SW-P-08 message into an [`OutboundFrame`] for a link.
///
/// # Errors
///
/// [`SwP08Error`] if the message cannot be encoded (e.g. an out-of-range
/// address).
pub fn frame_swp08(link: RouterLink, message: &SwP08Message) -> Result<OutboundFrame, SwP08Error> {
    Ok(OutboundFrame {
        link,
        bytes: super::swp08::encode_message(message)?,
    })
}

/// Encode an Ember+ Glow node into an [`OutboundFrame`] for a link.
#[must_use]
pub fn frame_ember(link: RouterLink, node: &GlowNode) -> OutboundFrame {
    OutboundFrame {
        link,
        bytes: node.encode_message(),
    }
}

/// Decode an inbound SW-P-08 frame read from the wire.
///
/// # Errors
///
/// [`SwP08Error`] on any frame/body decoding failure.
pub fn read_swp08(frame: &[u8]) -> Result<SwP08Message, SwP08Error> {
    super::swp08::decode_message(frame)
}

/// Decode an inbound Ember+ frame read from the wire.
///
/// # Errors
///
/// [`EmberError`] on any S101/BER decoding failure.
pub fn read_ember(frame: &[u8]) -> Result<GlowNode, EmberError> {
    GlowNode::decode_message(frame)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::super::ember::{GlowNode, GlowValue};
    use super::super::swp08::SwP08Message;
    use super::{frame_ember, frame_swp08, read_ember, read_swp08, RouterLink};

    #[test]
    fn swp08_frame_round_trips_through_the_transport_seam() {
        let msg = SwP08Message::Connect {
            matrix: 0,
            level: 1,
            destination: 5,
            source: 10,
        };
        let framed = frame_swp08(RouterLink::Tcp, &msg).unwrap();
        assert_eq!(framed.link, RouterLink::Tcp);
        assert_eq!(read_swp08(&framed.bytes).unwrap(), msg);
    }

    #[test]
    fn ember_frame_round_trips_through_the_transport_seam() {
        let node = GlowNode {
            number: 3,
            identifier: "dest".to_owned(),
            value: Some(GlowValue::string("CAM 3")),
        };
        let framed = frame_ember(RouterLink::Serial, &node);
        assert_eq!(framed.link, RouterLink::Serial);
        assert_eq!(read_ember(&framed.bytes).unwrap(), node);
    }
}
