//! A real MoQ session over a `WtSession`: the server wraps it with
//! `moq_tokio::transport::Session::new` and hands it to
//! `moq_net::Server::accept_request`; the client is moq-tokio's own
//! WebTransport client (web-transport-moq over noq), which subscribes and
//! reads a frame.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::*;
use moq_tokio::moq_net;
use multiview_h3::{AdmitToken, BoxFuture, H3Limits, WtHandler, WtSession};

struct MoqHandler {
    origin: moq_net::origin::Producer,
}

impl WtHandler for MoqHandler {
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
        let origin = self.origin.clone();
        Box::pin(async move {
            let transport = moq_tokio::transport::Session::new(session);
            let request = moq_net::Server::new()
                .accept_request(tokio::time::Instant::now().into_std(), transport)
                .await
                .expect("MoQ SETUP");
            let (session, driver) = request.with_publisher(&origin).ok().await.expect("MoQ ok");
            tokio::spawn(moq_net::time::run(driver));
            let _ = session.closed().await;
        })
    }

    fn protocols(&self) -> &[&'static str] {
        moq_net::ALPNS
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moq_setup_and_subscribe_over_wt_session() {
    let origin = moq_tokio::origin::spawn();
    let broadcast = origin.create_broadcast("test").unwrap();
    broadcast.announce(Default::default()).unwrap();
    let track = broadcast.create_track("video", None).unwrap();
    let mut group = track.append_group().unwrap();
    group
        .write_frame(moq_net::Timestamp::ZERO, b"hello".as_ref())
        .unwrap();
    group.finish().unwrap();

    let handler = Arc::new(MoqHandler {
        origin: origin.clone(),
    });
    let server = start(TestSite::new(), Some(handler), H3Limits::default(), None).await;

    let sub_origin = moq_tokio::origin::spawn();
    let consumer = sub_origin.consume();
    let mut announcements = consumer.announced();
    let mut config = moq_tokio::connect::Config::default();
    config.tls.insecure = Some(true);
    config.bind = Some("[::1]:0".parse().unwrap());
    let client = config
        .init(Default::default())
        .unwrap()
        .with_subscriber(sub_origin);
    let url: url::Url = format!("https://[::1]:{}/moq/x?ticket=t", server.addr.port())
        .parse()
        .unwrap();
    let _connection = tokio::time::timeout(
        TIMEOUT,
        client.with_reconnect(false).connect(url).established(),
    )
    .await
    .expect("MoQ connect timed out")
    .expect("MoQ connect failed");
    tokio::time::timeout(TIMEOUT, announcements.next())
        .await
        .expect("announce timed out")
        .expect("origin closed");
    let remote = tokio::time::timeout(TIMEOUT, consumer.request_broadcast("test"))
        .await
        .expect("broadcast timed out")
        .expect("broadcast resolves");
    let mut sub = remote.track("video").unwrap().subscribe(None).await.unwrap();
    let mut got = tokio::time::timeout(TIMEOUT, sub.recv_group())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = tokio::time::timeout(TIMEOUT, got.read_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload.to_vec(), b"hello");
    let _keep = (broadcast, track);
}
