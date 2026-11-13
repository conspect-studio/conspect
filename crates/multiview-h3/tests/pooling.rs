//! Pooling acceptance (design note section 5): page requests before,
//! during and after one WebTransport session on ONE QUIC connection; a
//! second CONNECT gets 429; admit rejections leave the slot free.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::{Buf, Bytes};
use common::*;
use multiview_h3::{AdmitToken, BoxFuture, Counters, H3Limits, WtHandler, WtSession};
use tokio::sync::mpsc;
use web_transport_trait::{RecvStream as _, SendStream as _, Session as _};

/// Echoes every bidi stream, opens one uni stream saying "hi", reports
/// the session and its end.
struct EchoHandler {
    reject_ticket: Option<&'static str>,
    sessions: mpsc::UnboundedSender<(String, Option<String>)>,
    ended: mpsc::UnboundedSender<String>,
    admitted: Mutex<Vec<String>>,
}

impl WtHandler for EchoHandler {
    fn admit(
        &self,
        req: &http::request::Parts,
        _peer: SocketAddr,
    ) -> Result<AdmitToken, http::StatusCode> {
        let query = req.uri.query().unwrap_or_default().to_owned();
        if self.reject_ticket.is_some_and(|t| query.contains(t)) {
            return Err(http::StatusCode::UNAUTHORIZED);
        }
        self.admitted.lock().unwrap().push(query.clone());
        Ok(Box::new(query))
    }

    fn accept(
        &self,
        session: WtSession,
        token: AdmitToken,
        req: http::request::Parts,
        _peer: SocketAddr,
    ) -> BoxFuture<'static, ()> {
        let sessions = self.sessions.clone();
        let ended = self.ended.clone();
        Box::pin(async move {
            let token = *token.downcast::<String>().unwrap();
            assert!(req.uri.path().starts_with("/moq"));
            let _ = sessions.send((token, session.protocol().map(str::to_owned)));
            let mut uni = session.open_uni().await.unwrap();
            uni.write_all(b"hi").await.unwrap();
            uni.finish().unwrap();
            let echo = {
                let session = session.clone();
                async move {
                    while let Ok((mut send, mut recv)) = session.accept_bi().await {
                        tokio::spawn(async move {
                            let data = recv.read_all().await.unwrap();
                            send.write_all(&data).await.unwrap();
                            send.finish().unwrap();
                        });
                    }
                }
            };
            let closed = session.closed();
            tokio::pin!(closed);
            tokio::select! {
                () = echo => {}
                err = &mut closed => {
                    let _ = ended.send(err.to_string());
                    return;
                }
            }
            let err = closed.await;
            let _ = ended.send(err.to_string());
        })
    }

    fn protocols(&self) -> &[&'static str] {
        &["moq-lite-03", "moqt-15"]
    }
}

struct Harness {
    server: Server,
    sessions: mpsc::UnboundedReceiver<(String, Option<String>)>,
    ended: mpsc::UnboundedReceiver<String>,
}

async fn harness(reject_ticket: Option<&'static str>) -> Harness {
    let (stx, srx) = mpsc::unbounded_channel();
    let (etx, erx) = mpsc::unbounded_channel();
    let handler = Arc::new(EchoHandler {
        reject_ticket,
        sessions: stx,
        ended: etx,
        admitted: Mutex::new(Vec::new()),
    });
    let server = start(TestSite::new(), Some(handler), H3Limits::default(), None).await;
    Harness {
        server,
        sessions: srx,
        ended: erx,
    }
}

async fn open_session(client: &mut Client, path: &str) -> (http::Response<()>, ClientStream) {
    let fut = async {
        let mut stream = client
            .send
            .send_request(connect_request(
                path,
                Some(r#""other", "moqt-15", "moq-lite-03""#),
            ))
            .await
            .unwrap();
        let resp = stream.recv_response().await.unwrap();
        (resp, stream)
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .expect("CONNECT timed out")
}

fn quarter(id: h3::quic::StreamId) -> u64 {
    id.into_inner()
}

async fn recv<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(TIMEOUT, rx.recv())
        .await
        .expect("timed out")
        .expect("channel closed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pooled_connection_serves_pages_around_one_session() {
    let mut h = harness(None).await;
    let mut client = h3_client(&h.server).await;

    // Before.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/before")).await;
    assert_eq!(resp.status(), 200);

    // The session.
    let (resp, mut connect) = open_session(&mut client, "/moq/x?ticket=t").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["sec-webtransport-http3-draft"], "draft02");
    assert_eq!(resp.headers()["wt-protocol"], "\"moqt-15\"");
    let (token, protocol) = recv(&mut h.sessions).await;
    assert_eq!(token, "ticket=t");
    assert_eq!(protocol.as_deref(), Some("moqt-15"));
    let session_id = quarter(connect.id());

    // During: a page on the same connection.
    let (resp, body) = fetch(&mut client.send, request(http::Method::GET, "/during")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body, b"hello /during");

    // Server-opened uni stream reaches the client.
    let (id, mut uni, prefix) = recv(&mut client.uni).await;
    assert_eq!(id, session_id);
    let mut got = prefix.map(|p| p.to_vec()).unwrap_or_default();
    got.extend(uni.read_to_end(1024).await.unwrap());
    assert_eq!(got, b"hi");

    // Client-opened bidi echo with the 0x41 header.
    let (mut send, mut recv_stream) = client.conn.open_bi().await.unwrap();
    let mut header = Vec::new();
    multiview_h3::varint::encode(0x41, &mut header);
    multiview_h3::varint::encode(session_id, &mut header);
    send.write_all(&header).await.unwrap();
    send.write_all(b"echo me").await.unwrap();
    send.finish().unwrap();
    let echoed = tokio::time::timeout(TIMEOUT, recv_stream.read_to_end(1024))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(echoed, b"echo me");

    // A second CONNECT on the same connection: 429, stream finished.
    let (resp, mut second) = open_session(&mut client, "/moq/x?ticket=u").await;
    assert_eq!(resp.status(), 429);
    let end = tokio::time::timeout(TIMEOUT, second.recv_data())
        .await
        .unwrap()
        .unwrap();
    assert!(end.is_none(), "the 429 stream was not finished");
    assert_eq!(Counters::get(&h.server.endpoint.counters().wt_429), 1);

    // The client closes the session with a CLOSE capsule.
    let capsule = multiview_h3::capsule::encode_close(7, "done");
    connect.send_data(Bytes::from(capsule)).await.unwrap();
    connect.finish().await.unwrap();
    let ended = recv(&mut h.ended).await;
    assert!(ended.contains("code 7"), "{ended}");
    // The server finishes its side of the CONNECT stream.
    let tail = tokio::time::timeout(TIMEOUT, async {
        while let Some(mut d) = connect.recv_data().await? {
            d.advance(d.remaining());
        }
        Ok::<_, h3::error::StreamError>(())
    })
    .await
    .unwrap();
    assert!(tail.is_ok());

    // After: pages still served on the same connection.
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
    // A later CONNECT on this connection: 429 (one session per connection).
    let (resp, _) = open_session(&mut client, "/moq/x?ticket=v").await;
    assert_eq!(resp.status(), 429);
    assert_eq!(Counters::get(&h.server.endpoint.counters().wt_sessions), 1);
    assert_eq!(
        Counters::get(&h.server.endpoint.counters().connections_accepted),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admit_rejection_keeps_the_slot_free() {
    let mut h = harness(Some("bad")).await;
    let mut client = h3_client(&h.server).await;
    let (resp, mut stream) = open_session(&mut client, "/moq/x?ticket=bad").await;
    assert_eq!(resp.status(), 401);
    let end = tokio::time::timeout(TIMEOUT, stream.recv_data())
        .await
        .unwrap()
        .unwrap();
    assert!(end.is_none());
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/page")).await;
    assert_eq!(resp.status(), 200);
    let (resp, _connect) = open_session(&mut client, "/moq/x?ticket=good").await;
    assert_eq!(resp.status(), 200);
    let (token, _) = recv(&mut h.sessions).await;
    assert_eq!(token, "ticket=good");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_outside_prefix_is_404_and_session_end_by_fin() {
    let mut h = harness(None).await;
    let mut client = h3_client(&h.server).await;
    let (resp, _) = open_session(&mut client, "/other").await;
    assert_eq!(resp.status(), 404);
    let (resp, _) = open_session(&mut client, "/moqx").await;
    assert_eq!(resp.status(), 404);
    // `/moq` exactly is under the prefix.
    let (resp, mut connect) = open_session(&mut client, "/moq").await;
    assert_eq!(resp.status(), 200);
    // FIN on the CONNECT stream ends the session.
    connect.finish().await.unwrap();
    let ended = recv(&mut h.ended).await;
    assert!(ended.contains("gone"), "{ended}");
    let (resp, _) = fetch(&mut client.send, request(http::Method::GET, "/after")).await;
    assert_eq!(resp.status(), 200);
}
