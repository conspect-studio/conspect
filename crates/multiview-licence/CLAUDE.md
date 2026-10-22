# multiview-licence — agent notes

Machine-side entitlement plane (Conspect, ADR-0050): signed entitlement/lease resource, Ed25519 lease-verification, enforcement ladder as pure data, fingerprint scoring.
Inv #1 (never off air): computes data and verifies signatures only — no engine dependency, no engine handle, no process control, no I/O. Do not add a network call or anything that could stop/stall output.
Data minimisation: fingerprint scoring is over salted digests handed in — never gather raw serials/MACs here. Verification-only, no key generation/RNG in non-test code.
Constants are exact (`constants.rs`) — never restate/round lease terms or thresholds elsewhere; property tests pin day boundaries, never weaken them.
The heartbeat network client (`heartbeat.rs`, off-by-default `heartbeat` feature) keeps last-good on every failure/withheld lease — never off air. Live HTTP transport lives in the cli, not here.
Read first: [ADR-0050](../../docs/decisions/ADR-0050.md), [ADR-0096](../../docs/decisions/ADR-0096.md) before touching heartbeat.
