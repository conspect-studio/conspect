//! A raw QUIC session carries the application close code directly, with no HTTP/3
//! mapping, so the peer must decode it the same way and keep the first close.

use std::{net::Ipv6Addr, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::time::timeout;
use web_transport_moq::{noq, Session};

/// With both crypto features enabled, as `--all-features` does, the crate cannot pick a
/// provider for us, so the test installs one.
fn install_crypto_provider() {
    #[cfg(all(feature = "aws-lc-rs", feature = "ring"))]
    {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// A connected raw QUIC client and server session, plus the endpoints that drive them.
async fn raw_pair() -> Result<(Session, Session, [noq::Endpoint; 2])> {
    install_crypto_provider();

    let CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KeyPair::serialize_der(
        &signing_key,
    )));

    let server_config = noq::ServerConfig::with_single_cert(vec![cert.der().clone()], key)?;
    let server = noq::Endpoint::server(server_config, (Ipv6Addr::LOCALHOST, 0).into())?;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone())?;
    let client_config = noq::ClientConfig::with_root_certificates(Arc::new(roots))?;
    let client = noq::Endpoint::client((Ipv6Addr::LOCALHOST, 0).into())?;

    let connecting = client.connect_with(client_config, server.local_addr()?, "localhost")?;
    let accept = async { anyhow::Ok(server.accept().await.context("no connection")?.await?) };
    let (client_conn, server_conn) =
        tokio::try_join!(async { anyhow::Ok(connecting.await?) }, accept)?;

    Ok((
        Session::raw(client_conn),
        Session::raw(server_conn),
        [client, server],
    ))
}

fn assert_code(err: &impl web_transport_trait::Error, code: u32) {
    assert_eq!(
        err.session_error(),
        Some((code, "kicked".to_string())),
        "{err}"
    );
}

/// The peer's raw close code reaches `closed`, the accept and stream paths, and
/// survives a later local `close`.
#[tokio::test]
async fn peer_close_code() -> Result<()> {
    let (client, server, _endpoints) = raw_pair().await?;

    let mut send = server.open_uni().await?;
    send.write_all(b"x").await?;
    let mut recv = timeout(Duration::from_secs(5), client.accept_uni()).await??;
    let mut buf = [0u8; 1];
    recv.read_exact(&mut buf).await?;

    server.close(4075, b"kicked");

    let err = timeout(Duration::from_secs(5), client.closed()).await?;
    assert_code(&err, 4075);
    assert_code(&client.accept_uni().await.err().unwrap(), 4075);
    assert_code(&client.open_uni().await.err().unwrap(), 4075);
    assert_code(&recv.read(&mut buf).await.err().unwrap(), 4075);

    // Closing an already closed session changes nothing.
    client.close(1, b"");
    assert_code(&client.closed().await, 4075);
    assert_code(&client.close_reason().unwrap(), 4075);

    Ok(())
}

/// An unknown HTTP/3 STOP must never look like FIN acknowledgement.
#[tokio::test]
async fn unknown_stop_is_not_acknowledged() -> Result<()> {
    use web_transport_trait::SendStream as _;
    let (client, server, _endpoints) = raw_pair().await?;
    let mut send = client.open_uni().await?;
    send.write_all(b"payload").await?;
    let mut recv = timeout(Duration::from_secs(5), noq::Connection::accept_uni(&server)).await??;
    recv.stop(noq::VarInt::from_u32(0))?; // outside WebTransport error mapping
    assert!(timeout(Duration::from_secs(5), send.closed()).await?.is_err());
    Ok(())
}
#[tokio::test]
async fn finished_payload_is_acknowledged() -> Result<()> {
    use web_transport_trait::SendStream as _;
    let (client, server, _endpoints) = raw_pair().await?;
    let mut send = client.open_uni().await?;
    send.write_all(b"payload").await?;
    send.finish()?;
    let mut recv = timeout(Duration::from_secs(5), server.accept_uni()).await??;
    assert_eq!(recv.read_to_end(100).await?, b"payload");
    timeout(Duration::from_secs(5), send.closed()).await??;
    Ok(())
}
