# Native authority harness client: first slice only

This slice does not execute an authority scenario. No authority harness or
production request has been made. Paid delivery remains disabled. The native
base is 0029a549 (TCP first slice, not QUIC or device evidence); its separate
security review is not replaced by this work.

## Explicit test CA

`VerifiedHttpsTransport::new()` retains default trust roots.
`VerifiedHttpsTransport::from_trusted_ca_pem(pem_bytes)` adds exactly one supplied
PEM certificate to that client only. It uses rustls with ordinary certificate
chain, time, hostname and IP SAN verification. No global trust installation,
environment trust override or certificate-verification bypass is needed.
The future manifest adapter must pass the manifest's public test CA bytes to
this constructor. The offline CLI does not yet construct a transport.

Actual local IPv6-loopback TLS tests generate ephemeral synthetic CA/server
keys. They verify explicit CA + correct IP SAN succeeds, default roots reject
that CA, and explicit CA still rejects a wrong SAN. No secret headers are logged.

## Offline entry

From this worktree:

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo run --locked \
  --manifest-path tools/media-crypto/Cargo.toml \
  --bin native-authority-scenario -- --check-manifest /secure/manifest.json
```

Only a manifest path is accepted, never credential values. The file must be a
regular file owned by the effective user, mode exactly 0600, with one hard link
and at most 64 KiB. Symlinks are refused at open; checks use the opened descriptor.
Bytes are zeroized on drop. JSON syntax is checked without extracting fields or
printing serde errors, paths, URLs or credentials. Successful output explicitly
states schema/live execution are unsupported. No network code is reachable.

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo test --locked \
  --manifest-path tools/media-crypto/Cargo.toml \
  --test verified_https --bin native-authority-scenario
```

## Not ready for controlled authority proof

After this first slice was implemented, web memory
`01a1c21e-9d6c-77f3-9a04-fe78242dd215` supplied the base manifest schema and
real action routes. It still lacks an independent trusted mapping for all raw16
booking/taker/receiver-engine identities, and lacks the withdrawal story ID.
The mapping must also cover feed/version/publisher, authorization generation 0
and current publisher generation. Never derive expected binding IDs from the
same signed grant being verified. Its `transport.identityNote` describes wire
offsets, not an independent authorization source. The parser deliberately does
not claim schema agreement until that mapping DTO is available.

The handoff also describes a pre-held publisher lease but omits expiry and
not-before metadata required to adopt it safely. Reacquisition after expiry must
not bypass the existing process-local fence/revocation contract. Resolve that
handoff before live execution. No endpoint or missing field will be invented.

Still missing: schema-aware parser, authority publisher lease/root adapter into
the native coordinator, bounded off-clock renewal/grant scheduler, two-taker
binding validation, HEVC plus five AAC authority scenario, real release/withdraw/
exhaustion measurements, separate target TCP reachability check, and controlled
live execution after the captain supplies a reviewed launch manifest. A parsed
JSON file or local TLS test is not real-D1 connected media proof.
