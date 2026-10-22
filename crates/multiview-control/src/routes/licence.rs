//! The **local licence** REST surface under `/api/v1/licence` (CONSPECT-1,
//! ADR-0050 / the Conspect brief §11).
//!
//! Three endpoints render the machine-side entitlement plane, all **local** —
//! there are **no** licence-server calls *here* (the device→server heartbeat
//! client is the cli's feature-gated `heartbeat` loop, CONSPECT-3/ADR-0096; this
//! surface only reports the resulting local lease state):
//!
//! * `GET /api/v1/licence` — the computed licence resource (tier + the
//!   enforcement-ladder `state`/`enforcement` level + dated lease). **Enforcement
//!   is data**: this always answers `200` and never `5xx`, even when no lease is
//!   installed (it reports `licensed: false`). Role: read.
//! * `POST /api/v1/licence/lease` — install a presented signed lease **binding**
//!   (CBOR body). Verify the `Ed25519` signature against the pinned issuer key,
//!   check fingerprint continuity, reject a stale grant, then install. On success
//!   `200 {lease, valid_to}`; on rejection an RFC 9457 problem (`signature_invalid`
//!   → 422, `fingerprint_mismatch` → 409, `lease_stale` → 409). Role: write.
//! * `GET /api/v1/licence/challenge` — the `<host>.challenge` export as an
//!   `application/cbor` attachment (salted digests + counters only — data
//!   minimisation). Role: read.
//!
//! # Never off air (invariant #1 / #10)
//!
//! This surface reads + verifies **data**. The store it renders holds an
//! `RwLock` over a single verified entitlement read off the engine hot loop, and
//! verification is pure signature math — neither holds an engine handle, spawns a
//! task, or sends on an engine channel. A wedged client of these routes cannot
//! back-pressure the engine, and no computed state takes a running program off
//! air (the ladder is conveniences the engine *samples*, never control flow
//! here).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use multiview_licence::challenge::{ChallengeCounters, ChallengeFile};
use multiview_licence::store::{InstallError, LeaseBinding, LicenceStatusView};
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::problem::Problem;
use crate::state::AppState;

/// The maximum size, in bytes, of a presented lease-binding CBOR body. A binding
/// is a signed lease (a serial + a handful of dates + a 64-byte signature) plus a
/// small entitlement — well under a kilobyte. The cap bounds the read so a
/// malformed/oversized upload is rejected cheaply (bad-inputs-are-the-purpose),
/// never buffered unboundedly.
const MAX_BINDING_BYTES: usize = 64 * 1024;

/// The `application/cbor` media type the challenge export is served as
/// ([RFC 8949 §10](https://www.rfc-editor.org/rfc/rfc8949#section-10)).
const CBOR_MEDIA_TYPE: &str = "application/cbor";

/// The computed licence resource rendered at `GET /api/v1/licence`.
///
/// A single shape that always serialises (never `5xx`): `licensed` is `true` with
/// the full computed `status` when a verified lease is installed, and `false`
/// with `status: null` when none is — an honest "unlicensed" data report rather
/// than an error. The control plane, the chrome banner, and the portals all read
/// the same computed `status.enforcement` level; there is no second opinion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct LicenceResource {
    /// Whether a verified lease is currently installed.
    pub licensed: bool,
    /// The computed licence status when a lease is installed; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = Option<crate::openapi_schemas::LicenceStatusDoc>)
    )]
    pub status: Option<LicenceStatusView>,
}

impl LicenceResource {
    /// The resource for a computed status (a lease is installed).
    #[must_use]
    fn licensed(status: LicenceStatusView) -> Self {
        Self {
            licensed: true,
            status: Some(status),
        }
    }

    /// The unlicensed resource (no lease installed) — still a `200` data report.
    #[must_use]
    fn unlicensed() -> Self {
        Self {
            licensed: false,
            status: None,
        }
    }
}

/// The `200` body of a successful `POST /api/v1/licence/lease` install: the
/// installed lease's serial and its `valid_to` (the lease term expiry, RFC 3339).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LeaseInstalled {
    /// The serial of the lease that was verified + installed.
    pub serial: String,
    /// The instant the installed lease term expires (RFC 3339).
    pub valid_to: String,
}

/// The exhaustive list of fields a licensing **heartbeat** payload carries
/// (brief §7/§8, ADR-0050 §3): the licence id, the salted hardware-fingerprint
/// digest vector, the app version, and the lease serial. **Reported, never raw**
/// — these name *what* the heartbeat sends (salted digests, never raw
/// serials/MACs, §8); the surface is read-only so the operator can see the
/// minimal licensing-keep-alive payload without it ever co-mingling with the
/// opt-in telemetry pipe. Stable slugs the portal + machine share.
const HEARTBEAT_PAYLOAD_FIELDS: &[&str] = &[
    "licence_id",
    "fingerprint_digest_vector",
    "app_version",
    "lease_serial",
];

/// The read-only heartbeat-status surface (`GET /api/v1/licensing/heartbeat-status`).
///
/// This reports **honestly from local lease state** — the transport the active
/// lease arrived over, the lease install instant as `last_at`, the lease's
/// `next_contact_due` as `next_due`, and the exhaustive payload-field list —
/// regardless of whether the cli's feature-gated `heartbeat` loop
/// (CONSPECT-3/ADR-0096) is the producer that installed the lease (it drives the
/// same `LeaseStore::install_binding` convergence the offline file-drop uses).
/// There is **no** mutating endpoint for the heartbeat (the spec mandates none) —
/// the `POST /api/v1/account/licence/heartbeat` force-now action is a later,
/// feature-gated item (brief §11 #4), not this read surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct HeartbeatStatus {
    /// The instant this machine last accepted a lease (the install instant), RFC
    /// 3339; `None` when no lease is installed (no heartbeat yet — honest).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_at: Option<String>,
    /// When the next licensing contact is due (the active lease's
    /// `next_contact_due`), RFC 3339; `None` when no lease is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_due: Option<String>,
    /// The transport the active lease arrived over: `"direct"` (online server
    /// contact), `"relay"` (mesh relay), or `"file"` (a dropped offline lease).
    /// `"none"` when no lease is installed.
    pub transport: String,
    /// The exhaustive list of fields a heartbeat payload carries
    /// ([`HEARTBEAT_PAYLOAD_FIELDS`]) — reported, never raw identifiers (§8).
    pub payload_fields: Vec<String>,
}

impl HeartbeatStatus {
    /// The payload-field list, always reported (the shape is fixed regardless of
    /// lease state).
    fn payload_fields() -> Vec<String> {
        HEARTBEAT_PAYLOAD_FIELDS
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }
}

/// `GET /api/v1/licensing/heartbeat-status` — the read-only heartbeat status
/// (role: read).
///
/// Always `200` (it is a data report, never an error path): the honest local
/// heartbeat status (transport + last/next contact + payload fields). With no
/// lease installed it reports `transport: "none"` and null contact instants. The
/// spec mandates **no** mutating endpoint exists for the heartbeat — this is
/// read-only, and the router wires only `GET`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/licensing/heartbeat-status",
        tag = "licence",
        responses(
            (status = 200, description = "The honest local heartbeat status (read-only; no mutating endpoint exists).", body = HeartbeatStatus),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_heartbeat_status(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<HeartbeatStatus>> {
    principal.role.require(Action::Read)?;
    // Honest local report: the active lease (if any) supplies the transport +
    // next-due; the store supplies the install instant (last contact). This route
    // makes no licence-server call — the device→server heartbeat is the cli's
    // feature-gated `heartbeat` loop (CONSPECT-3/ADR-0096); this surface only reads
    // the local lease state that loop (or the offline file-drop) installed.
    let status = match state.licence.store.current() {
        Some(entitlement) => {
            let last_at = state.licence.store.installed_at().map(|t| t.to_rfc3339());
            HeartbeatStatus {
                last_at,
                next_due: Some(entitlement.lease.next_contact_due.to_rfc3339()),
                transport: entitlement.lease.source.transport().to_owned(),
                payload_fields: HeartbeatStatus::payload_fields(),
            }
        }
        None => HeartbeatStatus {
            last_at: None,
            next_due: None,
            transport: "none".to_owned(),
            payload_fields: HeartbeatStatus::payload_fields(),
        },
    };
    Ok(Json(status))
}

/// `GET /api/v1/licence` — the computed licence resource (role: read).
///
/// Always `200` (enforcement is **data**, never an error path): a `licensed:true`
/// resource with the computed ladder `state`/`enforcement` when a lease is
/// installed, a `licensed:false` resource otherwise.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/licence",
        tag = "licence",
        responses(
            (status = 200, description = "The computed licence resource (enforcement is data; always 200).", body = LicenceResource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_licence(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<LicenceResource>> {
    principal.role.require(Action::Read)?;
    let resource = match state.licence.store.status() {
        Some(status) => LicenceResource::licensed(status),
        None => LicenceResource::unlicensed(),
    };
    Ok(Json(resource))
}

/// `POST /api/v1/licence/lease` — verify + install a presented signed lease
/// binding (CBOR body; role: write).
///
/// Returns `200 {lease, valid_to}` on success, or an RFC 9457 problem on
/// rejection: `signature_invalid` (422), `fingerprint_mismatch` (409),
/// `lease_stale` (409). A binding that is not well-formed CBOR is a `422`
/// `malformed_binding` (bad-inputs-are-the-purpose). When no issuer key has been
/// pinned, the install is refused with a `409` `no_pinned_key`. A rejection never
/// degrades the machine — the previously-installed (or empty) state stays put
/// (fail toward leniency).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/licence/lease",
        tag = "licence",
        request_body(content = Vec<u8>, description = "The signed lease binding, canonical CBOR.", content_type = "application/cbor"),
        responses(
            (status = 200, description = "Lease verified + installed.", body = LeaseInstalled),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to install.", body = crate::problem::Problem),
            (status = 409, description = "Fingerprint mismatch, stale grant, or no pinned key.", body = crate::problem::Problem),
            (status = 413, description = "The binding body exceeds the size cap.", body = crate::problem::Problem),
            (status = 422, description = "Malformed CBOR or an invalid signature.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn install_lease(
    State(state): State<AppState>,
    principal: Principal,
    body: Bytes,
) -> Response {
    if let Err(err) = principal.role.require(Action::Write) {
        return err.into_response();
    }
    if body.len() > MAX_BINDING_BYTES {
        return Problem::new(413, "binding_too_large", "Lease binding too large")
            .with_detail(format!(
                "the lease binding body exceeds the {MAX_BINDING_BYTES}-byte cap"
            ))
            .into_response();
    }

    // Decode the presented binding. Garbage / truncated CBOR is a typed 422,
    // never a panic (bad-inputs-are-the-purpose).
    let binding = match LeaseBinding::from_bytes(&body) {
        Ok(binding) => binding,
        Err(err) => {
            return Problem::new(422, "malformed_binding", "Malformed lease binding")
                .with_detail(format!(
                    "the body is not a well-formed lease binding: {err}"
                ))
                .into_response();
        }
    };

    // An issuer key must be pinned to verify a binding. Without one we refuse
    // rather than installing an unverifiable lease (never off air: the running
    // program is untouched, the machine simply stays at its current state).
    let Some(pinned) = state.licence.pinned.as_ref() else {
        return Problem::new(409, "no_pinned_key", "No issuer key pinned")
            .with_detail("the machine has no pinned issuer key to verify a lease against")
            .into_response();
    };

    let now = state.licence.store.now();
    match state.licence.store.install_binding(&binding, pinned, now) {
        Ok(lease) => {
            let valid_to = lease.valid_to_rfc3339();
            // Account-side evidence trail (ADR-0053 §4 / brief §10): every lease
            // install is an immutable, timestamped, actor-attributed entry. The
            // detail carries the lease serial + its term expiry — never a raw
            // identifier (data minimisation, brief §8). Written off the hot loop
            // into a control-plane store (inv #10).
            state.audit_account(
                &principal.key_id,
                crate::account_audit::AccountAuditKind::LeaseInstall,
                Some(serde_json::json!({
                    "serial": lease.serial,
                    "valid_to": valid_to,
                })),
            );
            let installed = LeaseInstalled {
                serial: lease.serial,
                valid_to,
            };
            (StatusCode::OK, Json(installed)).into_response()
        }
        Err(err) => install_error_problem(&err).into_response(),
    }
}

/// `GET /api/v1/licence/challenge` — the `<host>.challenge` export as an
/// `application/cbor` attachment (role: read).
///
/// The challenge carries **only** salted digests + monotonic counters — never a
/// raw serial, MAC, or hostname (data minimisation, brief §3/§8). The digests +
/// counters are sourced from the entitlement plane; CONSPECT-1 renders the
/// machine's current installed-lease count and an empty salted-digest set when
/// the cli has not yet supplied a salted machine fingerprint (the cli wiring is
/// CONSPECT-10). The body is canonical CBOR a portal consumes byte-for-byte.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/licence/challenge",
        tag = "licence",
        responses(
            (status = 200, description = "The salted challenge export (CBOR attachment).", content_type = "application/cbor"),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_challenge(State(state): State<AppState>, principal: Principal) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }

    let challenge = challenge_for(&state);

    match challenge.to_cbor() {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, CBOR_MEDIA_TYPE),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"host.challenge\"",
                ),
            ],
            bytes,
        )
            .into_response(),
        // Encoding a plain derived `Serialize` cannot fail in practice; if it
        // somehow did we still answer honestly (never a 5xx that the chrome banner
        // would read as a harder enforcement rung).
        Err(err) => Problem::new(500, "challenge_encode", "Challenge encode failed")
            .with_detail(err.to_string())
            .into_response(),
    }
}

/// The challenge document to export.
///
/// The cli assembles the salted-digest + counter challenge for this machine
/// (brief §3/§8) and hands it to the control plane via
/// [`crate::state::LicenceState::with_challenge`] (CONSPECT-10). The control
/// plane only renders it — it never gathers a raw identifier itself (data
/// minimisation). Until the cli supplies one, an empty-but-well-formed challenge
/// is served: zeroed counters except the locally-observable installed-lease
/// count, and NO digests (so the endpoint never fails and never leaks an
/// identifier).
fn challenge_for(state: &AppState) -> ChallengeFile {
    if let Some(challenge) = state.licence.challenge.clone() {
        return challenge;
    }
    let lease_installs = u64::from(state.licence.store.status().is_some());
    ChallengeFile::new(
        String::new(),
        Vec::new(),
        ChallengeCounters::new(0, 0, lease_installs),
    )
}

/// Map a typed [`InstallError`] onto its RFC 9457 problem document.
///
/// The mapping is total and stable (ADR-0050 §11 / the store's own doc): a
/// tampered/forged signature is `422 signature_invalid`; a below-threshold
/// fingerprint is `409 fingerprint_mismatch`; an older (replayed) grant is `409
/// lease_stale`. Every one leaves the active state untouched (fail toward
/// leniency).
fn install_error_problem(err: &InstallError) -> Problem {
    match err {
        InstallError::SignatureInvalid => Problem::new(
            422,
            "signature_invalid",
            "Lease signature verification failed",
        )
        .with_detail("the lease binding's signature did not verify against the pinned issuer key"),
        InstallError::FingerprintMismatch { score, threshold } => {
            Problem::new(409, "fingerprint_mismatch", "Machine fingerprint mismatch").with_detail(
                format!(
                    "fingerprint score {score} is below the {threshold} match threshold; \
                     this machine must re-claim"
                ),
            )
        }
        InstallError::Stale { active, incoming } => {
            Problem::new(409, "lease_stale", "Stale lease grant").with_detail(format!(
                "the presented grant ({incoming}) is older than the active lease ({active}); \
                 the active lease never goes backwards"
            ))
        }
        // `InstallError` is `#[non_exhaustive]`: a future rejection variant maps
        // to a generic `422` rejection (the binding was refused) rather than ever
        // letting an unhandled case 5xx — the active lease still stays put.
        _ => Problem::new(422, "lease_rejected", "Lease binding rejected")
            .with_detail("the presented lease binding was rejected"),
    }
}
