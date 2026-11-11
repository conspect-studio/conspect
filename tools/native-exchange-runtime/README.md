# Native exchange first connected slice (experimental, OFF)

This standalone runtime uses the actual pinned MoQ model (`moq-net 0.3.21`)
and hang container (`0.21.22`) from runtime b53/444/e52. It does not copy an
old CLI pipeline. The production media callsite is `publish`: complete hang
serialization -> canonical MC02 seal -> actual group `write_frame`. The native
receive seam is `ReceiverRuntime::open`: MG02-verified receiver -> MC02 decrypt
-> durable replay commit -> caller hang parse and native decoder. Authority raw
16-byte `Binding` values are explicit; opaque HTTP identifiers are never converted.

`ReceiverRuntime` keeps an exclusive state lock, owner-only state parent, durable
per-scope replay counters, and a sticky deny across reconnections/restarts. State
contains no key material. Keep it on persistent storage. State deletion is not a
reconnect. Do not share state paths between takers. An incomplete stage, poison marker, or active marker after an unclean crash
fails closed. Clean healthy shutdown removes the active marker; failed durable
writes leave it. Recovery after a crash requires a separate authority-controlled
procedure, not deletion or automatic grant retry. The bounded replay scope cap deliberately fails closed at 128;
long-running journal compaction remains work.

The native output implements `Flush` and must destroy encoded/decoded/display/
audio buffers synchronously. Call `fence` at every decoder/presentation/audio
boundary and from an independent expiry timer; neither a transport disconnect nor
an encoder tick establishes usable delivery. `decoded` accepts actual decoded
media timestamps. `record_decoded` matches an advancing interval then reserves a
receipt and calls canonical durable `record_and_queue` before any HTTP send.

`renew_publisher_lease` is a CONTROL-WORKER hook only. The canonical authority
publisher checks its local lease fence on every encrypt and grant. Network renew
must never run on the output clock. `next_grant_attempt` returns a bounded
next-epoch timer instant; it does not spin or perform a retry.

## Canonical authority candidate (not approved for a live harness)

The command owns one canonical `PublisherLease`, shared by the bootstrap,
`HttpAuthorityRootProvider`, and the retained `AuthorityRootGuard`. No second
HTTP lease actor or synthetic root/fence exists. A transport gate additionally
pins all four authenticated raw root identities against the trusted registration
mapping; canonical epoch, generation, key and lease parsing remains unchanged.

The old harness contract is insufficient. Before any lease acquisition this
candidate requires `scenario.expectedIdentities.shared.contributorOrg` and
`publisherInstance`, both raw canonical 16-byte base64url mappings. The fresh
fragment must repeat both and all existing shared mappings unchanged. An explicit
`publisherInstance` may equal `publisherMachine` only when the authority contract
says so; the runtime does not infer this equality. The current-root DTO must
include actual `epochStartMs` and `epochEndMs`. These are requested contract
amendments, not claims that the old server emits them. Missing fields fail closed.
The three local command scenarios use synthetic canonical DTOs, not captured
web0790 compatibility evidence. No live call or production gate is authorized.

Native FFmpeg `showinfo` observations supply dimensions, rational frame rate and
advancing decoded PTS. Unknown queue age, bitrate, jitter and backpressure are
`Option::None` and omitted from HTTP quality. Receipt IDs use OS CSPRNG entropy;
the spool persists them before POST. RS05 records encode unknown measurements
and durable ACK accounting high-water explicitly; prior local spool formats fail
closed. ACK validation and exact receipt removal share one durable transaction.
Per-booking ACK totals cannot regress, charged deltas must reconcile, and reserved
balance cannot increase without an explicit funding operation. The server must
still deduplicate stable IDs/sequences. This validates server accounting, never
an engine wallet. Journals persist in the owner-only bootstrap `.runtime-state`
sibling directory, outside temporary FFmpeg files. Decode faults return errors;
RAII shutdown flushes native queues, joins bounded control I/O and releases the lease.
See [source-registration contract](SOURCE-REGISTRATION.md) for required web closure.

## Replay adjudication and panic safety (post-637f5cb3)

The earlier review's ordinary-process-crash/Drop premise is incorrect: SIGKILL
cannot execute Rust destructors. The lifetime `.active` marker was already durable
before `accept` returned, and `open` retained plaintext locally until persistence.
The real gap was an unwind caught outside `open`/`refresh`, followed by a later
explicit healthy Drop, between canonical mutation and the old `persist` entry.

A fsynced poison intent and unhealthy live fence now precede **every** decrypt or
same-epoch renewal. Commit includes replay state and the signed grant expiry;
only a successful directory-fsynced commit restores health. Until then `fence`
denies and renewal retains the old presentation deadline. Canonical crypto APIs
are unchanged. Test-only thread-local hooks panic or park immediately after real
canonical mutation, before state persistence. Tests catch unwind outside the
wrapper, explicitly drop later, and deny old-grant restart; subprocess tests kill
with SIGKILL at both boundaries and verify durable active/poison markers. No
plaintext is returned or presentation acknowledged at those boundaries.

State, journal, spool and manifest reads now use a safe rustix FD-relative private
directory capability. Every path component rejects symlinks; the pinned parent
must be effective-UID-owned mode 0700. Leaf opens use NOFOLLOW and fstat to reject
nonregular, multiply-linked, wrong-owner and non-0600 files. Manifest reads remain
bounded to 64 KiB. Substituting the pathname after opening cannot redirect writes.
ACK regression tests additionally prove empty pending state after restart and
rejection of duplicate ACK/requeue. Test fixture parents explicitly use 0700;
existing expiry, epoch, generation and authority assertions are not relaxed.

Local verification: transaction-boundary unit tests 3/3; private-state unit test
1/1; retained connected tests 5/5; retained TLS authority scenarios 8/8 (108.28s);
nonce journal 4/4, protocol 6/6, receipt 3/3, spool 4/4 and bootstrap 3/3.
A later connected repeat failed the unchanged real monotonic expiry at the media
loop (4/5 passed, 6.47s); the earlier 5/5 pass is not a claim of timing stability.
The unchanged media test then passed in isolation (4.80s); contention sensitivity
remains a validation caveat. No TTL or security assertion was relaxed. Strict runtime library/tests Clippy
passes with warnings denied. Builds used one
job, no debug symbols, isolated local targets; no workspace-wide suite was run.

These are local security repairs, **not** live web contract compatibility or
SECURITY READY approval. The raw identity mappings and epoch-window contract above
remain unresolved. No live harness call or deployment was made.

## Edge-fix verification (post-faed1490)

Use `CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0` for these warm-target commands:

- `cargo test --locked --manifest-path tools/native-exchange-runtime/Cargo.toml --test authority_scenario --test connected -- --test-threads=1`: 8 TLS scenarios and 5 connected tests pass. Actual two-epoch transition persists the new scope before the first decrypt; injected stage failure withholds plaintext and poisons reconnect. Malformed decode returns an error without unwind.
- `cargo test --locked --manifest-path tools/media-crypto/Cargo.toml --test authority_http --test spool`: 29 authority and 4 spool tests pass, including failed-fetch generation adoption and restart accounting regression/stable retry.
- Strict `cargo clippy --locked --manifest-path <crate>/Cargo.toml --all-targets -- -D warnings` passes for both standalone crates.

The initial unpaced eight-scenario parallel TLS run failed three scenarios at
real seal/decrypt epoch deadlines. Serial execution diagnosed fixture resource
contention; it did not establish parallel safety. The TLS fixture now explicitly
admits one six-stream FFmpeg scenario at a time. All authority deadlines and
security assertions remain unchanged. Deliberately late-root responses permit
only expected peer timeout disconnects; command rejection and zero-receipt
assertions still apply. The original default-thread TLS command passes 8/8 with
explicit fixture admission (104.12s). Warm targets total 2.43GiB; measured local
growth during this task is below 0.1GiB. Final host free disk is 368GiB, above
the 360GiB floor. No live harness or deployed DTO compatibility is claimed.

## Earlier local evidence

`CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 cargo test --locked --manifest-path
 tools/native-exchange-runtime/Cargo.toml --test connected -- --nocapture`:
3 targeted tests passed after security repairs (4.92s). Strict all-target Clippy
with `-D warnings`, rustfmt and diff checks passed. Target 928MiB; free disk 432GiB. Six-track decoded advancing intervals: denied taker
`[4,23,23,23,23,23]`; continuing taker `[9,47,47,47,47,47]`. Latest measured
synchronous deny flush: 6797 microseconds, all 16 queued observer entries removed,
per-queue bound 4 and 347 overflow drops. A separate no-arrival timer with frozen
wall time closed at 142126 microseconds for a 100ms remaining grant, including
a same-grant refresh that must not restart the original monotonic deadline. Durable
reconnect replay, sticky deny, immediate wall expiry and a decoded-video durable
receipt spool commit passed. These numbers describe the fixture observer only.

## Honest boundary

No deployed CLI pipeline constructs this runtime yet. The test uses actual MoQ
broadcast/track/group media objects with local IPv6 TCP fixture ciphertext
connections, NOT a QUIC/MoQ network interoperability claim. It decodes actual
HEVC/five AAC access units with local FFmpeg and observes decoded timestamps;
its queue observer is not a physical display/audio device. Controlled-authority
release/withdraw/exhaustion endpoint behaviour, autonomous grant/lease timer
scheduler, production native decoder/presenter implementation, journal compaction,
real output-buffer cutoff, TLS/QUIC integration and application startup wiring
remain required. Do not claim paid delivery, receipt UI, public booking routes,
or production cutoff from this slice. Web0791 remains OFF/503. No live authority
calls, production signing keys, host deployment or application feature flag change.
