# Harness bootstrap baseline — NOT connected native proof

Contract: web harness `99688907`, memory `01a1c26a-cfc6-7a53-800c-1e4d0e4b1a3b`.
The previous active-lease handoff is rejected. No live authority calls were made during implementation.

Build with one job and no incremental artifacts:

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo build --locked --manifest-path tools/media-crypto/Cargo.toml --bin native-authority-scenario
```

**Do not launch until independent review and secure manifest handoff.**
Proposed baseline command (paths only; no secrets in arguments):

```sh
tools/media-crypto/target/debug/native-authority-scenario --bootstrap /secure/manifest.json --marker /secure/bookings.json --expected /secure/expected.json
```

This is a real HTTPS control-plane bootstrap, not a completed scenario command.
It acquires a fresh CSPRNG process nonce and opaque server generation, creates two
60-second bookings, mints and discards initial grants for receiver mapping, writes
an exclusive 0600 marker with exactly `{bookingA,bookingB}`, and waits at most
120 seconds for the independent fragment. The coordinator renews its lease and
extends bookings while it waits. One-second HTTP timeouts bound coordinator work;
no output clock exists in this binary and no media worker waits on it.
The captain copies the marker to web and the atomic D1-derived fragment back;
the containers do not share a filesystem. File opens reject symlinks, hardlinks,
wrong owner, permissive mode, nonregular files and oversized data. Never reuse
an old marker path. Expected identity values must be canonical raw16 base64url,
non-aliased, and refer to the exact fresh booking IDs.

**Exit status remains 1 even after successful bootstrap mapping validation.**
The program releases its lease and says the native scenario is not implemented.
It does not claim actual-grant validation, connected HEVC/AAC decoding, native
runtime integration, release/withdraw/exhaustion measurements, QUIC or device
presentation. Those remain required. The existing native `0029a549` library is
not imported by this baseline. Do not treat syntax checks or mapping validation
as the missing connected-media proof. The paid-delivery gate stays OFF.

Tests use a real local TLS socket, real certificate verification, actual JSON
wire acquire/book/renew/release, server-chosen opaque generation 793, and assert
marker and identity boundary behavior. They are not cryptographic media proof.
