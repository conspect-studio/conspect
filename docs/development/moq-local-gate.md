# MAIN candidate: CURRENT-HEAD proportional DRAFT gate

## Decision and scope

This replaces the obsolete component-branch single-chain/historical-receipt policy.
That policy cannot validate the main-based aggregate. This is a **new MAIN
candidate proportional decision**, subject to independent review of the exact
policy commit before execution. It does not rename, reset, or extend T78: that
task exhausted attempt 2. Do not run a third local whole-workspace test suite.
There is no copied, reset, or newly initialized workspace-attempt ledger here.

`bin/check` actually executes its complete command list against the clean current
HEAD, including a merge HEAD. It has no ancestry allowlist, historical-log
acceptance, availability skips, forced success, or inherited test-pass claim.
A nonzero command, timeout, resource error, changed HEAD, dirty tree, or missing
positive test output fails. A filtered Rust test command that matches zero tests
fails even if Cargo exits zero. Focused test filters select scope; they do not
mean the whole workspace suite ran.

**DRAFT is not merge approval.** Normal PR CI must execute its existing full
configured suite unmodified and be green before merge. The gate changes no CI
workflow. The existing CI workspace command has a pre-existing
`--skip tile_driven_meets_budget` argument; this policy neither adds that skip nor
claims that CI runs every workspace test without filters. The existing
[CI rationale](../../.github/workflows/ci.yml#L178-L183) cites
[ADR-E009](../decisions/ADR-E009.md): a wall-clock performance budget needs
consistent dedicated hardware, not shared CI. CI green alone does not certify
that budget or the separate received-media/no-falter requirement. Any correction
to that baseline requires separate owner action. No local command here uses `--skip`.

The captain owns push credentials and all pushes. Do not push or run the official
wrapper/issue a receipt before fresh cross-vendor policy review, owner coordination,
and a separate gate execution-budget decision. Review binds the exact commit
externally; the gate cannot certify its own review. After review the captain
integrates only this policy/test/docs delta onto the quiescent aggregate. The
runtime owner worktree and runtime source remain untouched.

## Actual command scope

The executable `COMMANDS` list is authoritative. Every selected command runs;
no result comes from an old log:

- Runner mechanical tests, actual formatting, locked default workspace check,
  strict all-target workspace Clippy, and dependency audit.
- Native MoQ output check and strict Clippy with `moq-output,gpl-codecs`.
- MoQ transport library; the whole `moq_outputs::` module (factory, parent
  conversion, and metadata tests); `outgoing_` tests including the pending
  handshake pair; pending handshake cancellation again; and relay URL file tests.
  The outgoing and handshake filters intentionally overlap, not disjoint evidence.
- `authority-bridge-unit-test` runs the five `bridge_tests` unit tests in the named
  `abr_authority` example through libtest. It does **not** execute the authority
  program's `main` (IVF ladder/registry/router). `moq-transport` is required to
  compile the example's imports; it does not select those tests.
- The actual native canonical H.264 pipeline -> HTTP authority test, with GPL
  codec feature enabled explicitly. OpenSSL executable on PATH and libav with
  libx264 are required fail-closed prerequisites. Before any expensive command,
  preflight rejects missing OpenSSL with a static error (also in validate-only).
  Missing libav/libx264 still fails compilation or the native leg, never an
  unavailable-pass or skip.
- Fresh web dependency install, the ABR/player files (`web-moq-abr`), focused
  preview test files (`web-preview`),
  complete web lint/build, and probe syntax.

The ABR path tuple is `src/preview/moqAbr.test.ts`,
`src/preview/MoqPlayerAbr.test.tsx`. The preview path tuple is
`src/preview/moqConnection.test.ts`, `src/preview/moqPlayback.test.ts`,
`src/preview/moqLeaseApi.test.ts`, `src/preview/MoqSelection.test.tsx`,
`src/preview/PreviewSurface.test.tsx`, `src/preview/capabilities.test.ts`,
`src/preview/outputPlayback.test.ts`, `src/preview/diagnostics.test.ts`, and
`src/preview/PreviewDiagnostics.test.tsx` (included). Paths are relative to web;
the mechanical tests pin both exact tuples, not an arbitrary file count.
No leg label claims a test-result count. These are Vitest
browser-component/unit tests, not a real live browser/network acceptance claim.
The required test logs must report positive test execution. Native/compiler and
web checks remain **UNRUN** until the captain authorizes execution. The gate does
not execute the full workspace test suite locally. Clippy remains required;
a compiler/lint failure must be fixed or reported, never relabeled as a pass.

## Resource and process contract

The runner reuses the resource/process machinery reviewed in `0b20f816`, not its
old task ledger. It forces `CARGO_BUILD_JOBS=2`, `CARGO_INCREMENTAL=0`, rejects
Cargo target-directory overrides, and uses only worktree-local targets.
The three root/tool targets count together. Free capacity must remain at least
150 GiB (`MIN_FREE` in `bin/check`; the build host disk is shared with other
repositories, so the floor sits below their short-term swings). Source authoring must keep this new lane below **2 GiB**. The runner
proposes a **15 GiB combined execution target cap**, not permission to execute.
A separate execution-budget review must approve that bound against measured
warm aggregate size and projected cold growth before any expensive gate runs.
The existing aggregate cache reached roughly 5.4 GiB; that is not a cold-peak
measurement or proof that 15 GiB fits. Cold native peak is unmeasured. Do not begin
an expensive gate merely to discover that it exceeds the cap. Do not duplicate or borrow another lane's
cache, remove its artifacts, or consume space up to ENOSPC.

Each command has a wall timeout: workspace/feature check and Clippy 20 minutes;
other commands 10 minutes. In particular the native H.264 HTTP leg has a
600s wall timeout including cold compilation: exceeding it **fails** the leg,
not a skip or permission to retry/extend the budget. Commands execute serially. A nonblocking worktree
flock prevents concurrent gate runs. The runner creates a separate process
session for each command; cleanup sends TERM and then KILL only to that owned
process group, including descendants after the leader exits. This cleanup does
not remove build directories or any owner's data.

Accounting pins directory descriptors, does not follow symlinks, counts the
larger of apparent/allocated bytes, and deduplicates hard-linked inodes. Only
vanished entries may be omitted; permission, I/O and directory-substitution
errors fail closed. Samples occur before commands, every five seconds while
active, and afterward. This is race-resistant traversal, **not atomic accounting
or a hard disk quota**: growth between samples can exceed a limit before detection.
Logs report sampled peak/growth, not an invented exact peak. Logs and dependency
install artifacts also use disk; the free-space floor covers the filesystem.

## Evidence and remaining concerns

`python3 -B scripts/test_final_head_gate.py` runs only mechanical tests. It tests
real nonzero command failure, dirty/change-HEAD rejection, timeout descendant
cleanup, failure-only records, zero-test rejection, resource boundaries and
vanishing/sparse/symlink accounting. Mocked successes are runner tests, never
native or browser evidence. `bin/check --validate-only` is a preflight, not a test
pass or a receipt. Actual runs write SHA/command/time/timeout/log/status records
under the local target directory. No historical receipt is read or fabricated.

The aggregate remains unfinished: historical Cloudflare QoS **failed**; protected
activation remains **OFF/unavailable** with unresolved security concerns; optional
A/V preview startup/application integration remains unfinished; distinct optional
cadence and HEVC metadata remain unsupported. This policy does not waive failed
tests, certify paid-provider QoS, assert deployment, or erase these concerns.
Normal CI green and independent runtime review are still required. A green draft
local run alone never permits merge or whole-feature completion.
