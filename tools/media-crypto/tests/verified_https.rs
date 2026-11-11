//! Actual loopback TLS, with synthetic credentials. No authority service calls.
use conspect_media_crypto::authority_http::{
    AuthorityHttpRequest, AuthorityHttpTransport, OrganisationCsk, VerifiedHttpsTransport,
};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::{pki_types::PrivatePkcs8KeyDer, ServerConfig, ServerConnection, StreamOwned};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
};

fn exchange(explicit_ca: bool, san: &str) -> bool {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![san.to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
        .unwrap();
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
            )
            .unwrap();
    let listener = TcpListener::bind("[::1]:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(Arc::new(config)).unwrap(), socket);
        // An untrusted certificate or wrong SAN must terminate before HTTP.
        let mut request = [0u8; 4096];
        if tls.read(&mut request).is_ok() {
            let _ = tls
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
    });
    let client = if explicit_ca {
        VerifiedHttpsTransport::from_trusted_ca_pem(ca.pem().as_bytes()).unwrap()
    } else {
        VerifiedHttpsTransport::new().unwrap()
    };
    let result = client.send(
        AuthorityHttpRequest {
            method: "GET",
            url: format!("https://[::1]:{port}/"),
            body: None,
            lease_headers: None,
        },
        &OrganisationCsk::new("synthetic-test-only".to_owned()).unwrap(),
    );
    server.join().unwrap();
    result.is_ok_and(|r| r.status == 200 && r.body == b"{}")
}

#[test]
fn explicit_ca_accepts_ip_san_over_real_tls() {
    assert!(exchange(true, "::1"));
}
#[test]
fn default_trust_rejects_untrusted_ca_over_real_tls() {
    assert!(!exchange(false, "::1"));
}
#[test]
fn explicit_ca_still_rejects_wrong_ip_san_over_real_tls() {
    assert!(!exchange(true, "wrong.invalid"));
}
#[test]
fn invalid_ca_pem_is_rejected() {
    assert!(VerifiedHttpsTransport::from_trusted_ca_pem(b"invalid").is_err());
}
