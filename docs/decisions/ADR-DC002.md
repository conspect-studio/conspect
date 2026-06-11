# ADR-DC002: Seed a gitignored repo-root .env from ~/.onepassword_token via host initializeCommand; inject at runtime with --env-file

- **Status:** Proposed
- **Area:** Dev Container
- **Date:** 2026-06-02
- **Source brief:** [devcontainer-design.md](../research/devcontainer-design.md)

## Decision

A host-side `initializeCommand` (`.devcontainer/initialize.sh`, POSIX sh, idempotent) ALWAYS creates a repo-root `.env` (empty if needed) and, when `.env` lacks a token but `~/.onepassword_token` exists, writes `OP_SERVICE_ACCOUNT_TOKEN` into it (newline-stripped, chmod 600, never echoed). The token is supplied to the container only at runtime via `runArgs:["--env-file","${localWorkspaceFolder}/.env"]`. `${localEnv:OP_SERVICE_ACCOUNT_TOKEN}` is surfaced in remoteEnv only as a harmless convenience.

## Rationale

`docker run --env-file <missing>` fails hard with 'no such file or directory' (verified on current Docker; devcontainer runArgs use docker run), so the file MUST be guaranteed to exist — unconditional creation is the key correctness point. `initializeCommand` runs on the host (can read host home, write the repo) and re-runs on every rebuild, so it must be idempotent (guarded so it never clobbers a user-set token). `${localEnv:}` reads a host ENV VAR, not a file, and is unreliable for GUI-launched VS Code on macOS, so the file bridge is primary. Runtime injection (not build ARG/ENV) avoids leaking the secret into image layers recoverable via docker history. Verified by dry-run across empty/seed/idempotent/user-set/no-leak cases.

## Alternatives considered

(a) `containerEnv/${localEnv:OP_SERVICE_ACCOUNT_TOKEN}` only — rejected as primary: empty for GUI-launched sessions. (b) Build ARG/ENV — rejected: leaks into layers. (c) Mount ~/.config/op or the token file into the container — rejected: larger surface area, unnecessary for pure service-account use. (d) docker-compose env_file with required:false — rejected: not a compose-based dev container.

## Consequences

Container always starts even with no token (op disabled, build/test still work — good for CI/macOS). Token rotation needs a container restart (--env-file re-read each docker run). The repo must keep `.env`/`.env.*` gitignored (already true) with a tracked `.env.example`. The host must actually hold ~/.onepassword_token for auto-seed; absence is handled gracefully.
