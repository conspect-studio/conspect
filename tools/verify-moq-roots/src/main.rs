fn main() {
    let mut from_der = rustls::RootCertStore::empty();
    let (accepted, rejected) = from_der.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    assert_eq!(rejected, 0);
    let anchors = webpki_roots::TLS_SERVER_ROOTS;
    assert_eq!(accepted, anchors.len());
    let mut stricter = 0;
    for root in &from_der.roots {
        let matches: Vec<_> = anchors.iter().filter(|anchor| anchor.subject == root.subject && anchor.subject_public_key_info == root.subject_public_key_info).collect();
        assert_eq!(matches.len(), 1, "each DER identity must have exactly one replacement");
        let replacement = matches[0];
        if root.name_constraints != replacement.name_constraints {
            assert!(root.name_constraints.is_none(), "must never remove existing constraints");
            assert_eq!(replacement.name_constraints.as_ref().map(|v| v.as_ref()), Some(&[0xa0,0x07,0x30,0x05,0x82,0x03,0x2e,0x74,0x72][..]), "only expected Mozilla .tr tightening is permitted");
            stricter += 1;
        }
    }
    for anchor in anchors {
        assert!(from_der.roots.iter().any(|root| root.subject == anchor.subject && root.subject_public_key_info == anchor.subject_public_key_info), "no added root identities");
    }
    assert_eq!(stricter, 1);
    println!("Verified {accepted} identical root identities; exactly one stricter .tr name constraint, no removed constraints or new trust anchors");
}
