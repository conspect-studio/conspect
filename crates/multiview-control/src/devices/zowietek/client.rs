//! The `zowietek` typed JSON-RPC-over-HTTP client — defensive by design
//! (managed-devices.md §3.1, ADR-M009).
//!
//! The `ZowieBox` family speaks plain HTTP JSON-over-POST:
//! `POST http://<device>/<module>?option=<getinfo|setinfo>&login_check_flag=1`
//! with a JSON body `{"group":…,"opt":…,"data":…}` and a
//! `{"rsp":…,"status":…,"data":…}` response envelope. **Supports `ZowieBox`** —
//! Multiview is independent of and not endorsed by `ZowieTek` (vendor posture).
//!
//! ## The transport seam (socket-free by construction)
//!
//! All client logic sits above an async [`ZowietekTransport`] trait: the real
//! network implementation ([`ReqwestTransport`], behind the off-by-default
//! `zowietek` feature) satisfies it, and a [`ScriptedTransport`] mock satisfies
//! it for unit tests. The default `cargo check` builds the client + its logic
//! and tests it through the scripted mock — no native deps, no sockets.
//!
//! ## Encoded hazards (verified on real units 2026-06-10)
//!
//! * **Per-device request serialization** — every request to one device runs
//!   under a single async mutex so two calls never overlap on the wire (the
//!   device has a documented, unspecified rate limit).
//! * **Lenient numeric status compare** — success is the all-zeros code
//!   (`"00000"` *and* `"000000"`); the human-readable `rsp` text drifts
//!   (`succeed`/`success`) and is **never** the decision input
//!   ([`ZowietekStatus`]).
//! * **Backoff on busy codes** — `00009` (too fast) and `00010` (restarting)
//!   are retried after a capped backoff, not surfaced as a hard failure.
//! * **Empty-body as a distinct protocol error** — some `getinfo` shapes return
//!   an empty body when the group/opt does not fit the current workmode; that is
//!   [`ZowietekClientError::EmptyBody`], never parsed-or-defaulted to success.
//! * **Advisory query verb** — the URL `option=` verb is advisory; the body
//!   `group`/`opt` are authoritative. `login_check_flag=1` rides every URL.
//! * **Reboot without response** — LAN/mDNS/port changes + reboot drop the
//!   socket with no HTTP response; [`ZowietekClient::fire_and_forget`] treats
//!   that as the expected outcome so the caller rides UNREACHABLE→reconnect.
//! * **bps bitrate** — the bitrate newtype ([`BitrateBps`]) locks to bits/sec
//!   with a magnitude guard against kbps-shaped values (the vendor doc is
//!   ambiguous; firmware is bps).

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;

#[cfg(feature = "zowietek")]
pub use net::ReqwestTransport;

pub use scripted::{ScriptedReply, ScriptedRequest, ScriptedTransport};

/// The lowest plausible video bitrate in **bits/sec**. A field below this is
/// almost certainly a kbps-shaped value the doc's unit ambiguity leaked
/// (e.g. `12000` meaning 12 Mbps): rejected, never silently accepted.
const MIN_PLAUSIBLE_BPS: u64 = 100_000;

/// The maximum number of automatic retries on a busy status (`00009`/`00010`)
/// before the call surfaces the busy condition to the caller.
const MAX_BUSY_RETRIES: u32 = 4;

/// The base backoff between busy retries; doubles each attempt, capped.
const BUSY_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// The backoff ceiling between busy retries.
const BUSY_BACKOFF_MAX: Duration = Duration::from_secs(4);

/// A device-reported video bitrate in **bits per second**.
///
/// The vendor doc is ambiguous (parameter tables in kbps, examples in bps);
/// current firmware reports and accepts bps (12 Mbps = `12_000_000`), so this
/// newtype locks the unit and guards the magnitude so a kbps-shaped value cannot
/// be mistaken for a (1000× too small) bps value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BitrateBps(u64);

impl BitrateBps {
    /// Wrap a device-reported field as a bits/sec bitrate, guarding the
    /// magnitude.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError::ImplausibleBitrate`] when `field` is `0` or below
    /// [`MIN_PLAUSIBLE_BPS`] — a value that small is a kbps-shaped figure the
    /// doc's unit ambiguity leaked, not a real bps bitrate.
    pub fn from_field(field: u64) -> Result<Self, ZowietekClientError> {
        if field < MIN_PLAUSIBLE_BPS {
            return Err(ZowietekClientError::ImplausibleBitrate { field });
        }
        Ok(Self(field))
    }

    /// The bitrate in bits per second.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The advisory URL query verb (`option=getinfo` / `option=setinfo`).
///
/// **Advisory only** — the body `group`/`opt` select the operation; this only
/// shapes the URL query string the firmware tolerates either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RpcVerb {
    /// `option=getinfo` — a read.
    GetInfo,
    /// `option=setinfo` — a write.
    SetInfo,
}

impl RpcVerb {
    /// The `option=` query token for this verb.
    #[must_use]
    pub const fn query_token(self) -> &'static str {
        match self {
            Self::GetInfo => "getinfo",
            Self::SetInfo => "setinfo",
        }
    }
}

/// A parsed device status code with **lenient numeric** semantics.
///
/// Built from the firmware's `status` string (and optionally the `rsp` text,
/// which is *never* used for the success decision). The all-zeros code is
/// success in either the five-zero or six-zero form; the rest are classified by
/// their numeric value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZowietekStatus {
    /// The numeric value of the status code (`"00009"` → `9`). `None` when the
    /// code is not numeric (treated as a non-success, non-busy failure).
    numeric: Option<u32>,
    /// The raw status string verbatim (for error reporting / passthrough).
    raw: String,
}

impl ZowietekStatus {
    /// Parse a status code string leniently, ignoring any `rsp` text.
    #[must_use]
    pub fn from_code(code: &str) -> Self {
        Self {
            numeric: code.trim().parse::<u32>().ok(),
            raw: code.to_owned(),
        }
    }

    /// Parse from the `rsp` + `status` pair. The `rsp` text is accepted only for
    /// error reporting; the success/classification decision is the numeric
    /// `status` alone (the text drifts succeed/success and is never branched on).
    #[must_use]
    pub fn from_parts(_rsp: &str, status: &str) -> Self {
        Self::from_code(status)
    }

    /// The raw status string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether this is the all-zeros success code (`"00000"` or `"000000"`).
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.numeric == Some(0)
    }

    /// Whether this is the rate-limit ("operation too fast") code `00009`.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        self.numeric == Some(9)
    }

    /// Whether this is the mid-reboot/"restarting" code `00010`.
    #[must_use]
    pub fn is_restarting(&self) -> bool {
        self.numeric == Some(10)
    }

    /// Whether this is the "workmode not supported" code `00004` — the
    /// management channel answered, but the requested group/opt does not fit the
    /// current workmode (e.g. a decode-table call on an encoder-mode box).
    #[must_use]
    pub fn is_workmode_unsupported(&self) -> bool {
        self.numeric == Some(4)
    }

    /// Whether this code is a transient "busy" condition worth a backoff+retry
    /// (rate-limited or restarting).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.is_rate_limited() || self.is_restarting()
    }
}

/// The raw outcome of one transport round-trip, before status interpretation.
///
/// The transport seam returns this; the client interprets it. An absent body
/// (`None`) is the empty-body hazard; a [`TransportResponse::Dropped`] is the
/// reboot-without-response hazard.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportResponse {
    /// The device answered with a (possibly empty) HTTP body.
    Body(Option<Vec<u8>>),
    /// The socket dropped with no HTTP response — the expected outcome of a
    /// reboot / LAN-change request (fire-and-forget), and an UNREACHABLE signal
    /// for any other request.
    Dropped,
}

/// The transport seam every `zowietek` client logic path runs above.
///
/// Implemented by the real [`ReqwestTransport`] (feature `zowietek`) and the
/// [`ScriptedTransport`] test mock. `post` performs **one** HTTP POST and
/// returns the raw outcome; all retry/serialization/status logic lives in
/// [`ZowietekClient`], above this trait, so it is fully socket-free testable.
///
/// `post` is written as an explicit `-> impl Future<…> + Send` (RPITIT) rather
/// than a plain `async fn`: the supervised **poller actor** is spawned on the
/// multi-thread Tokio runtime ([`tokio::spawn`], which requires `Send`), and it
/// holds the client's request future across `.await` points. A bare `async fn`
/// in a public trait gives **no** `Send` bound on the returned future, so the
/// spawned task would fail to compile; the `+ Send` bound here is the real fix
/// (not an `#[allow(async_fn_in_trait)]` papering over a genuine `Send`
/// problem). It also means the trait carries no auto-trait surprise for callers.
pub trait ZowietekTransport: Send + Sync {
    /// Issue one `POST /<module>?option=<verb>&login_check_flag=1` with `body`
    /// as the JSON request body.
    ///
    /// # Errors
    ///
    /// [`TransportError`] only for a transport-level failure the client cannot
    /// classify (a connection refused before any bytes, a malformed URL). A
    /// reboot-style socket drop is **not** an error — it is
    /// [`TransportResponse::Dropped`], so the caller can treat it as expected.
    fn post(
        &self,
        module: &str,
        verb: RpcVerb,
        body: &Value,
    ) -> impl std::future::Future<Output = Result<TransportResponse, TransportError>> + Send;
}

/// A transport-level failure the client could not classify into a protocol
/// outcome (connection refused before any bytes, a malformed URL).
#[derive(Debug, Error)]
#[error("zowietek transport error: {message}")]
pub struct TransportError {
    /// A human-readable description.
    pub message: String,
}

impl TransportError {
    /// Build a transport error from any displayable cause.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// An error from the typed client (above the transport seam).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ZowietekClientError {
    /// The device returned an empty body — a protocol error distinct from an
    /// HTTP failure (the group/opt did not fit the module or current workmode).
    #[error("device {module:?} returned an empty body (group/opt does not fit the workmode)")]
    EmptyBody {
        /// The module the empty body came from.
        module: String,
    },
    /// The response body was not the expected JSON envelope.
    #[error("device {module:?} returned a malformed response: {detail}")]
    Malformed {
        /// The module the malformed body came from.
        module: String,
        /// The parse detail.
        detail: String,
    },
    /// The device answered with a non-success status code.
    #[error("device {module:?} returned status {status:?} ({detail})")]
    Status {
        /// The module the status came from.
        module: String,
        /// The raw status code string.
        status: String,
        /// A short classification of the status (e.g. `workmode-unsupported`).
        detail: &'static str,
    },
    /// The device stayed busy (`00009`/`00010`) across every retry.
    #[error("device {module:?} stayed busy ({status:?}) across {retries} retries")]
    Busy {
        /// The module that stayed busy.
        module: String,
        /// The last busy status seen.
        status: String,
        /// How many retries were exhausted.
        retries: u32,
    },
    /// The socket dropped with no HTTP response on a request that expected one
    /// (the device is unreachable / rebooting).
    #[error("device {module:?} dropped the connection with no response (unreachable/rebooting)")]
    Unreachable {
        /// The module whose request was dropped.
        module: String,
    },
    /// A transport-level failure surfaced from below the seam.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// A device-reported bitrate field was too small to be a bps value.
    #[error("implausible bitrate field {field} (too small for bits/sec; kbps-shaped?)")]
    ImplausibleBitrate {
        /// The offending field value.
        field: u64,
    },
}

/// A logged-in session: the `uuid` the device returns at login, used only at
/// logout (managed-devices.md §3.1).
#[derive(Debug, Clone)]
pub struct ZowietekSession {
    uuid: String,
}

impl ZowietekSession {
    /// The session uuid (used only at logout).
    #[must_use]
    pub fn uuid(&self) -> &str {
        &self.uuid
    }
}

/// The minimal login response envelope.
#[derive(Debug, Deserialize)]
struct LoginEnvelope {
    status: String,
    #[serde(default)]
    data: Option<LoginData>,
}

/// The `data` block of a login response.
#[derive(Debug, Deserialize)]
struct LoginData {
    #[serde(default)]
    uuid: Option<String>,
}

/// A generic response envelope (`{rsp, status, data}`).
#[derive(Debug, Deserialize)]
struct RpcEnvelope {
    #[serde(default)]
    status: String,
    #[serde(default)]
    data: Option<Value>,
}

/// The typed, defensive client for one device.
///
/// Holds the transport seam, the device credentials, and a **per-device
/// serialization mutex** so all requests to this device are ordered (the
/// device's documented rate limit). Every public call funnels through
/// [`ZowietekClient::request`], which applies the busy-code backoff/retry, the
/// lenient status compare, and the empty-body guard.
pub struct ZowietekClient<T: ZowietekTransport> {
    transport: std::sync::Arc<T>,
    username: String,
    password: String,
    /// Serializes every request to this device (the rate-limit guard). The lock
    /// guards only the wire ordering; it is never held across the engine.
    gate: Mutex<()>,
}

impl<T: ZowietekTransport> ZowietekClient<T> {
    /// Build a client over `transport` for the device, with login credentials.
    #[must_use]
    pub fn new(transport: std::sync::Arc<T>, username: &str, password: &str) -> Self {
        Self {
            transport,
            username: username.to_owned(),
            password: password.to_owned(),
            gate: Mutex::new(()),
        }
    }

    /// Log in: `POST /system` body
    /// `{"group":"account","opt":"login_account","data":{username,password}}`,
    /// keeping the returned `uuid` for logout.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] on a non-success status, an empty/malformed body,
    /// or a dropped socket.
    pub async fn login(&self) -> Result<ZowietekSession, ZowietekClientError> {
        let body = serde_json::json!({
            "group": "account",
            "opt": "login_account",
            "data": { "username": self.username, "password": self.password },
        });
        let bytes = self
            .raw_request(RpcVerb::SetInfo, "system", &body)
            .await?
            .ok_or_else(|| ZowietekClientError::EmptyBody {
                module: "system".to_owned(),
            })?;
        let env: LoginEnvelope =
            serde_json::from_slice(&bytes).map_err(|e| ZowietekClientError::Malformed {
                module: "system".to_owned(),
                detail: e.to_string(),
            })?;
        let status = ZowietekStatus::from_code(&env.status);
        if !status.is_success() {
            return Err(classify_status("system", &status));
        }
        let uuid = env
            .data
            .and_then(|d| d.uuid)
            .ok_or_else(|| ZowietekClientError::Malformed {
                module: "system".to_owned(),
                detail: "login succeeded but returned no uuid".to_owned(),
            })?;
        Ok(ZowietekSession { uuid })
    }

    /// Log out: `POST /system` carrying the session `uuid`.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] on a non-success status or a malformed body.
    pub async fn logout(&self, session: &ZowietekSession) -> Result<(), ZowietekClientError> {
        // `request` wraps `{group, opt, data}`, so `data` is the inner payload
        // (the session uuid the device consumes at logout).
        let data = serde_json::json!({ "uuid": session.uuid });
        self.request(
            session,
            RpcVerb::SetInfo,
            "system",
            "account",
            "logout_account",
            data,
        )
        .await?;
        Ok(())
    }

    /// A `getinfo` call: returns the response `data` value on success.
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] per [`ZowietekClient::request`].
    pub async fn get_info(
        &self,
        session: &ZowietekSession,
        module: &str,
        group: &str,
        data: Value,
    ) -> Result<Value, ZowietekClientError> {
        let opt = group;
        self.request(session, RpcVerb::GetInfo, module, group, opt, data)
            .await
    }

    /// The core request path: build the `{group,opt,data}` body, POST it under
    /// the advisory `verb`, retry on busy codes with a capped backoff, and
    /// interpret the status leniently.
    ///
    /// Returns the response `data` value (or `Value::Null` when the device
    /// returns a success with no `data` block).
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] on an empty/malformed body, a non-success
    /// status, a still-busy device after the retry budget, or a dropped socket.
    pub async fn request(
        &self,
        _session: &ZowietekSession,
        verb: RpcVerb,
        module: &str,
        group: &str,
        opt: &str,
        data: Value,
    ) -> Result<Value, ZowietekClientError> {
        let body = serde_json::json!({ "group": group, "opt": opt, "data": data });
        let mut attempt: u32 = 0;
        loop {
            let bytes = self
                .raw_request(verb, module, &body)
                .await?
                .ok_or_else(|| ZowietekClientError::EmptyBody {
                    module: module.to_owned(),
                })?;
            let env: RpcEnvelope =
                serde_json::from_slice(&bytes).map_err(|e| ZowietekClientError::Malformed {
                    module: module.to_owned(),
                    detail: e.to_string(),
                })?;
            let status = ZowietekStatus::from_code(&env.status);
            if status.is_success() {
                return Ok(env.data.unwrap_or(Value::Null));
            }
            if status.is_busy() {
                if attempt >= MAX_BUSY_RETRIES {
                    return Err(ZowietekClientError::Busy {
                        module: module.to_owned(),
                        status: status.as_str().to_owned(),
                        retries: attempt,
                    });
                }
                let delay = busy_backoff(attempt);
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                continue;
            }
            return Err(classify_status(module, &status));
        }
    }

    /// A fire-and-forget call (reboot / LAN / mDNS / port change): the device is
    /// expected to drop the socket with no HTTP response.
    ///
    /// Returns `Ok(())` both when the device answers a success *and* when the
    /// socket drops (the expected reboot outcome) — the caller rides
    /// UNREACHABLE→reconnect from here. A non-success status that *does* come
    /// back is still surfaced as an error (the command was refused before the
    /// reboot).
    ///
    /// # Errors
    ///
    /// [`ZowietekClientError`] only when the device answers with a non-success
    /// status (the command was rejected, no reboot happened) or a malformed
    /// body — never for the expected dropped socket.
    pub async fn fire_and_forget(
        &self,
        _session: &ZowietekSession,
        module: &str,
        group: &str,
        opt: &str,
        data: Value,
    ) -> Result<(), ZowietekClientError> {
        let body = serde_json::json!({ "group": group, "opt": opt, "data": data });
        let _gate = self.gate.lock().await;
        match self.transport.post(module, RpcVerb::SetInfo, &body).await? {
            // The expected reboot outcome: the socket dropped, ride reconnect.
            TransportResponse::Dropped | TransportResponse::Body(None) => Ok(()),
            TransportResponse::Body(Some(bytes)) => {
                // The device answered before rebooting: a success means the
                // command is in flight; a non-success means it was refused.
                let env: RpcEnvelope =
                    serde_json::from_slice(&bytes).map_err(|e| ZowietekClientError::Malformed {
                        module: module.to_owned(),
                        detail: e.to_string(),
                    })?;
                let status = ZowietekStatus::from_code(&env.status);
                if status.is_success() {
                    Ok(())
                } else {
                    Err(classify_status(module, &status))
                }
            }
        }
    }

    /// One serialized transport round-trip: acquire the per-device gate, POST,
    /// and map the raw outcome into `Some(bytes)` / `None` (empty body) — or an
    /// [`ZowietekClientError::Unreachable`] for a dropped socket on a call that
    /// expected a response.
    async fn raw_request(
        &self,
        verb: RpcVerb,
        module: &str,
        body: &Value,
    ) -> Result<Option<Vec<u8>>, ZowietekClientError> {
        let _gate = self.gate.lock().await;
        match self.transport.post(module, verb, body).await? {
            TransportResponse::Body(Some(bytes)) if bytes.is_empty() => Ok(None),
            TransportResponse::Body(body) => Ok(body),
            TransportResponse::Dropped => Err(ZowietekClientError::Unreachable {
                module: module.to_owned(),
            }),
        }
    }
}

/// Map a non-success, non-busy status into the typed error class.
fn classify_status(module: &str, status: &ZowietekStatus) -> ZowietekClientError {
    let detail = if status.is_workmode_unsupported() {
        "workmode-unsupported"
    } else {
        "device-error"
    };
    ZowietekClientError::Status {
        module: module.to_owned(),
        status: status.as_str().to_owned(),
        detail,
    }
}

/// The capped exponential backoff for retry attempt `attempt` on a busy status.
fn busy_backoff(attempt: u32) -> Duration {
    let factor = 1u64 << attempt.min(16);
    let ms = BUSY_BACKOFF_BASE
        .as_millis()
        .saturating_mul(u128::from(factor));
    let capped = ms.min(BUSY_BACKOFF_MAX.as_millis());
    Duration::from_millis(u64::try_from(capped).unwrap_or(u64::MAX))
}

/// The scripted-transport test mock — drives the client socket-free.
mod scripted {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::Value;

    use super::{RpcVerb, TransportError, TransportResponse, ZowietekTransport};

    /// A scripted reply the mock returns for the next request to a module.
    #[derive(Debug, Clone)]
    pub struct ScriptedReply {
        response: TransportResponse,
        delay: Duration,
    }

    impl ScriptedReply {
        /// A reply carrying a JSON body.
        // The owned `Value` is the ergonomic mock-builder shape callers want
        // (`ScriptedReply::json(json!({…}))`); it is serialized eagerly here, so
        // there is nothing to borrow longer-term.
        #[must_use]
        #[allow(clippy::needless_pass_by_value)]
        pub fn json(value: Value) -> Self {
            let bytes = serde_json::to_vec(&value).unwrap_or_default();
            Self {
                response: TransportResponse::Body(Some(bytes)),
                delay: Duration::ZERO,
            }
        }

        /// A reply with an empty body (the empty-body hazard).
        #[must_use]
        pub fn empty_body() -> Self {
            Self {
                response: TransportResponse::Body(Some(Vec::new())),
                delay: Duration::ZERO,
            }
        }

        /// A reply where the socket drops with no response (reboot hazard).
        #[must_use]
        pub fn socket_dropped() -> Self {
            Self {
                response: TransportResponse::Dropped,
                delay: Duration::ZERO,
            }
        }

        /// Make this reply take `delay` to produce (to exercise serialization).
        #[must_use]
        pub fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }
    }

    /// One request the mock recorded.
    #[derive(Debug, Clone)]
    pub struct ScriptedRequest {
        /// The module path.
        pub module: String,
        /// The advisory verb.
        pub verb: RpcVerb,
        /// The full URL the real transport would hit (for verb/flag assertions).
        pub url: String,
        /// The JSON request body.
        pub body: Value,
    }

    #[derive(Debug, Default)]
    struct Inner {
        /// Per-module FIFO of scripted replies.
        replies: HashMap<String, Vec<ScriptedReply>>,
        /// Every request recorded, in order.
        requests: Vec<ScriptedRequest>,
        /// Per-module reply cursor.
        cursor: HashMap<String, usize>,
    }

    /// A scripted transport mock: queue replies per module, then drive the
    /// client. Records every request for assertions and tracks the maximum
    /// observed in-flight concurrency to prove per-device serialization.
    #[derive(Clone)]
    pub struct ScriptedTransport {
        inner: Arc<Mutex<Inner>>,
        in_flight: Arc<AtomicUsize>,
        max_concurrency: Arc<AtomicUsize>,
    }

    impl ScriptedTransport {
        /// A fresh, empty scripted transport.
        #[must_use]
        pub fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(Inner::default())),
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_concurrency: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Queue `reply` as the next reply for a request to `module`.
        pub fn push(&self, module: &str, reply: ScriptedReply) {
            let mut inner = self.lock();
            inner
                .replies
                .entry(module.to_owned())
                .or_default()
                .push(reply);
        }

        /// The most recent recorded request, if any.
        #[must_use]
        pub fn last_request(&self) -> Option<ScriptedRequest> {
            self.lock().requests.last().cloned()
        }

        /// How many requests were recorded for `module`.
        #[must_use]
        pub fn request_count(&self, module: &str) -> usize {
            self.lock()
                .requests
                .iter()
                .filter(|r| r.module == module)
                .count()
        }

        /// The `/streamplay` requests in the order they were issued (for the
        /// close-before-open assertion).
        #[must_use]
        pub fn streamplay_request_order(&self) -> Vec<ScriptedRequest> {
            self.lock()
                .requests
                .iter()
                .filter(|r| r.module == "streamplay")
                .cloned()
                .collect()
        }

        /// The maximum number of requests ever in flight at once — `1` proves
        /// per-device serialization held.
        #[must_use]
        pub fn max_observed_concurrency(&self) -> usize {
            self.max_concurrency.load(Ordering::SeqCst)
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        /// Take the next scripted reply for `module`, recording the request.
        fn take(&self, request: ScriptedRequest) -> ScriptedReply {
            let mut inner = self.lock();
            let module = request.module.clone();
            inner.requests.push(request);
            let idx = inner.cursor.entry(module.clone()).or_insert(0);
            let cursor = *idx;
            *idx += 1;
            inner
                .replies
                .get(&module)
                .and_then(|r| r.get(cursor))
                .cloned()
                .unwrap_or_else(|| {
                    // No scripted reply: a benign success keeps a test from
                    // hanging while making the missing-script obvious.
                    ScriptedReply::json(serde_json::json!({ "rsp": "succeed", "status": "00000" }))
                })
        }
    }

    impl Default for ScriptedTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ZowietekTransport for ScriptedTransport {
        async fn post(
            &self,
            module: &str,
            verb: RpcVerb,
            body: &Value,
        ) -> Result<TransportResponse, TransportError> {
            let url = format!(
                "http://device/{module}?option={}&login_check_flag=1",
                verb.query_token()
            );
            let reply = self.take(ScriptedRequest {
                module: module.to_owned(),
                verb,
                url,
                body: body.clone(),
            });
            // Track in-flight concurrency around the (possibly delayed) reply so
            // a serialized client never shows >1.
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrency.fetch_max(now, Ordering::SeqCst);
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(reply.response)
        }
    }
}

/// The real reqwest-backed transport — off-by-default (`zowietek` feature).
#[cfg(feature = "zowietek")]
mod net {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use multiview_config::device::net_guard::{self, DialPolicy};
    use serde_json::Value;

    use super::{RpcVerb, TransportError, TransportResponse, ZowietekTransport};

    /// The boxed error type reqwest's DNS resolver returns.
    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// A reqwest DNS resolver that screens every RESOLVED IP against the
    /// outbound-dial SSRF policy (SEC-02, CWE-918). Because reqwest resolves
    /// (and re-resolves, per redirect / reconnect) through this seam, a public
    /// name that answers with an internal address — a DNS-rebind — is refused at
    /// resolution time, so the device poller never dials the cloud-metadata
    /// endpoint or an internal host.
    struct ScreeningResolver {
        policy: Arc<DialPolicy>,
    }

    impl reqwest::dns::Resolve for ScreeningResolver {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let policy = Arc::clone(&self.policy);
            Box::pin(async move {
                // reqwest supplies the host only; the real port is applied by
                // reqwest to the returned IPs, so port 0 here is a placeholder.
                let boxed = |e: std::io::Error| -> BoxError { Box::new(e) };
                let resolved: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                    .await
                    .map_err(boxed)?
                    .collect();
                net_guard::screen_resolved(resolved.iter().map(SocketAddr::ip), &policy)
                    .map_err(|e| -> BoxError { Box::new(e) })?;
                let addrs: reqwest::dns::Addrs = Box::new(resolved.into_iter());
                Ok(addrs)
            })
        }
    }

    /// A live HTTP transport to one `ZowieBox` device, over plain HTTP (the vendor
    /// API is plain HTTP JSON-over-POST; a management VLAN is recommended in the
    /// operator docs since credentials cross the wire unencrypted).
    ///
    /// `reqwest` is built with the rustls TLS backend (never OpenSSL) so the
    /// dependency graph stays native-lib-free; plain HTTP needs no TLS at all,
    /// but the rustls backend keeps an `https://` device address working.
    pub struct ReqwestTransport {
        client: reqwest::Client,
        base: String,
    }

    impl ReqwestTransport {
        /// Build a transport for the device at `base` (e.g.
        /// `http://[fd00:db8::42]`). The base is the scheme+authority; the
        /// module path and query are appended per request.
        ///
        /// The client screens the dial target against `dial_policy` (SEC-02,
        /// CWE-918) on the **actual** address reqwest will dial:
        ///
        /// * An **IP-literal** host — including the alt-encoded IPv4 forms
        ///   (`2130706433`, `0x7f000001`, `0177.0.0.1`) and IPv4-mapped IPv6
        ///   (`[::ffff:169.254.169.254]`) — is parsed with the same WHATWG `url`
        ///   parser reqwest uses, canonicalized to a concrete `IpAddr`, and
        ///   screened here with [`net_guard::screen_ip`]. reqwest dials an
        ///   IP-literal host **directly, without invoking any DNS resolver**, so a
        ///   resolver-only screen never sees these — screening the parsed literal
        ///   on this path is what closes the bypass.
        /// * A **DNS name** is screened lazily by the [`ScreeningResolver`], which
        ///   resolves, screens every answer, and returns **only** the vetted
        ///   addresses reqwest then dials — pinning the vetted result defeats
        ///   DNS-rebind.
        ///
        /// The client also disables any environment proxy
        /// ([`no_proxy`](reqwest::ClientBuilder::no_proxy)) so a system
        /// `HTTP(S)_PROXY` cannot dial the destination unscreened on our behalf,
        /// and follows **no** redirects
        /// ([`redirect::Policy::none`](reqwest::redirect::Policy::none)) — a
        /// validated host cannot 30x-bounce the poller to a blocked one, and the
        /// device's named credentials are never re-sent to a redirect target.
        ///
        /// # Errors
        ///
        /// [`TransportError`] if `base` is not a valid URL/authority, has no host,
        /// resolves to a blocked dial target, or the HTTP client cannot be built.
        pub fn new(
            base: &str,
            timeout: std::time::Duration,
            dial_policy: Arc<DialPolicy>,
        ) -> Result<Self, TransportError> {
            // A device address is either a full `http(s)://` URL or a bare
            // authority (discovery emits `[fd00:db8::42]:5961`, no scheme).
            // Normalize to a URL — default scheme plain http (the vendor API is
            // HTTP JSON-over-POST) — so the SSRF screen and reqwest parse the
            // exact same canonical host, and so `post` always builds a valid URL.
            let normalized = if base.contains("://") {
                base.trim_end_matches('/').to_owned()
            } else {
                format!("http://{}", base.trim_end_matches('/'))
            };
            let parsed = url::Url::parse(&normalized)
                .map_err(|e| TransportError::new(format!("invalid device url {base:?}: {e}")))?;
            match parsed.host() {
                // An IP literal is dialled directly by reqwest (no resolver hop):
                // screen the canonical parsed IP here, fail-closed.
                Some(url::Host::Ipv4(v4)) => {
                    net_guard::screen_ip(std::net::IpAddr::V4(v4), &dial_policy)
                        .map_err(|e| TransportError::new(e.to_string()))?;
                }
                Some(url::Host::Ipv6(v6)) => {
                    net_guard::screen_ip(std::net::IpAddr::V6(v6), &dial_policy)
                        .map_err(|e| TransportError::new(e.to_string()))?;
                }
                // A DNS name is screened + pinned by the ScreeningResolver below.
                Some(url::Host::Domain(_)) => {}
                None => {
                    return Err(TransportError::new(format!(
                        "device url {base:?} has no host to dial"
                    )));
                }
            }
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                // A system HTTP(S)_PROXY must not dial the destination on our
                // behalf — that would bypass the screen entirely.
                .no_proxy()
                .dns_resolver(Arc::new(ScreeningResolver {
                    policy: dial_policy,
                }))
                .build()
                .map_err(|e| TransportError::new(e.to_string()))?;
            Ok(Self {
                client,
                base: normalized,
            })
        }
    }

    impl ZowietekTransport for ReqwestTransport {
        async fn post(
            &self,
            module: &str,
            verb: RpcVerb,
            body: &Value,
        ) -> Result<TransportResponse, TransportError> {
            // login_check_flag=1 rides every URL; the option= verb is advisory.
            let url = format!(
                "{}/{module}?option={}&login_check_flag=1",
                self.base,
                verb.query_token()
            );
            let result = self.client.post(&url).json(body).send().await;
            match result {
                Ok(response) => match response.bytes().await {
                    Ok(bytes) => Ok(TransportResponse::Body(Some(bytes.to_vec()))),
                    // The body never arrived: treat as a dropped response (the
                    // device may be rebooting mid-reply).
                    Err(_) => Ok(TransportResponse::Dropped),
                },
                Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => {
                    // No HTTP response at all — the reboot-without-response /
                    // unreachable hazard. The client rides UNREACHABLE→reconnect.
                    Ok(TransportResponse::Dropped)
                }
                Err(e) => Err(TransportError::new(e.to_string())),
            }
        }
    }
}

/// SSRF dial-path regression tests (SEC-02, CWE-918) for the real reqwest
/// transport, exercised on the **actual connect path** with a real
/// `TcpListener`: a blocked target (loopback / alt-encoded loopback / a name
/// resolving to loopback) must never reach the listener, while an ALLOWED target
/// (a routable interface IP) DOES — so the refusals fail because the screen
/// blocks, not because the transport is inert.
///
/// A prior guard screened only DNS *names* (a custom reqwest resolver), but
/// reqwest dials an IP-literal / alt-encoded URL (`http://2130706433/` =
/// `127.0.0.1`) **without invoking the resolver**, so those bypassed the screen
/// and reached loopback / the cloud-metadata endpoint. These tests exercise the
/// bytes-on-the-wire path a name-only screen cannot see.
#[cfg(all(test, feature = "zowietek"))]
mod dial_screen_tests {
    // Test-only: `expect`/`unwrap` are the standard test vocabulary here (the
    // harness reports a panic as a failure). clippy's allow-*-in-tests does not
    // fire for non-`#[test]` helper fns under a `cfg(all(test, feature = …))`
    // module, so allow explicitly (rule 20).
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};
    use std::sync::Arc;
    use std::time::Duration;

    use multiview_config::device::net_guard::DialPolicy;
    use serde_json::json;

    use super::{ReqwestTransport, RpcVerb, ZowietekTransport};

    /// Drive the real screened transport (built with the non-breaking
    /// `allow_lan()` policy) at `url` and report whether `listener` ever accepted
    /// a connection. For a blocked target the screen refuses before any socket
    /// opens, so the listener stays idle; for an allowed target the POST opens the
    /// socket and the connection lands on the backlog. Either way a single
    /// non-blocking `accept` distinguishes the two.
    fn transport_reaches(url: &str, listener: &TcpListener) -> bool {
        // The non-breaking default policy: allowlistable LAN (private/ULA/carrier)
        // is dialable; the never-legit ranges (loopback/link-local/…) are refused
        // unconditionally — so a refusal is the never-legit floor and a reach is a
        // genuinely allowed dial, not an allowlist quirk.
        let policy = Arc::new(DialPolicy::allow_lan());
        if let Ok(transport) = ReqwestTransport::new(url, Duration::from_millis(500), policy) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            let body = json!({ "group": "account", "opt": "login_account", "data": {} });
            // Ignore the outcome: on a bypass it is the socket landing on the
            // listener that fails the test, not the POST's return value (a
            // connect that hangs maps to Dropped, not an error).
            let _ = rt.block_on(transport.post("system", RpcVerb::SetInfo, &body));
        }
        // Any connection reqwest opened is queued on the listener's backlog by
        // now (the TCP handshake completes at connect time, before the POST times
        // out), so a single non-blocking accept detects it. Count ONLY a
        // successful accept as "reached": a `WouldBlock` means no connection
        // landed, and any *other* accept error (Interrupted/ConnectionAborted/…)
        // is treated as not-reached too — so the positive control fails loud on a
        // spurious error rather than passing vacuously, and a refusal test never
        // reads a stray error as a reach.
        listener.accept().is_ok()
    }

    /// Discover a routable, non-loopback local interface IP — an address the OS
    /// routes to and, being assigned to a local interface, a listener can bind and
    /// a dial to it loops back locally. A `UdpSocket::connect` to a documentation
    /// address (RFC 5737 TEST-NET-1 / RFC 3849) sends **no** packet; it only makes
    /// the kernel pick the source interface it would dial from, which `local_addr`
    /// reveals. That address is a routable unicast IP — private/ULA (allowlistable,
    /// so `allow_lan()` permits it) or globally routable (unblocked) — never
    /// loopback or link-local, i.e. an *allowed* dial target. Every CI runner and
    /// dev host has such an interface.
    fn routable_interface_ip() -> Option<IpAddr> {
        // IPv4 (192.0.2.0/24) first, then IPv6 (2001:db8::/32) for a v6-only host.
        for (bind, probe) in [
            ("0.0.0.0:0", "192.0.2.1:80"),
            ("[::]:0", "[2001:db8::1]:80"),
        ] {
            let Ok(sock) = UdpSocket::bind(bind) else {
                continue;
            };
            if sock.connect(probe).is_err() {
                continue;
            }
            if let Ok(local) = sock.local_addr() {
                let ip = local.ip();
                // Only an address the screen will actually PERMIT is a valid
                // positive-control target. `allow_lan()` refuses loopback,
                // unspecified AND link-local (IPv4 169.254/16, IPv6 fe80::/10) as
                // never-legit; the kernel never picks a multicast/broadcast source.
                // Excluding those leaves private/ULA/CGNAT/global unicast — every
                // one of which `allow_lan()` permits — so a discovered address is
                // guaranteed reachable through the screen (else this test would
                // flake on a host whose egress source is link-local).
                let never_legit = ip.is_loopback()
                    || ip.is_unspecified()
                    || match ip {
                        IpAddr::V4(v4) => v4.is_link_local(),
                        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
                    };
                if !never_legit {
                    return Some(ip);
                }
            }
        }
        None
    }

    /// Bind an idle (never-`accept`ing, non-blocking) listener on `ip:0`,
    /// returning it and its ephemeral port.
    fn idle_listener_on(ip: IpAddr) -> (TcpListener, u16) {
        let listener = TcpListener::bind((ip, 0)).expect("bind idle listener");
        listener
            .set_nonblocking(true)
            .expect("listener set_nonblocking");
        let port = listener.local_addr().expect("listener local_addr").port();
        (listener, port)
    }

    /// Bind an idle loopback listener that never `accept`s on its own.
    fn idle_loopback_listener() -> (TcpListener, u16) {
        idle_listener_on(IpAddr::V4(Ipv4Addr::LOCALHOST))
    }

    #[test]
    fn refuses_loopback_literal_before_connecting() {
        let (listener, port) = idle_loopback_listener();
        // A plain loopback literal: reqwest dials 127.0.0.1 directly (no resolver
        // hop), so a name-only screen misses it. It must be refused.
        assert!(
            !transport_reaches(&format!("http://127.0.0.1:{port}/"), &listener),
            "loopback literal dial reached the listener (SSRF screen bypassed)"
        );
    }

    #[test]
    fn refuses_alt_encoded_loopback_before_connecting() {
        let (listener, port) = idle_loopback_listener();
        // `2130706433` is the 32-bit decimal form of 127.0.0.1: the WHATWG `url`
        // parser reqwest uses normalizes it to the loopback literal and dials it
        // directly. Rust's `IpAddr` parser does NOT recognize this form, so the
        // config-load literal screen misses it — the dial-time screen must catch
        // it on the canonical parsed IP.
        assert!(
            !transport_reaches(&format!("http://2130706433:{port}/"), &listener),
            "alt-encoded (decimal) loopback dial reached the listener (SSRF screen bypassed)"
        );
    }

    #[test]
    fn refuses_named_host_resolving_to_loopback_before_connecting() {
        let (listener, port) = idle_loopback_listener();
        // `localhost` is a DNS *name* (`url::Host::Domain`), not a literal, so it
        // is not screened pre-dial — it exercises the OTHER half of the fix: the
        // `ScreeningResolver` resolves it, screens every answer with
        // `screen_resolved` (fail-closed on the first blocked IP), and returns
        // only vetted addresses for reqwest to dial. `localhost` resolves to
        // loopback (127.0.0.1 and/or ::1 — Linux getaddrinfo returns both), a
        // never-legit range, so the resolver refuses and reqwest never opens the
        // socket. reqwest DOES consult the custom resolver for named hosts
        // (unlike IP literals), so this proves the resolver screen is wired in and
        // defeats a name that answers with an internal address (DNS-rebind).
        assert!(
            !transport_reaches(&format!("http://localhost:{port}/"), &listener),
            "named-host (localhost -> loopback) dial reached the listener (resolver screen bypassed)"
        );
    }

    /// Detection control: prove the idle listener + the non-blocking `accept` that
    /// [`transport_reaches`] relies on actually detect a real connection. Without
    /// it, the three `!transport_reaches` refusals above could pass *vacuously* —
    /// a listener whose `accept` never reported a connection would read as "dial
    /// blocked" even with the screen removed. A raw `TcpStream::connect` (no
    /// screen) lands on the backlog, and the same non-blocking `accept` the screen
    /// tests use must see it.
    ///
    /// This isolates the accept machinery; that a fully-screened transport DOES
    /// reach an allowed listener is proven separately by
    /// `screened_transport_reaches_an_allowed_interface_ip`. Only *loopback* is
    /// refused unconditionally by
    /// [`screen_ip`](multiview_config::device::net_guard::screen_ip) — not every
    /// bindable address — so a routable interface IP is an allowed, reachable
    /// target.
    #[test]
    fn the_non_blocking_accept_detects_a_real_connection() {
        let (listener, port) = idle_loopback_listener();
        // Hold the stream open past the accept so the connection stays established.
        let _client = std::net::TcpStream::connect(("127.0.0.1", port))
            .expect("raw connect to the idle loopback listener");
        assert!(
            listener.accept().is_ok(),
            "the non-blocking accept must detect a real connection (else the screen tests are vacuous)"
        );
    }

    /// Positive control (the panel-asked shape): the SAME real `ReqwestTransport`,
    /// under the SAME `allow_lan()` screen the refusal tests use, DOES reach a
    /// listener bound to an ALLOWED target — proving the three `!transport_reaches`
    /// refusals fail because the screen BLOCKS the target, not because the
    /// transport is inert (a `post` that never dialled would pass every refusal
    /// too, and the detection control above only proves the `accept` side is live).
    ///
    /// Loopback cannot be the allowed target — `screen_ip` refuses it
    /// unconditionally — but a routable non-loopback **interface** IP can: it is
    /// private/ULA (allowlistable, so `allow_lan()` permits it) or globally
    /// routable (unblocked), and being assigned to a local interface a dial to it
    /// loops back to a listener bound there. If a regression makes `post` stop
    /// dialling, ONLY this test goes red; the refusal tests stay green.
    #[test]
    fn screened_transport_reaches_an_allowed_interface_ip() {
        // Fail loud (never a silent skip, rule 19) if the host has no routable
        // non-loopback interface — that is a networkless environment, not a pass.
        let ip = routable_interface_ip()
            .expect("a routable non-loopback interface to bind an allowed dial target on");
        let (listener, port) = idle_listener_on(ip);
        let url = match ip {
            IpAddr::V6(v6) => format!("http://[{v6}]:{port}/"),
            IpAddr::V4(v4) => format!("http://{v4}:{port}/"),
        };
        assert!(
            transport_reaches(&url, &listener),
            "the screened transport did NOT reach an allowed interface-IP listener \
             ({url}) — a refusal test could be passing on an inert transport"
        );
    }
}
