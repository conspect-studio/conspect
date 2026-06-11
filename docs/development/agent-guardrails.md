> **Engineering guardrails standard — non-negotiable.** Engineering *process & quality gates* for AI agents working on Multiview. Summarized in [`../../AGENTS.md`](../../AGENTS.md) and [`../../CLAUDE.md`](../../CLAUDE.md); extends [`../architecture/conventions.md`](../architecture/conventions.md) (canonical naming/structure). Derived from a verification-hardened research workflow (2026-06-02). The config blocks here are the target for implementation; the strict lint set is intentionally NOT yet wired into the live `Cargo.toml` while crate bodies are stubs.

---

# Multiview Engineering Guardrails for AI Agents (Non-Negotiable)

> **Status:** Proposed · **Date:** 2026-06-02 · **Applies to:** every change authored by an AI coding agent (Claude Code, OpenAI Codex, Gemini, etc.) in this repo.
> This document is the enforceable standard. It extends — and never contradicts — [`docs/architecture/conventions.md`](../architecture/conventions.md) (the canonical naming/structure/invariants source of truth) and is summarized in `AGENTS.md`/`CLAUDE.md`. Where the Rust code and conventions.md differ from anything here on *naming/structure*, those win; this doc governs *engineering process and quality gates*.

Three pillars, all enforced as **blocking CI** and (where possible) **agent hooks**, because instruction prose is followed only some of the time while hooks/CI are deterministic: **(1) Absolute typing**, **(2) TDD-first with real tests**, **(3) Adversarial cross-vendor review** — plus baseline scope/secrets/supply-chain/commit guardrails.

---

## A. Absolute typing — no untyped, no escape hatches

### A.1 Rust

Centralize lint policy in the **workspace root `Cargo.toml`** via `[workspace.lints]`; every member crate (and `xtask`) opts in with `[lints]\nworkspace = true`. The ready-to-paste block lives in [`_workspace-lints.toml`](./_workspace-lints.toml).

Lint groups **must** carry `priority = -1` (or lower) because cargo emits lints alphabetically on the rustc command line — without it, a later individual `allow` of a noisy pedantic lint gets clobbered.

| Lint (`clippy::` unless noted) | Level | Why |
|---|---|---|
| `unwrap_used`, `expect_used` | deny | No panicking accessors in non-test code |
| `panic`, `todo`, `unimplemented`, `unreachable` | deny | No panic-family control flow in production paths |
| `get_unwrap`, `indexing_slicing` | deny | No `.get().unwrap()` / unchecked `a[i]` (use `.get()` + `?`/`match`) |
| `as_conversions` | deny | No lossy/`as` casts — use `TryFrom`/`TryInto`/`From` |
| `exit`, `mem_forget`, `dbg_macro`, `print_stdout`, `print_stderr`, `str_to_string` | deny | No process exit, leaks, stray debug/IO; use `tracing` |
| `pedantic` (group) | warn, `priority = -1` | Granular safety (cast lints, etc.), selectively `allow` the few noisy ones |
| `missing_errors_doc`, `missing_panics_doc`, `must_use_candidate` | warn | Force documenting failure modes |
| `unsafe_code` (rust) | forbid | No `unsafe` in safe crates. **Exception:** FFI crates (`multiview-ffmpeg`, vendor backends) override to `unsafe_code = "deny"` locally with a justified `// SAFETY:` comment per block |

**Verification notes (carry into config):**
- The whole `restriction` group must **not** be enabled wholesale — it contains contradictory lints. Cherry-pick the individual lints above.
- When both `unwrap_used` and `expect_used` are denied, `unwrap_used` will sometimes suggest `expect()` (clippy #9222) — the *real* fix is `?` / `match` / `unwrap_or` / `let-else`, not `expect`.
- A lint's **category** (restriction vs pedantic) determines its default level and **can move between releases** — re-check the [live clippy index](https://rust-lang.github.io/rust-clippy/master/index.html) when bumping the toolchain.

**Test scoping.** Root `clippy.toml`:
```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests = true
allow-dbg-in-tests = true
allow-print-in-tests = true
allow-indexing-slicing-in-tests = true   # recent clippy addition (PR #13854); needs a recent toolchain
```
These **only** relax lints in `#[cfg(test)]`/`#[test]` code — **not** in integration tests under `tests/`, `examples/`, or `benches/` (clippy #13981/#9612/#9062), nor in non-`#[test]` helpers inside a `#[cfg(test)]` module. **Every file under `tests/` must carry** at the top:
```rust
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
```

**Structural rules (prefer types over runtime strings):** use the **newtype pattern** with `TryFrom` validation (invalid values can't exist), the **typestate pattern** for state machines (wrong-state ops fail to compile, no `dyn`/vtable), `#[non_exhaustive]` on public enums, and **exhaustive `match`** (no catch-all `_` that silently swallows new variants). **Ban `dyn Any`** and string-keyed type dispatch.

### A.2 TypeScript / React (`web/`)

`web/tsconfig.json` must enable `strict` **plus** the flags `strict` deliberately omits:

| Option | Required | Catches |
|---|---|---|
| `strict` | true | `noImplicitAny`, `strictNullChecks`, `useUnknownInCatchVariables`, … |
| `noUncheckedIndexedAccess` | true | `arr[i]`/`obj[k]` is `T \| undefined` — a large class strict misses |
| `exactOptionalPropertyTypes` | true | distinguishes `p?: T` from `p: T \| undefined` |
| `noPropertyAccessFromIndexSignature`, `noImplicitOverride`, `noFallthroughCasesInSwitch`, `noImplicitReturns`, `verbatimModuleSyntax`, `noUnusedLocals`, `noUnusedParameters` | true | misc. correctness |

ESLint flat config ([`web/eslint.config.js`](../../web/eslint.config.js)) extends `tseslint.configs.strictTypeChecked` + `stylisticTypeChecked` with **type-aware linting** (`parserOptions.projectService: true`, `tsconfigRootDir`). This bans `any` (`no-explicit-any` + the `no-unsafe-*` suite) and the escape hatches:

| Rule | Setting |
|---|---|
| `@typescript-eslint/no-explicit-any` | error |
| `@typescript-eslint/no-non-null-assertion` | error (bans the `!` operator) |
| `@typescript-eslint/ban-ts-comment` | `ts-ignore: true`, `ts-nocheck: true`, `ts-expect-error: 'allow-with-description'`, `minimumDescriptionLength: 10` |
| `@typescript-eslint/no-unsafe-type-assertion`, `consistent-type-assertions` | error |

Prefer `@ts-expect-error` (with a ≥10-char description) over `@ts-ignore` — it self-cleans (errors when no longer needed). **Caveats:** `strictTypeChecked` is **not** semver-stable (pin the `typescript-eslint` version, review changelogs before upgrading); type-aware rules **silently disable** if a file isn't in a tsconfig — verify `projectService` covers all linted files.

### A.3 CI gates (blocking)
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cd web && tsc --noEmit && eslint . --max-warnings=0
```

---

## B. TDD-first with REAL tests (anti-reward-hacking)

Tests are the external source of truth the agent cannot argue with — **only if protected from the agent that writes the code.** The documented failure mode is reward hacking: weakening assertions, over-mocking, deleting/`.skip`/`#[ignore]`-ing tests, or editing the code-under-test to fit a weak test.

### B.1 Red-green-refactor — prove the red
1. **Write the failing test BEFORE any implementation.** Agents default to implementation-first; TDD must be explicit.
2. **Run it and paste the actual failing output** (the command + what it returned) into the PR. Show evidence, never assert success.
3. **Commit the failing test(s) as a separate commit** (`test: failing test for X`) before writing implementation — this makes any later weakening diff-visible.
4. **Implement to green WITHOUT editing the tests.**

### B.2 Test-protection rules (hard, copied into `AGENTS.md`)
- **NEVER** modify, weaken, delete, skip, `#[ignore]`/`.skip`/`.only` an existing test to make a build pass.
- **NEVER** weaken an assertion (e.g. `assert_eq!` → `assert!`, `toBe(x)` → `toBeTruthy()`).
- **NEVER** edit code-under-test to conform to a test you suspect is wrong — **STOP and ask a human.**
- A legitimate test change goes in **its own commit, justified in the PR, and reviewed**.
- **Ban assertion-free / tautological tests** (asserting the implementation back to itself; "does not throw" alone). Every test asserts real behavior.
- Changes to `PROPTEST_CASES` / `fc.configureGlobal({ numRuns })` / coverage thresholds are **test-weakening** and reviewable.

### B.3 Mutation testing — the real quality target

Coverage is a **floor, never a target** (Goodhart): it tells you what is *not* tested, not whether tests verify behavior. **Mutation score is the target.** `cargo-mutants` injects bugs; a **surviving (MISSED) mutant** in covered code is an automatic review finding — exactly the signature of a tautological test. Use `nextest` and **scope to the PR diff** for speed; full run nightly on `main`.

```toml
# .cargo/mutants.toml  (canonical key inventory: cargo mutants --emit-schema=config; docs: mutants.rs/config-file.html)
test_tool = "nextest"            # needs cargo-mutants >= 24.1.0
exclude_globs = ["crates/*/benches/**", "xtask/**"]
timeout_multiplier = 2.0
minimum_test_timeout = 30.0
```
```bash
# PR (fast, diff-only)            # Nightly on main (full)
cargo mutants --in-diff git.diff -vV     cargo mutants --in-place -vV
```
**Exit codes (verified against cargo-mutants 27.0.0):** `0` all caught · `1` usage error · `2` MISSED mutants (test gap → fail PR) · `3` timeout (tune `--timeout`) · `4` baseline already failing (fix tests first) · `5`/`6` `--in-diff` errors · `70` internal. CI must treat `4` distinctly from `2`. (As of 27.0.0, *no mutants generated* exits `0` — relevant when an `--in-diff` PR has no mutable lines.)

TypeScript equivalent: **StrykerJS** — set `thresholds.break` explicitly (defaults to `null` = no failure) and use incremental mode for PRs.

### B.4 Property & state-machine tests
Required for pure/algorithmic and stateful engine logic, because an agent can't special-case generated inputs:
- **Rust:** `proptest` (auto-shrinks; **commit `proptest-regressions/` to git** so known-bad seeds replay) + `proptest-state-machine` for the engine/framestore state machines.
- **TS:** `fast-check` (incl. model-based commands).

### B.5 Held-out acceptance suite (SpecBench pattern)
Keep an acceptance/e2e suite the authoring agent does **not** see or run during implementation; CI runs it as the true gate. A growing gap between agent-visible and held-out pass rates is a reward-hacking signal.

### B.6 Enforcement
- **Agent hook (PreToolUse):** deny edits to test-file paths (`*.test.*`, `*.spec.*`, `tests/**`, `__tests__/**`). Bash-matching hooks miss in-process edits, so **also** add a CI step that diffs test files and flags removed/weakened assertions.
- **Stop hook / CI:** block turn/PR completion until the suite is green and evidence is shown.

---

## C. Adversarial cross-vendor review

Code authored primarily by one vendor/model **MUST** be reviewed by a **different** vendor (Claude ↔ OpenAI Codex ↔ Gemini) before a human approves. Two stacked mechanisms, both required:

1. **Context separation (biggest cheap win):** the reviewer runs in a **fresh session/subagent** seeing **only** (a) the diff, (b) the spec/PLAN, (c) the checklist — never the author's chat history. Fresh-session review beats same-session self-review even with the *same* model (CCR, arXiv 2603.12123: F1 28.6% vs 24.6%, p=0.008; +4.7 F1 on code; +11pp on critical errors).
2. **Vendor diversity:** different training → less-correlated blind spots. Both separation **and** model heterogeneity independently help (do not over-claim one beats the other).

### C.1 Process
| Step | Tooling (verify before pinning — surfaces drift) |
|---|---|
| Claude wrote it → Codex reviews | `/plugin marketplace add openai/codex-plugin-cc` → `/codex:adversarial-review --base main` (commands current as of 2026-06-02) |
| Codex/other wrote it → Claude reviews | bundled `/code-review` + `/security-review` (fresh subagent) |
| High-risk diff (auth, concurrency, data migration, money) | 3-reviewer panel (Claude + Codex + Gemini) + coordinator synthesis + mandatory human sign-off |

Attach the cross-vendor review output to the PR. **AI review is never the merge gate** — a human is the named final approver (branch protection: required deterministic checks + ≥1 human approval).

### C.2 Reviewer brief (scope to avoid manufactured findings)
> Report **only** defects in correctness, security, spec/requirements conformance, and the Multiview typing & TDD guardrails. Do **not** report style, naming, or speculative defense-in-depth. Ignore lockfiles/generated/minified files. If you find nothing, name the highest-residual-risk area and why it's acceptable.

**Checklist (hand to the reviewer):**
- [ ] No `any`/`unwrap`/`expect`/`panic`/`dyn Any`/`@ts-ignore`/non-null `!`; matches exhaustive; newtypes used.
- [ ] Tests are test-first & behavior-asserting; **no** test deleted/weakened/skipped; no code-under-test edited to fit a weak test; no tautological/over-mocked tests.
- [ ] No silent suppression (`#[allow]`, `eslint-disable`, `.skip`) without an inline justification.
- [ ] Diff is minimal and in-scope; secrets/supply-chain clean.
- [ ] Security per OWASP Top 10 for Agentic Applications.

**Cautions:** treat **unanimous** AI approval as a yellow flag (require ≥1 substantive risk statement). AI review does **not** cover TOCTOU/race conditions, timing/authz logic — those need property/concurrency tests + human review.

---

## D. Baseline guardrails

### D.1 Scope & process
- **Explore → Plan → Implement → Commit.** Plan/spec for multi-file or ambiguous work; skip planning only if the diff fits in one sentence.
- **Minimal diff:** every changed line traces to the request. State an explicit **out-of-scope (Not Included)** boundary. Ask when ambiguous.
- **No silent suppression:** disabling/weakening any lint, test, or type-check needs an inline justification comment and is a reviewable event. **Fix root cause, not symptom.**
- **Show evidence, not assertions** (command + output + exit code).
- Run agents **sandboxed / least-privilege** (devcontainer); beware indirect prompt injection from repo issues/docs/dep READMEs (OWASP LLM #1).

### D.2 Determinism
Commit `Cargo.lock` + JS lockfile; pin `rust-toolchain.toml` + Node version. CI builds with `--locked` (cargo **ignores** the lockfile without it) and `npm ci`. Eliminate timestamp/RNG/filesystem-order/env/concurrency nondeterminism; no floating version ranges.

### D.3 Secrets
Never commit/echo secrets. Use the 1Password flow (`op read` → `chmod 600` temp file → `; rm -f` after, or `op ssh-agent`). Layered **gitleaks**: pre-commit on staged changes + CI on every PR. Current command (the `protect`/`detect` subcommands are deprecated/hidden since v8.19.0):
```bash
gitleaks git --pre-commit --redact --staged --verbose   # or the official pre-commit hook id `gitleaks`
```

### D.4 Supply chain
`cargo deny check` (advisories + bans + licenses + sources) via `EmbarkStudios/cargo-deny-action@v2` (**not `@v1`** — pin a tag/SHA for stricter hygiene) — already wired in `deny.toml`. Add `cargo audit`/`cargo vet`. Pinning is **necessary but not sufficient** — combine pin + audit + provenance (SLSA/Sigstore) for any published artifact.

### D.5 Commits & ADRs
- **Conventional Commits** (`feat`/`fix`/`docs`/…). End AI commit messages with `Co-Authored-By: <model>` and (where DCO applies) `git commit --signoff` for human accountability. The `Co-authored-by:` block needs a blank line before it, one per line, no blank lines between entries.
- Record non-trivial architectural/dependency decisions as a lightweight **ADR** in [`docs/decisions/`](../decisions/) (the *why*); behavioral always/never rules live in `AGENTS.md`/`CLAUDE.md` (keep them concise — bloat causes agents to ignore instructions).

### D.6 Error handling
Propagate with `?`; never swallow (no empty `catch`, no `let _ = <Result>` without justification, no empty error match arms). `unwrap`/`expect`/`panic` banned outside tests; `expect` with context only for proven invariants.

### D.7 Definition of Done
- [ ] New behavior tests written test-first; whole suite + held-out suite green (evidence shown).
- [ ] Lints + types clean, **zero new suppressions**.
- [ ] `cargo mutants --in-diff` shows no missed mutants in changed code.
- [ ] Adversarial cross-vendor review passed; human approved.
- [ ] `cargo deny check` + gitleaks clean; lockfiles committed.
- [ ] Docs/ADRs updated; diff minimal and in-scope.