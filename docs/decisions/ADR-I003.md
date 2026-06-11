# ADR-I003: Control persistence — SQLite/sqlx behind an off-by-default `sqlite` feature; in-memory trait Repository is the tested default; scoped cargo-deny ignore of RUSTSEC-2024-0436

- **Status:** Accepted
- **Area:** Implementation Build-out
- **Date:** 2026-06-03
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)
- **Realizes / refines:** [ADR-W006](ADR-W006.md) (SQLite via sqlx), [ADR-W002](ADR-W002.md) (utoipa); guardrail [ADR-G004](ADR-G004.md) (no silent suppression / supply-chain)

## Decision

`multiview-control` defines persistence behind a `Repository` trait. The default, always-compiled implementation is an **in-memory store** — it is the one exercised by the crate's tests and by the default `cargo build`/`cargo check`. The SQLite/sqlx backend specified by ADR-W006 is implemented but kept behind an **OFF-BY-DEFAULT `sqlite` feature**, because SQLite's license (public-domain "blessing", not an SPDX identifier on the cargo-deny allowlist) is not currently in the workspace `deny.toml` allowlist, and we will not add a license exception or vendor the amalgamation as part of the foundation build-out. Separately, the build-out records one **scoped, justified `cargo deny` ignore: `RUSTSEC-2024-0436`** — the compile-time-only `paste` crate is unmaintained and reaches the tree transitively through `utoipa-axum`, with no safe upgrade available. The ignore is documented inline in `deny.toml` with this rationale and is the only advisory ignore.

## Rationale

The default build must stay `cargo deny`-clean (guardrail: secrets/supply-chain, ADR-G004), pure-Rust, and GPU/native-dep-free so it remains the CI enabler for the whole workspace. Gating `sqlite` keeps the SQLite license question out of the default deny graph until it is resolved deliberately, while still letting ADR-W006's design be built and tested under the feature. A trait-based in-memory `Repository` lets every control-plane test (routes, ETag/If-Match concurrency, command-bus shell) run with no database, no migrations, and no native compile step — fast, deterministic, and deny-clean. `RUSTSEC-2024-0436` is `paste`, a proc-macro that runs only at compile time (no runtime attack surface) and is pulled in transitively by `utoipa-axum`, which we depend on per ADR-W002; there is no maintained drop-in and no upstream version that drops it, so a *scoped* ignore with an inline justification is the correct, non-silent way to keep `cargo deny` green — exactly the "allow-with-justification, fix root cause when possible" rule from the guardrails rather than a blanket suppression.

## Alternatives considered

- **Add SQLite's license to the deny allowlist now** — rejected for the foundation pass: that is a deliberate licensing decision to make explicitly, not a side effect of wiring persistence; gating defers it cleanly.
- **Make `sqlite` the default backend** — rejected: pulls SQLite into the default deny graph and adds a native build step to the inner-loop CI before the licensing call is made.
- **Drop `utoipa-axum` to avoid `paste`** — rejected: ADR-W002 commits to utoipa + utoipa-axum for OpenAPI; the dependency is load-bearing and the advisory is compile-time-only.
- **Globally ignore unmaintained-crate advisories** — rejected: violates no-silent-suppression; the ignore is scoped to the single advisory id with an inline reason.
- **`rusqlite` behind a DB actor / flat files / sled** — already weighed and rejected in ADR-W006.

## Consequences

- Production persistence requires building with `--features sqlite`; the default binary persists in memory only and loses state on restart. This is acceptable for the build-out and for tests; deployments must opt in.
- The `Repository` trait is the seam that keeps the in-memory and SQLite backends interchangeable, so both must honor the same ETag/version and atomicity contract from ADR-W006.
- The `RUSTSEC-2024-0436` ignore must be revisited whenever `utoipa-axum` updates or `paste` gains a maintained successor; it is the single tracked advisory exception and should not become a precedent for unscoped ignores.
- Enabling SQLite by default later (resolving the license question) is a follow-up that re-converges with ADR-W006's intent; until then ADR-W006 documents the target and this ADR documents the gated as-built state.
