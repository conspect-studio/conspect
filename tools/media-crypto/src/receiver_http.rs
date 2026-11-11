//! Frozen booking-grant client. Control plane only; no live route is assumed.
//! Opaque HTTP ids and the authoritative raw crypto binding are independent.
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use serde_json::json;

use crate::authority_http::{
    valid_id, validated_origin, AuthorityHttpRequest, AuthorityHttpTransport, OrganisationCsk,
};
use crate::{Binding, Error, Receiver, Result, EPOCH_MS};

const MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;
const UNCERTAINTY_MS: u64 = 1_000;

/// Explicit HTTP strings supplied by the authority registration, never derived
/// from raw binding bytes. No Debug on the client or secret response.
pub struct ReceiverHttpIds {
    org: String,
    booking: String,
    machine: String,
}
impl ReceiverHttpIds {
    pub fn new(org: String, booking: String, machine: String) -> Result<Self> {
        if [&org, &booking, &machine].iter().any(|s| !valid_id(s))
            || [org.as_str(), booking.as_str()]
                .iter()
                .any(|s| matches!(*s, "." | ".."))
        {
            return Err(Error::Denied);
        }
        Ok(Self {
            org,
            booking,
            machine,
        })
    }
}

/// One response only. No fallback, automatic retry, or expiry extension.
/// After a denial close to an epoch boundary, the caller must wait for
/// `next_epoch_ms` before one subsequent attempt, with its own bounded deadline.
pub struct HttpReceiverGrantClient {
    url: String,
    ids: ReceiverHttpIds,
    expected: Binding,
    key: VerifyingKey,
    credential: Arc<OrganisationCsk>,
    transport: Arc<dyn AuthorityHttpTransport>,
}
impl HttpReceiverGrantClient {
    /// `expected` must come from an authoritative registration mapping and use
    /// the current publisher generation, not guessed or hashed HTTP ids.
    pub fn new(
        origin: &str,
        ids: ReceiverHttpIds,
        expected: Binding,
        pinned_exchange_key: VerifyingKey,
        credential: Arc<OrganisationCsk>,
        transport: Arc<dyn AuthorityHttpTransport>,
    ) -> Result<Self> {
        if expected.generation != 0 {
            return Err(Error::Denied);
        }
        let mut url = validated_origin(origin)?;
        url.path_segments_mut()
            .map_err(|_| Error::Denied)?
            .pop_if_empty()
            .extend([
                "v0",
                "organisations",
                &ids.org,
                "exchange",
                "bookings",
                &ids.booking,
                "grants",
            ]);
        Ok(Self {
            url: url.to_string(),
            ids,
            expected,
            key: pinned_exchange_key,
            credential,
            transport,
        })
    }

    /// Make exactly one authenticated request. Both samples are Unix ms; sample
    /// `now_after` after the response through the supplied clock, not beforehand.
    pub fn fetch(
        &self,
        requested_expiry_ms: u64,
        max_age_ms: u64,
        clock: impl Fn() -> u64,
    ) -> Result<Receiver> {
        let before = clock();
        if requested_expiry_ms > MAX_SAFE_INTEGER
            || before > MAX_SAFE_INTEGER
            || requested_expiry_ms <= before.saturating_add(UNCERTAINTY_MS)
        {
            return Err(Error::Expired);
        }
        let response = self.transport.send_grant(AuthorityHttpRequest {
            method: "POST", url: self.url.clone(),
            body: Some(serde_json::to_vec(&json!({"machineId": self.ids.machine, "requestedExpiryMs": requested_expiry_ms})).map_err(|_| Error::Invalid)?),
            lease_headers: None,
        }, &self.credential)?;
        let after = clock();
        if after < before || after > MAX_SAFE_INTEGER {
            return Err(Error::Expired);
        }
        if response.status != 200 {
            return Err(Error::Denied);
        }
        let directives: Vec<_> = response.cache_control.split(',').map(str::trim).collect();
        if !response
            .content_type
            .eq_ignore_ascii_case("application/octet-stream")
            || !directives
                .iter()
                .any(|s| s.eq_ignore_ascii_case("no-store"))
            || !directives.iter().any(|s| s.eq_ignore_ascii_case("private"))
            || directives.iter().any(|s| s.eq_ignore_ascii_case("public"))
            || response.body.len() != 264
            || response.content_length != Some(264)
        {
            return Err(Error::Invalid);
        }
        let receiver = Receiver::accept(
            &self.key,
            self.expected,
            &response.body,
            after,
            max_age_ms,
            UNCERTAINTY_MS,
        )?;
        let expiry = u64::from_be_bytes(
            response.body[160..168]
                .try_into()
                .map_err(|_| Error::Invalid)?,
        );
        if expiry > requested_expiry_ms {
            return Err(Error::Denied);
        }
        Ok(receiver)
    }
}

/// Earliest next-epoch attempt. Callers must use a timer and a bounded overall
/// retry budget; this client deliberately has no immediate retry loop.
pub fn next_epoch_ms(now: u64) -> Result<u64> {
    (now / EPOCH_MS)
        .checked_add(1)
        .and_then(|e| e.checked_mul(EPOCH_MS))
        .ok_or(Error::Invalid)
}
