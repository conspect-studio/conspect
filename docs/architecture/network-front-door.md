# Network front door: everything on TCP 443 and UDP 443

Design note for [ADR-0107](../decisions/ADR-0107-single-front-door-443.md). Change class R3: the
front door handles untrusted network traffic in the same process as the output clock (invariants #1
and #10). Governing issue: #280. Epoch: 2026-11-12.

Read the ADR for the decisions and the evidence behind them. This note holds the working detail: the
diagrams, the demux tables, the fallback matrix, the config schema, the test plan and the slice plan.

## 1. What a viewer's network allows, and what each rung needs

| Network the viewer is on | UDP 443 | TCP 443 direct | Only an HTTP CONNECT proxy on 443 |
| --- | --- | --- | --- |
| N1 open internet | yes | yes | n/a |
| N2 corporate egress that allows UDP 443 only (QUIC-friendly) | yes | yes | n/a |
| N3 no UDP at all | no | yes | n/a |
| N4 explicit HTTP proxy, any bytes in the tunnel | no | no | yes, tunnel passes any bytes |
| N5 explicit HTTP proxy that only passes real TLS | no | no | yes, but the first tunnel byte must be 0x16 |
| N6 TLS-intercepting proxy (enterprise MITM CA) | no | no | yes, and it terminates TLS itself |

| Rung | Transport | Needs |
| --- | --- | --- |
| MoQ | WebTransport = HTTP/3 extended CONNECT over QUIC | UDP 443. Browsers never send QUIC through an HTTP proxy |
| WebRTC | ICE: STUN, then DTLS, SRTP and SCTP on the selected pair | UDP host candidate on 443, or ICE-TCP (RFC 6544) on TCP 443, or a TURN relay |
| Frames (stills) | HTTPS GET | anything that loads the site |
| Site, API, WS, SSE, HLS | HTTPS | anything that loads the site. HTTP/3 where UDP 443 passes |

## 2. Shape

```
                      +----------------------------------------------------------------+
 UDP 443 ([::]) ----> | udp-door task (front-door Tokio runtime, own OS threads)        |
                      |  recvmmsg/GRO batch (UdpSocketState, already a moq-noq-udp dep)  |
                      |  per-datagram: first byte -> class (table 3)                     |
                      |  new flows: per-source bucket (v4 / v6 /64, LRU) + global budget |
                      |  try_send into bounded lane queues; full = drop + count          |
                      +-----------+-------------------------------+---------------------+
                                  |                               |
                     quic lane (established 1024, new 256)   rtc lane (established 1024, new 256)
                                  v                               v
          noq::Endpoint::new_with_abstract_socket          UnifiedEndpoint driver (str0m)
          ONE QUIC endpoint, ALPN h3, grease_quic_bit off   existing ufrag/peer demux (Rtc::accepts)
                 |                                                  |
          h3 server connection (hyperium h3 + h3-webtransport)      |
            |-- request (GET/POST/...)  -> axum Router (site, API, HLS)
            |-- extended CONNECT :protocol=webtransport, :path=/moq/... -> moq-net session
                                  |                               |
          sends: straight to the shared socket (GSO via UdpSocketState), never through the door

                      +----------------------------------------------------------------+
 TCP 443 ([::]) ----> | tcp-door accept loop: global cap, per-IP cap, 5 s first-byte deadline
                      |  peek 1 byte:                                                    |
                      |   0x16       -> rustls (ALPN h2, http/1.1)                         |
                      |                 SNI = turn.<host>? -> TURN-over-TLS (slice 8)     |
                      |                 else -> hyper serve loop (ADR-W031 floor) -> Router|
                      |   0x00..0x05 -> ICE-TCP: first frame <= 1535 B (STUN) -> rtc lane  |
                      |   other      -> close                                              |
                      +----------------------------------------------------------------+
 TCP 80  ([::]) ----> 301 to https://<configured host><path> (no body read, 5 s deadline)

 program runtime (output clock task) -- a different Tokio runtime; shares no socket, lock or queue
                                       with any of the above (inv. #1, section 13)
```

Socket ownership: the engine binds both sockets once at startup (or receives them from systemd,
section 9) and owns them for its lifetime. The QUIC endpoint never sees the raw socket. It receives
through its lane queue and sends through a `UdpSender` that writes to the shared socket. The spike
proved this works unchanged with moq-noq 1.3.5 (`AsyncUdpSocket` + `UdpSender`, the same seam
`vendor/moq-tokio/src/noq.rs:572-637` already uses).

## 3. UDP 443 demultiplexer

The first byte decides, per RFC 9443 section 3 (which replaces RFC 7983 section 7):

| First byte | Class | Goes to | Notes |
| --- | --- | --- | --- |
| 0-3 | STUN (also TURN control) | rtc lane | Binding requests must carry the magic cookie (RFC 8489 section 5). str0m parses them only when length >= 20 |
| 4-15 | none | drop | RFC 9443: MUST drop |
| 16-19 | ZRTP | drop | never negotiated |
| 20-63 | DTLS | rtc lane | |
| 64-79 | TURN ChannelData if the source is a TURN server we allocated on, else QUIC short header | TURN client driver, or quic lane | the existing TURN client (`relay_io::classify_inbound`) owns the server addresses |
| 80-127 | QUIC short header (fixed bit set, long-header bit clear) | quic lane | |
| 128-191 | RTP/RTCP | rtc lane | see the qualification below |
| 192-255 | QUIC long header (Initial, 0-RTT, Handshake, Retry) | quic lane | |

Why this works for the traffic that matters: QUIC v1 and v2 (RFC 9369) packets carry the fixed bit
0x40 (RFC 9000 sections 17.2 and 17.3) unless the receiver advertised `grease_quic_bit` (RFC 9287).
RFC 9443 sections 1 and 3 require a multiplexing endpoint not to send that transport parameter.
moq-noq enables greasing by default (`EndpointConfig::grease_quic_bit`, default `true`); the front
door sets it to `false`. Then every v1/v2 long-header packet a client sends us is 0b11xxxxxx
(192-255) and every short-header packet is 0b01xxxxxx (64-127). noq also rejects a packet without
the fixed bit when greasing is off (`moq-noq-proto` `packet.rs`, `"fixed bit unset"`), so a
misclassified datagram cannot become a connection.

Qualification (review finding): the claim is about compliant inbound v1/v2 packets, not every
datagram that calls itself QUIC. A Version Negotiation packet fixes only its top bit (RFC 8999
section 5.3; RFC 9000 section 17.2.1 makes the fixed bit a SHOULD), so a VN with first byte 128-191
would be classified RTP. A server never needs to receive VN (clients send it only to servers that
offered a version they reject, and a server never initiates). A peer that sends v1/v2 packets with
the fixed bit cleared is noncompliant once we have not advertised greasing. Both are dropped (str0m
rejects them as unparsable or as belonging to no session); neither is delivered to QUIC. Slice 1
tests VN, Retry, stateless reset, coalesced packets and v2 packets explicitly.

What is not decided by the first byte, and does not need to be:

* Which WebRTC session owns a STUN/DTLS/RTP datagram: str0m's `Rtc::accepts` (ufrag plus
  MESSAGE-INTEGRITY for STUN, the learned 4-tuple after that), as today (`transport/unified.rs:735`).
* Which QUIC connection owns a datagram: noq's connection-ID table.

### Established flows, new flows, and what an attacker can and cannot change

Every lane has two bounded queues, *established* and *new*, and drains established first.

* **Established** is a set of 4-tuples written **only by the consumers after validation**: the rtc
  lane adds a 4-tuple when str0m reports ICE connected for it (an attacker cannot do this without
  the session's ufrag and password: STUN MESSAGE-INTEGRITY), and the QUIC lane adds a connection's
  remote address once its handshake is confirmed (address validated by the handshake round trip,
  RFC 9000 section 8). Entries are removed when the session or connection ends or migrates. The set
  is therefore bounded by the session caps (`webrtc.max_sessions`, `network.quic.max_connections`),
  never by attacker traffic, and is never cleared wholesale. (The spike's "anyone we sent to" set
  with clear-when-full was a spike shortcut; the reviewers showed a spoofer could flush it. It is
  not the design.)
* **New** is everything else. It passes a per-source token bucket first (spike: 500 datagrams/s,
  burst 200, key = IPv4 address or IPv6 /64). The bucket table is bounded (16 384 entries) with LRU
  eviction, so an attacker who mints many keys evicts their own entries, and legitimate established
  flows never consult it. A global new-flow budget sits on top (a per-second cap on all new-flow
  datagrams), so a spoofed or distributed flood is bounded in userspace work even though no
  per-source key can stop it.
* **What the established path still charges (slice 3, issue #325).** The established set is keyed
  on the remote address, which an off-path spoofer can forge if it knows a viewer's exact IP and
  port. So the bypass is not total: a supported-version Initial always pays the Initial and global
  budgets (VN triggers always pay theirs), every established peer has its own generous token
  bucket (20 000/s, burst 4 000) so one spoofed 4-tuple cannot take the whole established queue,
  and a Version Negotiation shape in the RTP range (first byte 128-191, zero version) is dropped
  from any source that is not established. The residual risk is stated in "Slice 3 as built".

### Stateless replies the endpoint can be made to send

| Reply | Trigger | Control |
| --- | --- | --- |
| QUIC Version Negotiation | long header with an unsupported version | noq answers unconditionally and RFC 9000's 3x anti-amplification limit does **not** apply (no connection exists). The dispatcher reads the version field (bytes 1-4) and only passes unsupported-version packets of at least 1200 bytes (RFC 9000 section 14.1: clients pad Initials) through the new-flow bucket and a global VN budget; the reply is about 60 bytes, so the factor is below 1 |
| QUIC Retry | Initial when more than `retry_above_handshakes` are pending | Retry is smaller than the Initial; the token proves the address on the next Initial |
| QUIC stateless reset | short header for an unknown CID | `EndpointConfig::min_reset_interval` is **one interval for the whole endpoint**, not per peer (moq-noq-proto `endpoint.rs`). The front door sets it explicitly (200 ms) and only lets unknown short headers through the new-flow bucket. A flood can still occupy the interval; the cost is that a client whose connection died (engine restart) learns it at its idle timeout (30 s) instead of at once, and the MoQ client then reconnects. Accepted and stated |
| STUN Binding response | Binding Request | str0m answers only for a live session's ufrag with a valid MESSAGE-INTEGRITY (`ice/agent.rs` `accepts_message`), so a spoofed request draws nothing. Since slice 3 a well-formed STUN message from a non-established source also pays its own per-source (50/s, burst 50) and global (5 000/s) STUN budgets before reaching str0m |

Batching: `UdpSocketState::recv` reads up to 32 messages per syscall with GRO (64 segments on the
spike host). Sends use GSO through `UdpSocketState::send`. These are pure Rust (`moq-noq-udp`,
already in `Cargo.lock`). The existing WebRTC driver reads one datagram per `recvmsg`
(`transport/unified.rs:674-720`) and keeps its per-wake budget (256 datagrams or 2 ms) for the lane.

## 4. TCP 443 demultiplexer

| First byte | Meaning | Handler | Pre-auth bound |
| --- | --- | --- | --- |
| 0x16 | TLS handshake record (ClientHello) | rustls `Acceptor`: read ClientHello, then route: ALPN exactly `acme-tls/1` -> TLS-ALPN-01 challenge responder (RFC 8737, any state); SNI `turn.<host>` -> TURN-TLS; otherwise the site config (`http/1.1`, later `h2`) | ClientHello <= 16 KiB, handshake deadline 10 s |
| 0x00-0x05 | RFC 4571 length prefix (16-bit) whose first frame is <= 1535 bytes: an ICE-TCP connection | rtc lane (`Protocol::Tcp`) | the **first** frame must be <= 1535 bytes and a STUN Binding Request for a live session's ufrag within 5 s, else close. After str0m accepts it, frames up to the RFC 4571 maximum of 65 535 bytes are read into a bounded per-connection buffer (DTLS flights and media may exceed an MTU on TCP, RFC 6544 section 3) |
| anything else (including `G` of a plaintext `GET`) | not ours | close | 1 byte |

Bounds that hold before any authentication: a global connection cap and a per-IP cap enforced at
`accept` (the ADR-W031 `max_connections_per_ip` floor, applied before the first byte is read), a 5 s
deadline for the first byte, a 10 s TLS handshake deadline, the existing 20 s header-read timeout
for HTTP/1, and a 1535-byte cap on the first, unauthenticated ICE-TCP frame. The classifier reads one byte with
`peek`, so it cannot be made to buffer.

HTTP/2. ADR-W031 pinned ALPN to `http/1.1` because hyper's HTTP/2 server has no header-read timeout.
That reason still holds. The front door offers `h2` only once slice 7 adds an HTTP/2 floor:
`max_concurrent_streams`, `max_header_list_size`, a keep-alive PING timeout, and a per-connection
deadline for the first complete HEADERS. HTTP/3 already covers the multiplexing benefit for
browsers that can reach UDP 443. Until slice 7 ships, TCP 443 negotiates `http/1.1`.

TURN over TLS (slice 8). Chrome 149's `turns:` client sends SNI and **no ALPN** (measured, section
11). So a TURN-TLS connection cannot be routed by ALPN. It is routed by SNI: the ICE server URL is
`turns:turn.<site host>:443?transport=tcp`, and the rustls `Acceptor` hands a ClientHello whose SNI
is the TURN name to the TURN server instead of hyper. The certificate must therefore carry the
`turn.<site host>` SAN (RFC 6125 section 6.4): when `[network.turn] enabled = true`, config
validation adds it to `network.tls.domains` and the ACME order covers both names (one challenge per
name, RFC 8555 section 7.1.3). Chrome's no-ALPN behaviour was observed once, on Chrome 149; it is
not assumed for other browsers, which is why routing is by SNI, which every TURN-TLS client must
send to verify the name.

## 5. HTTP/3 for the site and API

One QUIC **endpoint** (one UDP socket, one listener, ALPN `h3`, one certificate) serves both the site
and MoQ. It does not follow that they share one QUIC **connection**. Chromium creates WebTransport
sessions with `allow_pooling = false` by default
([`net/quic/web_transport_client.h`](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/net/quic/web_transport_client.h)),
and the pinned MoQ client sets `allowPooling: false` explicitly (`@moq/net` 0.4.7
`connection/connect.js:295`). So today a MoQ viewer opens a dedicated QUIC connection for each
WebTransport session, beside the page's own HTTP/3 connection to the same origin. The endpoint tells
them apart per connection, after the h3 SETTINGS exchange: an extended CONNECT with
`:protocol = webtransport` (RFC 9220, draft-ietf-webtrans-http3) whose `:path` is under `/moq/`
becomes a MoQ session (`moq-net` accepts any `web_transport_trait::Session`); every other request
goes to the same axum `Router` that serves HTTP/1.1, through a small `h3` -> `http::Request` adapter.

Pooled connections (one connection carrying page requests and WebTransport sessions,
draft-ietf-webtrans-http3 sections 1 and 2.2) must still be handled correctly, because a browser
may pool when it is allowed to. `h3-webtransport 0.1.2` takes the whole `h3::server::Connection` by
value when a session is accepted (`server.rs`, `WebTransportSession::accept`) and keeps it inside
that session, so a second independent session on the same connection cannot be built with its API
(review finding). The front door therefore advertises **one WebTransport session per connection**
(`max_webtransport_sessions(1)`; the WebTransport max-sessions setting tells a pooling client to open
another connection for the next session). On a connection that carries a session, the driver keeps
accepting through the session's `accept_bi`: `AcceptedBi::Request` page requests go to the router,
and any further extended CONNECT is answered `429` with the stream closed, never left hanging.
Several sessions per connection would need a connection-wide session manager (a fork or an upstream
change to h3-webtransport); that is not designed here because no fallback-matrix row needs it. The
spike did **not** prove the pooled case: its driver returned on the first CONNECT, and Chrome's page
load and WebTransport session used separate connections. Slice 2's acceptance: a native client that
pools sends GETs before, during and after one WebTransport session on one connection; a second
CONNECT on it gets `429`; closing the session leaves the connection serving GETs.

The vendored `web-transport-moq` cannot serve the site on that endpoint. Its `Server::accept` treats
the first bidirectional stream as a CONNECT and errors on anything else
(`vendor/web-transport-moq/src/connect.rs` `Connecting::accept`), and its crate docs say: "If you
want to support HTTP/3 on the same host/port, you should use another crate (ex. `h3-webtransport`)"
(`vendor/web-transport-moq/src/lib.rs`). The front door therefore uses hyperium `h3 = 0.0.8`,
`h3-webtransport = 0.1.2` and `h3-datagram = 0.0.2` (MIT, pure Rust) over a noq adapter. The adapter
is `h3-quinn 0.0.10` ported to `moq-noq 1.3.5` with three edits: `read_chunk` takes one argument,
it returns `Bytes` rather than `Chunk`, and `ReadError::IllegalOrderedRead` does not exist. Slice 2
vendors it as `crates/multiview-h3`, with the h3-quinn tests ported and its own fuzz target. A MoQ
session then needs a `web_transport_trait::Session` implementation over
`h3_webtransport::WebTransportSession`.

h3 server settings, explicit (h3 0.0.8 defaults are all off or unbounded, `config.rs`):
`enable_extended_connect(true)` (SETTINGS_ENABLE_CONNECT_PROTOCOL, RFC 9220 section 3),
`enable_datagram(true)` (H3_DATAGRAM, RFC 9297 section 2.1.1, plus the QUIC `max_datagram_frame_size`
transport parameter, RFC 9221), `enable_webtransport(true)`, `max_webtransport_sessions(1)` (above), and `max_field_section_size(16384)` because the default
is `VarInt::MAX` and the QPACK decoder allocates each field before checking the total. A test sends
an oversized header block and expects a stream error, not an allocation.

WebTransport draft version. `h3-webtransport 0.1.2` answers with `sec-webtransport-http3-draft:
draft02` and the pre-draft-07 setting ids; Chrome 149 accepted that in the spike. The current IETF
draft (draft-ietf-webtrans-http3-16) changes settings, flow control and the use of RESET_STREAM_AT.
The design therefore pins what it emits, records the browsers it was measured against, and makes
"WebTransport works in Chrome, Firefox and Safari current releases" a slice 2 acceptance on the
office matrix, not an assumption. A browser that fails the WebTransport handshake falls to WebRTC
by the existing ladder; it is never left without a picture.

nginx cannot carry MoQ: nginx 1.26's `ngx_http_v3_module` is marked experimental, does not proxy
HTTP/3 to an upstream, and has no extended CONNECT / WebTransport
([nginx docs](https://nginx.org/en/docs/http/ngx_http_v3_module.html),
[nginx#1429](https://github.com/nginx/nginx/issues/1429)). A UDP-level `stream` proxy cannot share
UDP 443 with nginx's own HTTP/3 listener either. So the engine owns UDP 443, and for HTTP/3 to
cover the site too, the engine must answer the site's HTTP/3 requests on that endpoint.

Headers and policies:

| Item | Decision |
| --- | --- |
| `Alt-Svc` | `h3=":443"; ma=86400` on every TCP response unless `udp443 = unreachable` (section 10.1). One rule everywhere: `unverified` still advertises, because Alt-Svc is advice the browser verifies per connection (RFC 7838 section 2.4) and a passive proof from one network says nothing about another viewer's network |
| HTTPS DNS record | documented in the runbook (`alpn="h3,http/1.1"`); optional, the engine does not manage DNS |
| 0-RTT | off (`max_early_data_size = 0`). Every API mutation is non-idempotent, and the API is authenticated per request, so the replay risk (RFC 9001 section 9.2, RFC 8470) buys no measurable latency. Revisit only with a measured need and a `425 Too Early` gate for non-GET |
| HSTS | `Strict-Transport-Security: max-age=31536000` in `serving` only. Never in `setup` (section 5a) |
| TCP 80 | 301 to `https://<configured public host><path-and-query>`. Built from config, never from the request `Host` (no open redirect). No other content on port 80; HTTP-01 is not supported |
| TLS-ALPN-01 | the rustls resolver answers `acme-tls/1` with the challenge certificate on TCP 443 (RFC 8737). Same listener, ALPN-selected |
| Certificate | ACME only, through one `CertificateSource` seam (section 5a). Hot reload swaps an `Arc<CertifiedKey>` behind a `ResolvesServerCert` shared by the TCP and QUIC configs; new handshakes pick it up and live connections keep theirs |
| Key log | the vendored server enables `rustls::KeyLogFile` (`vendor/moq-tokio/src/noq.rs:699`). It is inert unless `SSLKEYLOGFILE` is set. The front door does not set `key_log` at all |
| WebSocket / SSE | `/api/v1/ws` stays HTTP/1.1 Upgrade on TCP. HTTP/3 has no Upgrade, and RFC 9220 WebSocket-over-h3 has no browser that needs it here; browsers open the WS over TCP when the page came over h3. SSE `/api/v1/events` works over h3 as an ordinary streaming response. `?ticket=` stays single-use; access logs drop the query string |
| HLS | served by the engine's existing `/hls/{output}/` route. Playlists `max-age=1`, segments immutable (contract already in `deploy/hls-nginx.conf`). Range/206 required. Slice 5 measures TTFB against the nginx path (memory note: nginx adds about 5 ms per segment) with a zero-copy body (file chunks from a bounded pool) |
| Body limits | the existing per-route limits apply unchanged on h3 (`DefaultBodyLimit` on the shared router). Header blocks are capped by the explicit `max_field_section_size(16384)` above |
| Per-IP limiter | keyed on the real peer address for direct connections: QUIC remote address on h3, TCP peer on TCP. Behind a proxy (section 8) it uses the PROXY protocol v2 source, never `X-Forwarded-For` from an untrusted hop |
| Static UI | the `embed-web` build is the default for appliance builds (no web root to deploy). A directory override stays for development |

## 5a. Certificates: TLS is required, and it is ACME

Operator, verbatim, 2026-11-12: "With regard to TLS - we are going to be opinionated. It's
_required_ and it must be ACME. `DNS-01` preferred (as we will use) or `TLS-ALPN-01`." Then "And
later later - a company's own ACME server if they wish to use internal certs", "So, I guess we should
startup with a self generated certificate? And the only thing the user can do is set up their ACME
(unless it's in the config already and we can generate?)", "DNS-01 support - first provider is
Cloudflare - Global Key or Token, and we will add more later", "Okay, so Let's Encrypt, Google and
Custom ACME from day one as you flagged", and, for air-gapped sites, "let's just backlog that for now"
(#282).

**One seam.** Every TLS consumer (TCP 443 rustls, the QUIC endpoint's rustls config, TURN-over-TLS)
takes its certificate from one `CertificateSource`: a current `Arc<CertifiedKey>` plus a change
notification (`tokio::sync::watch`), read by one `ResolvesServerCert`. ACME is the only product
implementation. The self-generated bootstrap certificate is an internal state of the ACME source, not
an operator option. A later "installed certificate" source for air-gapped sites (#282) is an
addition behind the same seam, not a redesign. WebRTC DTLS keeps its per-session self-signed
certificate, with its fingerprint in the SDP: that is the WebRTC protocol (RFC 8827), not an
exception to "TLS is ACME".

**Two front-door states.**

| State | Certificate | TCP 443 serves | UDP 443 | Everything else |
| --- | --- | --- | --- | --- |
| `setup` | self-generated bootstrap (ECDSA P-256, SAN = `network.public_host` only, validity 7 days, a new key on every boot; key 0600 in the state directory, deleted once an ACME certificate lands; SHA-256 fingerprint printed in the log and by `multiview tls status`) | the certificate setup page and its API, `GET /api/v1/tls/status`, the 80 -> 443 redirect. No HSTS | QUIC listener **not started** (browsers neither use Alt-Svc nor WebTransport on a certificate error: Chromium processes Alt-Svc only when `!IsCertStatusError` ([http_network_transaction.cc](https://chromium.googlesource.com/chromium/src/+/main/net/http/http_network_transaction.cc)); WebTransport has no click-through) | UI, API, WS, SSE, HLS, WHEP, MoQ leases, TURN answer `503` with problem type `/problems/tls-setup-required`. Output clock, inputs, outputs (HLS files, SRT, ...) run normally: invariant #1 never depends on a certificate |
| `serving` | an ACME certificate | everything | started; Alt-Svc per section 5 | everything |

`setup` -> `serving` is a hot swap of the resolver with no restart. `serving` never falls back to the
bootstrap certificate.

**Setup authentication.** Reaching port 443 first must not mean owning the engine. The setup surface
requires the admin bearer credential or a one-time setup code printed on the host's log and by
`multiview tls setup-code` (8 random words, valid 1 hour, 5 failed attempts per source per 15
minutes, then 15 minutes locked). The setup page shows the bootstrap fingerprint and asks the operator
to compare it with the browser's before entering anything.

**Secrets never cross the setup page.** On the bootstrap certificate an interceptor between the
operator and the host can read anything typed into the page, and the fingerprint comparison is a
request, not a control (review finding). So the page accepts **no secret**: no Cloudflare token or
global key, no EAB HMAC. Secrets come only from the host: config names environment variables (#168
rule: secrets never in the TOML), or `multiview tls set-credential` (run on the host, talking to the
loopback listener) writes `<state_dir>/secrets/<name>` mode 0600. Never logged, never in an API
response, `Debug` redacted. The setup page shows state and reasons, lets an authenticated operator
choose CA, challenge and domain names (non-secret), and starts issuance against credentials already
stored on the host. `TLS-ALPN-01` needs no secret, so it is the path that can be completed from the
page alone.

**Challenges.**

* `DNS-01` (preferred). Provider interface: `validate_credential()`, `publish_txt(fqdn, value) ->
  Handle`, `confirm_at_authoritative(fqdn, value, deadline)`, `remove_txt(handle)` (always called,
  on every path). The ACME core knows nothing provider-specific. Checked on paper against Route 53
  (`ChangeResourceRecordSets` UPSERT/DELETE, `GetChange` INSYNC) and RFC 2136 dynamic update with
  TSIG: both fit the four calls. Built now: Cloudflare only, with both credential forms: global key
  (`X-Auth-Email` + `X-Auth-Key`) and API token (`Authorization: Bearer`). Token minimum permissions
  `Zone - DNS - Edit` and `Zone - Zone - Read` on the zone
  ([cert-manager Cloudflare guide](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/));
  the runbook recommends the token because the global key can change everything on the account.
  `validate_credential()` runs before any order (token: `GET /user/tokens/verify` then zone lookup;
  key: zone lookup) and reports which permission is missing. Wildcard names need `DNS-01`.
  `confirm_at_authoritative` asks every authoritative nameserver directly on port 53. Some
  networks intercept all outbound port 53 (UDP and TCP) and answer from a hijacking resolver, so
  that check can never pass there. `[network.tls.dns] confirm` (`auto` default, `authoritative`,
  `doh`): `auto` first sends one query to RFC 5737/3849 documentation addresses where nothing
  listens; a reply to that query (a DNS response with the query's id, from a probe address) means
  interception, and confirmation then uses DNS over HTTPS (RFC 8484 GET
  to `cloudflare-dns.com`, through the screened egress client). Trade-off: DoH is a resolver's
  view, not a check of each authoritative server; it is Cloudflare's own resolver, adjacent to the
  authoritative servers of a Cloudflare zone, and the challenge TTL is 60 s. The verdict is
  cached per provider for 10 minutes.
* `TLS-ALPN-01` (RFC 8737). Answered on TCP 443: a ClientHello offering only `acme-tls/1` gets the
  challenge certificate (built with `rcgen`'s `CustomExtension::new_acme_identifier`), in both
  states, before any real certificate exists. It needs the CA to reach TCP 443 of the host.
  Let's Encrypt IP-address certificates (generally available since 2026-01-15, short-lived profile
  only) can be validated only with `http-01` or `tls-alpn-01`
  ([Let's Encrypt, first IP certificate](https://letsencrypt.org/2025/07/01/issuing-our-first-ip-address-certificate),
  [GA post](https://letsencrypt.org/2026/01/15/6day-and-ip-general-availability)): so `TLS-ALPN-01`
  is what makes a host with no DNS name work.
  The ACME service starts before the front door attaches the `acme-tls/1` responder (that happens
  once the control plane is built), and the CA's validator connects as soon as a challenge is set
  ready. So the challenge resolver carries a "responder installed" signal that the door sets after
  installing it, and a TLS-ALPN-01 attempt waits for it (60 s, on the ACME thread) after reaching
  the CA and before ordering; without it the attempt fails as `tls-alpn-responder-not-installed`
  (cadence while waiting: `tests/acme_cadence.rs`
  `clock_keeps_cadence_while_waiting_for_the_alpn_responder`; the late-responder issuance:
  `tests/acme_pebble.rs` `tls_alpn01_waits_for_a_late_responder`).
  After any failed attempt that created an order, its still-`pending` authorizations are
  deactivated (best effort, bounded): Boulder hands an existing pending order for the same account
  and names back on the next new-order request, and a stale one must not be reused. Every failure
  also records a bounded (512 bytes, built in a capped buffer: each CA field at most 200 bytes, at
  most 8 subproblems/challenges/authorizations), sanitized human detail (the CA's problem type, detail,
  subproblems and the challenge's own error), shown as `last_error_detail`.
* `HTTP-01` is not supported.

| Mode | DNS-01 | TLS-ALPN-01 | Reported when unavailable |
| --- | --- | --- | --- |
| `direct` | yes | yes | n/a |
| `behind_proxy` | yes | only if the proxy passes ClientHellos with ALPN `acme-tls/1` through at layer 4 (SNI/ALPN routing in `stream`) | `tls status`: "TLS-ALPN-01 needs TCP 443 to reach the engine; use DNS-01" |
| CA offers neither | n/a | n/a | `setup` with reason `ca-offers-no-supported-challenge` |

**CAs, as data, all three from day one.**

| Profile | Directory (production / staging) | EAB | Notes (read from the live directories, 2026-11-12) |
| --- | --- | --- | --- |
| `letsencrypt` | `https://acme-v02.api.letsencrypt.org/directory` / `https://acme-staging-v02.api.letsencrypt.org/directory` | no | profiles `classic`, `shortlived` (160 h), `tlsserver`; `renewalInfo` (ARI) present |
| `google` | `https://dv.acme-v02.api.pki.goog/directory` / `https://dv.acme-v02.test-api.pki.goog/directory` | required (`externalAccountRequired: true`); key id + HMAC from `gcloud publicca external-account-keys create`, unused secrets expire after 7 days ([Google Cloud docs](https://docs.cloud.google.com/certificate-manager/docs/public-ca-tutorial)) | profiles `minimal`, `standard`; `renewalInfo` present |
| `custom` | operator URL | optional | `trust_anchor` PEM bundle for the directory connection when it chains to a private root; never an "accept invalid certificates" switch. Browsers trust such certificates only where the company installed its root; the product does not work around that (no `serverCertificateHashes` design) |

A profile resolves to directory URL + EAB requirement + trust anchor + preferred ACME profile. The
order flow has no per-CA code. Account credentials are stored per directory
(`<state_dir>/account.<directory-host>-<first 8 hex of sha256(directory URL)>.json`, 0600; the
hash keeps two directories on one host apart), so changing CA never reuses another CA's
account. When the configured CA changes, the current certificate keeps serving until the new CA
issues. No automatic fallback from one CA to another (not asked for; not designed).

**Renewal.** ARI (RFC 9773) when the directory offers `renewalInfo`: renew inside the suggested
window. Otherwise at one third of the lifetime remaining, with jitter. Short-lived profiles make this
a days-scale loop, so the scheduler is a timer task on the control plane, never on a data path.
Failures: keep the current certificate, bounded exponential backoff capped at 6 h and below the CA's
failed-validation limit, an alarm in the UI and log at 50% of remaining lifetime, critical at 25%.
Once the certificate has expired, the front door keeps serving it (browsers show an error; nothing
silently downgrades), `tls status` and the loopback admin listener report `expired`, and recovery
does not need a browser: everything the setup page does is also a CLI command and a config key,
because a host that already has an HSTS entry cannot be clicked through
([RFC 6797 section 12.1](https://www.rfc-editor.org/rfc/rfc6797#section-12.1)).

**Loopback admin listener.** `[control] bind = "[::1]:8080"` stays, plain HTTP, loopback only (a
non-loopback address is rejected at load). It exists for the CLI (`multiview tls status`, setup code,
credential) and for local tests. It is not a way around "TLS is required": nothing outside the host
can reach it.

**Isolated networks.** A host that cannot reach a public CA and its DNS provider stays in `setup`
until it can reach an ACME server. The custom-directory path is in the first TLS slice because the
tests need it (Pebble is a custom directory with a private root), and it serves a company that runs
its own ACME server. It is **not** the answer for an air-gapped site that runs no ACME server: that
is #282 (backlog). `[control.tls] mode = "static"` and the `MULTIVIEW_MOQ_CERTIFICATE`/`_PRIVATE_KEY`
files are today the only way such a site can supply a certificate, so their removal is a separate late
slice that waits for #282's decision.

**Tests.** Pebble (generic flow, both challenges: Pebble validates `TLS-ALPN-01` on a configurable
`tlsPort` and supports `externalAccountBindingRequired` with MAC keys,
[pebble README and test config](https://github.com/letsencrypt/pebble)), plus `pebble-challtestsrv`
for DNS, a private root as `trust_anchor`, and failure injection (credential rejected, TXT never
propagates, CA rate limit `429`/`rateLimited`, CA unreachable). This runs in CI without secrets. A
documented live check against the Let's Encrypt and Google staging directories needs a Cloudflare
token and a Google EAB secret: they live in 1Password and the job runs manually (`workflow_dispatch`),
never on pull requests.

### Slice 5 engine as built (PR A)

Library, behind the `acme` feature of `multiview-control` (forwarded by `multiview-cli`); boot
wiring, the `multiview tls` CLI and the setup routes are PR B (next section).

* Files: `crates/multiview-control/src/acme/{mod,egress,dns,cloudflare,store,engine}.rs`. instant-acme
  0.8.5 is built without its `hyper-rustls` client; the engine supplies the HTTP client. No OpenSSL.
* Egress (CA directory and Cloudflare API): HTTPS only, no proxy, a resolver that resolves once and
  screens every address (reqwest dials exactly those), IP literals screened before sending, at most 3
  redirects each re-screened, connect 10 s / request 30 s, bodies capped at 1 MiB while streaming.
  Refused: loopback, link-local (incl. `169.254.169.254`), RFC 1918, ULA, CGNAT, multicast,
  unspecified, broadcast and IPv4-mapped/compatible forms. `[network.tls] allow_private_directory =
  true` (custom CA only, together with `trust_anchor`) admits RFC 1918 / ULA / CGNAT / loopback for
  the ACME directory client only; link-local, multicast, unspecified and mapped forms stay refused.
  With `trust_anchor`, only that PEM bundle is trusted (opened without following symlinks).
* State (`state_dir`, 0700, files 0600, atomic rename + fsync): `account.<host>-<hash>.json`,
  `active.pem` (key + chain + a `CONSPECT ACME META` section with the non-secret metadata, one file so
  one rename commits all three; the served key swaps only after that commit),
  `pending-txt.json` (DNS-01 ledger: intent before `publish_txt`, handle after, entry removed after
  `remove_txt`; at most 64 entries; replayed by `cleanup_pending()` at start), `secrets/<NAME>` as
  the fallback for every env-var name.
* Cleanup: every attempt runs inside a 10 min timeout. The aftermath runs after the attempt, outside
  that timeout, under its own total cap (60 s): ALPN challenges are removed at once, then published
  TXT records are removed (30 s total, 30 s per call), then, after a failure, the placed order's
  still-pending authorizations are deactivated (30 s total, 10 s per call). A `Shutdown` cuts the
  aftermath short. A `Shutdown` during the attempt runs only ALPN removal and at most 1 s of TXT
  removal, with no CA call. `AcmeHandle` drop still detaches the thread after 2 s, so process
  teardown is never delayed. A TXT record not removed in time stays in the ledger and is retried at
  the next start, which also covers a crash or kill.
* Runtime: `AcmeService` runs on its own OS thread (`acme`) with its own current-thread Tokio runtime;
  commands go through a bounded queue (capacity 4, full = dropped); `CertificateSource::status()` is
  the only read path. `tests/acme_cadence.rs` proves the output clock keeps cadence while the CA
  hangs or refuses. Against an in-process fake CA whose account step succeeds, it also covers three
  more cases: the ACME thread waiting in `wait_responder_ready` (TLS-ALPN-01 responder never
  installed); a failed order being examined (`challenge_errors`, then re-reading the order for
  deactivation); and a deactivation that hangs, cut by the aftermath cap. It also proves that a
  `Shutdown` during a hanging aftermath stops the thread within the drop budget.
* Renewal: ARI window (uniform random instant, `Retry-After` floors the next ARI check) or two thirds
  of the lifetime ±(lifetime/30, at most one day); failures back off 15 min doubling to 6 h, never
  under 60 s. Alarm: while renewal fails, under 50 % of the lifetime left = warning, under 25 % =
  critical; past `notAfter` = expired (still served). `CertificateStatus` carries `alarm`,
  `renewal_due_at`, `next_attempt_at`; an optional hook feeds the alarm surface (PR B: the
  `tls-certificate` health warning).

### Slice 5 setup, boot wiring and CLI as built (PR B)

Operator runbook: [acme-certificates.md](../runbooks/acme-certificates.md).

* The `acme` feature is in the `nvidia`, `apple`, `linux-vaapi` and (through them) `full` presets.
  A config with `[network.tls]` in a build without it is a startup error, and `multiview validate`
  reports it.
* Boot (`crates/multiview-cli/src/tls_boot.rs`) runs before the control plane binds and never on
  the clock path:
  * a bootstrap certificate is created; its key is written 0600 and its fingerprint logged;
  * `AcmeCertificateSource` + `ChallengeResolver` are built;
  * `AcmeService` starts. A stored valid certificate is installed first; otherwise issuance starts
    at once. The bootstrap certificate is rotated every 6 days on the service thread while in
    setup;
  * a one-time setup code is printed on stderr (journal) only, never through `tracing`, so the
    `GET /api/v1/logs` ring never holds it.
* The DNS provider re-reads its credential at the start of every attempt. A credential missing at
  boot keeps the engine in setup (`credential-missing: NAME` in the status reasons), and
  `multiview tls set-credential` + `issue` then completes without a restart.
* Alarms reuse the health-warning surface the UI already shows (`GET /api/v1/health`): new code
  `tls-certificate`, severity warning or critical (expired = critical). It is raised and cleared
  through the engine's drop-oldest event lane by the engine's alarm hook. The `alarm_store` was not
  used: its records are media-timeline X.733 alarms keyed by probes, while health warnings are the
  latched, coalescing, remediation-carrying surface this is.
* `multiview_control::tls_setup` (`acme`, unix):
  * `front_door_router(state)` is the control router behind the setup gate. The slice-4 TCP door
    serves it (see "Slice 4 and slice 5 together as built" below). In `setup`, only these answer:
    * `GET /tls-setup` (a self-contained page with a CSP, the fingerprint and "Issue now", and no
      secret input);
    * `GET /api/v1/tls/status`;
    * `POST /api/v1/tls/setup/session`;
    * `POST /api/v1/tls/issue`;
    * `GET /` → `303 /tls-setup`.

    Everything else is `503` `/problems/tls-setup-required` with `Retry-After: 30`, and HSTS is
    stripped. In `serving` the gate is one read of the source state.
  * Setup codes are 8 BIP-39 words, valid 1 h, single use, compared in constant time. Sessions are
    32 random bytes in an `HttpOnly; Secure; SameSite=Strict` cookie, valid 1 h and only in setup.
    5 failures per source (IPv6 /64) in 15 min lock it for 15 min. The LRU table holds 4096 entries.
  * The admin listener (`[control] listen`, not gated) carries `GET /api/v1/tls/status` and three
    routes that require a loopback TCP peer AND an admin principal: `POST setup-code`,
    `POST credential` (a name referenced by the config, written to `secrets/<NAME>` 0600, 8 KiB
    cap, never logged or echoed) and `POST issue`. The front door never mounts these.
* `multiview tls status | setup-code | set-credential NAME | issue` talk HTTP/1.1 to the loopback
  admin listener; `[::]` is dialled as `[::1]` and any other address is refused. `status` and
  `set-credential` work offline from `state_dir`.
* QUIC: with `[network.tls]` and `network.https_port`, the front-door QUIC lane is taken at start
  and held. A control-plane task waits on `subscribe()` (cancelled at shutdown), then starts MoQ
  with `Runtime::listen_resolver_on` and a `SourceResolver` over the ACME source. This uses a new
  opt-in `resolver` field on vendored `moq-tokio`'s `Listen`, recorded in
  `vendor/MOQ-DELIVERY-PROVENANCE.md`. Until then the MoQ provider is unavailable (honest `503`).
  The `MULTIVIEW_MOQ_CERTIFICATE` files are not used on the front door with `[network.tls]`.
  Without `[network.tls]` nothing changes.
* The setup page does not choose the CA, challenge or names, unlike the "lets an authenticated
  operator choose CA, challenge and domain names" wording above. They come from `[network.tls]`
  only, so the TOML stays the single source of truth and recovery needs no browser. The page shows
  them and starts issuance.
* PR B was written before slice 4 merged; the door wiring below closes that gap.

### Slice 4 and slice 5 together as built (TCP door with ACME)

Slice 4 merged first (#328), so slice 5 PR B wires the ACME source into its TCP door
(`crates/multiview-cli/src/control.rs` `attach_front_door_site`, `frontdoor.rs` `install_site`):

* The TCP door needs `network.mode` (and `https_port`). `[network.tls]` without `mode` has no
  door, and the boot warns that the setup page and TLS-ALPN-01 are unreachable from outside.
* Site TLS (section 4 `site`): `site_server_config` over the ACME `CertificateSource` (ALPN
  `http/1.1`, a `SourceResolver` read per handshake), so `setup` serves the bootstrap certificate
  and `serving` the issued one; an `install` or renewal reaches the next handshake with no
  restart. `[network.tls]` wins over `[control.tls]` for the door; with both, a warning is logged
  and the `[control] listen` listener keeps its own static TLS unchanged.
* `TlsRouting.acme` (section 4 ALPN exactly `acme-tls/1`): `acme_challenge_server_config`, a
  rustls config with ALPN exactly `acme-tls/1` over the `ChallengeResolver`, so TLS-ALPN-01 is
  answered on the door in both states. The `TODO(#291)` is gone.
* The door's router is `tls_setup::front_door_router_with` (the control router plus the HLS
  mounts, behind the setup gate). Without `[network.tls]` the door keeps slice 4's ungated router
  on the `[control.tls]` certificate. The `[control] listen` listener stays the ungated admin
  listener. `router_with` starts the routing relay and the metrics sampler only for the admin
  surface, so building the door router does not start them twice.
* `TlsContext.behind_proxy` follows `network.mode = "behind_proxy"`, so `tls status` reports the
  section 5a TLS-ALPN-01 hint.
* The real client address: the door hands `SiteService::serve` the PROXY v2 source address in
  `behind_proxy` mode (the TCP peer in `direct`), and `ConnectionServer` installs it as
  `ConnectInfo`. The setup-code limiter and session log read that `ConnectInfo`, so they key on
  the real client, never the proxy. The session cookie is `Secure`, which is correct on the door.
* Test: `crates/multiview-cli/tests/tls_door.rs` runs the real front door with a TCP door on
  `[::1]` and a dead CA: bootstrap certificate and the 503 gate, status without HSTS, the RFC 8737
  challenge certificate (acmeIdentifier extension) for an `acme-tls/1`-only hello, the swap after
  `install` and the open gate, and in `behind_proxy` the limiter keyed per PROXY v2 source.

### Changes to #167-#172

| Issue | Stands | Changes |
| --- | --- | --- |
| #167 parent | goal, short-lived profile, ARI | DNS-01 **and** TLS-ALPN-01; three CAs; token and global key; serves the front door, not only the control plane |
| #168 config | env-var-name secrets, provider enum, state_dir | moves to `[network.tls]` (see section 10); adds `ca = "letsencrypt" \| "google" \| "custom"`, `eab_kid_env`/`eab_hmac_env`, `trust_anchor`, `challenge = "dns-01" \| "tls-alpn-01"`, Cloudflare `token_env` as an alternative to `email_env`+`api_key_env`; `static` mode stays until #282 |
| #169 Cloudflare | API usage, authoritative-NS wait, cleanup, redaction | adds the token form and `validate_credential()`; the trait follows the four-call interface above |
| #170 engine | instant-acme 0.8, per-directory account, atomic store, backoff, idempotent restart | TLS-ALPN-01 challenge selection, EAB, `trust_anchor` HTTP client, setup/serving state machine and the bootstrap certificate |
| #171 hot reload | resolver swap, engine never waits for ACME | the resolver is the front door's single `CertificateSource`, shared by TCP, QUIC and TURN; QUIC listener starts on `serving` |
| #172 docs/CI | runbook, Pebble job | runbook covers all three CAs, both challenges, CAA (`validationmethods=dns-01,tls-alpn-01`), the setup flow and recovery by CLI; the ADR is this one (ADR-0107) plus no separate TLS-1 ADR |

Missing from the old issues and added here: the setup state and its authentication, TLS-ALPN-01,
EAB and private trust anchors, Google, token credentials, CLI recovery, and the QUIC listener's
dependency on a real certificate.

## 6. WebRTC on 443

* Host candidates on UDP 443 from the shared socket. `host_candidates()` already derives the port
  from the socket's local address (`transport/mod.rs:890`), so moving the socket moves the
  candidates. The advertised public addresses stay `webrtc.advertised_addresses`, moved to
  `[network.advertise]`.
* ICE-TCP passive candidates on TCP 443: `Candidate::builder().tcp().host(addr).tcptype(Passive)`.
  str0m 0.16.2 supports `Protocol::Tcp` with RFC 4571 framing; the framing is the caller's job
  (`str0m` `io/mod.rs`: "TCP (See RFC 4571 for framing)"). Measured: Chrome 149 connects via ICE-TCP
  to the shared TCP port in 42-54 ms, directly and through a plain CONNECT proxy (section 11).
* Firefox gathers active ICE-TCP candidates and connects to passive server candidates since
  Firefox 54 ([Mozilla blog](https://blog.mozilla.org/webrtc/active-ice-tcp-punch-firewalls-directly/)).
  Safari: unverified. LiveKit reports Safari failures over TCP-only paths
  ([livekit#2483](https://github.com/livekit/livekit/issues/2483)). The office matrix (section 12)
  measures Safari on mst001 before slice 4 is closed.
* `Protocol::SslTcp` (Chromium's pseudo-TLS framing) is not used. It looks like TLS on the wire, but
  it is a fixed fake handshake that a TLS-validating proxy rejects. Real TLS (TURN-TLS, section 7)
  is the answer for N5/N6.

## 7. Built-in TURN: the reasoning

The engine is the media peer and has a public address. Host-UDP-443 and ICE-TCP-443 then reach
it from every network where the client can open a socket to the engine's address directly, which
covers N1, N2 and N3. A classic TURN server adds nothing there. The relay would be the engine
relaying to itself.

| Network | Host UDP 443 | ICE-TCP 443 | TURN over TLS 443 (built in) | Evidence |
| --- | --- | --- | --- | --- |
| N1, N2 | works | works | not needed | spike, Chrome 149 |
| N3 no UDP | no | works | not needed | spike: ICE-TCP only, Chrome 149, 42-54 ms |
| N4 CONNECT proxy, any bytes | no | works in Chrome (measured): Chromium sends P2P TCP through the proxy; other browsers unverified | not needed | spike: Chrome 149 behind a CONNECT proxy, DC open, 50/50 echo, both `default` and `disable_non_proxied_udp` policies; Chromium `services/network/p2p/socket_tcp.cc` uses `ProxyResolvingClientSocketFactory` |
| N5 CONNECT proxy, TLS only | no | **fails**: first tunnel byte 0x00 | works (Chrome 149's `turns:` reached the server through the TLS-only proxy as a real ClientHello) | spike: TLS-only proxy dropped ICE-TCP, `ice=disconnected`; passed `turns:` |
| N6 TLS-intercepting proxy | no | fails | fails unless the MITM proxy passes the TURN SNI through. TURN has no HTTP semantics for it to inspect | reasoning; unverified |
| Engine behind NAT with no port forward | needs the existing TURN client to an external TURN server | same | n/a | existing client `crates/multiview-webrtc/src/turn/` |
| Firefox with ICE-TCP disabled (`media.peerconnection.ice.tcp=false`) behind no-UDP | no | no | works with `turns:` | Firefox pref; Mozilla blog above |

Decision: a built-in TURN-over-TLS server is needed for N5 and for clients with ICE-TCP disabled,
and only those. It is the last WebRTC rung, slice 8. It is minimal. It relays only to the engine's
own WebRTC endpoint, in process, and is never a general relay:

* Transport: TLS on TCP 443, routed by SNI `turn.<host>` (Chrome sends no ALPN). TURN over UDP 443
  is **not** built: every network that passes UDP 443 already reaches the host candidate, so a UDP
  TURN allocation would only add a hop.
* RFC 8656 subset: Allocate (REQUESTED-TRANSPORT UDP), Refresh, CreatePermission, Send/Data
  indication, ChannelBind, and ChannelData over the TLS stream (RFC 8656 section 12 framing; the
  TCP-relay extension RFC 6062 is not needed because the relayed leg is UDP-shaped and in process). The allocation has no real relayed socket. The `XOR-RELAYED-ADDRESS` is a
  synthetic address that the rtc lane recognises, and CreatePermission is accepted only for the
  engine's own host candidates. The relay path is an in-process queue into the rtc lane, the same
  shape as the existing relay-candidate handling (`transport/relay_io.rs`).
* Auth: the long-term credential mechanism (RFC 8489 section 9.2, RFC 8656 section 7.2). An
  unauthenticated Allocate gets `401` with REALM and a stateless NONCE (HMAC of time and the TCP
  peer address); every later request carries USERNAME, REALM, NONCE and MESSAGE-INTEGRITY, verified
  before any state exists; a stale nonce gets `438` and the client retries (RFC 8489 section
  9.2.5). FINGERPRINT is verified when present and not required (RFC 8489 section 14.7). Refresh,
  CreatePermission and ChannelBind are authenticated the same way. The nonce binds the TCP peer
  only; it does nothing against a stolen credential, which the lease binding below handles.
* Credentials: REST ephemeral usernames `expiry:lease-id`, password HMAC-SHA1 over a per-boot secret
  (the scheme `turn/auth.rs` implements on the client side). `expiry` is the preview lease's own
  expiry, not a fixed 600 s, and the credential is valid only while that lease is live. It reaches
  the browser in the WHEP lease response, which is authenticated and never logged with its body.
  A leaked credential is usable only until the lease ends, only for one allocation, and only towards
  the engine's own media endpoint.
* Proof required for slice 8: real Chrome and Firefox perform Allocate, get a relay candidate, and
  play media through it (the spike only observed the ClientHello).
* Per-object authorization: the username binds the allocation to one authenticated preview lease
  (WHEP session id). CreatePermission and ChannelBind for any peer except that lease's server
  candidates is rejected with 403. The allocation dies with the lease.
* Quotas: one allocation per lease, at most 4 per source IP, global cap equal to
  `webrtc.max_sessions` (default 64). Allocation lifetime is the lease's remaining lifetime, capped
  at 600 s (RFC 8656 default); a Refresh extends it only while the lease is live, and a lease
  refresh is what makes a longer allocation possible.
* ICE servers handed to the browser: the WHEP lease response carries
  `iceServers: [{urls: "turns:turn.<host>:443?transport=tcp", username, credential}]` only when
  slice 8 is enabled. `WhepPlayer` already accepts an `rtcConfig` (`web/src/preview/WhepPlayer.tsx:108`).

Codec reuse: `crates/multiview-webrtc/src/turn/message.rs` already encodes and decodes every method
and attribute above (Allocate, Refresh, CreatePermission, ChannelBind, Send, Data,
MESSAGE-INTEGRITY, FINGERPRINT, XOR-RELAYED-ADDRESS). Slice 7 adds a server role and fuzz tests over
the same codec. It does not fork it.

## 8. Behind-a-proxy mode

Some deployments keep a reverse proxy (nginx, Caddy, a cloud load balancer) on TCP 443. That stays
supported as `[network] mode = "behind_proxy"`:

* The proxy owns TCP 443 and forwards to the engine's TCP listener on `[::1]:8443` (or a Unix
  socket). The engine trusts PROXY protocol v2 from a configured `trusted_proxies` list only, and
  takes the client address from it for the per-IP limiter and logs (fixes the "every client is ::1"
  fact). It fails closed: a connection without a valid PROXY v2 header, or from a source not in
  `trusted_proxies`, is closed before any byte of HTTP is read. Today's limiter fails open with a
  one-time warning when `ConnectInfo` is missing (`crates/multiview-control/src/limits.rs:591`);
  `behind_proxy` mode must not inherit that.
* UDP 443 is forwarded untouched to the engine (a plain DNAT or a UDP port forward, never an
  HTTP/3-terminating proxy), so MoQ and site HTTP/3 keep working.
* `Alt-Svc` is still sent by the engine. The proxy must pass it through.
* ICE-TCP and TURN-TLS cannot pass through an HTTP reverse proxy. In this mode they are off unless
  the proxy forwards a dedicated TCP port at layer 4 (`stream` in nginx) to the engine's front-door
  listener. That is documented, and the capability report says which rungs are available.

## 9. Privilege, platforms, restarts

| Target | How 80/443 are bound |
| --- | --- |
| Linux systemd (mda001) | systemd socket activation: `conspect.socket` with `ListenStream=[::]:443`, `ListenStream=[::]:80`, `ListenDatagram=[::]:443` twice with `ReusePort=yes` (the validated-flow steering pair, as built in slice 6), `BindIPv6Only=both`, `ReceiveBuffer=8M`; sockets are matched by type and port, so no `FileDescriptorName=` is needed. The service keeps `User=operations` and needs no capability. Fallback: `AmbientCapabilities=CAP_NET_BIND_SERVICE` when started without the socket unit |
| Restarts | with socket activation the sockets survive an engine restart: the kernel queues TCP SYNs (backlog) and UDP datagrams (rcvbuf) while the engine restarts, so a browser's retry lands on the new process instead of a refused port. QUIC connections still die with the process (keys are in memory); clients reconnect |
| Container | `--cap-add NET_BIND_SERVICE` is not needed in Docker when the container runs as root inside its namespace. Rootless: `net.ipv4.ip_unprivileged_port_start=443` in the container sysctls, or map host 443 to 8443. `deploy/compose.yaml` publishes `443/tcp`, `443/udp`, `80/tcp` |
| macOS | reported to allow unprivileged binds below 1024 since macOS 10.14 (unverified: no primary Apple source found); launchd `Sockets` can pass the sockets in either way. The office matrix checks it on mst001 before slice 4 closes |
| Windows | no privileged-port restriction; the Windows Firewall needs inbound rules for TCP 443/80 and UDP 443 (the installer adds them) |

## 10. Config schema and migration

```toml
[network]
mode = "direct"                  # "direct" | "behind_proxy"; absent = no TCP listener (slice 4)
public_host = "mda001.example.net"  # used for the 80->443 redirect, Alt-Svc authority, TURN SNI
https_port = 443                 # TCP and UDP
tcp_port = 443                   # as built (slice 4): the direct-mode TCP port, default https_port
http_redirect_port = 80          # 0 disables the redirect listener
bind = "[::]"                    # IPv6-first; an IPv4-only address is rejected at load
advertise = ["2404:db8::10", "203.0.113.10"]  # public addresses for ICE candidates (was webrtc.advertised_addresses)
trusted_proxies = []             # PROXY v2 sources (behind_proxy mode)
proxy_listen = "[::1]:8443"      # behind_proxy mode TCP listener

workers = 2                      # OS threads of the front door's own Tokio runtime (never the program runtime)

[network.udp]
recv_buffer_bytes = 8_388_608    # SO_RCVBUF request; startup warns when the kernel clamps it (rmem_max)
new_flow_pps_per_source = 500
new_flow_burst = 200
new_flow_global_pps = 50_000     # cap on all unvalidated datagrams per second
validated_steering = true        # Linux: SO_REUSEPORT + BPF, validated flows on their own queue

[network.quic]
retry_above_handshakes = 8       # send Retry (address validation) once this many handshakes are open
max_connections = 1024
idle_timeout_secs = 30
min_reset_interval_ms = 200      # endpoint-wide (noq), not per peer

[network.ice_tcp]
enabled = true

[network.turn]
enabled = false                  # slice 8; true once TURN-TLS ships
allocations_per_source = 4

[network.reachability]
probe_url = ""                   # optional external probe hook (section 10.1)

# --- certificates (section 5a). Secrets are never in the TOML: only env-var NAMES. ---
[network.tls]
ca = "letsencrypt"               # "letsencrypt" | "google" | "custom"
staging = false                  # use the profile's staging directory
challenge = "dns-01"             # "dns-01" | "tls-alpn-01"
domains = ["mda001.example.net"] # SANs; IP literals allowed with tls-alpn-01 (Let's Encrypt short-lived)
contact_email = "ops@example.net"
acme_profile = "shortlived"      # used only when the directory advertises it
state_dir = "/var/lib/conspect/tls"  # 0700; account.<host>-<hash>.json, active.pem, pending-txt.json, bootstrap.key, secrets/
[network.tls.dns]
provider = "cloudflare"
token_env = "CLOUDFLARE_API_TOKEN"   # Zone:DNS:Edit + Zone:Zone:Read (recommended)
# or the global key form:
# email_env = "CLOUDFLARE_EMAIL"
# api_key_env = "CLOUDFLARE_API_KEY"
propagation_timeout_s = 120
```

The other two CA profiles, each shown with the other challenge:

```toml
# Google Trust Services, TLS-ALPN-01 (needs TCP 443 reachable by the CA; no DNS credential)
[network.tls]
ca = "google"
challenge = "tls-alpn-01"
domains = ["mda001.example.net"]
eab_kid_env = "GTS_EAB_KID"
eab_hmac_env = "GTS_EAB_HMAC"
state_dir = "/var/lib/conspect/tls"

# A company's own ACME server, DNS-01 through Cloudflare
[network.tls]
ca = "custom"
directory = "https://acme.corp.example/acme/directory"
trust_anchor = "/etc/conspect/corp-root.pem"   # PEM bundle for the directory connection
# allow_private_directory = true               # only if the directory is on loopback / RFC 1918 /
#                                              # ULA / CGNAT (or `localhost`); needs trust_anchor
challenge = "dns-01"
domains = ["mda001.corp.example"]
eab_kid_env = "CORP_EAB_KID"                   # optional
eab_hmac_env = "CORP_EAB_HMAC"
state_dir = "/var/lib/conspect/tls"
[network.tls.dns]
provider = "cloudflare"
token_env = "CLOUDFLARE_API_TOKEN"
```

(Google with DNS-01 and Let's Encrypt with TLS-ALPN-01 are the same keys with the `challenge` and
`[network.tls.dns]` lines swapped. The runbook carries all six combinations.)

Legacy settings and their fate (deprecation warns at load for one release, then rejected):

| Old | New | Behaviour after migration |
| --- | --- | --- |
| `webrtc.udp_port` (8189) | `network.https_port` | if set to a non-443 value, the engine binds a second, legacy WebRTC-only socket on it for one release and warns |
| `webrtc.advertised_addresses` | `network.advertise` | copied |
| `MULTIVIEW_MOQ_BIND` | none | warned for one release while it still selects a legacy MoQ socket, then rejected; MoQ rides the front door |
| `MULTIVIEW_MOQ_ENDPOINT` | derived: `https://<public_host>/moq` | warned for one release, then rejected |
| `MULTIVIEW_MOQ_CERTIFICATE`, `_PRIVATE_KEY` | the front door's ACME `CertificateSource` | honoured (warned) until the late removal slice, which waits for #282 |
| `[control.tls] mode = "static"` | the same | stays until #282 is decided; removal is its own late slice |
| `[control.tls] mode = "acme"` (#168 draft) | `[network.tls]` | never shipped; the issue is re-cut to the new section |
| `MULTIVIEW_MOQ_PROTOCOL` | stays (moq-transport-16/18) until the draft settles | unchanged |
| `[control] bind = "[::1]:8080"` | stays as the loopback admin/API listener; the front door is separate | unchanged |

### 10.1 Reachability self-test and capability reporting

Today's failure (#280) was invisible for days: the engine advertised MoQ on a port the estate firewall
dropped. The front door makes reachability a measured capability:

* Self-probe at startup and every 10 minutes: the engine sends a QUIC Version Negotiation probe (the
  same packet `lead/quicvn.py` sends) and a TCP connect to each `network.advertise` address on 443
  **from inside**. This only proves the local bind and hairpin; it cannot see an upstream firewall.
* External proof, passive: each QUIC handshake completed from a non-local, non-advertised source
  address updates `udp443_seen_external = <timestamp>`. Same for TCP and ICE-TCP.
* State and decay: `reachable` while the last external proof (passive or active) is younger than
  24 h; then back to `unverified`. `unreachable` only from an active external probe that failed
  (or the operator's probe hook says so), and it stays until a later proof succeeds. A host with no
  viewers for a day therefore reads `unverified`, which is honest; the probe hook is what turns it
  into a fact.
* External proof, active (optional): `network.reachability.probe_url`, an operator-supplied HTTPS
  endpoint that sends the VN probe back from outside and returns the verdict.
* `GET /api/v1/capabilities` gains `network: {tls_state: "setup" | "serving", tls_expires_at,
  udp443: "reachable" | "unverified" | "unreachable", tcp443, ice_tcp, turn_tls, last_external_quic, nft_floor: "present" | "absent" | "unknown"}`,
  also pushed on the realtime channel (`/api/v1/ws`, SSE fallback) when it changes. The UI shows a
  banner when `udp443` is not `reachable` within 15 minutes of startup. `Alt-Svc` and the MoQ rung are offered only when
  `udp443 != "unreachable"`.

## 11. Spike evidence (2026-11-11, scratch only, deleted from the tree)

Program: a scratch crate with moq-noq 1.3.5, h3 0.0.8 + h3-webtransport 0.1.2 over the ported
adapter, str0m 0.16.2, rustls 0.23 (ring). Client: Chrome for Testing 149.0.7827.55 (puppeteer),
curl 8.14.1 (nghttp3), and a native `web-transport-moq` client. Host: build container, 56 cores,
loopback.

| # | What ran | Result |
| --- | --- | --- |
| S1 | curl `--http3-only` GET on the shared UDP port | `HTTP=3 200`, 8 ms |
| S2 | native WebTransport, 500 echo streams | setup 3.8 ms, p50 242 µs, p99 2.8 ms |
| S3 | Chrome: page over h3, WebTransport `/echo`, RTCPeerConnection signalled over WT `/rtc`, all on one UDP port (one endpoint; separate QUIC connections: the spike driver did not serve requests on a WebTransport connection) | `nextHopProtocol=h3`; WT ready 5.7-20.6 ms; ICE+DTLS+SCTP open 23-49 ms; 400/400 WT echoes and 400/400 DC echoes interleaved; 3/3 runs; IPv4 and IPv6 |
| S4 | one 200-byte datagram for every first byte 0-255 | STUN 4, DTLS 44, RTP 64, QUIC 128 (16 in 64-79), dropped 16 (4-15, 16-19); 0 delivered to the wrong stack: all non-session datagrams rejected by str0m, noq created 0 connections |
| S5 | 600k pps single-source flood for 12 s (garbage, supported-version QUIC Initial shape, random short header, STUN, RTP). Single source only; the spike's established-flow set was "anyone we sent to" with clear-when-full, which review showed a multi-source spoofer could flush; the design replaces it (section 3) | per-source bucket dropped 99.7%; noq `incoming` stayed 1 (random Initials allocate nothing); RSS flat 9.4-12 MB; an established WT session kept 1000/1000 echoes (p50 229-269 µs, p99 1.7-46 ms, max 1.4-4.5 s) |
| S6 | same flood, kernel counters | `Udp RcvbufErrors` +2.83 M of 3.80 M sent in 6 s: the kernel queue (rmem_max 212 992) overflows before userspace sees the flood, so new handshakes took 1.2-13 s during a flood and established sessions saw stalls |
| S7 | 60 Hz output-clock thread in the same process | late > 2 ms: 15-42 per 15 s idle vs 17-80 under flood; max lateness 4.6-8.8 ms idle vs 7.5-23.7 ms under flood. No shared lock or queue; CPU contention only, on a loaded shared container host (load ~75 on 56 cores) |
| S8 | Chrome WebRTC with only a `tcptype passive` candidate on the shared TCP port; TLS on the same port | DC open 42-54 ms, 100/100 echo, 2/2 runs, v4 and v6; `openssl s_client` on the same port classified as TLS (0x16) |
| S9 | Chrome behind an HTTP CONNECT proxy | ICE-TCP through the proxy: DC open, 50/50 echo (policies `default` and `disable_non_proxied_udp`); through a TLS-only proxy: tunnel first byte 0x00, dropped, ICE fails |
| S10 | Chrome `turns:` client hello, direct and through the TLS-only proxy | first byte 0x16, SNI present, **no ALPN extension**; passes the TLS-only proxy |

What S6 and S7 mean for the design:

* **The kernel queue is shared, and userspace cannot protect it.** A line-rate flood fills the one
  UDP socket's receive queue and the kernel drops paying viewers' packets before the dispatcher sees
  anything. Three layers, in order of strength:
  1. **Separate queues for validated flows.** Once a WebRTC 4-tuple is ICE-connected or a QUIC
     connection is confirmed, its packets are steered to a second socket in the same
     `SO_REUSEPORT` group by a classic BPF program keyed on the established source addresses
     (built in slice 3: CBPF, not eBPF, see "Slice 3 as built"). A flood
     of unvalidated traffic then fills the listener queue, not the one paying viewers use. This is
     the only layer that also works against spoofed and distributed floods. Linux only; other
     platforms keep one queue (stated as a platform limit).
  2. **Pre-socket per-source limits** (nftables `meter` on UDP 443 for packets not in conntrack
     state `established`). Required before UDP 443 is opened to the internet, and checked by the
     engine at startup where it can read the ruleset (a warning in the capability report when
     absent). Honest limit: it stops a real source, not a spoofed one (it depends on BCP 38 at the
     attacker's network) and not a botnet.
  3. **Buffer sizing.** systemd `ReceiveBuffer=8M` (`SO_RCVBUFFORCE`, which bypasses `rmem_max`);
     the engine itself tries `SO_RCVBUFFORCE` first (works with `CAP_NET_ADMIN`), falls back to
     `SO_RCVBUF`, reads the result back and warns when the usable size is below the request.
  Beyond these, a volumetric attack is an upstream (network/DDoS-provider) problem; the engine's job
  is that it cannot be turned into an amplifier and that its own state stays bounded.
* **Output clock exposure is real and must be designed out, not explained away.** The production
  tick loop is **not** on a dedicated thread today: `ProgramSet` runs each program with
  `tokio::spawn` (`crates/multiview-engine/src/programset.rs:546`), `RealtimePacer` waits with
  `tokio::time::sleep` (`crates/multiview-engine/src/runtime.rs:125`), and both run on the
  `#[tokio::main]` multi-thread runtime (`crates/multiview-cli/src/main.rs:124`), the same runtime
  that serves the control plane. A front door on that runtime could occupy the workers the tick task
  needs. S7 measured exactly this kind of exposure (a separate std thread, so CPU contention only,
  and lateness still grew from 4.6-8.8 ms to 7.5-23.7 ms max on a host at load ~75). Decision: the
  front door (UDP dispatcher, QUIC endpoint, h3 and router handlers it serves, TCP door, TURN) runs on
  **its own Tokio runtime** with a fixed, small worker count (`network.workers`, default 2), started
  on its own OS threads. It never runs on the runtime that drives programs. Slice 1 creates that
  runtime. Slice 3's chaos suite runs the engine's real clock with a flood running and fails on any
  repeat or slip that a whole-process stall does not explain (the stall witness, issue #321). Moving the program clock itself onto a dedicated
  thread is outside this design; the dedicated front-door runtime removes the front door from that
  question.

## 12. Fallback matrix (target behaviour)

Time-to-picture is measured from page load to the first decoded frame on desktop Chrome on mst001
(office), through `lead/officeprobe.sh`. Today's numbers come from issue #280 and the memory record of
2026-11-12.

| Network | Site | MoQ | WebRTC | Stills | User gets | Target time-to-picture |
| --- | --- | --- | --- | --- | --- | --- |
| any, front door in `setup` (no ACME certificate yet) | certificate setup page only, h1 on TCP 443, behind a certificate warning | no (QUIC not started) | no | no | the setup page, which names the state and the reason | n/a: no live pictures until `serving` |
| any, certificate expired | browser certificate error (HSTS: no click-through) | no | no | no | an error; the operator recovers by CLI | n/a |
| N1 open | h3 (Alt-Svc) | yes | not used | not used | MoQ | <= 1.5 s |
| N2 UDP 443 only | h3 | yes | host UDP 443 when the browser has no WebTransport (Safari version support unverified; checked in the office matrix) | no | MoQ, else WebRTC | <= 1.5 s |
| N3 no UDP | h1 over TCP | skipped: capability `udp443` from the server is per-host, so the client also races: WT handshake deadline 1.5 s, then WebRTC | ICE-TCP 443 | no | WebRTC over TCP | <= 3 s (today: 4 s MoQ timeout, then a 60 s hold to stills) |
| N4 CONNECT proxy (conditional per browser and proxy policy) | h1 via proxy | skipped (no QUIC through proxies) | ICE-TCP via proxy (Chrome, measured S9); Firefox through a proxy unverified ([Bug 1251542](https://bugzilla.mozilla.org/show_bug.cgi?id=1251542) reported ICE-TCP/TURN via HTTP proxy broken in Firefox 44), TURN-TLS (slice 8) covers it | no | WebRTC | <= 3 s |
| N5 TLS-only proxy (conditional: measured with Chrome only) | h1 via proxy | skipped | TURN-TLS 443 (slice 8) | before slice 8: stills | WebRTC after slice 8, stills before | <= 4 s |
| N6 TLS-intercepting proxy | h1 via proxy | skipped | TURN-TLS only if the MITM passes SNI `turn.*` through untouched | yes | stills (honest caption) | <= 2 s to stills |
| UDP 443 open but the engine's upstream firewall drops it (#280) | h1 | skipped once `udp443 = unreachable`; never advertised in Alt-Svc | host UDP fails, ICE-TCP | no | WebRTC over TCP | <= 3 s |

How the UI skips a dead rung fast:

1. The capability report says whether UDP 443 has been seen from outside (section 10.1). If it is
   `unreachable`, the client never tries MoQ.
2. Otherwise the client races: it starts the WT handshake and, if it is not ready in 1.5 s
   (QUIC handshake p99 on the office link is measured in slice 1), it releases the MoQ lease and starts WHEP.
   The 60 s "release unacknowledged" hold (`web/src/preview/PreviewSurface.tsx:248`) must fall to
   WebRTC, not stills. PR #283 already does this.
3. WHEP offers include ICE-TCP candidates from day one, so a no-UDP network costs no extra round trip.
4. A per-browser memory (sessionStorage) of "MoQ failed on this origin" skips straight to WebRTC for
   15 minutes; after that, or when the realtime channel pushes a capability change, the next
   picture tries MoQ again. (A viewer's own network can block UDP while the server's is fine, so a
   server-side `reachable` alone does not clear it.)
5. Every row for a front door in `serving` with a valid certificate assumes the ladder's last rung:
   if MoQ and WebRTC both fail, the surface shows stills with a caption naming the reason; none of
   those rows ends at "no picture". The `setup` and expired-certificate rows are the deliberate
   exceptions: there the site itself is not usable and recovery is by the operator.

## 13. Invariants

* **#1, by topology.** The front door runs on a dedicated Tokio runtime (`network.workers` OS
  threads, section 11), never on the runtime whose task drives the output clock. It shares no lock
  with the clock or compositor. The only data paths from the engine to the front door are the
  existing egress structures (drop-oldest rings to str0m, the encoded-window registry to MoQ), whose
  producers never wait (ADR-0048, ADR-0103). Router handlers reached over h3 or TCP talk to the engine
  through the same channels as today's control plane.
* **#10, every structure bounded, nothing cleared by an attacker.** Dispatcher lanes (1024
  established + 256 new per lane, `try_send`, drop); the established 4-tuple set (bounded by session
  and connection caps, written only after validation, entries removed with their session); source
  buckets (16 384, LRU eviction, never a global clear); a global new-flow budget; per-source STUN
  buckets (16 384, LRU) and a global STUN budget; per-established-peer buckets (bounded by the
  established-set caps); one steering program at a time (at most 1 244 peers, 4 096 instructions);
  the sender-waker list (1 024, overflow wakes every held waker); ICE-TCP
  per-connection out queue (256) and read buffer (one RFC 4571 frame, 65 535 bytes); TCP per-IP and
  global connection caps at accept; QUIC `max_connections` and per-connection stream limits; h3
  `max_field_section_size`; TURN allocations (one per lease, 4 per source, global cap). Nothing
  awaits a client: UDP sends are non-blocking (`try_send_to` / noq's `UdpSender`); TCP writes run in
  the per-connection task, fed by a bounded queue with `try_send`.
* **Amplification.** Before address validation noq applies the 3x limit (RFC 9000 section 8.1);
  Retry above `retry_above_handshakes` (wired in slice 3's PR B); Version Negotiation and stateless
  reset are rate-limited as in section 3 (the 3x limit does not cover them); STUN answers only an
  authenticated Binding Request (str0m); TURN's pre-auth reply is a 401 no larger than the request,
  with no state. `tests/amplification.rs` measures all three QUIC shapes against a real endpoint.

## 14. Test plan

| Layer | Test | Where |
| --- | --- | --- |
| Demux unit | every first byte 0-255 has exactly one class; 64-79 depends only on the TURN-server set | `multiview-frontdoor` unit test (spike version: `every_first_byte_has_exactly_one_class`) |
| Demux property | random datagrams never reach noq as a new connection unless they are valid Initials; never reach str0m unless STUN/DTLS/RTP | proptest |
| Fuzz | `cargo fuzz` targets: UDP classifier + str0m `Receive::new`; TCP first-byte + RFC 4571 deframer; TURN server request parser (slice 8); the vendored h3 adapter and h3 request path (slice 2) | `fuzz/` (nightly job, not the PR gate) |
| Integration, unprivileged | the front door on `[::1]:0` (TCP and UDP): h3 GET via a native h3 client, WebTransport MoQ subscribe via `web-transport-moq`, str0m-to-str0m WebRTC over UDP and over ICE-TCP, TLS h1 on the same TCP port | `cargo test -p multiview-frontdoor`; the existing str0m loopback harness in `multiview-webrtc` |
| Browser, CI | headless Chrome on loopback: S3/S8 above as a test (WT + DC + h3); a native pooling client sends GETs around one WT session on one connection; a second CONNECT gets 429 | web e2e job (puppeteer-core is already used by lead probes) |
| Flood/chaos (#1, #10) | S5 floods from one source, from many spoofed sources and from rotating IPv6 /64s (garbage, Initial, unsupported-version Initial, short header with well-formed CIDs, STUN, RTP, ICE-TCP SYN flood, slowloris on TCP 443, half-open TLS, TURN Allocate flood) against a running engine with a live output: assert 0 "ticks behind", output PTS delta constant (`ffprobe`), an established WebRTC viewer keeps >= 95% of frames, RSS bounded | `crates/multiview-engine/tests/` chaos suite + a scripted soak |
| Soak | 24 h with 50 viewers mixed MoQ/WebRTC-UDP/WebRTC-TCP and a 10-minute flood every hour | mda001-class host, after slice 3 |
| Network matrix, office | from mst001 desktop Chrome, Safari and Firefox: (a) normal; (b) block UDP except 443 (pf rule); (c) block all UDP; (d) through a CONNECT proxy; (e) through a TLS-only proxy; (f) through a TLS-intercepting proxy (mitmproxy). Record rung and time-to-picture per row | `lead/officeprobe.sh` + `ladderprobe-remote.mjs`; `lead/quicvn.py` for raw QUIC reachability |

## 15. Slices

| # | Slice | Risk | Crates | Acceptance (office network, mst001 desktop Chrome unless stated) |
| --- | --- | --- | --- | --- |
| 1 | Shared UDP socket: `multiview-frontdoor` crate owns one dual-stack UDP socket and the first-byte dispatcher; the existing `UnifiedEndpoint` and the moq-tokio noq server attach to it as lanes (noq via `new_with_abstract_socket`, grease off). MoQ stays on web-transport-moq (CONNECT-only), so the site keeps TCP. Config `[network] https_port`, front-door Tokio runtime of its own, established set written only after validation, VN/reset limits. Runs on a non-public port or behind the office firewall until slice 3 lands; public UDP 443 on mda001 waits for slice 3 | R3 | new `multiview-frontdoor`, `multiview-webrtc`, `multiview-moq`, `vendor/moq-tokio` (expose socket injection), `multiview-cli` | MoQ and WebRTC both reach the office browser on one UDP port; demux unit + property tests incl. VN/Retry/reset/coalesced/v2; S4 sweep: 0 mis-deliveries; memory bounded under the S5 floods; tick lateness under flood no worse than with the front door disabled (same host, same flood) |
| 2 | HTTP/3 for the site: vendored h3 adapter (`multiview-h3`), h3 request -> axum adapter, MoQ moved to h3-webtransport sessions on the same endpoint; Alt-Svc gated on reachability | R3 | `multiview-h3`, `multiview-frontdoor`, `multiview-control`, `multiview-moq` | office Chrome loads the UI with `nextHopProtocol=h3`; MoQ plays from the same UDP endpoint alongside the site (separate connections in stock Chrome); a native pooling client gets GETs and one WT session on one connection; site still works over h1 when UDP is blocked |
| 3 | Flood hardening and soak: per-source buckets, Retry policy, rcvbuf sizing and clamp warning, optional SO_REUSEPORT steering, nftables runbook, the chaos suite | R3 | `multiview-frontdoor`, `multiview-engine` tests, runbook | chaos suite green incl. multi-source and IPv6 /64-rotating floods; 24 h soak 0 ticks behind with 50 legitimate viewers (MoQ, WebRTC-UDP, WebRTC-TCP) plus a 10-minute flood each hour; established viewer >= 95% frames under a 600k pps spoofed flood **with validated-flow steering on**; legitimate aggregate of at least 200k datagrams/s sustained with 0 kernel drops. Gate for opening public UDP 443 |
| 4 | Engine TCP 443 listener: rustls with the first-byte demux, ICE-TCP passive candidates (RFC 4571 deframer to str0m), TCP 80 redirect, `behind_proxy` mode with PROXY v2 (fail closed). Certificate until slice 5: the existing `[control.tls] mode = "static"` files through the `CertificateSource` seam; nginx stays on mda001 until slice 5 issues by ACME | R3 | `multiview-frontdoor`, `multiview-control`, `multiview-webrtc`, `multiview-config` | office Chrome with all UDP blocked plays WebRTC over TCP 443; Firefox too; Safari result recorded; nginx no longer needed on mda001 |
| 5 | ACME certificates for the front door (re-cut #167-#172): `CertificateSource` seam, setup/serving states, bootstrap certificate, setup authentication, instant-acme engine with Let's Encrypt, Google (EAB) and custom (trust anchor) profiles, DNS-01 Cloudflare (token and global key) and TLS-ALPN-01, ARI renewal, CLI `tls status`/`setup-code`/`set-credential`, Pebble CI job, runbook. Until this lands, the static certificate path named in slice 4 (and the MoQ certificate files) keeps serving, so slices 1-4 are not blocked | R3 | `multiview-frontdoor`, `multiview-control` (`acme/`), `multiview-config`, `multiview-cli`, CI, runbook | fresh mda001-class host: `setup` page reachable only with the setup code; DNS-01 via Cloudflare token issues from Let's Encrypt staging and production and swaps to `serving` with no restart; Pebble job green for both challenges, EAB and a private root; output ticks throughout |
| 6 | HLS and static serving without nginx: zero-copy segment body, Range/206, cache headers, embedded UI by default; mda001 cutover runbook (nginx removed, systemd socket activation) | R2 | `multiview-control`, `deploy/`, runbook | segment TTFB within 2 ms of nginx's; mda001 serves TCP 443/80 and UDP 443 from the engine |
| 7 | HTTP/2 on TCP 443 with an h2 floor (max streams, header-list size, PING timeout, first-HEADERS deadline) | R2 | `multiview-control` | h2 slowloris test drops at the deadline; ALPN `h2` negotiated |
| 8 | Built-in TURN-over-TLS (SNI `turn.<host>`), lease-bound REST credentials, in-process relay, ICE server list in the WHEP lease | R3 | `multiview-webrtc` (server role on `turn/message.rs`), `multiview-frontdoor`, `multiview-control`, `web/` | office Chrome through a TLS-only proxy plays WebRTC; no allocation without a lease; open-relay test (CreatePermission to a third-party address) gets 403 |
| 9 | Reachability self-test, capability report, UI banner, fast rung skipping (1.5 s race, sessionStorage memory), legacy setting removal | R2 | `multiview-frontdoor`, `multiview-control`, `web/` | with UDP 443 dropped upstream: the UI shows the banner and reaches WebRTC in <= 3 s; with it open: MoQ in <= 1.5 s |
| 10 | Retire `[control.tls] mode = "static"` and `MULTIVIEW_MOQ_CERTIFICATE`/`_PRIVATE_KEY`, after #282 decides the air-gapped installed-certificate source | R2 | `multiview-config`, `multiview-control`, `multiview-cli` | only after #282 is decided; air-gapped path exists or is explicitly declined |

Slices 1, 2, 3 and 4 are independently useful: 1 fixes #280 for MoQ and WebRTC with no TCP change; 2
adds site HTTP/3; 3 is the abuse floor that must exist before 443 is the only door; 4 removes the
nginx dependency and covers no-UDP networks. Slice 5 (ACME) depends on 4 for TLS-ALPN-01 only and
does not block 1-3. Public UDP 443 opens only after slice 3. 6, 7, 8 depend on 4; 9 depends on 1; 10 waits for #282.

### Slice 1 as built (issue #287)

* Crate `multiview-frontdoor`: `FrontDoor::start` builds its own multi-thread runtime
  (`network.workers` threads named `frontdoor-N`), binds `[::]:https_port` dual-stack on
  that runtime's reactor and spawns the one dispatcher task there. `classify` is the
  section 3 table plus structural checks (STUN cookie and exact length; DTLS and RTCP
  minimum headers; Initial >= 1200 bytes; VN (version 0) and Retry dropped at a server;
  unsupported versions >= 1200 bytes are Version Negotiation triggers and pay a global
  200/s budget; short headers >= 21 bytes). QUIC v2 is not spoken by moq-noq 1.3.5, so a
  v2 Initial is a VN trigger.
* Lanes: QUIC lane is a `noq::AsyncUdpSocket` (sends via `UdpSocketState` with GSO);
  the WebRTC lane hands datagrams to `UnifiedEndpoint::run_shared`, which sends on a
  duplicate descriptor of the same socket. Established sets: WebRTC peers are written
  when a session is connected and str0m accepted a datagram from that address
  (`Session::validated_peer`); QUIC peers when the MoQ accept loop admits a session
  (handshake complete). Both reference counted, capped, removed when the session ends.
* MoQ: `moq-tokio` takes the lane through `server::Config::socket` (vendor change
  recorded in `vendor/MOQ-DELIVERY-PROVENANCE.md`); the provider builds the server and
  runs its accept loop on the front-door runtime.
* Config: `[network] https_port, advertise, workers`, in the same `NetworkConfig` as
  slice 5's `public_host` and `[network.tls]` (both optional; those two come together).
  Setting `network.https_port` is the switch: unset (no `[network]`, or ACME settings
  only), the legacy sockets stay unchanged. Set: `webrtc.udp_port` and `MULTIVIEW_MOQ_BIND`
  are ignored with a deprecation warning (not a second legacy socket; that is a later
  slice if needed).
* Not in slice 1: steering, nftables, Retry policy knobs, rcvbuf via systemd (slice 3);
  the output clock itself still runs on the `#[tokio::main]` runtime (#311). The MoQ
  per-admission publication pump and its `moq-net` origin driver are also still spawned
  on the caller's (control-plane, i.e. main) runtime by `Runtime::admit*`; they never touch
  the socket and are fed by bounded drop-oldest windows.
* Budgets as built: per source 500/s burst 200; global new flow 50 000/s; Version
  Negotiation triggers 200/s and supported-version Initials 200/s, each charged
  **before** the per-source and global buckets; noq holds at most 64 pending handshakes
  and 4 MiB of their data (`max_incoming`, `incoming_buffer_size_total`).
* Shutdown: the daemon drops the front door last, after the control-plane drain, with a
  bounded `shutdown_timeout` (2 s) on a blocking-pool thread, so the socket is closed when
  teardown returns.

### Slice 3 as built, part A: flood hardening in `multiview-frontdoor` (issues #289, #321, #325)

**Validated-flow steering (Linux).** `FrontDoorConfig::steering` (default `true`).

* Group: two dual-stack UDP sockets on the same `[::]:port`, both `SO_REUSEPORT`, created in a
  fixed order so the kernel group index is known: the **listener** first (index 0: every
  unvalidated datagram, and every send), then the **established** socket (index 1). With port 0
  the listener's ephemeral port is read back and the established socket binds it.
  `steering::ReuseportGroup` is that pair, public so tests exercise the kernel mechanism directly.
* Program: classic BPF via `SO_ATTACH_REUSEPORT_CBPF` (on the listener; it applies to the group).
  CBPF needs no `CAP_BPF`/`CAP_SYS_ADMIN` and works with `kernel.unprivileged_bpf_disabled=1`. The
  program sees `skb->data` at the UDP payload, so every header field is read through `SKF_NET_OFF`:
  IP version nibble; IPv4 source address and (after the IHL, `ldxb msh`) source port; IPv6 only
  when the next header is UDP (an extension header goes to the listener), the four address words
  and the port. A load that fails returns 0 (listener). It returns 1 when the source's key is in
  the set.
* Key and lookup: a 32-bit hash of a family tag, the address words and the port, salted with a
  fresh random value per program (std `RandomState`), computed identically in userspace and in
  CBPF (xor, mul, rsh; 32-bit wrapping). IPv4-mapped peers hash as IPv4, since an IPv4 datagram on
  the dual-stack socket carries an IPv4 header. The sorted keys form a balanced binary search
  (`jeq`/`jgt`, with a 32-bit `ja` to each right subtree). The program length depends only on the
  number of keys; `STEERING_MAX_PEERS` = 1 244 is the largest set that fits `BPF_MAXINSNS` (4 096;
  2 379 instructions at the cap), checked on both sides by a unit test. Peers beyond the cap stay
  on the listener (served through the new-flow path and their per-peer bucket) and are counted
  (`steering_overflow`).
* Collisions: a non-member whose salted hash equals a member's only shares the established queue.
  The dispatcher checks the exact `EstablishedSet` for every datagram from either socket; a
  non-member read from the established socket pays the new-flow limits like any other new flow
  and is counted (`steered_unverified`).
* Update path, off the data plane: `EstablishedSet` bumps a generation and notifies on every
  insert that adds an address and every remove that deletes one. A steering-updater task on the
  front-door runtime waits for either set, debounces 20 ms, snapshots both sets (QUIC and WebRTC
  peers in one program) plus the configured TURN servers, compiles and attaches with one
  `setsockopt` (the kernel swaps the program under RCU). The dispatcher never compiles or
  attaches. A failed attach keeps the previous program and counts `steering_update_errors`
  (warning at most every 10 s).
* Dispatcher: one task, sole owner of the limiter. It waits for either socket and drains the
  established socket first (the per-wake budget of 1 024 datagrams covers both), then the
  listener.
* Fallback: if the pair cannot be created (second socket, `SO_REUSEPORT`) or the first attach
  fails, the door runs one socket on the same port; off Linux it always does.
  `FrontDoor::steering()` reports `ReuseportCbpf` or `SingleSocket { reason }`, logged at start
  (info when active or disabled by configuration, warn for a fallback on Linux).
* Group membership: the kernel lets a socket join a reuseport group only with the same effective
  uid as the group, so only a process running as the engine's own user could take a share of the
  traffic. Run the engine as a dedicated user.
* Residual risk (issue #325): steering and the established bypass are keyed on the source address
  and port. An off-path spoofer that knows a viewer's exact IP and port lands in the established
  kernel queue and shares it with that viewer; its userspace share is bounded by the viewer's
  per-peer bucket (20 000/s, burst 4 000; `dropped_established_peer`), its Initials still pay the
  Initial and global budgets, and it cannot reach any other viewer's queue position. A key a
  spoofer cannot forge (QUIC connection ID, ICE-validated 5-tuple in the kernel) is not used:
  connection IDs are not visible to CBPF for WebRTC, and the address key keeps one program for
  both lanes.

**STUN caps.** `Budget::Stun` for a well-formed STUN message from a non-established source:
per-source 50/s burst 50 (an ICE agent paces checks at Ta >= 50 ms per pair) in its own bounded
LRU (same `max_sources` cap and key) and a global 5 000/s, both checked before the generic
per-source and global buckets, so a refused STUN message spends nothing else. Established peers
(consent freshness) never pay it. Counters `dropped_stun_per_source`, `dropped_stun_global`.

**Receive buffers.** Each socket tries `SO_RCVBUFFORCE` first, then `SO_RCVBUF`, reads back, and
reports `usable` (Linux: half of what `getsockopt` returns). A warning names the request, the
grant, `net.core.rmem_max` and the fix (`docs/runbooks/udp-443-flood.md`, slice 3 PR B).
`FrontDoor::buffer_report()` returns the shallower of the pair.

**Kernel drops.** A sampler task reads `/proc/net/udp6` every second (dual-stack sockets are
listed there), matches each socket's inode (`/proc/self/fd/<fd>`) and stores the `drops` column in
`kernel_drops_listener` / `kernel_drops_established`, warning at most every 10 s when they grow.
Off Linux the gauges stay 0 and `kernel_drops_supported()` is false.

**Other #325 fixes.** Supported-version Initials always pay the Initial and global budgets (also
from established addresses); per-established-peer buckets as above; a QUIC VN shape in the RTP
range from a non-established source is dropped (0x80 RTP is not affected; an established peer's
datagram is never touched); the sender-waker list is capped at 1 024 (a full list wakes every held
waker and restarts, so no sender is ever left parked).

**Chaos suite.** `multiview_frontdoor::flood` (shapes, sources: one, many IPv4 loopback, rotating
IPv6 /64s via `IPV6_FREEBIND`) drives `tests/chaos_flood.rs`, the steering and amplification
tests, and `examples/udp443_flood.rs` (the second-host tool: `--target --pps --secs --threads
--sources --shapes`). Chaos arms: idle; door on with an established MoQ viewer under each flood kind
(one source, many IPv4 sources, rotating /64s); door off with the many-source flood. Per arm: tick
count within 2, every PTS exactly `MediaTime::from_tick(index)` with consecutive indices, on Linux
zero unexplained repeats (absolute, every arm), viewer >= 95% of groups, RSS bound, and with steering the viewer's datagrams
read from the established socket.

**The #321 stall witness.** The slice-1 suite asserted `repeats == 0` on every arm, which asserted
that the CI host never stalls the process for more than a period (macOS run 43192931229: one
~104 ms whole-process stall, three last-good repeats, door off). The clock absorbs such a stall by
design (ADR-T018). The witness is a std thread that sleeps 1 ms in a loop on the clock's own time
base and records every sleep that took at least 5 ms, stamped inside the sleep (clock read just
before `sleep`, just after it returns), so a gap holds only time the witness was blocked. What
needs explaining is a repeat (a tick behind, the metric #289 asserts). A repeat is explained only
when its lateness minus the witness-observed stall time inside its `[deadline, emit]` window is
under one period: without the time the process was seen not running, it would not have been
behind. The excuse is proportional, never a pass for any overlap. The suite asserts zero
unexplained repeats, absolutely, plus the exact PTS of every tick. `tests/stall_witness.rs`
records the red case (a SIGSTOP/SIGCONT stall of 150 ms fails the old assertion with 3-4
repeats), the green case (all of them explained), a pure classifier test on fixed numbers, and
three controls that must stay unexplained: a 150 ms spin on the program runtime while the process
runs; a 100 ms program-runtime block immediately followed by a 150 ms SIGSTOP; and a 100 ms block
under a 20k pps loopback flood (review PR 333). Why proportional, measured: on macOS CI (run
43324610127) the witness and the clock thread were descheduled at slightly different moments
(gaps 43.6 and 76.5 ms against repeats 50 and 103 ms late), which a whole-window cover rule
blamed on the code; the proportional rule explains them and still fails every control.
**Platform limit (measured).** The witness is judged on Linux only. On macOS CI (run 43331219140)
it recorded 255-302 gaps of at least 5 ms in a 3 s run with no stall injected, and a 136 ms gap
while another thread merely spun (headless processes there get coalesced timers), so it can
neither prove nor excuse a stall. On macOS the chaos suite asserts what the design guarantees on
any host (one frame per tick, consecutive indices, exact PTS from the tick, the viewer's >= 95 %,
RSS), prints that ticks behind were not judged, and reports the repeats; the witness-judged
controls are `cfg(linux)`. The production target (mda001) and the ubuntu CI runner are Linux,
where the absolute zero-unexplained-repeats assertion runs in every arm. A whole-process stall
witness cannot see thread-level starvation of the program runtime: on a host oversubscribed 2x
(60 busy loops on 56 cores) one in six runs showed one such repeat, in the door-off arm as often
as in a door-on arm; at the build host's normal load (load average 32-76) 10/10 runs had 0
repeats in every arm. CI runners are not oversubscribed that way; a failure there is a real
signal to investigate, never to rerun blindly.

### Slice 4 as built (issue #290)

PR1 of slice 4: the TCP door, ICE-TCP lane, `behind_proxy` mode, port-80 redirect and the site over
the engine's own TLS. The str0m ICE-TCP consumer and the browser ladder are PR2 of this slice.

* **Config** (`[network]`, all optional, every saved `desired.json`/TOML keeps loading):
  `mode` (`"direct"` | `"behind_proxy"`), `tcp_port`, `http_redirect_port`, `bind`, `trusted_proxies`,
  `proxy_listen`, `[network.ice_tcp] enabled`. **`mode` absent means no TCP listener at all**: the
  slice-1 UDP door only, so a saved state with only `https_port = 443` starts unchanged while another
  server (nginx on mda001) still owns TCP 443. `mode` requires `https_port`. `bind` must be an IPv6
  literal (default `[::]`, dual-stack; an IPv4 literal or IPv4-mapped address is rejected at load).
  `trusted_proxies` takes IP literals and CIDR prefixes (at most 64), required non-empty for
  `behind_proxy`. `proxy_listen` defaults to `[::1]:8443` and rejects `0.0.0.0`.
* **Deviation, `tcp_port`**: section 10 has one `https_port` for TCP and UDP. `tcp_port` (default
  `https_port`) lets an operator stage the TCP door on a second port while another server still owns
  TCP 443, then drop the key at cutover.
* **`public_host` relaxed**: it is valid alone now (the redirect target); `[network.tls]` still
  requires it. `direct` with the redirect enabled requires `public_host`: the redirect target never
  comes from the `Host` header. The redirect port must differ from the TCP door port.
* **Topology (#1)**: `FrontDoor::start_tcp` binds the listener inside the front-door runtime
  (backlog 1024, dual-stack for `[::]`, never `0.0.0.0`) and runs the accept loop, every
  per-connection task, the ICE-TCP reader/writer tasks and the redirect there. The site service is
  the control-plane router on `multiview_control::ConnectionServer`, run on the front-door runtime.
* **Accept, in order**: accept (an error counts and backs off 50 ms, never exits); the global cap
  (1024) before any byte is read; the per-source cap (256, IPv4 address or IPv6 /64) on the TCP peer
  in `direct`. Behind a proxy: the TCP peer must be in `trusted_proxies` (else closed before any
  read), then a PROXY v2 header (binary, `PROXY` command, `TCP4`/`TCP6` only, at most 536 bytes of
  addresses + TLVs) within the 5 s first-byte deadline; anything else (missing, v1 text, `LOCAL`,
  UDP/unix, oversize) closes before any byte of the client's protocol is read. The header's source is
  the peer for the per-source cap, `ConnectInfo` and logs.
* **First byte**, `peek` under the 5 s deadline from accept: `0x16` is TLS; `0x00`-`0x05` reads the
  2-byte RFC 4571 length (never TLS, so consuming it is safe) and 20..=1535 is ICE-TCP (when
  enabled); anything else closes.
* **TLS**: `LazyConfigAcceptor` over a reader capped at 16 KiB until the `ClientHello` parses; hello
  plus handshake bounded by 10 s from the first byte. ALPN exactly `acme-tls/1` routes to the ACME
  responder config (None until slice 5: closed); SNI `turn.<public_host>` is closed (reserved for
  slice 8); everything else is the site (ALPN `http/1.1` only) and is handed, with the admission
  guard, to the site service. The site is the same router and `ServeOptions` as the plain
  `[control] bind` listener (header-read timeout, `ConnectInfo`, upgrades, the control per-IP cap on
  the real peer, drain on the control-plane shutdown). The plain listener keeps running unchanged.
  The certificate is `[control.tls] mode = "static"` through `StaticCertificateSource` +
  `SourceResolver` (feature `tls`, unix); without it the door logs a warning and serves ICE-TCP only.
* **ICE-TCP lane contract** (`FrontDoor::take_ice_tcp_lane`): the first frame must arrive within the
  5 s deadline, be <= 1535 bytes and structurally a STUN Binding Request (type `0x0001`, cookie,
  exact length, a `USERNAME` containing `:`). An optional pre-check hook on the full `USERNAME`
  (`set_ice_tcp_username_check`) defaults to accept. Until a consumer takes the lane, connections
  close right after their first frame. Registered connections (table keyed by canonical peer, capped
  at the door's connection cap; a second connection from the same peer is refused) start in
  **probation**: frames go to the lane, but until the consumer queues an outbound frame to that peer
  (str0m answering an authenticated Binding Request, i.e. `MESSAGE-INTEGRITY` verified) at most 8
  inbound frames, each <= 1535 bytes and a STUN message (first byte `0x00`/`0x01`), are accepted,
  and with no outbound frame within 5 s of accept the connection closes
  (`ice_tcp_probation_expired`). After probation frames up to 65 535 bytes flow (a zero-length frame
  closes). Each `IceTcpFrame` carries `peer` (canonical) and `local` (accepted socket, unmapped).
  Inbound: one lane queue of 1024, `try_send`, full drops and counts. Outbound:
  `IceTcpSender::send` never blocks; per connection at most 256 frames **and 256 KiB** (drop-oldest;
  the byte cap is an addition: 256 maximum-size frames would otherwise pin 16 MiB per connection);
  the connection's **writer** closes a stuck peer (`ice_tcp_closed_stuck`), measuring progress
  against wall time so a peer trickling ACKs cannot keep its slot: (a) each frame must be written
  completely within the write timeout (5 s) of its write starting, never reset by partial writes;
  (b) after every pop the writer checks the queue against its high-water mark (half the cap: 128
  frames or 128 KiB) and closes once it has stayed at or over it for the write timeout
  (`backlogged_since`, cleared only when the queue drops below). `send` never closes or waits.
  The first frame's STUN attribute list is walked to the end (header, value and padding must fit)
  before the `USERNAME` is accepted. 30 s without an inbound frame closes. `IceTcpSender::close(peer)` drops a
  connection.
* **Redirect**: `[bind]:http_redirect_port`, own caps (256 global, 16 per source), 5 s total per
  connection, at most 8 KiB read for the request line; `301` to `https://<public_host>[:port]<path>`
  (IPv6 literal bracketed, port omitted when 443), path + query kept, the fragment dropped, anything
  with control characters, spaces, `\`, a leading `//` or not in origin form becomes `/`. The `Host`
  header is never read.
* **Stats**: TCP counters for accepted, refusals (global, per source, proxy), first-byte and
  handshake timeouts, classified closes, TLS site/ACME/failed, TURN-reserved, the ICE-TCP lifecycle
  and drops, the redirect, plus `tcp_open_connections` / `redirect_open_connections` gauges.
* **Failure posture**: a TCP or redirect bind failure is logged; the UDP door keeps running.
* **Tests**: unit and property tests for the first-byte classifier (all 256 bytes, all lengths), the
  PROXY v2 parser and the RFC 4571 deframer; `tests/tcp_door.rs` (TLS h1 and ICE-TCP on one port over
  IPv4 and IPv6, the ICE-TCP gates and probation, `behind_proxy` fail-closed and header address, the
  redirect, the deadlines, the per-source cap, a stuck reader, a trickling reader closed within 3x
  the write timeout); `tests/tcp_chaos.rs` (connect flood, first-byte and TLS slowloris, half-open
  TLS, stuck ICE-TCP readers, oversized and garbage frames against the real tick loop). The attacker
  runs in-process, so the chaos test compares like with like (the `chaos_flood.rs` pattern): three
  runs, `idle` (door up, no attack), `attack_door_on` and `attack_door_off` (the door shut down, the
  same port served by a trivial `TcpListener` on its own 2-worker runtime that reads and discards
  with a 5 s deadline), the same attack paced to `FRONTDOOR_TCP_CHAOS_CPS` (default 2000) connection
  attempts/s. Asserted: tick counts within 2 of elapsed/period in every run, idle without repeats,
  door-on repeats at most door-off + 1, door-on p99 within 4 ms of the worse of door-off and idle,
  door-on worst tick within one period of theirs; and the door's own bounds: the connection cap,
  RSS growth, fd count back to baseline, every defence counter (stuck writers included) fired.
* **ACME wiring (#291)**: the door's `TlsRouting.acme` route is wired to slice 5's
  `ChallengeResolver` via `multiview_control::acme_challenge_server_config` (explicit `aws-lc-rs`
  provider, safe default protocol versions, ALPN `acme-tls/1` only) and
  `FrontDoorRun::install_acme_resolver`, which installs only the ACME half of the routing
  (`FrontDoor::set_acme_config`) so a previously installed site certificate keeps serving;
  symmetrically, `install_site` now installs only the site half (`FrontDoor::set_site_config`), so
  it never clobbers a previously installed ACME responder back to `None`. `multiview run` starts
  the `AcmeService` and calls `install_acme_resolver` once in slice 5 PR B; this crate does not call
  it yet.
* **Deferred**: systemd socket activation (section 9) and the mda001 cutover (slice 6). The str0m
  ICE-TCP consumer and the browser's selected-pair report are below (PR2 of this slice). The browser
  ladder itself needed no change: MoQ fails over to WebRTC, and the browser gathers its active TCP
  candidate against the advertised passive one when UDP is blocked.

#### Slice 4 PR2: the str0m ICE-TCP consumer (issue #290)

* **Candidates**: `EndpointConfig::ice_tcp_port` (default `None`). When set, every role that gathers
  host candidates — WHIP ingest, WHEP-serve output (Program), `whip_push`, and the native preview
  `WhepEgress` (wall + input/program preview, `with_host_candidates_tcp`) — adds one **passive**
  ICE-TCP host candidate (`tcptype passive`) per concrete host IP (the same advertised IPs as UDP,
  `network.advertise`) on the TCP door's port, **after** its UDP candidates. Priorities are str0m's
  own (RFC 8445 type preference 126 for a UDP host, 90 for a TCP host), never overridden, so UDP stays
  preferred. `UnifiedEndpoint::bind` (the socket-owning `run()`) forces `ice_tcp_port = None`: it has
  no TCP lane.
* **Session seam**: `Session::add_tcp_passive_candidate`, `handle_datagram_proto(Transport, ..)`
  (`handle_datagram` = UDP) and `poll_transmit_routed() -> Outbound { transport, source, destination,
  payload }` carrying str0m's `Transmit::proto`. TCP and UDP candidates share `addr:443`, so every
  driver path routes by `transport`, never by address: a TCP transmit is never sent over UDP (it is
  dropped when no TCP lane is wired).
* **Driver**: `UnifiedEndpoint::run_shared_with_tcp(udp lane, Option<impl SharedTcp>, stop)`
  (`run_shared` = no TCP). The `select!` is biased tick → UDP → TCP, and the tick uses
  `MissedTickBehavior::Delay` (a late tick runs once, never a burst; the socket-owning `run()` too).
  Each lane wake drains at most one budget (256 items / 2 ms). **Fairness rule**: when a wake
  exhausts its budget, the *other* lane is drained once, without waiting, with the same budget
  (`try_next` loop) — a permanently ready UDP lane cannot starve TCP (or vice versa); every lane is
  served at least once per two budgets. An exhausted wake is then followed by `yield_now` and one
  full servicing pass (the bounded handoff), and the biased tick arm is polled first on the next
  iteration. No TURN classification (TURN is UDP here); the frame's `local` resolves onto the
  passive TCP candidates; frames fan to every role as `Transport::Tcp`. Outbound goes through `Egress { socket, tcp: Option<&dyn TcpSend> }`;
  `TcpSend::send` is the lane's synchronous drop-oldest queue, so nothing awaits a TCP peer
  (invariants #1/#10). `multiview-frontdoor` implements `SharedTcp` for `IceTcpLane` (feature
  `webrtc`); the str0m answer to an authenticated Binding Request is what lifts the door's probation.
* **NAT 1:1 (`network.advertise`)**: the door reports the accepted socket's local address unmapped
  (`IceTcpFrame.local`, canonical like `peer`), and `resolve_local_destination` unmaps an
  IPv4-mapped arrival (`[::ffff:10.0.0.5]:443`) before its same-family match, so an IPv4 ICE-TCP
  client behind NAT 1:1 resolves onto the advertised IPv4 passive candidate, not the IPv6 one.
* **ACME route (`acme-tls/1`)**: `acme_challenge_server_config` wraps the `ChallengeResolver` in a
  private challenge-only resolver: it answers only when ALPN is exactly `acme-tls/1`, SNI is present
  and `ChallengeResolver::challenge(name)` has a published challenge; otherwise it returns no
  certificate and rustls aborts the handshake. The ACME route never presents the site or bootstrap
  key (the `ChallengeResolver`'s own site-fallback `resolve` is unchanged).
* **Outbound queue**: `IceTcpSender::send` moves evicted (drop-oldest) frames out under the
  per-connection lock and frees them after releasing it; `ice_tcp_dropped_out` counts each.
* **Established set**: `Session::validated_peer()` reports only a peer validated over **UDP**; a
  session whose last accepted non-STUN datagram came over ICE-TCP reports none, so the UDP socket's
  established set never learns an address from a TCP path.
* **cli**: `FrontDoorRun::take_ice_tcp_lane()` is `Some` only when the TCP door runs with
  `network.ice_tcp.enabled`; `spawn_unified_webrtc` then sets `ice_tcp_port` to the door's TCP port
  and runs `run_shared_with_tcp`. **`behind_proxy` = no ICE-TCP** (design section 8): the door runs
  with `ice_tcp = false` and no lane is handed out.
* **Tests**: `multiview-webrtc/tests/ice_tcp_session.rs` (passive vs active TCP sessions connect and
  carry SRTP over an in-memory TCP shuttle on IPv4/IPv6, `validated_peer()` stays `None`; a UDP peer
  still pairs and validates over UDP with the TCP candidate listed after UDP);
  `multiview-frontdoor/tests/ice_tcp_webrtc.rs::webrtc_over_ice_tcp_with_no_udp` (a real door, a
  WHIP ingest endpoint on `run_shared_with_tcp`, a str0m client with no UDP socket at all over a real
  `TcpStream`: ICE + DTLS, >= 10 RTP packets into the ingest ring, the answer carries
  `tcptype passive` on the door port, 0 UDP datagrams at the door, nothing established; IPv6 and
  IPv4 loopback); `ice_tcp_session.rs::ipv4_active_client_connects_behind_nat_1to1_with_a_mapped_local`
  (advertised-only candidates, mapped local resolved as the driver does: ICE+DTLS connect);
  `unified.rs::a_saturated_udp_lane_cannot_starve_the_tcp_lane_or_the_tick` (an always-ready UDP
  lane plus N TCP frames: all N consumed and the idle tick keeps firing);
  `multiview-cli/tests/acme_tls_alpn_resolver.rs::acme_route_is_challenge_only` (unpublished SNI and
  no SNI fail the handshake with no certificate presented; the published name still works).
* **Not in this PR**: a real-browser ICE-TCP check (browsers gather active TCP candidates
  themselves); the deferred items above other than the str0m consumer.

### Slice 6 as built (issue #292)

The engine serves TCP 443, TCP 80 and UDP 443 on mda001; nginx is removed by the
[front-door cutover runbook](../runbooks/front-door-cutover.md).

* **Socket activation** (`multiview-frontdoor::activation`, `deploy/systemd/conspect.socket`):
  `main` adopts `LISTEN_FDS` (`listenfd`, which checks each descriptor's type, sets close-on-exec
  and clears `LISTEN_PID`/`LISTEN_FDS`) before the Tokio runtime exists, so the environment is
  never changed while another thread reads it. Each inherited socket is matched to its listener
  by **type and port** (TCP `tcp_port`/`https_port` = the TCP door, TCP `http_redirect_port` =
  the redirect, UDP `https_port` = the UDP door); an unmatched socket is logged. Any listener
  without an inherited socket binds its own, so the non-activated path is unchanged.
  `FrontDoor::start_with` / `start_tcp_with` / `start_redirect_with` take the inherited sockets.
* **Deviation, UDP pair**: section 9 had one `ListenDatagram=`. Slice 3a's validated-flow
  steering needs a `SO_REUSEPORT` pair whose members share an owner, so the unit passes **two**
  UDP 443 sockets with `ReusePort=yes`, in listener-then-established order (the reuseport group
  index order); the engine attaches the CBPF program to the inherited pair exactly as to one it
  bound. One inherited UDP socket runs single-socket (logged with the reason).
* **Restart window**: the door accepts before the control plane is built; a site `ClientHello`
  in that window (with activation: every connection the kernel queued during the restart) waits
  for the site, bounded by the 10 s handshake deadline (`FrontDoor::set_site_pending`), instead
  of being closed. With no `[control]` nothing waits.
* **Certificates for the cutover**: `[network] cert_file`/`key_file` give the door its own static
  certificate without making the loopback admin listener TLS; exclusive with `[network.tls]`.
  Static pairs (these and `[control.tls]`) are reloaded without a restart: every 30 s each file
  is opened once, `fstat`ed and read (at most 1 MiB) on the blocking pool; when the stamps
  (device, inode, size, mtime of the opened file; certbot re-points `live/` symlinks) change, the
  bytes just read are parsed and key/leaf-checked and swapped into the
  `StaticCertificateSource` the resolver reads per handshake; a broken pair keeps the old
  certificate. `[network] acme_webroot` makes the
  port-80 listener answer `/.well-known/acme-challenge/<base64url token>` from
  `<webroot>/.well-known/acme-challenge/` (regular files, at most 1 KiB, no symlinks), so certbot
  `--webroot` renews without nginx. The port-80 listener now reads to the end of the request
  head (`\r\n\r\n`, at most 8 KiB, 5 s), or answers 300 ms after a complete request line when
  the client stops sending, so a request line split across segments is parsed whole. The move to the engine's own ACME and the point where
  certbot is retired are in the runbook.
* **HLS** (`multiview-output::hls::http`): confinement, open and `fstat` in one blocking-pool
  hop (was five `tokio::fs` round trips); the length and validators come from the opened handle;
  64 KiB body chunks (`BODY_CHUNK`, never the whole file); nginx's `ETag` format
  (`"<mtime hex>-<size hex>"`), `Last-Modified`, `If-None-Match`/`If-Modified-Since` -> `304` (with `Range`, only an exact
  strong tag revalidates), `If-Range`. `deploy/hls-nginx.conf` stays the contract; `tests/hls_nginx_parity.rs` pins it.
  Zero-copy `sendfile` is not possible under rustls without kTLS; the bounded chunked read is
  the as-built form, and `scripts/hls-ttfb.py` measures TTFB against nginx (acceptance on
  mda001).
* **UI** (`multiview-control::spa`): the representation (identity or gzip, from
  `Accept-Encoding`) is chosen first; strong `ETag` per representation (SHA-256, `-gz` suffix),
  `304` only for the selected representation's tag; gzip for text assets with
  `Vary: Accept-Encoding` (compressed once per content hash, cached for the process; bounded by
  the servable file set), a missing `/assets/*` is `404`. `[control] ui_dir` serves a UI build from
  disk instead of the embedded copy, read into memory at start-up (at most 10 000 files /
  256 MiB) so a request never touches the filesystem.
* **Invariants #1/#10**: everything above runs on the front-door runtime or the control plane's;
  file I/O on the blocking pool; nothing is shared with the program runtime.
  `multiview-cli/tests/frontdoor_site_chaos.rs` runs the real tick loop while 64 stalled 4 MiB
  downloads, download/range lanes and a port-80 flood hit the door: one frame per tick, no extra
  repeats, RSS growth under 64 MiB.
* **Not in this slice**: HTTP/2 (slice 7), the mda001 measurements (TTFB, restart window, office
  browser rows of #290), which the lead runs from the runbook.
