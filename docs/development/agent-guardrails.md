# Toolchain guardrails — lint policy, test tooling, review scoping

**What this file is:** the *forensic* reference for this repository's quality tooling — the lint
set and why each entry is worded the way it is, the traps in clippy/`clippy.toml`, mutation-testing
exit codes, tool-version drift, and the reviewer brief. These are facts about tools, learned the
hard way, that are not derivable from the config files themselves.

**What this file is not:** the process contract. Which gates a change owes, when a test must come
first, who reviews, and when it merges are in
[`docs/standards/engineering.md`](../standards/engineering.md) and scale by change class. Canonical
naming/structure/invariants are in [`conventions.md`](../architecture/conventions.md); where the
Rust code and conventions differ from anything here, those win.

Section letters (§A.1, §B.3, §C.2 …) are stable — `clippy.toml` and `web/eslint.config.js` cite
them by name.

---

## A. Typing policy

### A.1 Rust

Lint policy is centralized in the workspace root `Cargo.toml` under `[workspace.lints]`; every
member crate opts in with `[lints]\nworkspace = true`. A ready-to-paste copy lives in
[`_workspace-lints.toml`](./_workspace-lints.toml) — that is a **doc snippet, not the live
manifest**, and nothing checks the two for drift.

**This lint set is live** — it is wired in the root `Cargo.toml` and enforced by
`cargo clippy --locked --workspace --all-targets -- -D warnings` in CI.

| Lint (`clippy::` unless noted) | Level | Why |
|---|---|---|
| `unwrap_used`, `expect_used` | deny | No panicking accessors in non-test code |
| `panic`, `todo`, `unimplemented`, `unreachable` | deny | No panic-family control flow in production paths |
| `get_unwrap`, `indexing_slicing` | deny | No `.get().unwrap()` / unchecked `a[i]` (use `.get()` + `?`/`match`) |
| `as_conversions` | deny | No lossy/`as` casts — use `TryFrom`/`TryInto`/`From` |
| `exit`, `mem_forget`, `dbg_macro`, `print_stdout`, `print_stderr`, `str_to_string` | deny | No process exit, leaks, stray debug/IO; use `tracing` |
| `pedantic` (group) | warn, `priority = -1` | Granular safety (cast lints, etc.), selectively `allow` the few noisy ones |
| `missing_errors_doc`, `missing_panics_doc`, `must_use_candidate` | warn | Force documenting failure modes |
| `unsafe_code` (rust) | forbid | No `unsafe` in safe crates. **Exception:** the FFI crates override to `unsafe_code = "deny"` locally with a justified `// SAFETY:` comment per block |

**The traps, in order of how often they bite:**

- Lint groups **must** carry `priority = -1` (or lower), because cargo emits lints alphabetically on
  the rustc command line — without it, a later individual `allow` of a noisy pedantic lint gets
  clobbered.
- The whole `restriction` group must **not** be enabled wholesale — it contains contradictory lints.
  Cherry-pick the individual lints above.
- When both `unwrap_used` and `expect_used` are denied, `unwrap_used` will sometimes suggest
  `expect()` (clippy #9222) — the *real* fix is `?` / `match` / `unwrap_or` / `let-else`, not
  `expect`.
- A lint's **category** (restriction vs pedantic) determines its default level and **can move
  between releases** — re-check the
  [live clippy index](https://rust-lang.github.io/rust-clippy/master/index.html) when bumping the
  toolchain.
- **`unsafe_code = "forbid"` cannot be relaxed downstream**, so the six FFI crates
  (`multiview-ffmpeg`, `-ndi-sys`, `-rist-sys`, `-ntpsys`, `-i915pmu`, `-webrtc`) cannot inherit and
  override in one table. Each drops `[lints] workspace = true` and **restates the whole clippy set
  verbatim** with `unsafe_code = "deny"`. There is no drift check on those seven copies — if you
  change the workspace set, change all six by hand.
- `clippy::undocumented_unsafe_blocks` is **not** enabled anywhere. The `// SAFETY:` convention is
  enforced by review only.

**Test scoping.** Root `clippy.toml`:
```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests = true
allow-dbg-in-tests = true
allow-print-in-tests = true
allow-indexing-slicing-in-tests = true   # recent clippy addition (PR #13854); needs a recent toolchain
```

**What these actually cover — verified empirically against clippy 0.1.96 with `clippy-driver
--test`, which is how cargo compiles `tests/*.rs`:** they **do** suppress inside a `#[test]` function
in an integration test, and they do **not** suppress at module scope in the same file — helper and
fixture builders, `impl` blocks, `const fn`s, and any non-`#[test]` support function. This repo's
integration tests are helper-heavy, so **every file under `tests/` needs** a file-level attribute:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
```
All 517 integration-test files currently carry one. Separately, `as_conversions`, `str_to_string`,
`todo`, `unimplemented`, `unreachable`, `get_unwrap`, `exit` and `mem_forget` have **no
`allow-*-in-tests` knob in clippy at all** — they are never relaxed, even inside a `#[test]`, which
is why 45+ files extend the header with `clippy::as_conversions`, `clippy::cast_possible_truncation`
and friends. The failure mode is discovery-by-CI-red: nothing warns you at authoring time.

**Structural rules (prefer types over runtime strings):** the **newtype pattern** with `TryFrom`
validation (invalid values cannot exist), the **typestate pattern** for state machines (wrong-state
ops fail to compile, no `dyn`/vtable), `#[non_exhaustive]` on public enums, and **exhaustive
`match`** (no catch-all `_` that silently swallows new variants). **Ban `dyn Any`** and string-keyed
type dispatch.

### A.2 TypeScript / React (`web/`)

`web/tsconfig.json` enables `strict` **plus** the flags `strict` deliberately omits:

| Option | Required | Catches |
|---|---|---|
| `strict` | true | `noImplicitAny`, `strictNullChecks`, `useUnknownInCatchVariables`, … |
| `noUncheckedIndexedAccess` | true | `arr[i]`/`obj[k]` is `T \| undefined` — a large class strict misses |
| `exactOptionalPropertyTypes` | true | distinguishes `p?: T` from `p: T \| undefined` |
| `noImplicitOverride`, `noFallthroughCasesInSwitch`, `noImplicitReturns`, `verbatimModuleSyntax`, `noUnusedLocals`, `noUnusedParameters` | true | misc. correctness |

The ESLint flat config ([`web/eslint.config.js`](../../web/eslint.config.js)) extends
`tseslint.configs.strictTypeChecked` + `stylisticTypeChecked` with type-aware linting
(`parserOptions.projectService`). It bans `any` (`no-explicit-any` + the `no-unsafe-*` suite),
non-null `!`, and `@ts-ignore`/`@ts-nocheck`; `@ts-expect-error` is allowed with a ≥10-character
description because it self-cleans (it errors once no longer needed).

**Caveats:** `strictTypeChecked` is **not** semver-stable — pin the `typescript-eslint` version and
review changelogs before upgrading. Type-aware rules **silently disable** if a file is not in a
tsconfig, so verify `projectService` covers everything you lint. Today `scripts/**`, `e2e/**`,
`playwright.config.ts`, `src/api/schema.ts` and `*.config.js` are outside type-aware linting, and the
test globs relax `no-non-null-assertion` and the `no-unsafe-*` suite for mocking.

---

## B. Test tooling

### B.1 Test-protection rules

Tests are the external source of truth the agent cannot argue with — **only if protected from the
agent that writes the code.** The documented failure mode is reward hacking: weakening assertions,
over-mocking, deleting/`.skip`/`#[ignore]`-ing tests, or editing the code-under-test to fit a weak
test.

- **NEVER** modify, weaken, delete, skip, `#[ignore]`/`.skip`/`.only` an existing test to make a
  build pass.
- **NEVER** weaken an assertion (e.g. `assert_eq!` → `assert!`, `toBe(x)` → `toBeTruthy()`).
- **NEVER** edit code-under-test to conform to a test you suspect is wrong — **STOP and ask a human.**
- A legitimate test change goes in **its own commit, justified in the PR, and reviewed**.
- **Ban assertion-free / tautological tests** (asserting the implementation back to itself; "does
  not throw" alone). Every test asserts real behaviour.
- Changes to `PROPTEST_CASES` / `fc.configureGlobal({ numRuns })` / coverage thresholds are
  **test-weakening** and reviewable.

### B.2 Property & state-machine tests

Required for pure/algorithmic and stateful engine logic, because an agent cannot special-case
generated inputs.

- **Rust:** `proptest` (auto-shrinks; **commit `proptest-regressions/` to git** so known-bad seeds
  replay) + `proptest-state-machine` for the engine/framestore state machines.
- **TS:** `fast-check`, including model-based commands.

### B.3 Mutation testing

Coverage is a **floor, never a target** (Goodhart): it tells you what is *not* tested, not whether
tests verify behaviour. **Mutation score is the target.** `cargo-mutants` injects bugs; a surviving
(MISSED) mutant in covered code is exactly the signature of a tautological test.

> **Not wired.** [ADR-G002](../decisions/ADR-G002.md) requires a mutation gate, but **no workflow in
> `.github/workflows/` runs `cargo mutants`**, and there is no nightly workflow at all. The config
> and exit-code table below are what to wire, not what runs today. Do not describe this as an active
> gate anywhere.

```toml
# .cargo/mutants.toml  (canonical key inventory: cargo mutants --emit-schema=config)
test_tool = "nextest"            # needs cargo-mutants >= 24.1.0
exclude_globs = ["crates/*/benches/**", "xtask/**"]
timeout_multiplier = 2.0
minimum_test_timeout = 30.0
```
```bash
# PR (fast, diff-only)                   # Nightly on main (full)
cargo mutants --in-diff git.diff -vV     cargo mutants --in-place -vV
```

**Exit codes (verified against cargo-mutants 27.0.0):** `0` all caught · `1` usage error ·
`2` MISSED mutants (test gap → fail PR) · `3` timeout (tune `--timeout`) · `4` baseline already
failing (fix tests first) · `5`/`6` `--in-diff` errors · `70` internal. **CI must treat `4`
distinctly from `2`.** As of 27.0.0, *no mutants generated* exits `0` — relevant when an `--in-diff`
PR has no mutable lines.

TypeScript equivalent: **StrykerJS** — set `thresholds.break` explicitly (it defaults to `null`,
i.e. never fails) and use incremental mode for PRs.

### B.4 Held-out acceptance suite

Keep an acceptance/e2e suite the authoring agent does not see or run during implementation; CI runs
it as the true gate. A growing gap between agent-visible and held-out pass rates is a reward-hacking
signal.

### B.5 Enforcement note

A `PreToolUse` hook that matches on Bash cannot see in-process edits, so a hook alone never protects
test files — **also** add a CI step that diffs test files and flags removed or weakened assertions.

---

## C. Review scoping

Why cross-vendor, fresh-context review is the shape it is:

1. **Context separation** is the biggest cheap win — the reviewer runs in a fresh session seeing
   only the diff, the spec, and the checklist, never the author's chat history. Fresh-session review
   beats same-session self-review even with the *same* model (CCR, arXiv 2603.12123: F1 28.6% vs
   24.6%, p=0.008; +4.7 F1 on code; +11pp on critical errors).
2. **Vendor diversity** — different training gives less-correlated blind spots. Separation and model
   heterogeneity independently help; do not over-claim that one beats the other.

Who reviews what, and when, is the class matrix in
[engineering.md](../standards/engineering.md) Part A. The operational mechanics live in the
[codex-review runbook](../runbooks/codex-review.md) and the `review-wave` workflow.

### C.2 Reviewer brief (scope to avoid manufactured findings)

> Report **only** defects in correctness, security, spec/requirements conformance, and the Multiview
> typing & test guardrails. Do **not** report style, naming, or speculative defence-in-depth. Ignore
> lockfiles/generated/minified files. If you find nothing, name the highest-residual-risk area and
> why it is acceptable.

**Checklist (hand to the reviewer):**
- [ ] No `any`/`unwrap`/`expect`/`panic`/`dyn Any`/`@ts-ignore`/non-null `!`; matches exhaustive; newtypes used.
- [ ] Tests assert behaviour; **no** test deleted/weakened/skipped; no code-under-test edited to fit a weak test; no tautological or over-mocked tests.
- [ ] No silent suppression (`#[allow]`, `eslint-disable`, `.skip`) without an inline justification.
- [ ] Diff is minimal and in-scope; secrets and supply chain clean.
- [ ] Security per OWASP Top 10 for Agentic Applications.

**Cautions:** treat **unanimous** AI approval as a yellow flag — require at least one substantive
risk statement. AI review does **not** cover TOCTOU, race conditions, or timing/authorization logic;
those need property/concurrency tests plus human review.

---

## D. Tool-version gotchas

- **gitleaks** — the `protect`/`detect` subcommands are deprecated and hidden since v8.19.0. The
  current invocation is:
  ```bash
  gitleaks git --pre-commit --redact --staged --verbose   # or the official pre-commit hook id `gitleaks`
  ```
  The local `lefthook` hook **silently skips when the binary is absent**; `.github/workflows/gitleaks.yml`
  is the authoritative run.
- **cargo-deny** — use `EmbarkStudios/cargo-deny-action@v2` (**not `@v1`**), pinned to a tag or SHA.
  `cargo deny check` covers advisories + bans + licenses + sources via `deny.toml`. Pinning is
  **necessary but not sufficient** — combine pin + audit + provenance (SLSA/Sigstore) for any
  published artefact.
- **cargo** ignores `Cargo.lock` unless you pass `--locked`. Every CI invocation passes it; so should
  yours when reproducing a CI failure.
- **Error handling** — propagate with `?`; never swallow (no empty `catch`, no `let _ = <Result>`
  without justification, no empty error match arms). `expect` with context is acceptable only for a
  proven invariant in non-hot startup code.
