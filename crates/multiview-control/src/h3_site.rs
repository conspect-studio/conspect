//! The control-plane router served over HTTP/3 (front-door slice 2, issue
//! #288, design note `docs/architecture/network-front-door.md` section 5),
//! and the `Alt-Svc` advertisement on the TCP door.
//!
//! [`H3Site`] adapts the SAME axum router the TCP door serves to
//! [`multiview_h3::Site`]: the request body streams on demand, the peer
//! `SocketAddr` is installed as `ConnectInfo` (the SEC-14 per-IP key, exactly
//! as `drive_connection` does for HTTP/1.1), and the router's own layers
//! (`DefaultBodyLimit`, rate and concurrency caps, the setup gate) apply
//! unchanged. The per-connection population cap of the TCP floor
//! (`ConnectionAdmission`) is per TCP connection and is NOT applied here: the
//! h3 endpoint's own caps bound the population (1024 connections, 64 per IPv4
//! address / IPv6 /64, 128 requests in flight per connection).
//!
//! HTTP/3 has no `Upgrade`: an h3 `GET /api/v1/ws` reaches the WebSocket
//! handler without the upgrade handshake and gets axum's clean `4xx`
//! rejection (after the auth gate); the realtime stream stays on HTTP/1.1.
//!
//! [`AltSvc`] is the runtime switch for the `alt-svc: h3=":<port>"` header the
//! TCP door adds to every response ([`alt_svc_layer`]): on only once the h3
//! endpoint is up and UDP is not known unreachable (`[network] alt_svc`, the
//! operator's manual "udp443 unreachable" until the slice-9 prober drives
//! [`AltSvc::set`]).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::Router;
use http_body_util::BodyExt;
use multiview_h3::{BoxFuture, RequestBody, ResponseBody, Site};
use tower::ServiceExt as _;

/// How long a browser may cache the `Alt-Svc` advertisement (24 h).
pub const ALT_SVC_MAX_AGE: u32 = 86_400;

/// The control-plane router as an HTTP/3 [`Site`].
#[derive(Clone)]
pub struct H3Site {
    app: Router,
    shutdown: tokio::sync::watch::Receiver<bool>,
}

impl std::fmt::Debug for H3Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Site").finish_non_exhaustive()
    }
}

impl H3Site {
    /// Serves `app`; once `shutdown` turns `true` (or its sender is dropped)
    /// every new request is answered `503` (requests already running finish
    /// under the endpoint's own bounds).
    #[must_use]
    pub fn new(app: Router, shutdown: tokio::sync::watch::Receiver<bool>) -> Self {
        Self { app, shutdown }
    }

    fn shutting_down(&self) -> bool {
        *self.shutdown.borrow() || self.shutdown.has_changed().is_err()
    }
}

fn into_response_body(body: axum::body::Body) -> ResponseBody {
    body.map_err(Into::into).boxed_unsync()
}

fn unavailable() -> axum::http::Response<ResponseBody> {
    let mut response = axum::http::Response::new(into_response_body(axum::body::Body::empty()));
    *response.status_mut() = axum::http::StatusCode::SERVICE_UNAVAILABLE;
    response
}

impl Site for H3Site {
    fn call(
        &self,
        req: axum::http::Request<RequestBody>,
        peer: SocketAddr,
    ) -> BoxFuture<'static, axum::http::Response<ResponseBody>> {
        if self.shutting_down() {
            return Box::pin(std::future::ready(unavailable()));
        }
        let app = self.app.clone();
        Box::pin(async move {
            let mut req = req.map(axum::body::Body::new);
            req.extensions_mut().insert(ConnectInfo(peer));
            // `Router` is always ready and its error type is `Infallible`.
            match app.oneshot(req).await {
                Ok(response) => response.map(into_response_body),
                Err(never) => match never {},
            }
        })
    }
}

/// The `Alt-Svc` advertisement switch: off until the h3 endpoint is up (and
/// never while the certificate is in `setup`), on only when the operator has
/// not declared UDP unreachable. Lock-free: one atomic load per response.
#[derive(Debug)]
pub struct AltSvc {
    on: AtomicBool,
    value: axum::http::HeaderValue,
}

impl AltSvc {
    /// An `h3=":<port>"; ma=86400` advertisement, initially off.
    #[must_use]
    pub fn new(port: u16) -> Self {
        let text = format!("h3=\":{port}\"; ma={ALT_SVC_MAX_AGE}");
        Self {
            on: AtomicBool::new(false),
            // A port number and fixed ASCII: always a valid header value.
            value: axum::http::HeaderValue::from_str(&text)
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("clear")),
        }
    }

    /// Turns the advertisement on or off (the slice-9 UDP reachability
    /// prober and the endpoint start/stop drive this).
    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Release);
    }

    /// Whether the advertisement is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// The header value to send now, if any.
    #[must_use]
    pub fn header_value(&self) -> Option<axum::http::HeaderValue> {
        self.is_on().then(|| self.value.clone())
    }
}

/// Adds `alt-svc` to every response of `router` while `alt_svc` is on (the
/// TCP door's site; never the plain `[control]` listener).
pub fn alt_svc_layer(router: Router, alt_svc: Arc<AltSvc>) -> Router {
    router.layer(axum::middleware::map_response(
        move |mut response: axum::response::Response| {
            let alt_svc = Arc::clone(&alt_svc);
            async move {
                if let Some(value) = alt_svc.header_value() {
                    response
                        .headers_mut()
                        .insert(axum::http::header::ALT_SVC, value);
                }
                response
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_svc_value_and_switch() {
        let alt = AltSvc::new(443);
        assert!(alt.header_value().is_none());
        alt.set(true);
        assert_eq!(
            alt.header_value().as_ref().and_then(|v| v.to_str().ok()),
            Some("h3=\":443\"; ma=86400")
        );
        alt.set(false);
        assert!(alt.header_value().is_none());
    }
}
