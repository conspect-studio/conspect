# ADR-0107: Single network front door: the engine owns TCP 443 and UDP 443 (HTTP/3 + MoQ + WebRTC on one QUIC/UDP socket, ICE-TCP and TLS on one TCP listener, built-in TURN-over-TLS only for TLS-only proxies)

- **Status:** Accepted
- **Area:** Web/API · Preview · Realtime transport · Security (invariants #1, #10)
- **Date:** 2026-11-12
- **Source:** operator directives of 2026-11-12 (network: quoted under Context; TLS: quoted under Decision H); incident #280 (MoQ on UDP 4443 never reached a viewer outside the build host); TLS issues #167-#172 (re-cut by this ADR) and #282 (air-gapped, backlog)
- **Design note:** [network-front-door.md](../architecture/network-front-door.md) (demux tables, fallback matrix, config schema, test plan, slices)
- **Refines:** [ADR-0048](ADR-0048.md) (one WebRTC UDP socket, default 8189), [ADR-W029](ADR-W029.md) (TLS-0 rustls termination), [ADR-W031](ADR-W031.md) (HTTP/1-only serve floor), [ADR-0042](ADR-0042.md) (dual-stack binds)

## Context

The operator: "The entire platform should be running on TCP/443 and UDP/443 (with TCP/HTTP/80 http
redirecting to the TLS URLs) ... HTTPS, QUIC, WebRTC, MoQ, TURN?, etc should all be on port 443",
and "Bulletproof extends to the network layer and 'it just works'. Let's get QUIC / HTTP/3 working
for the site/interface as well. If we _should_ have a turn server - then we should have a turn server
built in as well." The product rule for every live picture is MoQ, then WebRTC, then frames.

What is true today:

- MoQ listens on its own UDP port (`MULTIVIEW_MOQ_BIND`, `crates/multiview-control/src/moq_startup.rs:44`)
  through vendored `moq-tokio`/`web-transport-moq` on the QUIC stack `moq-noq` 1.3.5
  (`crates/multiview-moq/src/provider.rs:962`). WebRTC owns one dual-stack UDP socket,
  `webrtc.udp_port` default 8189 (`crates/multiview-webrtc/src/config.rs:25`,
  `transport/mod.rs:835`, `transport/mod.rs:912`). On mda001 the estate firewall forwarded neither
  UDP 4443 nor anything except 8189 and the SRT grant, so no outside viewer ever got MoQ, and nothing
  reported it (#280).
- nginx 1.26 terminates TCP 443 and proxies to the engine on `[::1]:8080`; it has no HTTP/3 listener.
  The engine's own optional rustls listener pins ALPN to `http/1.1` (`crates/multiview-control/src/lib.rs:897`)
  because hyper's HTTP/2 server has no header-read timeout (ADR-W031). The per-IP limiter keys on the
  TCP peer (`crates/multiview-control/src/limits.rs:589`), so behind nginx every client is `::1`.
- The engine has a TURN **client** (`crates/multiview-webrtc/src/turn/`), no TURN server. The WHEP
  player takes an ICE configuration, default host-only (`web/src/preview/WhepPlayer.tsx:108`).
- The vendored `moq-tokio` server already gives noq an engine-owned socket abstraction
  (`vendor/moq-tokio/src/noq.rs:572-637`, `noq::Endpoint::new_with_abstract_socket` at `:771`).

Constraints: invariants #1 (output clock never blocks on a client) and #10 (bounded queues drop,
the engine never awaits a client); IPv6-first binds; default build LGPL-clean and free of native
dependencies; per-object authorization on every resource id; TLS is required and ACME (DNS-01
preferred, TLS-ALPN-01 allowed; Decision H), with the existing static mode kept for air-gapped sites
until #282 is decided.

## Decision

**The engine is the network front door.** It binds `[::]:443/udp`, `[::]:443/tcp` and `[::]:80/tcp`
itself (systemd socket activation on Linux, section 9 of the note), and every browser-facing
protocol except SRT and mDNS rides those sockets. nginx becomes optional; a `behind_proxy` mode keeps
a reverse proxy on TCP 443 while UDP 443 is forwarded untouched to the engine.

A. **One UDP 443 socket, demultiplexed by first byte (RFC 9443 section 3).** A `udp-door` task in a
new crate `multiview-frontdoor` owns the socket, reads in recvmmsg/GRO batches (`UdpSocketState`
from `moq-noq-udp`, already a dependency), and classifies each datagram: 0-3 STUN, 20-63 DTLS,
128-191 RTP/RTCP go to the WebRTC lane (the existing `UnifiedEndpoint` and its str0m demux);
64-127 and 192-255 go to the QUIC lane (64-79 to the TURN client instead when the source is a TURN
server it allocated on); 4-19 are dropped. The QUIC endpoint is built with
`EndpointConfig::grease_quic_bit(false)`: RFC 9443 sections 1 and 3 require a multiplexing endpoint
not to send `grease_quic_bit` (RFC 9287); then compliant v1/v2 packets never use 128-191 (a Version
Negotiation packet, which fixes only its top bit per RFC 8999 section 5.3, or a noncompliant packet
with the fixed bit cleared, is dropped, never delivered to QUIC). Each lane has two bounded queues:
established (4-tuples the consumers mark only after validation: ICE connected with STUN
MESSAGE-INTEGRITY, or a confirmed QUIC handshake; bounded by the session caps, never cleared by
traffic) and new (behind a per-source token bucket with LRU eviction and a global budget). All fed
by `try_send`; full means drop and count. Version Negotiation and stateless resets, which noq sends
outside the 3x amplification limit, pass the new-flow limits, and the endpoint-wide
`min_reset_interval` is set explicitly. Sends bypass the dispatcher and go straight to the socket
with GSO. The QUIC stack sees the socket only through `AsyncUdpSocket`/`UdpSender`. The whole front
door runs on its own Tokio runtime (`network.workers` OS threads), never on the runtime that drives
the output clock: today that clock is a `tokio::spawn`ed task on the main runtime
(`crates/multiview-engine/src/programset.rs:546`, `runtime.rs:125`, `crates/multiview-cli/src/main.rs:124`).
On Linux, validated flows are steered to their own socket in a `SO_REUSEPORT` group, so a flood of
unvalidated traffic fills a kernel queue no paying viewer shares. The steering program is classic BPF
(`SO_ATTACH_REUSEPORT_CBPF`), not eBPF: it needs no `CAP_BPF`/`CAP_SYS_ADMIN` and works with
`kernel.unprivileged_bpf_disabled=1` (slice 3, issue #289).

B. **HTTP/3 for the site: the engine terminates TLS (shape 1).** One QUIC endpoint (ALPN `h3`, the
same ACME certificate as TCP) serves both the site/API and MoQ. Browsers do not pool WebTransport
with page requests by default (Chromium `allow_pooling = false`; the MoQ client sets
`allowPooling: false`), so in practice MoQ and the page use separate connections to the same
endpoint. When a client does pool, the connection keeps serving page requests alongside one
WebTransport session; the server advertises one session per connection and refuses a second CONNECT
on it (h3-webtransport 0.1.2 owns the connection per session, so more would need a fork that no
fallback row requires). Per request it routes extended CONNECT `:protocol=webtransport` under
`/moq/` to MoQ and everything else to the existing axum router. The h3 settings are set explicitly
(extended CONNECT, H3_DATAGRAM, one WebTransport session per connection, `max_field_section_size` 16 KiB; h3's
defaults are off or unbounded). The WebTransport draft version emitted is pinned and measured per
browser. This
needs hyperium `h3` 0.0.8 + `h3-webtransport` 0.1.2 + `h3-datagram` 0.0.2 (MIT, pure Rust) and a noq
adapter (h3-quinn 0.0.10 ported with three edits, vendored as `multiview-h3`), because the vendored
`web-transport-moq` accepts only a CONNECT on a connection (its docs send same-port HTTP/3 to
`h3-webtransport`). TCP 443 serves HTTP/1.1 now, plus `h2` once an HTTP/2 floor exists (slice 7). TCP
80 answers 301 to `https://<network.public_host>` built from config. `Alt-Svc: h3=":443"` is sent
only while the reachability self-test does not say UDP 443 is unreachable. 0-RTT is off. HSTS is
sent only in the `serving` state (Decision H).

C. **WebRTC on 443.** Host candidates on UDP 443 from the shared socket, plus ICE-TCP passive
candidates (RFC 6544) on TCP 443 using str0m's `Protocol::Tcp` with RFC 4571 framing done by the
front door. The TCP 443 listener tells ICE-TCP from TLS by the first byte: 0x16 is a TLS record,
0x00-0x05 is an RFC 4571 length prefix whose first frame (a STUN Binding Request for a live ufrag,
within 5 s) is at most 1535 bytes, later frames up to the RFC 4571 maximum of 65 535; anything else
closes.

D. **Built-in TURN: yes, minimal, and last.** A TURN-over-TLS server on TCP 443, routed by SNI
`turn.<host>`, implementing the RFC 8656 subset Allocate/Refresh/CreatePermission/Send/Data/ChannelBind
over the TLS stream, relaying **only** in process to the engine's own WebRTC endpoint (synthetic
relayed address; CreatePermission for any other peer is 403), with REST ephemeral credentials whose
expiry is the preview lease's own and which die with the lease. Long-term credential flow per RFC
8489 section 9.2 / RFC 8656 section 7.2 (401 with REALM and a stateless NONCE, 438 on a stale nonce,
MESSAGE-INTEGRITY on every authenticated request, FINGERPRINT verified when present). The
certificate carries the `turn.<host>` SAN, added to the ACME order when TURN is enabled. No TURN over UDP. Reusing the existing codecs in `turn/message.rs`.

E. **TCP 443 demux bounds.** Global and per-IP connection caps enforced at accept, before the first
byte; 5 s first-byte deadline; one-byte `peek` classification; 10 s TLS handshake deadline;
ClientHello routing: ALPN exactly `acme-tls/1` to the TLS-ALPN-01 responder, SNI `turn.<host>` to
TURN, else the site (`http/1.1`, later `h2`); the ADR-W031 header-read floor; the ICE-TCP first-frame
rule above. `behind_proxy` mode trusts PROXY v2 only from `trusted_proxies` and fails closed.

F. **Operations.** Linux: `conspect.socket` (`ListenStream=[::]:443`, `[::]:80`,
`ListenDatagram=[::]:443`, `BindIPv6Only=both`, `ReceiveBuffer=8M`); the service keeps
`User=operations`, `AmbientCapabilities=CAP_NET_BIND_SERVICE` is the fallback. Sockets survive an
engine restart. One `[network]` config section replaces `webrtc.udp_port`,
`webrtc.advertised_addresses` and the `MULTIVIEW_MOQ_BIND/_ENDPOINT` variables (warned for one
release, then rejected). `MULTIVIEW_MOQ_CERTIFICATE/_PRIVATE_KEY` and `[control.tls] mode = "static"`
stay until #282 is decided (Decision H). Defaults: 443/80 on, legacy ports off.
`/api/v1/capabilities` reports `network.udp443 = reachable | unverified | unreachable`: `reachable`
while an external QUIC handshake or an external probe succeeded within the last 24 h, then back to
`unverified`; `unreachable` only from a failed external probe. Changes are pushed on the realtime
channel. The UI shows a banner while UDP 443 is not `reachable` 15 minutes after startup; `Alt-Svc`
and the MoQ rung are withheld only while it is `unreachable`.

G. **Fallback.** The matrix in the design note, section 12, is normative. Targets: MoQ in <= 1.5 s
where UDP 443 passes; WebRTC over TCP in <= 3 s where it does not (today: 4 s MoQ timeout, then a 60 s
hold); WebRTC via TURN-TLS in <= 4 s through a TLS-only proxy; stills in <= 2 s otherwise.

H. **TLS is required, and it is ACME.** Operator, verbatim, 2026-11-12: "With regard to TLS - we
are going to be opinionated. It's _required_ and it must be ACME. `DNS-01` preferred (as we will use)
or `TLS-ALPN-01`."; "And later later - a company's own ACME server if they wish to use internal
certs"; "So, I guess we should startup with a self generated certificate? And the only thing the user
can do is set up their ACME (unless it's in the config already and we can generate?)"; "DNS-01
support - first provider is Cloudflare - Global Key or Token, and we will add more later"; "Okay, so
Let's Encrypt, Google and Custom ACME from day one as you flagged". Design note section 5a carries
the detail. In short:
- One `CertificateSource` seam (current `Arc<CertifiedKey>` + change notification) feeds TCP 443,
  the QUIC endpoint and TURN-TLS. ACME is the only product implementation; an installed-certificate
  source for air-gapped sites (#282, backlog) would be an addition behind the same seam. WebRTC DTLS
  keeps its per-session self-signed certificate with the fingerprint in the SDP (RFC 8827): that is
  the protocol, not an exception.
- Two states. `setup`: a self-generated bootstrap certificate (key 0600, fingerprint in log and
  `multiview tls status`), TCP 443 serves only the authenticated certificate setup surface (admin
  credential or a one-time setup code printed on the host, attempts bounded), QUIC is not started,
  everything else answers 503 `/problems/tls-setup-required`, no HSTS. If the config already holds a
  complete ACME setup, issuance starts at boot with no UI step. `serving`: the ACME certificate,
  everything on. `setup` -> `serving` is a hot swap; `serving` never falls back to the bootstrap
  certificate. The output clock, inputs and outputs run in both states. The bootstrap key is new on
  every boot and its SAN is the public host only.
- Challenges: `DNS-01` (Cloudflare first, API token or global key, behind a four-call provider
  interface: validate, publish, confirm at the authoritative servers, remove) and `TLS-ALPN-01` (RFC
  8737, answered on TCP 443 for ALPN `acme-tls/1` in both states). `HTTP-01` is not supported.
- CAs as data, all three from day one: `letsencrypt`, `google` (EAB required), `custom` (directory
  URL, optional EAB, a PEM trust anchor for the directory; never an accept-invalid switch). Account
  per directory. ARI (RFC 9773) where offered, else one third of lifetime remaining.
- Secrets are never in the TOML (env-var names, or a CLI-written 0600 file), never logged, never in
  an API response, and never typed into the setup page: on the bootstrap certificate an interceptor
  could read them. The page shows the non-secret settings (CA, challenge, names come from
  `[network.tls]` only, so the config stays the single source of truth; as built in slice 5) and
  starts issuance against credentials already stored on the host. TLS-ALPN-01 needs no secret.
- Renewal failure keeps the current certificate and alarms at 50% and 25% of remaining lifetime.
  After expiry the front door keeps serving the expired certificate and reports `expired`; recovery
  works entirely by CLI and config (an HSTS host cannot be clicked through).
- The plain-HTTP control listener stays loopback-only (`[::1]:8080`) for the CLI and local tests.
- A host that cannot reach any ACME server stays in `setup`. The custom-directory path ships in the
  first TLS slice (tests use it with Pebble), but it is not the air-gapped answer; that is #282.
  `[control.tls] mode = "static"` and the `MULTIVIEW_MOQ_CERTIFICATE`/`_PRIVATE_KEY` files are kept
  until #282 is decided, because they are the only way such a site supplies a certificate today.
- Not doing: automatic fallback from one CA to another.

## Evidence

All spikes ran on 2026-11-11 in a scratch crate (deleted; logs in the lane scratch directory). Clients:
Chrome for Testing 149.0.7827.55, curl 8.14.1 (nghttp3), a native `web-transport-moq` client.

| Claim | Proof |
| --- | --- |
| One UDP socket can carry HTTP/3, WebTransport and WebRTC at once with the vendored QUIC stack | Chrome loaded a page over h3 (`nextHopProtocol=h3`), opened WebTransport (ready 5.7-20.6 ms), and opened a str0m WebRTC datachannel signalled over WT (ICE+DTLS+SCTP in 23-49 ms), all on one port (page and WebTransport on separate QUIC connections; the pooled case is a slice 2 acceptance, not spike evidence); 400/400 WT and 400/400 DC echoes interleaved, 3/3 runs, IPv4 and IPv6. curl `--http3-only`: 200 in 8 ms |
| The vendored stack needs no fork for socket injection | the spike used `noq::Endpoint::new_with_abstract_socket` with its own `AsyncUdpSocket` (receive from a bounded queue) and `UdpSender` (GSO send), the same seam as `vendor/moq-tokio/src/noq.rs:572-637` |
| `web-transport-moq` cannot serve the site | its server reads the first bidirectional stream as CONNECT and fails otherwise (`vendor/web-transport-moq/src/connect.rs`, `Connecting::accept`); its crate docs point same-port HTTP/3 to `h3-webtransport` (`vendor/web-transport-moq/src/lib.rs`) |
| h3 over noq is cheap | h3-quinn 0.0.10 compiled against moq-noq 1.3.5 after 3 edits and served both GET and WebTransport in the spike |
| Misrouted packets are dropped, not mis-delivered | one datagram per first byte 0-255: STUN 4, DTLS 44, RTP 64, QUIC 128, dropped 16; str0m rejected every non-session datagram; noq created 0 connections |
| Flood state stays bounded | single source at about 600 k datagrams/s for 12 s, five packet shapes: per-source bucket dropped 99.7%; noq allocated no state for random supported-version Initials; RSS flat at 9.4-12 MB; an established WT session kept 1000/1000 echoes. Not covered by the spike: multi-source and spoofed floods, unsupported-version Initials (VN replies), short headers with well-formed unknown CIDs (stateless resets); these are slice 3 chaos cases |
| The kernel queue is the weak point | in the same flood, `Udp RcvbufErrors` rose by 2.83 M of 3.80 M sent (rmem_max 212 992): new handshakes took 1.2-13 s and established flows saw stalls up to 4.5 s. Hence slice 3: validated-flow steering to a separate socket (the only layer that also holds against spoofed floods), nftables per-source meter (stops real sources only), `ReceiveBuffer` via systemd; public UDP 443 opens only after slice 3 |
| The output clock is exposed to CPU contention | a 60 Hz std thread in the spike process: late > 2 ms 15-42 idle vs 17-80 under flood per 15 s, max 4.6-8.8 ms vs 7.5-23.7 ms, on a host at load ~75. The production clock is a Tokio task on the shared main runtime, so the front door gets its own runtime and slice 3 must show no increase in "ticks behind" under flood |
| ICE-TCP on the TLS port works in Chrome | answer with only `tcptype passive` candidates on the shared TCP port: DC open 42-54 ms, 100/100 echo, v4 and v6; `openssl s_client` on the same port classified as TLS |
| ICE-TCP covers plain HTTP proxies, not TLS-only ones | Chrome behind a CONNECT proxy reached the ICE-TCP candidate through the proxy (Chromium routes P2P TCP through `ProxyResolvingClientSocketFactory`, `services/network/p2p/socket_tcp.cc`) with both `default` and `disable_non_proxied_udp` policies; through a proxy that passes only TLS the tunnel's first byte was 0x00, it was dropped, ICE failed |
| TURN-TLS passes a TLS-only proxy, and must be routed by SNI | Chrome 149's `turns:` ClientHello went through the TLS-only proxy; it carries SNI and **no ALPN extension** |
| Under `disable_non_proxied_udp` Chrome gathers no UDP | Chromium `port_allocator.cc` sets `PORTALLOCATOR_DISABLE_UDP | DISABLE_STUN | DISABLE_UDP_RELAY` ([source](https://chromium.googlesource.com/chromium/src/+/main/third_party/blink/renderer/platform/p2p/port_allocator.cc)); policy text ([WebRtcIPHandling](https://chromium.googlesource.com/chromium/src/+/main/components/policy/resources/templates/policy_definitions/WebRtc/WebRtcIPHandling.yaml)), RFC 8828 section 5.2 mode 4 |
| nginx cannot carry MoQ | `ngx_http_v3_module` is experimental and has no extended CONNECT / WebTransport ([docs](https://nginx.org/en/docs/http/ngx_http_v3_module.html), [nginx#1429](https://github.com/nginx/nginx/issues/1429)) |
| No HTTP/3 or WebTransport in `setup` | Chromium processes `Alt-Svc` only when the response has no certificate error (`net/http/http_network_transaction.cc`, `!IsCertStatusError`); a WebTransport handshake has no interstitial to click through. So QUIC starts only in `serving` |
| No click-through for an HSTS host | RFC 6797 section 12.1 ("no user recourse"): recovery after expiry must work without a browser |
| IP-address certificates need TLS-ALPN-01 | Let's Encrypt IP certificates (GA 2026-01-15) are short-lived only and validate with `http-01` or `tls-alpn-01`, never `dns-01` ([LE 2025-07-01](https://letsencrypt.org/2025/07/01/issuing-our-first-ip-address-certificate), [LE 2026-01-15](https://letsencrypt.org/2026/01/15/6day-and-ip-general-availability)) |
| Google requires EAB; both CAs offer ARI | live directories, 2026-11-12: `dv.acme-v02.api.pki.goog` and its `test-api` staging both `externalAccountRequired: true`, `renewalInfo` present, profiles `minimal`/`standard`; Let's Encrypt `renewalInfo` present, profiles `classic`/`shortlived`/`tlsserver`. EAB secrets come from `gcloud publicca external-account-keys create` and expire unused after 7 days ([Google Cloud docs](https://docs.cloud.google.com/certificate-manager/docs/public-ca-tutorial)) |
| Cloudflare token scope | `Zone - DNS - Edit` + `Zone - Zone - Read` ([cert-manager guide](https://cert-manager.io/docs/configuration/acme/dns01/cloudflare/)) |
| Pebble covers the CI path | Pebble validates TLS-ALPN-01 on its `tlsPort` and supports `externalAccountBindingRequired` ([letsencrypt/pebble](https://github.com/letsencrypt/pebble), `test/config/pebble-config-external-account-bindings.json`) |
| Firefox does ICE-TCP to a passive server candidate | [Mozilla WebRTC blog, Firefox 54](https://blog.mozilla.org/webrtc/active-ice-tcp-punch-firewalls-directly/). Firefox via HTTP proxy and Safari ICE-TCP: unverified, measured in the office matrix before slice 4 closes |

## Rationale

- **Why engine-owned UDP 443 is forced.** MoQ is WebTransport, which is HTTP/3. Site HTTP/3 is also
  HTTP/3, on the same port, certificate and ALPN, and only one process can own the UDP port (even
  though browsers usually give WebTransport its own connection). nginx cannot proxy WebTransport, so that process is the engine, and
  for HTTP/3 to work for the site the engine must answer site requests over HTTP/3 too. Doing TCP 443
  in the engine as well then removes the only reason for nginx (one certificate, one limiter keyed
  on the real client, one place for ACME TLS-ALPN-01) and is what ICE-TCP and TURN-TLS need.
- **Why first-byte demux and not separate sockets.** Separate ports are what failed in #280. RFC 9443
  makes the single port well defined, provided QUIC-bit greasing is off.
- **Why TURN at all.** Host UDP 443 plus ICE-TCP 443 reaches the engine from every network where the
  browser can open a socket to it, directly or through a plain CONNECT proxy (measured). They fail
  exactly where a proxy admits only real TLS (measured) or where ICE-TCP is disabled in the client.
  `turns:` passes there (measured). Without TURN those viewers drop to stills, which "it just works"
  rules out.
- **Why TURN is minimal.** The only peer is the engine itself. A general relay would be an open-relay
  and amplification risk with no matching row in the fallback matrix. UDP TURN would duplicate the
  host candidate on every network that passes UDP 443.
- **Why not HTTP/2 now.** ADR-W031's reason stands: hyper's h2 server has no header-read timeout. h3
  covers multiplexing for browsers that reach UDP 443. h2 arrives with its own floor (slice 7).

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| nginx keeps TCP 443 and adds `listen 443 quic` for the site; the engine keeps MoQ on another UDP port | two UDP listeners cannot share 443; MoQ on a non-443 port is the #280 failure; nginx cannot proxy WebTransport |
| nginx `stream` (L4) proxy on UDP 443 that splits by SNI to nginx-h3 or the engine | QUIC Initials are encrypted with version-derived keys; SNI routing needs QUIC-LB/Initial decryption that nginx `stream` does not do for UDP and that breaks on connection migration; adds a hop and a second TLS owner |
| Keep `web-transport-moq` and serve the site only over TCP | breaks "QUIC / HTTP/3 for the site"; the browser would still reuse the QUIC connection for the origin once Alt-Svc is known and get errors |
| MoQ on a WebSocket fallback over TCP 443 | `web/src/preview/moqConnection.ts` disables it on purpose (pinned client, no WebSocket race); WebRTC over ICE-TCP already covers no-UDP networks with real-time media |
| Run coturn (or another external TURN server) | GPL-free but a native C dependency and a second daemon; a general relay that must then be fenced; still needs the engine's TLS on 443, so it cannot share the port without the same demux |
| TURN over UDP 443 and TCP 443 (plain) | no network in the matrix needs them: UDP 443 reaches the host candidate, plain TCP 443 reaches ICE-TCP |
| A TLS-off, self-signed, or bring-your-own-certificate product mode | operator directive: TLS required, ACME only; the bootstrap certificate exists only in `setup` and serves only the setup surface |
| `CAP_NET_BIND_SERVICE` only, no socket activation | works, but loses socket survival across restarts and puts the privilege in the long-running process; kept as the fallback |
| 0-RTT on | replay of non-idempotent authenticated API calls (RFC 9001 section 9.2) for no measured gain |

## Consequences

- New crates: `multiview-frontdoor` (sockets, demux, lanes, TCP door, reachability), `multiview-h3`
  (vendored adapter, slice 2). New pure-Rust deps: `h3`, `h3-webtransport`, `h3-datagram` (MIT); they
  sit behind the existing `moq-transport` / `tls` feature gates, so a default `cargo check` stays free
  of native dependencies. `cargo deny` gates them.
- `vendor/moq-tokio` gains a small change to accept an injected `AsyncUdpSocket` (slice 1), recorded
  in `vendor/MOQ-DELIVERY-PROVENANCE.md`.
- `webrtc.udp_port`, `MULTIVIEW_MOQ_BIND`, `MULTIVIEW_MOQ_ENDPOINT`, `MULTIVIEW_MOQ_CERTIFICATE`,
  `MULTIVIEW_MOQ_PRIVATE_KEY` are deprecated (warn one release, then rejected). Operators move
  firewall rules from UDP 8189/4443/9001 to UDP 443.
- A volumetric flood can still exhaust the host's network capacity; the engine cannot fix that and
  says so. What the design does guarantee: the engine is not an amplifier, every userspace structure
  is bounded and cannot be flushed by traffic, unvalidated traffic cannot reach the kernel queue of
  validated flows on Linux, and the front door shares no runtime with the output clock. Paying
  viewers' media continuity under a spoofed flood is a measured slice 3 acceptance, and public UDP
  443 waits for it.
- Browser coverage is measured with Chrome 149 only. Firefox, Safari, ICE-TCP through proxies other
  than the test proxy, and the WebTransport draft each browser speaks are office-matrix acceptances
  in slices 2, 4 and 8; every `serving` row of the fallback matrix ends at stills at worst, never at
  no picture.
- The ADR-W031 HTTP/1-only floor stays until slice 7.
- Issues #167-#172 are re-cut into slice 5 (the design note lists what stands and what changes in
  each). The first TLS slice ships all three CA profiles and both challenges.
- Out of scope / known limit: air-gapped servers with no ACME server (#282, backlog). Until it is
  built, such a server stays in `setup` once the static mode is retired, so the static mode is not
  retired before #282 is decided (slice 10).
- Safari's ICE-TCP behaviour, Firefox behind a proxy, and TLS-intercepting proxies (N6) are
  unverified until the office matrix runs. N6 may end at stills, with an honest caption.
- The operator-facing runbook for the mda001 cutover (nginx removal, socket unit, firewall) is
  [front-door-cutover.md](../runbooks/front-door-cutover.md) (slice 6, #292). As built, the socket
  unit passes UDP 443 as a `ReusePort=yes` pair so validated-flow steering survives socket
  activation (design note, "Slice 6 as built").
- As built (slice 2, #288): the vendored `multiview-h3` adapter includes its own WebTransport
  session layer (derived from h3-webtransport 0.1.2) instead of upstream's `WebTransportSession`,
  which takes the whole h3 connection; ours leaves the connection in its driver, so pages keep
  being served and a second CONNECT is answered `429` while a session lives (design note,
  "Slice 2 as built").
