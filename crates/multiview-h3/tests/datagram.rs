//! WebTransport datagram framing (#358 review m2): the context prefix is the
//! QUARTER stream id of the CONNECT stream (RFC 9297 section 2.1,
//! draft-ietf-webtrans-http3), as browsers send it, and
//! `max_datagram_size` leaves room for it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use common::*;
use multiview_h3::{varint, AdmitToken, BoxFuture, H3Limits, WtError, WtHandler, WtSession};
use tokio::sync::mpsc;
use web_transport_trait::Session as _;

struct Hand(mpsc::UnboundedSender<WtSession>);

impl WtHandler for Hand {
    fn admit(
        &self,
        _req: &http::request::Parts,
        _peer: SocketAddr,
    ) -> Result<AdmitToken, http::StatusCode> {
        Ok(Box::new(()))
    }

    fn accept(
        &self,
        session: WtSession,
        _token: AdmitToken,
        _req: http::request::Parts,
        _peer: SocketAddr,
    ) -> BoxFuture<'static, ()> {
        let _ = self.0.send(session.clone());
        Box::pin(async move {
            let _ = session.closed().await;
        })
    }

    fn protocols(&self) -> &[&'static str] {
        &[]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datagrams_use_the_quarter_stream_id_both_ways() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let server = start(
        TestSite::new(),
        Some(Arc::new(Hand(tx))),
        H3Limits::default(),
        None,
    )
    .await;
    let mut client = h3_client(&server).await;
    // Two requests first, so the CONNECT is stream 8: quarter id 2, and a
    // full-id prefix (8) would be distinguishable.
    for _ in 0..2 {
        let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/")).await;
        assert_eq!(resp.status(), 200);
    }
    let mut connect = client
        .send
        .send_request(connect_request("/moq/x", None))
        .await
        .unwrap();
    let resp = tokio::time::timeout(TIMEOUT, connect.recv_response())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let connect_id = connect.id().into_inner();
    assert_eq!(connect_id, 8);
    let session = tokio::time::timeout(TIMEOUT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.session_id(), connect_id);

    // Server -> client: the wire prefix is session_id / 4.
    session.send_datagram(Bytes::from_static(b"srv")).unwrap();
    let wire = tokio::time::timeout(TIMEOUT, client.conn.read_datagram())
        .await
        .unwrap()
        .unwrap();
    let (prefix, len) = varint::decode(&wire).unwrap();
    assert_eq!(prefix, connect_id / 4);
    assert_eq!(&wire[len..], b"srv");

    // Client -> server: a quarter-id datagram is delivered; a full-id one
    // (what web-transport-moq 1.3.5 sends) is not for this session.
    let mut full = Vec::new();
    varint::encode(connect_id, &mut full);
    full.extend_from_slice(b"full-id");
    client.conn.send_datagram(Bytes::from(full)).unwrap();
    let mut quarter = Vec::new();
    varint::encode(connect_id / 4, &mut quarter);
    quarter.extend_from_slice(b"cli");
    client.conn.send_datagram(Bytes::from(quarter)).unwrap();
    let got = tokio::time::timeout(TIMEOUT, session.recv_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&got[..], b"cli");

    // max_datagram_size subtracts the prefix: exactly that payload fits,
    // one more byte does not.
    let max = session.max_datagram_size();
    assert!(max > 0);
    let prefix_len = varint::size(connect_id / 4).unwrap();
    assert_eq!(prefix_len, 1);
    session.send_datagram(Bytes::from(vec![0u8; max])).unwrap();
    assert_eq!(
        session.send_datagram(Bytes::from(vec![0u8; max + 1])),
        Err(WtError::Datagram("datagram too large"))
    );
}
