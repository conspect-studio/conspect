# ADR-G001: Absolute typing enforced via centralized workspace lints + TS strictTypeChecked, blocking in CI

- **Status:** Proposed
- **Area:** Engineering Guardrails
- **Date:** 2026-06-02
- **Source:** [agent-guardrails.md](../development/agent-guardrails.md)

## Decision

Enforce "no untyped / no escape hatches" for AI-authored code at two layers: (1) compiler/type-checker settings that make untyped code fail to build, and (2) linters denied in CI. Rust: centralize lint policy in root `[workspace.lints]` (members opt in with `[lints]\nworkspace = true`); deny the panic-family (unwrap_used, expect_used, panic, todo, unimplemented, unreachable, get_unwrap, indexing_slicing) and untyped lints (as_conversions, dbg_macro, print_stdout/stderr, str_to_string, exit, mem_forget); forbid unsafe_code outside FFI crates; warn pedantic with priority=-1. Relax in `#[cfg(test)]` via clippy.toml allow-*-in-tests, and require `#![allow(...)]` atop every tests/ file. TypeScript: tsconfig strict + noUncheckedIndexedAccess + exactOptionalPropertyTypes (+ override/returns/switch flags); ESLint strictTypeChecked (type-aware) banning any/no-unsafe-*, @ts-ignore/@ts-nocheck (ban-ts-comment), and non-null `!`. Gate on `cargo clippy --all-targets --all-features -- -D warnings`, `tsc --noEmit`, `eslint --max-warnings=0`.

## Rationale

AI agents reach for escape hatches (any, unwrap, dyn Any, @ts-ignore) that compile but hide bugs; only build-failing settings + denied lints make those impossible to merge. Centralizing in workspace.lints keeps 17 crates consistent and is the maintainable mechanism (RFC 3389). strict alone misses the index-undefined and exact-optional bug classes, so the two extra tsconfig flags are mandatory. Verified: the named clippy.toml allow-*-in-tests options (incl. allow-indexing-slicing-in-tests) exist with default=false against clippy master; the four cited untyped lints are category=restriction/allow.

## Alternatives considered

Per-crate #![deny(...)] attributes (rejected: drifts across 17 crates). Enabling the whole clippy restriction group (rejected: clippy docs warn it contains contradictory lints — cherry-pick). tsconfig strict only (rejected: ships the index-undefined bug class). eslint recommended only (rejected: doesn't ban any-flow via no-unsafe-*).

## Consequences

Some churn enabling exactOptionalPropertyTypes (React/3rd-party libs pass explicit undefined). priority=-1 on groups is required or individual allows get clobbered. allow-*-in-tests doesn't cover tests/ integration tests — each needs a file-level allow. typescript-eslint isn't semver-stable: pin the version and review changelogs. Lint categories drift between clippy releases — re-verify on toolchain bumps. Pin a clippy recent enough to know allow-indexing-slicing-in-tests.
