# Switcher first-slice release integration

## Scope and ancestry

Integration branch: `ship-rescue/switcher-release`, isolated worktree
`/workspaces/conspect/.worktrees/ship-rescue/switcher-release`.

Parents: reconciled base `1d9bc577` and reviewed switcher tip
`a44912b73ef47243045ed1c7d31d1d434df0b027`.
Their merge base is `f5c99baec40f6feba7cbca73090b83cfa01a1619`.
The switcher-only side contains the 20 commits from `a4abb03f` through
`a44912b7`, including the earlier frame/receipt/rollback work at `51315cd7`,
launcher work through `ca10192e`, lease recovery `1b839a9e`, and lint fixes
`a44912b7`. A true merge retains both histories without replaying old output
fixes or replacing the reconciled output root.

This is the reviewed opt-in first slice, not a full switcher implementation.
AUTO remains an honest 422 when the production provider cannot admit it.
A six-tile legacy canvas is not replaced: absent switcher config preserves the
legacy path. (Originally, opted-in multi-cell geometry failed closed; since
#307 it starts with Program as the one-cell M/E bus — see "One admission rule"
below.)
No final native-host acceptance run or deployment was performed. Those gates
remain with the captain after independent integration review.

## Conflict resolution

- `Cargo.lock`: retained both Cloudflare and compositor dependencies.
- `multiview-control/Cargo.toml`: retained Cloudflare's test feature and added
  compositor/framestore test dependencies. IPTV XML/gzip and MoQ dependencies
  remain intact.
- Locale catalogs: restored reconciled catalogs as the translation baseline,
  then ran Lingui extraction and compilation over the merged sources. No
  generated catalog/spec/type was edited by hand.
- Semantic conflict: the reconciled bounded idempotency store adds
  `Reservation::Capacity`. The switcher command route now returns the canonical
  `IdempotencyCapacity` error. A regression fills all 4096 slots and proves 503
  without a queued command or outcome.
- Imported idempotency tests exposed JSON-body command wrappers that still
  bypassed the shared sender. Media transport, cast volume, tally override,
  route take, apply-layout and device mode now use `sendCommand`, preserving
  response parsing and explicit keys while adding the default key.

All other source conflicts auto-merged. The output crate, program supervisor
and output registry are unchanged from the reconciled base. All 159 base
OpenAPI paths remain present. Web dependency manifests remain byte-identical.
Navigation/router changes only add the switcher entry; MoQ navigation remains.

## Generated artifacts

The clean worktree has no `cargo xtask` alias, so the actual repository binary
was invoked directly:

```
cargo run --locked -p xtask -- gen-openapi
cargo run --locked -p xtask -- gen-asyncapi
npm --prefix web run generate:api
npm --prefix web run generate:events
npm --prefix web run i18n:extract -- --clean
npm --prefix web run i18n:compile
```

## Passing evidence

Rust commands used `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0`, local `target/`,
and `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

- `cargo check --locked --workspace`: pass, default GPU/native-free build.
- `cargo fmt --all -- --check` and `git diff --check`: pass after formatting.
- CLI `--features ffmpeg --test switcher_first_slice`: 3 pass, including real
  HTTP cold-preview lease selection, received PGM/PVW pixels and opt-in
  multi-cell refusal. The earlier default invocation ran zero gated picture
  tests; only this feature-enabled invocation counts as picture evidence.
- CLI `--test software_run`: 10 pass, legacy path and clock/flood behavior.
- Control `switcher_lease_chaos`: 9 pass; `switcher_routes`: 6 pass;
  `switcher_realtime`: 1 pass.
- Config first slice: 5 pass; core switcher: 26 pass.
- Engine switcher bus/control/hook/plan/prime/repeat/runtime/state-machine:
  39 pass.
- Scoped control Clippy on all three switcher integration-test targets with
  `--locked ... -- -D warnings`: pass.
- Web lint and TypeScript no-emit: pass.
- Five imported switcher/correlation/idempotency test files: 34 pass.
- Apply-layout and device API tests: 18 pass.
- Web production build: pass.

## Concerns and remaining gates

Broader all-target Clippy is not green on this integrated base:

1. Default CLI all-target checking cannot import ffmpeg-gated `rendition_plan`
   from the existing rendition-plan test.
2. With ffmpeg, config rendition-plan tests have two `format_push_string`
   warnings at lines 430 and 462.
3. CLI library ffmpeg Clippy finds existing output-path warnings:
   `match_same_arms`, `too_many_lines`, and `inconsistent_struct_constructor`
   in pipeline output-worker code, plus `assigning_clones` in audio.

These regions are not changed by the switcher delta. They were not suppressed
or altered in this lane. The captain must reconcile its broader gate evidence.
Do not report full all-target Clippy PASS based on the scoped control result.

Web tests emit nonfatal React `act` warnings; production build reports existing
large chunks. `npm ci --ignore-scripts` reports 27 audit vulnerabilities
(15 moderate, 11 high, 1 critical); dependency manifests were not changed.
Arabic remains a stub locale (2472 untranslated of 2478 entries).

No full workspace test suite was run. No cross-vendor integration review, PR CI,
merge to main, native-host acceptance, or deployment is claimed here.

Resource check: local target reached 4.2 GiB, below the 10 GiB growth cap;
filesystem free space stayed above 360 GiB (lowest sampled 420 GiB, final
sample 474 GiB). No other worktree or gate-owner lane was edited.

## One admission rule on the live and start paths (#317, amended by #307)

With a switcher M/E declared, the Program canvas IS the M/E bus: one
full-canvas, opaque, `fill` cell bound to the M/E `program_source`. Until #307
the engine refused to start from any other canvas. Since #307 it derives that
canvas with one function, `MultiviewConfig::program_canvas`
(`multiview_config::program_canvas`): a document that already is the bus is
used unchanged (same cell id); any other document (a multi-cell wall, a styled
or windowed cell, a cell bound to another source) starts with a synthesized
Program cell (reserved id `me1.program`), and its own layout stays in the
layouts library as a stored layout routable to outputs. Only overlays with an
M/E are still refused (`SwitcherCanvasRefusal::Overlays`). `boot_admission` adds
`validate` + solve. The function is called by:

- `Pipeline::build` (start-up: the Program drive layout and the M/E cell id);
- `boot_model::write_desired`, the single choke point every `desired.json` commit
  passes through, so no command can persist a state the engine cannot boot;
- `POST /api/v1/commands/apply-layout`: with a switcher it answers `422`
  `/problems/switcher-owns-program` before the idempotency reservation, with
  nothing reserved, applied or persisted; `target: {output}` makes it an output
  take of the layout;
- the engine drain (`CommandDrain::switcher_program`): with an M/E the drive is
  keyed by the Program cell alone; a stored layout from the config watcher
  updates only the document mirror, never the Program drive, and never resets
  the M/E's Program binding (an `apply_layout_held` outcome says so). A no-document
  `ApplyLayout` is ignored before any solve (no clock-side work);
- the cell-edit routes (#307 review M1): with an M/E declared,
  `POST /commands/swap`, a video→cell `POST /routing/video/take` and a salvo
  take/fire that recalls a layout or cell sources answer `422`
  `/problems/switcher-owns-program` after authorization and before the
  idempotency reservation. They would change neither Program (the M/E bus) nor
  the stored layout outputs show; the detail names `PUT /api/v1/layouts/{id}`
  (routed outputs update live) and a switcher select. One that still reaches the
  drain is dropped (warned once) without mutating the working-config mirror;
- chaos (#307 review I2): `crates/multiview-cli/tests/switcher_layout_chaos.rs`
  drives a two-cell wall's synthesized Program canvas through the real
  `command_drain` inside the real `SwitcherFrameHook` for 600 manual-clock
  ticks while stored/no-document `ApplyLayout`, `SwapSource`, `RouteVideo`
  (wall cell and the Program cell id) and salvo arm/take flood the bounded
  command bus, Cut/SelectPreview flood the M/E mailbox, a stalled subscriber
  never reads either publisher, and the Preview source is lost for 100 ticks.
  It asserts one frame per tick, contiguous ticks, PTS = tick x 40 ms, no
  repeat, one full-canvas bound source on Program (never the wall), both
  queues filled and never blocked, at most `MAX_COMMANDS_PER_TICK`+1 M/E
  outcomes per tick, a per-tick command intake of at most twice the bus
  capacity, and a final Cut (Preview forced to the off-air source first) that
  completes and changes the Program picture. Review round 2 bounded
  `CommandReceiver::try_drain` to the bus capacity per call (it drained until
  empty while producers refilled), with a unit test under a refilling producer;
- `multiview validate <config>`, which now also checks what `multiview run` will
  start from (`.multiview/desired.json` when it exists) and exits non-zero with
  start-up's reason. Run it as the deploy preflight. It also warns while an
  earlier start-up fallback ([ADR-W032](../decisions/ADR-W032.md), amended by
  #324) is pending acknowledgement.
