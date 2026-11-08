# Local MoQ encoded-media fixture

Development/test only: **not an authenticated production provider**. Uses pinned
moq-net 0.3.21, moq-tokio 0.19.34 and hang 0.21.22; Cargo.lock pins transitive
versions. QUIC only (WebSocket/TCP features disabled), IPv6-first `[::]:4443`.
Select exactly draft 16 or 18; no implicit draft14 downgrade.

Generate the input once (FFmpeg with libvpx required):

```sh
mkdir -p tools/moq-fixture/local
ffmpeg -v error -f lavfi -i testsrc2=size=320x180:rate=30 -t 2 \
  -c:v libvpx -g 30 -deadline realtime -f ivf tools/moq-fixture/local/fixture.ivf
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout tools/moq-fixture/local/key.pem -out tools/moq-fixture/local/cert.pem \
  -days 10 -subj /CN=localhost -addext 'subjectAltName=DNS:localhost,IP:::1'
cargo run --locked --manifest-path tools/moq-fixture/Cargo.toml -j 2 -- \
  16 tools/moq-fixture/local/fixture.ivf tools/moq-fixture/local/cert.pem \
  tools/moq-fixture/local/key.pem
```

Subscribe to broadcast `fixture`, catalog `catalog.json`, video `video`.
VP8, 320x180, 30fps, no audio. The hang legacy frame mapping is a QUIC varint
microsecond timestamp followed by encoded VP8. Every keyframe begins a group;
media cache advertises a two-second ceiling. One shared producer loops immutable
encoded bytes regardless of viewer count. No per-viewer encoding occurs.
Certificate SHA256 is obtained with `openssl x509 -in .../cert.pem -outform DER |
openssl dgst -sha256`; pass its bytes as WebTransport `serverCertificateHashes`.
Do not disable browser certificate verification globally.

The reusable `VideoAdapter` accepts immutable encoded-time windows with binding
configuration generation, shared `Bytes`, microsecond timestamps and upstream
verified RAP flags. It validates the complete window (60 packets, 4MiB, 2 seconds)
before writes. Rejection or discontinuity requires RAP recovery; revocation
rejects further windows. The fixture uses this same video adapter.
`av::AvAdapter` additionally prevalidates every configured audio track before any
video write, applies one generation/gap recovery boundary, and caps combined
packets/bytes/timestamp span. Network writes are not atomic: a downstream error
can expose a prefix, explicitly forcing discontinuity. Upstream atomic admission,
source-demand fencing and global retained allocation budgets remain external.
The executable and browser evidence are video-only; audio decode is not proven.

## Measured local evidence (2026-11-08)

Browser owner ran the real Chromium headless shell against both explicit drafts.
Final draft16: 30 painted VP8 VideoFrames, timestamps 255000000..256466666us.
Final draft18: 30 painted VP8 VideoFrames, advancing timestamps, negotiated
`moq-transport-18`. Both used the pinned one-shot @moq/net client, WebTransport,
certificate hashes, no WebSocket fallback. Probe/log artifacts are maintained by
the browser owner. This evidence does not cover Cloudflare, audio, encryption,
production authorization or production feed integration.

Sessions are capped at eight and expire after 60 seconds. Input is capped at
16MiB and individual frames at 1MiB. This fixture has no production admission,
credential validation, source-demand ownership or revocation fence. Local
interop does not establish Cloudflare interop, authorization, or production
slow-consumer safety. Do not advertise product capabilities from its existence.
