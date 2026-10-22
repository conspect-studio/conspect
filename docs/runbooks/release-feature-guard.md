# Runbook — release-artifact feature guard (`_test-seams`, task #109)

## What it is and why

`multiview-control` carries a **test-only** Cargo feature, `_test-seams`, that
arms the config-watch post-probe interpose hook
(`WatchOptions::with_post_probe_interpose`) — a seam whose only job is to let an
integration test WRITE the watched config file mid-poll (the probe→apply TOCTOU
regression test, PR #7). It must **NEVER** be compiled into a shipped/release
artifact: it is inert unless a caller arms it, but it is an ordinary Cargo
feature that `--features _test-seams` or a workspace `--all-features` build can
still turn on.

`cargo deny` cannot gate a *feature* (only crates/licenses/advisories), and
there is no reliable "this is a release build" `cfg`. This guard closes that
gap. It was raised as follow-up **B(2)** by the #7 cross-vendor panel and filed
as task **#109**.

The guard resolves the **effective feature set** of every shipped release preset
and fails if any resolves a `*test-seam*` feature on any crate.

- Decision context: [ADR-W020](../decisions/ADR-W020.md) (config-watch),
  PR #7 panel follow-up B(2).
- Implementation: [`xtask/src/release_features.rs`](../../xtask/src/release_features.rs)
  (`cargo xtask check-release-features`).
- CI leg: `release-feature-guard` in
  [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml).

## What it checks

For each shipped release feature spec it runs `cargo tree` — **feature
RESOLUTION only, never a compile** — and asserts no resolved feature name
contains `test-seam` (case-insensitive):

```
cargo tree --locked -p multiview-cli --features <spec> -e no-dev \
  --prefix none --format "{p}|{f}"
```

- `-e no-dev` excludes dev-dependency edges, so the resolved set matches a
  non-test `cargo build --features <spec>` — this is exactly why `_test-seams`
  (enabled only via `multiview-control`'s **self dev-dependency**) stays out of a
  release graph, and a transitive dependency's own dev-dependencies never enter.
- The specs checked (`RELEASE_FEATURE_SPECS`) are the four umbrella presets in
  [`crates/multiview-cli/Cargo.toml`](../../crates/multiview-cli/Cargo.toml) —
  `nvidia`, `apple`, `linux-vaapi`, `full` — plus the **exact** `--features`
  strings every shipping source builds:
  - [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
    binaries → `ffmpeg,linux-vaapi`, `ffmpeg,apple`;
  - [`.github/workflows/docker.yml`](../../.github/workflows/docker.yml) GHCR
    images (its matrix `features:` → the `CARGO_FEATURES` build-arg) →
    `ffmpeg,linux-vaapi,web`, `ffmpeg,nvidia,web`;
  - [`deploy/Dockerfile`](../../deploy/Dockerfile) /
    [`deploy/Dockerfile.nvidia`](../../deploy/Dockerfile.nvidia)
    `ARG CARGO_FEATURES` defaults → `ffmpeg,linux-vaapi,web,ntp`,
    `ffmpeg,nvidia,web`; the documented GPL image adds `gpl-codecs`.

- **Anti-drift (no silent rot).** `RELEASE_FEATURE_SPECS` is not trusted to be
  hand-maintained. The `release_feature_specs_cover_every_shipped_combo` test
  ([`xtask/tests/release_features_drift.rs`](../../xtask/tests/release_features_drift.rs))
  **derives** the shipped combos straight from `SHIPPING_SOURCES` (the four files
  above) and asserts `RELEASE_FEATURE_SPECS` covers every one (order-insensitive
  set match). Adding a shipped preset/combo to any of those files without listing
  it in `RELEASE_FEATURE_SPECS` fails `cargo test` (and CI) — it cannot silently
  pass. A change to any shipping source keeps `changes.code == true`, so both the
  `test` leg (the drift test) and the `release-feature-guard` leg re-run.

> **When the drift test fails:** it prints each uncovered `--features` string. Add
> each to `RELEASE_FEATURE_SPECS` in
> [`xtask/src/release_features.rs`](../../xtask/src/release_features.rs) (or, if a
> shipping source moved, update `SHIPPING_SOURCES`), then re-run
> `cargo test -p xtask --test release_features_drift`.

The predicate matches the `_test-seams` **family** (name contains `test-seam`),
not every leading-underscore feature: third-party crates legitimately resolve
internal `_`/`__` features in a release build (e.g. `reqwest/__rustls`,
`dimpl/_crypto-common`), which must NOT be flagged.

## How to run it locally

```
cargo run --locked -p xtask -- check-release-features
```

Exit `0` and `release-feature guard OK` when clean; exit `1` and a per-spec
`FAIL … resolves forbidden seam feature(s)` list when a preset leaks a seam.

Fast, cargo-free unit tests for the parser/predicate/report logic:

```
cargo test -p xtask --lib release_features
```

The end-to-end health check against the real presets (shells out to `cargo
tree`):

```
cargo test -p xtask --test release_features_guard
```

## How to verify the guard actually catches a leak

Temporarily add a throwaway feature edge that pulls the seam into a preset, run
the guard (it must FAIL), then revert:

```
# in crates/multiview-cli/Cargo.toml, inside the `full = [ … ]` preset:
#     "multiview-control/_test-seams",
cargo run --locked -p xtask -- check-release-features   # → FAIL  full … multiview-control/_test-seams ; exit 1
git checkout -- crates/multiview-cli/Cargo.toml         # revert the throwaway edge
cargo run --locked -p xtask -- check-release-features   # → OK ; exit 0
```

## What to do if CI fails this gate

A failure means a shipped release preset now resolves a test-only seam — someone
wired an internal `_test-seams`-family feature into a release path. **Do not**
weaken the guard or add the seam to an allowlist. Instead:

1. Read the failure: it names the spec and the offending `<crate>/<feature>`,
   e.g. `full — multiview-control/_test-seams`.
2. Find the edge that pulls it in — invert the feature tree for the offending
   crate and read the activator chain up to the preset:

   ```
   cargo tree --locked -p multiview-cli --features full -e features -i multiview-control
   ```

   Look for the `multiview-control feature "_test-seams"` node; its parent chain
   (e.g. `… → multiview-cli feature "full" (command-line)`) is the edge to remove.
3. Remove that feature edge. Common causes:
   - a preset in `crates/multiview-cli/Cargo.toml` gained a
     `"multiview-control/_test-seams"` entry (or a sub-crate feature that
     forwards it);
   - a crate's **normal** (non-`dev-dependencies`) manifest enabled the seam —
     the seam must only ever be enabled via the crate's own `[dev-dependencies]`
     self-reference (`multiview-control = { path = ".", features =
     ["_test-seams"] }`), never a normal dependency or a default feature.
4. Re-run `cargo run -p xtask -- check-release-features` until green.

If a genuinely new test-only seam is being added, name it in the `_test-seams`
family (contains `test-seam`) and gate it via a self `[dev-dependencies]` entry
so it stays out of the normal graph — the guard then protects it automatically.
