//! Experimental exchange media cryptography. Paid delivery remains disabled.
#![forbid(unsafe_code)]
pub mod authority;
pub mod authority_http;
pub mod bootstrap;
pub mod journal;
pub mod private_state;
pub mod receipt;
pub mod receiver_http;
pub mod spool;
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
pub use authority::{
    AuthorityEpochRoot, AuthorityRootGuard, AuthorityRootProvider, CounterScope, EpochRootRequest,
    KeySource, LeaseFence, LeasePermit,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const EPOCH_MS: u64 = 5_000;
pub const PAID_DELIVERY_ENABLED: bool = false;
const HEADER_FIXED: usize = 97;
const GRANT_BODY: usize = 200;
const MAX_OBJECT: usize = 16 * 1024 * 1024;
const MAX_TRACKS: usize = 32;
const MAX_TRACK_LEN: usize = 64;
const ISSUER: &[u8; 16] = b"conspect-exch-v2";
const AUDIENCE: &[u8; 16] = b"conspect-engine2";

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Denied,
    Expired,
    Replay,
    Crypto,
    Capacity,
    /// The authority's `u64` server generation cannot be represented in the
    /// MC02/MG02 `publisher_generation` field, which is `u32`. Refused rather
    /// than truncated: a truncated generation would silently collide with
    /// another scope and could reuse a nonce.
    GenerationOverflow,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub booking: [u8; 16],
    pub taker: [u8; 16],
    pub feed: [u8; 16],
    pub version: [u8; 16],
    pub engine: [u8; 16],
    pub publisher: [u8; 16],
    pub generation: u64,
    pub publisher_generation: u32,
}
#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct Secret(pub(crate) [u8; 32]);
/// Persistently reserves one nonce counter before encryption. The store key must
/// include version, epoch, publisher and generation; a restart never resets it.
/// Reject a generation rollback and duplicate reservation transactionally.
pub trait CounterReservation {
    fn reserve(
        &mut self,
        version: [u8; 16],
        epoch: u64,
        publisher: [u8; 16],
        generation: u32,
    ) -> Result<u64>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectId {
    pub track: String,
    pub group: u64,
    pub object: u64,
    pub captured_ms: u64,
}
fn track_valid(track: &str) -> bool {
    !track.is_empty()
        && track.len() <= MAX_TRACK_LEN
        && !track.bytes().any(|b| b.is_ascii_control())
}
fn read_u64(b: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(b[offset..offset + 8].try_into().expect("length checked"))
}
fn nonce(generation: u32, counter: u64) -> [u8; 12] {
    let mut out = [0; 12];
    out[..4].copy_from_slice(&generation.to_be_bytes());
    out[4..].copy_from_slice(&counter.to_be_bytes());
    out
}
fn subkey(
    key: &[u8; 32],
    version: [u8; 16],
    publisher: [u8; 16],
    epoch: u64,
    track: &str,
) -> Result<Secret> {
    let mut info = Vec::from(*b"exchange-MC02-aead-key\0");
    info.extend(version);
    info.extend(publisher);
    info.extend(epoch.to_be_bytes());
    info.push(u8::try_from(track.len()).map_err(|_| Error::Capacity)?);
    info.extend(track.as_bytes());
    let mut derived = Secret([0; 32]);
    Hkdf::<Sha256>::new(None, key)
        .expand(&info, &mut derived.0)
        .map_err(|_| Error::Crypto)?;
    Ok(derived)
}
/// Single-owner publisher of one (version, publisher, epoch) root key.
/// Never reconstruct with the same root without the same durable reservation store.
pub struct Publisher {
    version: [u8; 16],
    feed: [u8; 16],
    publisher: [u8; 16],
    publisher_generation: u32,
    epoch: u64,
    key: Secret,
    reservation: Box<dyn CounterReservation>,
    tracks: BTreeSet<String>,
    last_now: u64,
    source: KeySource,
    /// The exclusive-lease fence consulted before every seal and grant. `None`
    /// only for the local/test-only path; it is set from the guard passed to
    /// [`Publisher::from_authority`].
    lease_fence: Option<Arc<dyn LeaseFence>>,
}
impl Publisher {
    /// LOCAL, TEST-ONLY key path: the epoch root is drawn here from the OS
    /// CSPRNG. A restart therefore produces a DIFFERENT root for the same epoch,
    /// which is exactly what protected delivery must not do. Protected
    /// deployments must use
    /// [`from_authority`](Self::from_authority); this constructor exists for the
    /// offline fixture and for tests, and reports
    /// [`KeySource::LocalTestOnly`].
    pub fn new(
        feed: [u8; 16],
        version: [u8; 16],
        publisher: [u8; 16],
        publisher_generation: u32,
        now: u64,
        reservation: Box<dyn CounterReservation>,
    ) -> Result<Self> {
        let mut key = Secret([0; 32]);
        getrandom::getrandom(&mut key.0).map_err(|_| Error::Crypto)?;
        Ok(Self {
            feed,
            version,
            publisher,
            publisher_generation,
            epoch: now / EPOCH_MS,
            key,
            reservation,
            tracks: BTreeSet::new(),
            last_now: now,
            source: KeySource::LocalTestOnly,
            lease_fence: None,
        })
    }

    /// PROTECTED key path: take the current epoch root from an authenticated
    /// [`AuthorityRootProvider`] and bind the durable reservation scope before
    /// anything can be sealed.
    ///
    /// Every identity in the returned context must be the one asked for, the
    /// epoch must be the current one, the authority's generation must be
    /// monotonic for this identity, the root must not contradict a root already
    /// seen for the same scope, and the `u64` server generation must fit the
    /// `u32` wire field. Nothing is sealed before all of that holds.
    ///
    /// The scope actually used for each durable counter reservation is
    /// `(version, publisher, epoch, generation)`, so a retry at the same
    /// generation and epoch continues the counter instead of restarting it.
    ///
    /// The lease is taken from `guard`: when the guard carries a
    /// [`LeaseFence`](crate::LeaseFence) the publisher re-checks it before every
    /// seal and every grant, so a revoked, expired or unrenewed lease stops
    /// sealing even though the root is still in memory. A guard without a fence
    /// is refused at seal time on this path: the provider alone cannot exclude a
    /// second host, and two hosts sharing a root and counter is AES-GCM nonce
    /// reuse.
    ///
    /// # Errors
    /// [`Error::Denied`] for an identity mismatch or root equivocation,
    /// [`Error::Expired`] for a stale, future or late context,
    /// [`Error::Replay`] for a generation rollback,
    /// [`Error::GenerationOverflow`] when the generation cannot fit `u32`, and
    /// whatever the provider itself returns.
    // The argument list mirrors the identity the authority root is bound to; a
    // parameter object here would hide which identities are actually checked.
    #[allow(clippy::too_many_arguments)]
    pub fn from_authority(
        provider: &dyn AuthorityRootProvider,
        guard: &mut AuthorityRootGuard,
        org: [u8; 16],
        version: [u8; 16],
        feed: [u8; 16],
        publisher_machine: [u8; 16],
        publisher: [u8; 16],
        now: u64,
        reservation: Box<dyn CounterReservation>,
    ) -> Result<Self> {
        let epoch = now / EPOCH_MS;
        let request = EpochRootRequest {
            org,
            version,
            feed,
            publisher_machine,
            publisher,
            epoch,
            now_ms: now,
        };
        let context = provider.current_epoch_root(&request)?;
        // 1. The provider must have answered for exactly the identity asked.
        if context.org != org
            || context.version != version
            || context.feed != feed
            || context.publisher_machine != publisher_machine
            || context.publisher != publisher
        {
            return Err(Error::Denied);
        }
        // 2. Current epoch only, no future prefetch, no late context: the same
        //    window the receiver enforces on the grant, so the root and the
        //    grant can never disagree about the epoch.
        let end = context
            .epoch
            .checked_add(1)
            .and_then(|v| v.checked_mul(EPOCH_MS))
            .ok_or(Error::Invalid)?;
        if context.epoch != epoch
            || context.issued_ms > now
            || context.issued_ms / EPOCH_MS != context.epoch
            || context.expiry_ms <= now
            || context.expiry_ms > end
        {
            return Err(Error::Expired);
        }
        // 3. The wire carries a u32 generation. Refuse an overflow instead of
        //    truncating it into another scope's nonce space.
        let publisher_generation =
            u32::try_from(context.server_generation).map_err(|_| Error::GenerationOverflow)?;
        // 4. Monotonic generation and no equivocation for a scope already seen.
        guard.accept(&context)?;
        Ok(Self {
            version,
            feed,
            publisher,
            publisher_generation,
            epoch,
            // Moved, not copied: the context keeps sole ownership of the root
            // until here, so no second plaintext copy of it exists.
            key: context.root,
            reservation,
            tracks: BTreeSet::new(),
            last_now: now,
            source: KeySource::Authority,
            // Taken from the guard so the fence is shared with the provider that
            // issued this root: one process, one lease observation.
            lease_fence: guard.lease_fence().cloned(),
        })
    }

    /// Which key path this publisher took.
    #[must_use]
    pub fn key_source(&self) -> KeySource {
        self.source
    }

    /// The durable reservation scope bound at construction. Diagnostics and
    /// tests only; it is not wire data.
    #[must_use]
    pub fn bound_scope(&self) -> CounterScope {
        CounterScope {
            version: self.version,
            publisher: self.publisher,
            epoch: self.epoch,
            generation: self.publisher_generation,
        }
    }
    /// Encrypt the entire serialized MoQ object, including its hang timestamp.
    /// Reservation is durable before any ciphertext is returned; a failed seal consumes it.
    pub fn encrypt(&mut self, id: &ObjectId, now: u64, bytes: &[u8]) -> Result<Vec<u8>> {
        if now < self.last_now
            || now / EPOCH_MS != self.epoch
            || id.captured_ms / EPOCH_MS != self.epoch
            || id.captured_ms > now
        {
            return Err(Error::Expired);
        }
        if !track_valid(&id.track) {
            return Err(Error::Invalid);
        }
        if bytes.len() > MAX_OBJECT {
            return Err(Error::Capacity);
        }
        if !self.tracks.contains(&id.track) && self.tracks.len() >= MAX_TRACKS {
            return Err(Error::Capacity);
        }
        // Every seal re-checks the exclusive lease locally: no HTTP, no lock, no
        // possible block. An expired, revoked or failed-renewal lease denies the
        // seal, and the durable counter is not consumed. A fence that is absent
        // while the authority path was taken is a misconfigured runtime: fail
        // closed rather than encrypt under a root another host may share.
        if self.source == KeySource::Authority && self.lease_fence.is_none() {
            return Err(Error::Denied);
        }
        if self
            .lease_fence
            .as_ref()
            .is_some_and(|fence| fence.permit(now).is_none())
        {
            return Err(Error::Denied);
        }
        self.last_now = now;
        let counter = self.reservation.reserve(
            self.version,
            self.epoch,
            self.publisher,
            self.publisher_generation,
        )?;
        if counter == u64::MAX {
            return Err(Error::Capacity);
        }
        let mut header = Vec::with_capacity(HEADER_FIXED + id.track.len());
        header.extend(b"MC02");
        header.extend(self.version);
        header.extend(self.feed);
        header.extend(self.publisher);
        header.extend(self.epoch.to_be_bytes());
        header.extend(self.publisher_generation.to_be_bytes());
        header.extend(counter.to_be_bytes());
        header.extend(id.group.to_be_bytes());
        header.extend(id.object.to_be_bytes());
        header.extend(id.captured_ms.to_be_bytes());
        header.push(u8::try_from(id.track.len()).map_err(|_| Error::Capacity)?);
        header.extend(id.track.as_bytes());
        let key = subkey(
            &self.key.0,
            self.version,
            self.publisher,
            self.epoch,
            &id.track,
        )?;
        let cipher = Aes256Gcm::new_from_slice(&key.0).map_err(|_| Error::Crypto)?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce(self.publisher_generation, counter)),
                Payload {
                    msg: bytes,
                    aad: &header,
                },
            )
            .map_err(|_| Error::Crypto)?;
        header.extend(encrypted);
        self.tracks.insert(id.track.clone());
        Ok(header)
    }
    /// Sign only a live booking assertion fetched from authoritative storage.
    /// A distinct exchange issuer key must be injected; never reuse licence keys.
    pub fn grant(
        &mut self,
        signer: &SigningKey,
        binding: Binding,
        active: bool,
        expires_ms: u64,
        now: u64,
    ) -> Result<Vec<u8>> {
        if !active
            || binding.version != self.version
            || binding.feed != self.feed
            || binding.publisher != self.publisher
            || binding.publisher_generation != self.publisher_generation
        {
            return Err(Error::Denied);
        }
        if now < self.last_now || now / EPOCH_MS != self.epoch || expires_ms <= now {
            return Err(Error::Expired);
        }
        // A media grant carries the epoch root, so it is fenced exactly like a
        // seal: no grant is issued after the lease expired or its renewal failed.
        if self.source == KeySource::Authority && self.lease_fence.is_none() {
            return Err(Error::Denied);
        }
        if self
            .lease_fence
            .as_ref()
            .is_some_and(|fence| fence.permit(now).is_none())
        {
            return Err(Error::Denied);
        }
        self.last_now = now;
        let end = self
            .epoch
            .checked_add(1)
            .and_then(|v| v.checked_mul(EPOCH_MS))
            .ok_or(Error::Invalid)?;
        let mut body = Zeroizing::new(Vec::with_capacity(GRANT_BODY + 64));
        body.extend(b"MG02");
        body.extend(ISSUER);
        body.extend(AUDIENCE);
        body.extend(binding.booking);
        body.extend(binding.taker);
        body.extend(binding.feed);
        body.extend(binding.version);
        body.extend(binding.engine);
        body.extend(binding.publisher);
        body.extend(binding.generation.to_be_bytes());
        body.extend(binding.publisher_generation.to_be_bytes());
        body.extend(self.epoch.to_be_bytes());
        body.extend(now.to_be_bytes());
        body.extend(expires_ms.min(end).to_be_bytes());
        body.extend(self.key.0);
        debug_assert_eq!(body.len(), GRANT_BODY);
        let signature = signer.sign(&body).to_bytes();
        let mut wire = Vec::with_capacity(GRANT_BODY + 64);
        wire.extend(&*body);
        wire.extend(signature);
        Ok(wire)
    }
}
/// One receiver per authorized epoch. Never recreate it with the same grant to
/// reset replay state; denied renewal must also flush external decode queues.
pub struct Receiver {
    binding: Binding,
    epoch: u64,
    expiry: u64,
    key: Secret,
    seen: BTreeMap<String, u64>,
    denied: bool,
    last_now: u64,
    max_age: u64,
    uncertainty: u64,
}
impl Receiver {
    pub fn accept(
        root: &VerifyingKey,
        expected: Binding,
        wire: &[u8],
        now: u64,
        max_age: u64,
        uncertainty: u64,
    ) -> Result<Self> {
        if wire.len() != GRANT_BODY + 64 || &wire[..4] != b"MG02" {
            return Err(Error::Invalid);
        }
        root.verify_strict(
            &wire[..GRANT_BODY],
            &Signature::from_slice(&wire[GRANT_BODY..]).map_err(|_| Error::Invalid)?,
        )
        .map_err(|_| Error::Denied)?;
        if &wire[4..20] != ISSUER
            || &wire[20..36] != AUDIENCE
            || wire[36..52] != expected.booking
            || wire[52..68] != expected.taker
            || wire[68..84] != expected.feed
            || wire[84..100] != expected.version
            || wire[100..116] != expected.engine
            || wire[116..132] != expected.publisher
            || read_u64(wire, 132) != expected.generation
            || wire[140..144] != expected.publisher_generation.to_be_bytes()
        {
            return Err(Error::Denied);
        }
        let epoch = read_u64(wire, 144);
        let issued = read_u64(wire, 152);
        let expiry = read_u64(wire, 160);
        let end = epoch
            .checked_add(1)
            .and_then(|v| v.checked_mul(EPOCH_MS))
            .ok_or(Error::Invalid)?;
        if epoch != now / EPOCH_MS
            || issued > now
            || issued / EPOCH_MS != epoch
            || expiry <= now.saturating_add(uncertainty)
            || expiry > end
            || max_age > EPOCH_MS
            || uncertainty > EPOCH_MS
        {
            return Err(Error::Expired);
        }
        Ok(Self {
            binding: expected,
            epoch,
            expiry,
            key: Secret(wire[168..200].try_into().map_err(|_| Error::Invalid)?),
            seen: BTreeMap::new(),
            denied: false,
            last_now: now,
            max_age,
            uncertainty,
        })
    }
    /// Renew the current epoch without clearing authenticated replay high-water.
    /// A changed root key or identity is rejected. The receiver itself is reused;
    /// constructing a fresh receiver for the same epoch is not a renewal.
    pub fn renew(&mut self, root: &VerifyingKey, wire: &[u8], now: u64) -> Result<()> {
        if self.denied || now < self.last_now {
            return Err(Error::Denied);
        }
        let next = Self::accept(
            root,
            self.binding,
            wire,
            now,
            self.max_age,
            self.uncertainty,
        )?;
        if next.epoch != self.epoch || next.key.0 != self.key.0 || next.expiry < self.expiry {
            return Err(Error::Denied);
        }
        self.expiry = next.expiry;
        self.last_now = now;
        Ok(())
    }
    pub fn deny(&mut self) {
        self.denied = true;
        self.key.zeroize();
    }
    /// Decrypt an object only if its authenticated identity equals the transport
    /// identity. Reject replay or out-of-order counters per track (late join may
    /// start on any counter; older reordering is intentionally discarded).
    pub fn decrypt(&mut self, wire: &[u8], expected: &ObjectId, now: u64) -> Result<Vec<u8>> {
        if self.denied {
            return Err(Error::Denied);
        }
        if now < self.last_now || now.saturating_add(self.uncertainty) >= self.expiry {
            self.deny();
            return Err(Error::Expired);
        }
        self.last_now = now;
        if wire.len() < HEADER_FIXED + 16
            || wire.len() > HEADER_FIXED + MAX_TRACK_LEN + MAX_OBJECT + 16
            || &wire[..4] != b"MC02"
        {
            return Err(Error::Invalid);
        }
        if wire[4..20] != self.binding.version
            || wire[20..36] != self.binding.feed
            || wire[36..52] != self.binding.publisher
            || read_u64(wire, 52) != self.epoch
            || wire[60..64] != self.binding.publisher_generation.to_be_bytes()
        {
            return Err(Error::Denied);
        }
        let counter = read_u64(wire, 64);
        let group = read_u64(wire, 72);
        let object = read_u64(wire, 80);
        let captured = read_u64(wire, 88);
        let track_len = usize::from(wire[96]);
        let header_end = HEADER_FIXED.checked_add(track_len).ok_or(Error::Invalid)?;
        if track_len == 0 || track_len > MAX_TRACK_LEN || wire.len() < header_end + 16 {
            return Err(Error::Invalid);
        }
        let track = std::str::from_utf8(&wire[97..header_end]).map_err(|_| Error::Invalid)?;
        if !track_valid(track)
            || track != expected.track
            || group != expected.group
            || object != expected.object
            || captured != expected.captured_ms
        {
            return Err(Error::Denied);
        }
        if captured / EPOCH_MS != self.epoch
            || captured > now.saturating_add(self.uncertainty)
            || now
                .saturating_sub(captured)
                .saturating_add(self.uncertainty)
                > self.max_age
        {
            return Err(Error::Expired);
        }
        if self.seen.get(track).is_some_and(|prior| counter <= *prior) {
            return Err(Error::Replay);
        }
        if !self.seen.contains_key(track) && self.seen.len() >= MAX_TRACKS {
            return Err(Error::Capacity);
        }
        let key = subkey(
            &self.key.0,
            self.binding.version,
            self.binding.publisher,
            self.epoch,
            track,
        )?;
        let cipher = Aes256Gcm::new_from_slice(&key.0).map_err(|_| Error::Crypto)?;
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&nonce(self.binding.publisher_generation, counter)),
                Payload {
                    msg: &wire[header_end..],
                    aad: &wire[..header_end],
                },
            )
            .map_err(|_| Error::Crypto)?;
        self.seen.insert(track.to_owned(), counter);
        Ok(plain)
    }
}
