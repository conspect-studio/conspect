use conspect_media_crypto::{authority_http::*, receiver_http::*, *};
use ed25519_dalek::{Signer, SigningKey};
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

fn binding() -> Binding {
    Binding {
        booking: [1; 16],
        taker: [2; 16],
        feed: [3; 16],
        version: [4; 16],
        engine: [5; 16],
        publisher: [6; 16],
        generation: 0,
        publisher_generation: 9,
    }
}
fn signed(b: Binding, expiry: u64) -> Vec<u8> {
    let mut w = b"MG02conspect-exch-v2conspect-engine2".to_vec();
    for id in [b.booking, b.taker, b.feed, b.version, b.engine, b.publisher] {
        w.extend(id);
    }
    w.extend(b.generation.to_be_bytes());
    w.extend(b.publisher_generation.to_be_bytes());
    w.extend(2u64.to_be_bytes());
    w.extend(10_000u64.to_be_bytes());
    w.extend(expiry.to_be_bytes());
    w.extend([42; 32]);
    let sig = SigningKey::from_bytes(&[7; 32]).sign(&w);
    w.extend(sig.to_bytes());
    w
}
fn response(wire: Vec<u8>) -> GrantHttpResponse {
    GrantHttpResponse {
        status: 200,
        body: Zeroizing::new(wire),
        content_type: "application/octet-stream".into(),
        cache_control: "private, no-store".into(),
        content_length: Some(264),
    }
}
struct Mock(Mutex<Option<GrantHttpResponse>>);
impl AuthorityHttpTransport for Mock {
    fn send(&self, _: AuthorityHttpRequest, _: &OrganisationCsk) -> Result<AuthorityHttpResponse> {
        panic!("JSON fallback forbidden")
    }
    fn send_grant(
        &self,
        r: AuthorityHttpRequest,
        c: &OrganisationCsk,
    ) -> Result<GrantHttpResponse> {
        assert_eq!(r.method, "POST");
        assert_eq!(r.url,"https://authority.invalid/v0/organisations/org_A%2FB/exchange/bookings/book%3F%23%25/grants");
        assert!(r.lease_headers.is_none());
        let body: serde_json::Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"machineId":"machine/ raw", "requestedExpiryMs":14000})
        );
        assert!(!format!("{c:?}").contains("secret"));
        Ok(self.0.lock().unwrap().take().expect("exactly one call"))
    }
}
fn fetch(r: GrantHttpResponse) -> Result<Receiver> {
    let client = HttpReceiverGrantClient::new(
        "https://authority.invalid",
        ReceiverHttpIds::new("org_A/B".into(), "book?#%".into(), "machine/ raw".into())?,
        binding(),
        SigningKey::from_bytes(&[7; 32]).verifying_key(),
        Arc::new(OrganisationCsk::new("secret".into())?),
        Arc::new(Mock(Mutex::new(Some(r)))),
    )?;
    client.fetch(14000, 1000, || 10000)
}
#[test]
fn immutable_zero_authorization_generation_is_valid_and_one_is_mismatch() {
    assert!(fetch(response(signed(binding(), 14000))).is_ok());
    let mut b = binding();
    b.generation = 1;
    assert!(matches!(
        fetch(response(signed(b, 14000))),
        Err(Error::Denied)
    ));
    // Prove the generation-one denial is binding, not a bad signature.
    assert!(Receiver::accept(
        &SigningKey::from_bytes(&[7; 32]).verifying_key(),
        b,
        &signed(b, 14000),
        10000,
        1000,
        1000
    )
    .is_ok());
}
#[test]
fn every_raw_binding_field_and_pinned_key_is_checked() {
    let mut cases = Vec::new();
    for field in 0..7 {
        let mut b = binding();
        match field {
            0 => b.booking[0] ^= 1,
            1 => b.taker[0] ^= 1,
            2 => b.feed[0] ^= 1,
            3 => b.version[0] ^= 1,
            4 => b.engine[0] ^= 1,
            5 => b.publisher[0] ^= 1,
            _ => b.publisher_generation += 1,
        };
        cases.push(signed(b, 14000));
    }
    let mut wrong_key = signed(binding(), 14000);
    let sig = SigningKey::from_bytes(&[8; 32]).sign(&wrong_key[..200]);
    wrong_key[200..].copy_from_slice(&sig.to_bytes());
    cases.push(wrong_key);
    for w in cases {
        assert!(matches!(fetch(response(w)), Err(Error::Denied)));
    }
}
#[test]
fn metadata_and_excess_body_fail_closed_without_fallback() {
    for case in 0..9 {
        let mut r = response(signed(binding(), 14000));
        match case {
            0 => r.content_type = "application/json".into(),
            1 => r.cache_control = "private".into(),
            2 => r.cache_control = "no-store".into(),
            3 => r.content_length = Some(265),
            4 => r.body.push(0),
            5 => {
                r.body.pop();
            }
            6 => r.cache_control = "public, private, no-store".into(),
            7 => r.content_length = None,
            _ => r.status = 503,
        };
        assert!(fetch(r).is_err());
    }
    for status in [401, 404, 503] {
        let mut r = response(Vec::new());
        r.status = status;
        assert!(matches!(fetch(r), Err(Error::Denied)));
    }
}
#[test]
fn expiry_is_not_expanded_and_uncertainty_is_strict() {
    assert!(matches!(
        fetch(response(signed(binding(), 14500))),
        Err(Error::Denied)
    ));
    assert!(matches!(
        fetch(response(signed(binding(), 11000))),
        Err(Error::Expired)
    ));
    assert!(fetch(response(signed(binding(), 11001))).is_ok());
    assert!(matches!(
        fetch(response(signed(binding(), 15001))),
        Err(Error::Expired)
    ));
    assert_eq!(next_epoch_ms(14999), Ok(15000));
    assert_eq!(next_epoch_ms(15000), Ok(20000));
    assert!(next_epoch_ms(u64::MAX).is_err());
    assert!(ReceiverHttpIds::new("..".into(), "b".into(), "m".into()).is_err());
}
