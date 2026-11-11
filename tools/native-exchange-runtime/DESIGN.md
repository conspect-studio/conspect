# R3 boundary and evidence limits

The new seam runs only on bounded egress and receiver workers. It must not run
on the fixed output clock: encryption allocates, durable counter/replay/receipt
transactions call fsync, and lease renew uses HTTP. A consumer must drop bounded
queue overflow, not back-pressure the clock. The test exercises queue overflow
and independent taker deny with actual encoded/decode media; this is a bounded
chaos case, not a production soak.

Authentication order: verified MG02 -> MC02 identity/AEAD/replay validation ->
durable replay transaction -> hang parse -> native decode -> advancing decoded
observation -> permission fence -> render/audio queue. Every presentation/audio
callback must fence again. Caller-supplied HTTP ids have no cryptographic identity
conversion. Trusted authority mapping supplies raw16 bindings.

State locks serialize one runtime lifetime per state path. Rename + file fsync +
directory fsync precede plaintext release. A crash can discard an object but must
not replay an object already released. Durable sticky deny closes a reconnect.
Keys remain only in the canonical receiver and never in state or logs. Local
wall time and monotonic time independently close permission. The application
must run an independent expiry timer even if no more ciphertext arrives.

This first slice exposes the real publish/receive callsites but does not install
an application scheduler or physical native sink. It does not prove release,
withdrawal, exhaustion, server entitlement mapping, device buffer latency, QUIC
wire interoperability, or an existing established production session cutoff.
The canonical crypto crate's lease tests are not a substitute for those proofs.
