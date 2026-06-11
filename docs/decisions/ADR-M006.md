# ADR-M006: Config-as-code import/export with versioning, rollback, and reference-only secrets

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Treat the full declarative config (sources/layouts/outputs/renditions/overlays/audio+subtitle routes/policies/settings) as the same serde schema in TOML (authoring) and JSON (canonical wire), with schema_version + migration. Provide GET /api/v1/config, POST /api/v1/config:validate, POST /api/v1/config:apply?dry_run, PUT /api/v1/config (replace), version history (GET /api/v1/config/versions, :diff), and POST /api/v1/config/versions/{rev}:rollback?dry_run. Apply/rollback reuse the ADR-M005 classifier so the UI shows which outputs reset before committing. Export offers secrets=ref (op:// or internal id) or secrets=redact and never emits plaintext; import of a config referencing an unknown secret fails clearly (source disabled with 'missing secret' status) rather than connecting with no auth. Every apply/rollback is snapshotted and audit-logged.

## Rationale

GitOps-style versioning, safe rollback, and committable config require lossless round-trip with reset-impact preview and strict secret hygiene. Reusing the runtime serde schema guarantees the UI editor and the file are two views of one thing validated by the same JSON Schema.

## Alternatives considered

Separate import/export schema from runtime (rejected: drift between file and live config); export with inline secrets (rejected: leaks credentials into committable files/logs); one-click rollback without diff (rejected: a naive rollback to a different geometry breaks all live consumers).

## Consequences

The audit/version store must be append-only/tamper-evident (WORM-backed or SIEM-forwarded) or it can be altered out-of-band. Import must validate against the capability matrix before apply. The secret store backend (1Password op://, vault, env) is an external dependency whose absence must degrade gracefully (disabled source + clear status).
