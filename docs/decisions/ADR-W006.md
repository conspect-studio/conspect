# ADR-W006: Config persistence: SQLite via sqlx + config-as-code

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Persist configuration in SQLite via sqlx 0.8.x (async, compile-time-checked, migrations, WAL + busy_timeout), version each mutable resource for ETag/If-Match optimistic concurrency, and provide config-as-code export/import as one versioned validated JSON/TOML document.

## Rationale

Transactional multi-resource atomicity is required for atomic relayout; compile-time-checked async queries fit axum; single-file DB is trivial to back up; ETag/If-Match prevents two operators clobbering a layout; config-as-code enables GitOps, DR and reproducible installs.

## Alternatives considered

rusqlite behind a dedicated DB actor (sync, no concurrent writes); flat TOML/JSON files (lose transactions and concurrent-write safety); sled (unstable on-disk format — rejected); external TSDB for metrics history (separate concern).

## Consequences

SQLite serializes writes (enable WAL + busy_timeout to avoid 'database is locked'); single-file SQLite assumes single-node — multi-instance HA would require rethinking; verify the sqlx SQLite driver builds identically on Linux (musl/static container) and macOS Apple Silicon in CI.
