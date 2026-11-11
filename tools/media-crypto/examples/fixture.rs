//! Offline fixture only. This is not native decode or paid-enforcement evidence.
use conspect_media_crypto::{
    Binding, CounterReservation, Error, ObjectId, Publisher, Receiver, Result,
};
use ed25519_dalek::SigningKey;
struct FixtureCounter(u64);
impl CounterReservation for FixtureCounter {
    fn reserve(&mut self, _: [u8; 16], _: u64, _: [u8; 16], _: u32) -> Result<u64> {
        let value = self.0;
        self.0 = self.0.checked_add(1).ok_or(Error::Capacity)?;
        Ok(value)
    }
}
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("expected INPUT OUTPUT".into());
    }
    if std::path::Path::new(&args[2]).exists() {
        return Err("output exists".into());
    }
    let input = std::fs::read(&args[1])?;
    let signer = SigningKey::from_bytes(&[42; 32]); // fixture only
    let binding = Binding {
        booking: [1; 16],
        taker: [2; 16],
        feed: [9; 16],
        version: [3; 16],
        engine: [4; 16],
        publisher: [5; 16],
        generation: 1,
        publisher_generation: 1,
    };
    let mut publisher = Publisher::new(
        binding.feed,
        binding.version,
        binding.publisher,
        binding.publisher_generation,
        10_000,
        Box::new(FixtureCounter(0)),
    )
    .map_err(|e| format!("{e:?}"))?;
    let grant = publisher
        .grant(&signer, binding, true, 15_000, 10_000)
        .map_err(|e| format!("{e:?}"))?;
    let mut receiver = Receiver::accept(&signer.verifying_key(), binding, &grant, 10_000, 1_000, 0)
        .map_err(|e| format!("{e:?}"))?;
    let id = ObjectId {
        track: "video".into(),
        group: 0,
        object: 0,
        captured_ms: 10_000,
    };
    let wire = publisher
        .encrypt(&id, 10_000, &input)
        .map_err(|e| format!("{e:?}"))?;
    let plain = receiver
        .decrypt(&wire, &id, 10_000)
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(plain, input);
    assert_eq!(
        publisher.grant(&signer, binding, false, 15_000, 10_001),
        Err(Error::Denied)
    );
    receiver.deny();
    assert_eq!(receiver.decrypt(&wire, &id, 10_001), Err(Error::Denied));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[2])?
        .write_all(&plain)?;
    println!(
        "fixture roundtrip {} bytes; NOT native decode proof",
        input.len()
    );
    Ok(())
}
