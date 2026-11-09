//! Closing a session and dropping it right away must still deliver the
//! CloseWebTransportSession capsule without ending the HTTP/3 control stream first.
//! Browsers treat a closed control stream as a connection error (RFC 9114 6.2.1)
//! and discard the capsule's code and reason.

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::time::timeout;
use web_transport_moq::{ALPN, Request, noq, proto};

/// With both crypto features enabled, as `--all-features` does, the crate cannot pick a
/// provider for us, so the test installs one.
fn install_crypto_provider() {
    #[cfg(all(feature = "aws-lc-rs", feature = "ring"))]
    {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// The server closes and drops its session; a hand-rolled HTTP/3 client, standing in
/// for a browser, must read the capsule while the server's control stream is still open.
#[tokio::test]
async fn close_then_drop_keeps_control_stream() -> Result<()> {
    install_crypto_provider();

    let CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KeyPair::serialize_der(
        &signing_key,
    )));

    // The workspace enables both rustls providers, so pick one explicitly.
    let provider = web_transport_moq::crypto::default_provider();

    let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)?;
    server_crypto.alpn_protocols = vec![ALPN.as_bytes().to_vec()];
    let server_config = noq::ServerConfig::with_crypto(Arc::new(
        noq::crypto::rustls::QuicServerConfig::try_from(server_crypto)?,
    ));
    let server = noq::Endpoint::server(server_config, (Ipv4Addr::LOCALHOST, 0).into())?;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone())?;
    let mut client_crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![ALPN.as_bytes().to_vec()];
    let client_config = noq::ClientConfig::new(Arc::new(
        noq::crypto::rustls::QuicClientConfig::try_from(client_crypto)?,
    ));
    let client = noq::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into())?;

    let url: url::Url = format!("https://localhost:{}/", server.local_addr()?.port()).parse()?;

    let serve = async {
        let conn = server.accept().await.context("no connection")?.await?;
        let session = Request::accept(conn).await?.ok().await?;
        session.close(4075, b"kicked");
        drop(session);
        anyhow::Ok(())
    };

    let peer = async {
        let conn = client
            .connect_with(client_config, server.local_addr()?, "localhost")?
            .await?;

        let mut settings = proto::Settings::default();
        settings.enable_webtransport(1);
        let mut control_send = conn.open_uni().await?;
        settings.write(&mut control_send).await?;

        let mut control_recv = conn.accept_uni().await?;
        proto::Settings::read(&mut control_recv).await?;

        let (mut connect_send, mut connect_recv) = conn.open_bi().await?;
        proto::ConnectRequest::new(url)
            .write(&mut connect_send)
            .await?;
        let response = proto::ConnectResponse::read(&mut connect_recv).await?;
        anyhow::ensure!(response.status == http::StatusCode::OK, "{response:?}");

        let capsule = proto::Http3CapsuleReader::new(connect_recv).read().await?;
        anyhow::ensure!(
            matches!(
                capsule,
                Some(proto::Capsule::CloseWebTransportSession { code: 4075, ref reason })
                    if reason == "kicked"
            ),
            "{capsule:?}"
        );

        // The server's control stream must stay open until the connection goes away,
        // and it must not ask us to stop ours.
        let control = control_recv.read_chunk(usize::MAX).await;
        anyhow::ensure!(
            matches!(control, Err(noq::ReadError::ConnectionLost(_))),
            "server control stream ended before the connection: {control:?}"
        );
        let stopped = control_send.stopped().await;
        anyhow::ensure!(
            matches!(stopped, Err(noq::StoppedError::ConnectionLost(_))),
            "server stopped our control stream before the connection closed: {stopped:?}"
        );

        anyhow::Ok(())
    };

    timeout(Duration::from_secs(5), async {
        tokio::try_join!(serve, peer)
    })
    .await??;

    Ok(())
}
