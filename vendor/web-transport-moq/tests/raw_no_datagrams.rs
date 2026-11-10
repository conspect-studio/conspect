//! A raw QUIC peer may not negotiate datagrams, which must not panic the other side.

use std::{net::Ipv4Addr, sync::Arc};

use anyhow::{Context as _, Result};
use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use web_transport_moq::{noq, Session};

/// With both crypto features enabled, as `--all-features` does, the crate cannot pick a
/// provider for us, so the test installs one.
fn install_crypto_provider() {
    #[cfg(all(feature = "aws-lc-rs", feature = "ring"))]
    {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// A client that disables datagrams reports a max datagram size of 0 to the server.
#[tokio::test]
async fn max_datagram_size_zero() -> Result<()> {
    install_crypto_provider();

    let CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KeyPair::serialize_der(
        &signing_key,
    )));

    let server_config = noq::ServerConfig::with_single_cert(vec![cert.der().clone()], key)?;
    let server = noq::Endpoint::server(server_config, (Ipv4Addr::LOCALHOST, 0).into())?;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone())?;
    let mut client_config = noq::ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = noq::TransportConfig::default();
    transport.datagram_receive_buffer_size(None);
    client_config.transport_config(Arc::new(transport));
    let client = noq::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into())?;

    let connecting = client.connect_with(client_config, server.local_addr()?, "localhost")?;
    let accept = async { anyhow::Ok(server.accept().await.context("no connection")?.await?) };
    let (_client_conn, server_conn) =
        tokio::try_join!(async { anyhow::Ok(connecting.await?) }, accept)?;

    assert_eq!(Session::raw(server_conn).max_datagram_size(), 0);

    Ok(())
}
