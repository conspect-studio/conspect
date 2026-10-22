//! The mesh **transport seam** — the trait that isolates the mDNS socket so the
//! announce/browse logic is testable **offline** (ADR-0051 §1, brief §13.2).
//!
//! The pure logic (build a signed [`AnnouncePayload`](crate::announce::AnnouncePayload),
//! verify a received one, fold an observation into the [`PeerTable`](crate::peer::PeerTable))
//! depends only on this trait, never on a concrete socket. The live mDNS-sd
//! implementation lives behind the off-by-default `mdns` feature
//! ([`crate::service`]); an in-memory fake drives the logic in unit tests with no
//! network. This is the documented pattern for live-socket code: the socket layer
//! is a trait, the logic is pure, and the live-network test is `#[ignore]`d +
//! hardware-gated.

use crate::announce::AnnouncePayload;
use crate::error::MeshError;

/// The TXT property key holding the chunk count.
#[cfg(any(feature = "mdns", test))]
pub(crate) const CHUNK_COUNT_KEY: &str = "c";

/// The maximum bytes in a chunk emitted by the mesh announcer.
#[cfg(any(feature = "mdns", test))]
pub(crate) const CHUNK_BYTES: usize = 200;

/// The maximum accepted chunk count (12.8 KiB at [`CHUNK_BYTES`] per chunk).
///
/// A legitimate announcement carries a small set of 32-byte salted hardware
/// digests plus compact entitlement metadata. Sixty-four chunks leave substantial
/// protocol headroom while bounding allocation and work from untrusted TXT input.
#[cfg(any(feature = "mdns", test))]
pub(crate) const MAX_CHUNKS: usize = 64;

/// A received mesh announcement from the transport: the raw wire bytes of a
/// peer's TXT-record payload. The transport never interprets it; consumers decode
/// it into the untrusted discovered-peer inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceivedAnnouncement {
    /// The raw wire bytes the transport observed (decoded by the logic via
    /// [`AnnouncePayload::from_wire`]).
    pub wire: Vec<u8>,
}

impl ReceivedAnnouncement {
    /// Wrap observed wire bytes.
    #[must_use]
    pub fn new(wire: Vec<u8>) -> Self {
        Self { wire }
    }

    /// Decode the observed bytes into a payload (a typed error on garbage).
    ///
    /// # Errors
    /// [`MeshError::MalformedPayload`] if the bytes are not a well-formed
    /// announce payload.
    pub fn decode(&self) -> Result<AnnouncePayload, MeshError> {
        AnnouncePayload::from_wire(&self.wire)
    }
}

/// Reassemble numbered TXT chunks through a transport-specific property lookup.
///
/// The attacker-controlled count is capped before allocation or chunk lookup.
#[cfg(any(feature = "mdns", test))]
pub(crate) fn reassemble_txt<'a>(
    mut property: impl FnMut(&str) -> Option<&'a str>,
) -> Option<Vec<u8>> {
    let count: usize = property(CHUNK_COUNT_KEY)?.parse().ok()?;
    if count > MAX_CHUNKS {
        return None;
    }
    let capacity = count.checked_mul(CHUNK_BYTES)?;
    let mut wire = Vec::with_capacity(capacity);
    for index in 0..count {
        let key = format!("p{index}");
        let chunk = property(&key)?;
        wire.extend_from_slice(chunk.as_bytes());
    }
    Some(wire)
}

/// Split the announce `wire` into the numbered mDNS TXT properties
/// (`c` = chunk count, then `p0`…`pN`), refusing a payload that would exceed the
/// `MAX_CHUNKS` cap.
///
/// The publish-side mirror of [`reassemble_txt`], with a deliberate asymmetry:
/// the receive side caps an *untrusted* inbound count and silently ignores
/// anything over the bound (returning `None`), but the publish side is encoding
/// *our own* payload, so an over-cap announce is a real, log-worthy fault — it
/// returns a typed [`MeshError::AnnounceTooLarge`] the caller logs, rather than
/// emitting an announce that every peer's receive-side guard would silently drop
/// (which would make this node invisible on the mesh). Today's payload shape keeps
/// a legitimate announce far under the cap; the guard is defence in depth against
/// future payload growth, and makes that failure observable at the source.
///
/// # Errors
/// * [`MeshError::AnnounceTooLarge`] if `wire` would split into more than
///   `MAX_CHUNKS` chunks.
/// * [`MeshError::AnnounceNotText`] if a chunk is not valid UTF-8 (so it cannot
///   ride an mDNS TXT string property) — refused, never silently dropped.
#[cfg(any(feature = "mdns", test))]
pub(crate) fn chunk_txt(wire: &[u8]) -> Result<Vec<(String, String)>, MeshError> {
    let chunks: Vec<&[u8]> = wire.chunks(CHUNK_BYTES).collect();
    if chunks.len() > MAX_CHUNKS {
        return Err(MeshError::AnnounceTooLarge {
            chunks: chunks.len(),
            max: MAX_CHUNKS,
        });
    }
    let mut props: Vec<(String, String)> = Vec::with_capacity(chunks.len() + 1);
    props.push((CHUNK_COUNT_KEY.to_owned(), chunks.len().to_string()));
    for (index, chunk) in chunks.iter().enumerate() {
        // A chunk must be valid UTF-8 to ride an mDNS TXT string property. Today's
        // announce is pure-ASCII JSON so this never fails, but the contract is
        // TOTAL: a non-UTF-8 chunk is refused with a typed error, never skipped.
        // Skipping it would push only `chunks.len() - 1` `p` properties under a
        // `c` count of `chunks.len()`, and every peer's `reassemble_txt` would then
        // miss `p{index}` and silently drop the whole announce (the exact failure
        // this guard prevents).
        let text = std::str::from_utf8(chunk)
            .map_err(|_| MeshError::AnnounceNotText { chunk_index: index })?;
        props.push((format!("p{index}"), text.to_owned()));
    }
    Ok(props)
}

/// The transport seam: publish this machine's announcement, and report observed
/// peer announcements. Implemented by the live mDNS-sd service ([`crate::service`],
/// `mdns` feature) and by an in-memory fake in tests.
///
/// Every method is **best-effort**: a transport failure is a typed error the
/// caller logs and carries on from — the mesh never stalls anything (invariant
/// #10). The trait holds no engine handle.
pub trait MeshTransport {
    /// Publish (or re-publish) this machine's announcement carrying `wire` (the
    /// encoded [`AnnouncePayload`]). Idempotent: re-announcing updates the TXT
    /// record.
    ///
    /// # Errors
    /// * [`MeshError::AnnounceTooLarge`] if `wire` would split into more than
    ///   `MAX_CHUNKS` TXT chunks (larger than the receive-side bound accepts) —
    ///   refused at the source so the over-cap announce is observable, never
    ///   silently dropped by every peer.
    /// * [`MeshError::AnnounceNotText`] if a chunk of `wire` is not valid UTF-8
    ///   (so it cannot ride a TXT string value) — refused at the source rather
    ///   than emitted with a missing chunk that every peer would silently drop.
    /// * [`MeshError::Transport`] if the transport could not publish (e.g. the
    ///   socket is down). Best-effort — the caller logs + retries on the next tick.
    fn announce(&self, wire: &[u8]) -> Result<(), MeshError>;

    /// Drain the announcements observed since the last poll (non-blocking). An
    /// empty vec means nothing new. Never blocks on the network — a poll that
    /// finds nothing returns immediately.
    ///
    /// # Errors
    /// [`MeshError::Transport`] if the transport could not be polled.
    fn poll_received(&self) -> Result<Vec<ReceivedAnnouncement>, MeshError>;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{chunk_txt, reassemble_txt, CHUNK_BYTES, CHUNK_COUNT_KEY, MAX_CHUNKS};

    use crate::error::MeshError;

    #[test]
    fn reassembly_rejects_hostile_count_before_reading_chunks() {
        // A recorded flag (asserted, never panicked) proves the over-limit count
        // is rejected before any chunk key `p{i}` is ever looked up.
        let count = 1_000_000_000_000_usize.to_string();
        let looked_up_chunk = std::cell::Cell::new(false);
        let wire = reassemble_txt(|key| {
            if key == CHUNK_COUNT_KEY {
                Some(count.as_str())
            } else {
                looked_up_chunk.set(true);
                None
            }
        });

        assert_eq!(wire, None);
        assert!(
            !looked_up_chunk.get(),
            "over-limit input must be rejected before reading any chunk"
        );
    }

    #[test]
    fn reassembly_accepts_exact_maximum_chunk_count() {
        let mut properties = HashMap::new();
        properties.insert(CHUNK_COUNT_KEY.to_owned(), MAX_CHUNKS.to_string());
        for index in 0..MAX_CHUNKS {
            properties.insert(format!("p{index}"), "x".to_owned());
        }

        let wire = reassemble_txt(|key| properties.get(key).map(String::as_str));

        assert_eq!(wire, Some(vec![b'x'; MAX_CHUNKS]));
    }

    #[test]
    fn reassembly_rejects_one_chunk_over_maximum() {
        let count = (MAX_CHUNKS + 1).to_string();
        let looked_up_chunk = std::cell::Cell::new(false);
        let wire = reassemble_txt(|key| {
            if key == CHUNK_COUNT_KEY {
                Some(count.as_str())
            } else {
                looked_up_chunk.set(true);
                None
            }
        });

        assert_eq!(wire, None);
        assert!(
            !looked_up_chunk.get(),
            "one-over-maximum must be rejected before reading any chunk"
        );
    }

    /// A maximal *legitimate* announce still reassembles well under the
    /// `MAX_CHUNKS` cap — the bound rejects only oversized/malicious TXT input,
    /// never real traffic (the residual headroom check for the #236 alloc-bound
    /// fix, so a genuine announcement is never silently dropped by the guard).
    ///
    /// The only variable-length fields in an `AnnouncePayload` are `digests` and
    /// the fixed 64-byte Ed25519 `signature`. The digest set mirrors the machine's
    /// fingerprint: `multiview-licence` models exactly five `ComponentKind`s
    /// (board/cpu/nic/disk/gpu), scored one-per-kind, so a canonical announce
    /// carries ~5 salted digests. This over-provisions by an order of magnitude —
    /// 64 distinct salted component digests, far beyond any real multiviewer host —
    /// so a pass proves the cap leaves ample headroom for legitimate announcements.
    #[test]
    fn max_legitimate_announce_reassembles_within_chunk_cap() {
        use chrono::{DateTime, Utc};
        use multiview_licence::EnforcementLevel;

        use crate::announce::{AnnouncePayload, EntitlementSummary, SaltedDigest};
        use crate::peer::PEER_DIGEST_LEN;
        use crate::ClaimState;

        // An order-of-magnitude over-provision of the ~5-kind canonical digest set.
        const MAX_REALISTIC_DIGESTS: usize = 64;

        // `AnnouncePayload::to_wire` is serde_json, which encodes each `[u8; 32]`
        // digest and the 64-byte signature as a JSON array of DECIMAL integers, so
        // a `0xFF` byte serialises to its widest three-digit form ("255"). Filling
        // every byte with `0xFF` makes the measured size a STRICT UPPER BOUND on any
        // real (random-valued) payload of the same shape, and keeps the byte-exact
        // chunk count deterministic (no RNG near a chunk boundary).
        let digests = vec![SaltedDigest::new([0xFF; PEER_DIGEST_LEN]); MAX_REALISTIC_DIGESTS];
        // Widest-serialising fixed fields too: kebab-case `claim_state` "unclaimed"
        // and `level` "block-new-instance", full-nanosecond RFC3339 lease bounds.
        let granted =
            DateTime::<Utc>::from_timestamp(1_700_000_000, 123_456_789).expect("valid instant");
        let expires =
            DateTime::<Utc>::from_timestamp(1_800_000_000, 987_654_321).expect("valid instant");
        let entitlement =
            EntitlementSummary::new(EnforcementLevel::BlockNewInstance, granted, expires);
        let payload = AnnouncePayload {
            protocol_version: u16::MAX,
            digests,
            claim_state: ClaimState::Unclaimed,
            entitlement,
            // A real Ed25519 signature is exactly 64 bytes; `0xFF` maximises each
            // byte's JSON width. A size fixture, not a valid signature — `to_wire`
            // serialises, it does not verify.
            signature: vec![0xFF; ed25519_dalek::Signature::BYTE_SIZE],
        };

        let wire = payload.to_wire().expect("a well-formed payload serialises");

        // The publish side splits the wire into `CHUNK_BYTES`-sized chunks (mirrors
        // `MdnsService::announce`: `wire.chunks(CHUNK_BYTES)` → the `c` count).
        let chunks: Vec<&[u8]> = wire.chunks(CHUNK_BYTES).collect();
        let chunk_count = chunks.len();

        // 1) The cap must ACCEPT a maximal legitimate announce.
        assert!(
            chunk_count <= MAX_CHUNKS,
            "a maximal legitimate announce ({MAX_REALISTIC_DIGESTS} digests, {} wire \
             bytes) needs {chunk_count} chunks but the cap is {MAX_CHUNKS} — the bound \
             is too tight for real traffic (REAL FINDING: raise MAX_CHUNKS, do not \
             shrink this test)",
            wire.len(),
        );
        // 2) ...with substantial headroom: even this ~10x over-provision must leave
        //    at least a quarter of the chunk budget free (a real host carries ~5
        //    digests ≈ a handful of chunks).
        let headroom = MAX_CHUNKS.saturating_sub(chunk_count);
        assert!(
            headroom >= MAX_CHUNKS / 4,
            "a maximal legitimate announce needs {chunk_count}/{MAX_CHUNKS} chunks — \
             only {headroom} free; the cap is set too close to legitimate traffic"
        );

        // 3) End-to-end: the real reassembly guard accepts it and losslessly
        //    reconstructs the wire (proving the bound never drops a legit announce),
        //    and the bytes decode back to the same payload.
        let mut properties: HashMap<String, String> = HashMap::new();
        properties.insert(CHUNK_COUNT_KEY.to_owned(), chunk_count.to_string());
        for (index, chunk) in chunks.iter().enumerate() {
            let text =
                std::str::from_utf8(chunk).expect("announce JSON is ASCII — never splits a char");
            properties.insert(format!("p{index}"), text.to_owned());
        }
        let reassembled = reassemble_txt(|key| properties.get(key).map(String::as_str))
            .expect("the guard accepts a legitimate announce within the cap");
        assert_eq!(
            reassembled, wire,
            "the reassembly guard must losslessly reassemble a maximal legitimate announce"
        );
        let decoded = AnnouncePayload::from_wire(&reassembled).expect("reassembled bytes decode");
        assert_eq!(
            decoded, payload,
            "a maximal legitimate announce round-trips through chunk reassembly"
        );
    }

    /// The publish side refuses a `wire` that would split into more than
    /// `MAX_CHUNKS` chunks, returning a typed `MeshError::AnnounceTooLarge`
    /// rather than emitting an announce every peer's receive-side bound would
    /// silently drop (which would make this node invisible on the mesh) — the
    /// publish-side mirror of the receive-side cap.
    #[test]
    fn chunk_txt_refuses_a_wire_past_the_chunk_cap() {
        // One byte past `MAX_CHUNKS` full chunks → `MAX_CHUNKS + 1` chunks.
        let over_cap = vec![b'x'; MAX_CHUNKS * CHUNK_BYTES + 1];
        assert_eq!(
            chunk_txt(&over_cap),
            Err(MeshError::AnnounceTooLarge {
                chunks: MAX_CHUNKS + 1,
                max: MAX_CHUNKS,
            }),
            "an over-cap announce must be refused at the publisher, not emitted"
        );
    }

    /// A `wire` at exactly the cap publishes (the bound is `>`, not `>=`) and the
    /// emitted properties reassemble losslessly through the receive-side guard —
    /// proving publish/receive chunking stays symmetric right up to the limit.
    #[test]
    fn chunk_txt_accepts_the_cap_and_round_trips() {
        let at_cap = vec![b'x'; MAX_CHUNKS * CHUNK_BYTES];
        let props = chunk_txt(&at_cap).expect("a wire at exactly the cap must publish");

        // `c` = the emitted chunk count, then one `p{i}` per chunk.
        assert_eq!(
            props.first(),
            Some(&(CHUNK_COUNT_KEY.to_owned(), MAX_CHUNKS.to_string())),
            "the `c` property must carry the emitted chunk count"
        );
        assert_eq!(
            props.len(),
            MAX_CHUNKS + 1,
            "one `c` property plus one property per chunk"
        );

        // Publish → receive symmetry: the guard reassembles the exact wire.
        let map: HashMap<String, String> = props.into_iter().collect();
        let reassembled = reassemble_txt(|key| map.get(key).map(String::as_str))
            .expect("the receive guard accepts an at-cap announce");
        assert_eq!(
            reassembled, at_cap,
            "publish/receive chunking must round-trip at the cap"
        );
    }

    /// `chunk_txt`'s contract is TOTAL: it returns either `Ok` with exactly one
    /// `p{i}` property per chunk, or a typed error — never `Ok` having silently
    /// dropped a chunk. A chunk that is not valid UTF-8 (so it cannot ride an
    /// mDNS TXT string value) is refused with `MeshError::AnnounceNotText`, not
    /// skipped: skipping it would emit `c` = N with only N-1 `p` properties, and
    /// every peer's `reassemble_txt` would then miss `p{index}` (its `?` yields
    /// `None`) and silently drop the whole announce — the exact silent-drop this
    /// guard exists to prevent, hiding inside the guard itself.
    ///
    /// Today's announce is pure-ASCII JSON so this never trips in practice (see
    /// `max_legitimate_announce_reassembles_within_chunk_cap`); the guard keeps
    /// the contract total against future payload growth or a non-ASCII field.
    #[test]
    fn chunk_txt_refuses_a_chunk_that_is_not_valid_utf8() {
        // A full ASCII first chunk, then a second chunk whose leading byte
        // (`0xFF`, never a valid UTF-8 start) is not text. The bad chunk is at
        // index 1, so the reported `chunk_index` must be the real index — a
        // hard-coded `0` would be wrong.
        let mut wire = vec![b'x'; CHUNK_BYTES];
        wire.push(0xFF);
        assert_eq!(
            chunk_txt(&wire),
            Err(MeshError::AnnounceNotText { chunk_index: 1 }),
            "a non-UTF-8 chunk must be refused with its index, never silently \
             dropped (which would emit c=N with N-1 p-properties, dropped by every peer)"
        );
    }
}
