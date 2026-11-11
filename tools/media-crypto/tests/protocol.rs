use conspect_media_crypto::*;
use ed25519_dalek::SigningKey;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

type Counters = Rc<RefCell<BTreeMap<([u8; 16], u64, [u8; 16], u32), u64>>>;
struct Durable(Counters);
impl CounterReservation for Durable {
    fn reserve(
        &mut self,
        version: [u8; 16],
        epoch: u64,
        publisher: [u8; 16],
        generation: u32,
    ) -> Result<u64> {
        let mut state = self.0.borrow_mut();
        let counter = state
            .entry((version, epoch, publisher, generation))
            .or_insert(0);
        let reserved = *counter;
        *counter = counter.checked_add(1).ok_or(Error::Capacity)?;
        Ok(reserved)
    }
}
fn binding() -> Binding {
    Binding {
        booking: [1; 16],
        taker: [2; 16],
        feed: [9; 16],
        version: [3; 16],
        engine: [4; 16],
        publisher: [5; 16],
        generation: 7,
        publisher_generation: 11,
    }
}
fn object(track: &str, group: u64, object: u64, captured_ms: u64) -> ObjectId {
    ObjectId {
        track: track.into(),
        group,
        object,
        captured_ms,
    }
}
fn setup(counters: Counters) -> (Publisher, Receiver, SigningKey) {
    let b = binding();
    let mut p = Publisher::new(
        b.feed,
        b.version,
        b.publisher,
        b.publisher_generation,
        10_000,
        Box::new(Durable(counters)),
    )
    .unwrap();
    let signer = SigningKey::from_bytes(&[42; 32]);
    let grant = p.grant(&signer, b, true, 15_000, 10_000).unwrap();
    let receiver = Receiver::accept(&signer.verifying_key(), b, &grant, 10_000, 1_000, 20).unwrap();
    (p, receiver, signer)
}
#[test]
fn media_roundtrip_replay_misbinding_and_denial() {
    let (mut p, mut r, _) = setup(Default::default());
    let id = object("video", 2, 7, 10_000);
    let wire = p.encrypt(&id, 10_000, b"whole hang object HEVC").unwrap();
    assert_eq!(&wire[..4], b"MC02");
    assert_eq!(wire[60..64], 11u32.to_be_bytes());
    assert_eq!(wire[64..72], 0u64.to_be_bytes());
    assert_eq!(
        r.decrypt(&wire, &object("video", 2, 8, 10_000), 10_000),
        Err(Error::Denied)
    );
    assert_eq!(
        r.decrypt(&wire, &id, 10_000).unwrap(),
        b"whole hang object HEVC"
    );
    assert_eq!(r.decrypt(&wire, &id, 10_000), Err(Error::Replay));
    let audio = object("audio-en", 2, 1, 10_001);
    let a = p.encrypt(&audio, 10_001, b"whole hang object AAC").unwrap();
    assert_eq!(
        r.decrypt(&a, &audio, 10_001).unwrap(),
        b"whole hang object AAC"
    );
    let mut bad = a.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(r.decrypt(&bad, &audio, 10_001), Err(Error::Replay));
    r.deny();
    assert_eq!(r.decrypt(&a, &audio, 10_001), Err(Error::Denied));
}
#[test]
fn grant_identity_signature_epoch_and_cross_version_reject() {
    let (mut p, _, signer) = setup(Default::default());
    let body = p.grant(&signer, binding(), true, 15_000, 10_000).unwrap();
    assert_eq!(body.len(), 264);
    assert_eq!(&body[..4], b"MG02");
    for changed in [36, 52, 68, 84, 100, 116, 132, 140] {
        let mut wire = body.clone();
        wire[changed] ^= 1;
        assert!(
            Receiver::accept(&signer.verifying_key(), binding(), &wire, 10_000, 1_000, 0).is_err()
        );
    }
    let mut legacy = body.clone();
    legacy[..4].copy_from_slice(b"MG01");
    assert!(Receiver::accept(
        &signer.verifying_key(),
        binding(),
        &legacy,
        10_000,
        1_000,
        0
    )
    .is_err());
    let mut wrong = binding();
    wrong.engine = [7; 16];
    assert!(matches!(
        Receiver::accept(&signer.verifying_key(), wrong, &body, 10_000, 1_000, 0),
        Err(Error::Denied)
    ));
    let other = SigningKey::from_bytes(&[41; 32]);
    assert!(Receiver::accept(&other.verifying_key(), binding(), &body, 10_000, 1_000, 0).is_err());
    assert!(Receiver::accept(&signer.verifying_key(), binding(), &body, 15_000, 1_000, 0).is_err());
    assert_eq!(
        p.grant(&signer, binding(), false, 15_000, 10_000),
        Err(Error::Denied)
    );
}
#[test]
fn same_epoch_renewal_preserves_replay_and_denial_cannot_be_undone() {
    let (mut publisher, mut receiver, signer) = setup(Default::default());
    let id = object("video", 4, 1, 10_000);
    let media = publisher.encrypt(&id, 10_000, b"frame").unwrap();
    assert_eq!(receiver.decrypt(&media, &id, 10_000).unwrap(), b"frame");
    let renewed = publisher
        .grant(&signer, binding(), true, 15_000, 10_001)
        .unwrap();
    assert_eq!(
        receiver.renew(&signer.verifying_key(), &renewed, 10_001),
        Ok(())
    );
    assert_eq!(receiver.decrypt(&media, &id, 10_001), Err(Error::Replay));
    receiver.deny();
    assert_eq!(
        receiver.renew(&signer.verifying_key(), &renewed, 10_001),
        Err(Error::Denied)
    );
    assert_eq!(receiver.decrypt(&media, &id, 10_001), Err(Error::Denied));
}
#[test]
fn counter_reservation_is_shared_across_track_and_rejects_failed_store() {
    let counters: Counters = Default::default();
    let (mut p, _, _) = setup(counters.clone());
    let video = p
        .encrypt(&object("video", 1, 0, 10_000), 10_000, b"v")
        .unwrap();
    let audio = p
        .encrypt(&object("audio", 0, 0, 10_000), 10_000, b"a")
        .unwrap();
    assert_ne!(&video[64..72], &audio[64..72]);
    assert_eq!(*counters.borrow().values().next().unwrap(), 2);
    if let Some(value) = counters.borrow_mut().values_mut().next() {
        *value = u64::MAX;
    }
    assert_eq!(
        p.encrypt(&object("video", 1, 1, 10_001), 10_001, b"x"),
        Err(Error::Capacity)
    );
}
#[test]
fn authenticated_header_and_ciphertext_tamper_fail_closed() {
    let (mut p, mut r, _) = setup(Default::default());
    let id = object("video", 2, 7, 10_000);
    let wire = p.encrypt(&id, 10_000, b"whole hang object").unwrap();
    for index in [20, 36, 64, 72, 80, 88, 97, wire.len() - 1] {
        let mut altered = wire.clone();
        altered[index] ^= 1;
        assert!(r.decrypt(&altered, &id, 10_000).is_err());
    }
    assert_eq!(r.decrypt(&wire, &id, 10_000).unwrap(), b"whole hang object");
    let mut old = wire.clone();
    old[..4].copy_from_slice(b"MC01");
    assert_eq!(r.decrypt(&old, &id, 10_000), Err(Error::Invalid));
}
#[test]
fn nist_aes256_gcm_known_vector() {
    use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
    let c = Aes256Gcm::new_from_slice(&[0; 32]).unwrap();
    let actual = c
        .encrypt(Nonce::from_slice(&[0; 12]), &[0; 16][..])
        .unwrap();
    assert_eq!(
        hex::encode(actual),
        "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919"
    );
}
