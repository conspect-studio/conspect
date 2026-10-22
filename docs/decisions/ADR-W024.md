# ADR-W024: Boot / Loaded / Running configuration model — resume, revert-to-start, promote-to-boot

- **Status:** Accepted
- **Area:** Web/API stack · config-as-code lifecycle (invariant #11 live-apply classification)
- **Date:** 2026-06-19
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md);
  builds directly on ADR-W020 (config-file watch + `expect_write()` self-write suppression),
  ADR-W015 (config export composition), ADR-W018/W019 (the one live-apply machinery),
  and invariants #1/#10/#11.

> **History.** This decision was first authored on a parallel stack (as that stack's
> "ADR-W022") that diverged from `main` before ADR-W020 landed. `main` independently assigned
> `W022`/`W023` to other accepted decisions, and ADR-W020 shipped most of the file-watch +
> `expect_write()` machinery this design assumed it would build. This ADR is the re-authored,
> renumbered decision on top of **current `main`**: it consumes ADR-W020's machinery rather
> than re-creating it, and is reconciled against the code that exists today (see §2 and
> "Reconciliation with current `main`").

## Context

After ADR-W020 a deployment's configuration moves through three independent triggers — the
WebUI/REST API, the file watcher, and restarts — but the process has **no model of where it
started, what it is running now, and what the file on disk says**. Concretely:

* A crash or power cut loses every live change since boot; there is no machine-written record
  of the running state to recover from. (The ADR-W020 config-versioning store is an
  **in-memory** `InMemoryConfigVersionStore`; it does not survive a restart, so it cannot be
  a resume source.)
* An operator who has drifted mid-show ("we changed six things during the broadcast") has no
  one-button way back to the known-good state the show **started** with — especially if the
  boot file itself was edited mid-show.
* The "edit live, then export, then scp the file back" loop (ADR-W015) works but is manual;
  the `expect_write()` suppression seam ADR-W020 built for a server-side promote flow has
  **no caller**.
* The UI shows live state as if it were the durable truth; there is no honest indicator that
  the running configuration has diverged from the boot baseline.

## Decision

### 1. The model (pinned)

> - **Boot** = the config file `multiview run` was started with: the deliberate cold-start
>   baseline; the watched + hand-edited file (the ADR-W020 watcher already watches it).
> - **Loaded** = an immutable snapshot of Boot taken at process start, kept in memory AND
>   persisted to disk (`<config-dir>/.multiview/loaded.toml`, atomic write at startup) — the
>   "recover to exactly where it was on start" target even if the boot file was edited
>   mid-show.
> - **Running** = Loaded + every live change since (API/UI edits, file-watch applies).
>   Continuously persisted — debounced (~2 s), atomic rename, machine-written, NEVER
>   watched — to `<config-dir>/.multiview/active.toml`. Compose source: the SAME document
>   composition `GET /api/v1/config/export` uses (`compose_export_document` in
>   `crates/multiview-control/src/routes/config.rs`) — reuse, do not duplicate.
>   Trigger: any successful resource/layout mutation + file-watch apply — ONE choke point
>   (the audit recorder), not sprinkled call sites.
> - **Cold-start policy**: `[control] start = "boot" | "resume"` (serde default `"boot"`;
>   the token is validated). `"resume"`: if `active.toml` exists, parses, AND validates,
>   load IT as the starting Running state (the boot file stays the watch target);
>   invalid/missing active → fall back to boot with a warning.
> - **Actions** (REST, OpenAPI-annotated, RBAC write, `Idempotency-Key` like other
>   commands, audited):
>   - `POST /api/v1/config/revert-to-start` — Running := Loaded, applied LIVE through the
>     one apply machinery (ADR-W020's diff→action application:
>     `diff(current-running-doc, loaded-doc)` → the same command paths; restart-only
>     sections get the same honest warnings). `202` + per-section applied/warned summary.
>   - `POST /api/v1/config/promote` — write the current Running document to the BOOT file
>     path (server-side), using the watcher's `expect_write()` suppression seam (this is
>     its caller) + a config-versioning commit + audit. Confirm-required in the UI.
> - **UI honesty**: a persistent divergence indicator with **Revert to start** (confirm,
>   destructive styling) and **Promote to boot** (confirm; explains it rewrites the config
>   file) actions.

`<config-dir>` is the directory containing the boot config file; the state directory is
`<config-dir>/.multiview/` (created on demand). Both `loaded.toml` and `active.toml` are
**machine-written canonical TOML** (`MultiviewConfig::to_toml` of the parsed, validated
document — comments in the hand-authored boot file are not carried; these are state files,
not authoring surfaces). The watcher never watches them, and `validate` runs on every write
path, so an `active.toml` that exists always round-trips `MultiviewConfig::validate`.

#### Why `revert-to-start` is **not** config-versioning `rollback`

`main` already exposes `POST /api/v1/config/{target}/rollback` (ADR-W020 versioning):
restore *any* prior committed revision of a tracked document as a new revision. That is a
**document-history** operation keyed by revision id. `revert-to-start` is a distinct
**operator capability**: "return the whole Running state to the deliberate cold-start
baseline (Loaded)", applied live through the engine via the one apply machinery — not a
history walk and not a new committed revision of the `boot` target. They coexist: rollback
answers "go to revision N of this document"; revert-to-start answers "undo the whole show's
live drift back to where we started". This ADR adds no parallel apply path — revert-to-start
calls the same `apply_document_diff` the watcher and the routes already use.

### 2. The apply machinery moves to `multiview-control` (one machinery, three triggers)

ADR-W020's diff→action application (`apply_diff` + the store resyncs + the shared
`resolve_layout_document` call path) lives in `multiview-cli/src/config_watch.rs`, which the
revert route cannot reach (the dependency arrow is `cli → control`). The `config_watch`
module **moves into `multiview_control::config_watch`** (the CLI keeps a re-export shim so
`multiview_cli::config_watch::*` paths — including the existing ADR-W020 integration tests —
compile unchanged). The apply core becomes the public
`apply_document_diff(state, actor, &diff, &next) -> ApplyOutcome { summary, restart, shed }`,
with the audit **actor** a parameter: the watcher passes `config-file`, the revert route
passes the authenticated principal. There is exactly one diff→apply implementation; the
watcher and the revert route are two callers of it (the routes were already the third trigger
via their own command submissions).

The move **reconciles `main`'s current module verbatim** (the shed-aware M1 retry, the
`config-file-apply-incomplete` warning, the content-paired `expect_write`); it does not
replace any of that behaviour. Only the module's *home* and the *visibility* of the apply
core change.

### 3. Running persistence: one choke point, debounced, fail-soft

* **Choke point:** `AppState::audit()` — every successful mutation in the control plane
  (resource CRUD, accepted commands, file-watch resyncs, revert applies) already records an
  audit entry there. After recording, it now also fires `running_changed`
  (`tokio::sync::Notify` on `AppState`): a one-permit, coalescing signal that can never
  queue, grow, or block (inv #10). No call site changes; future mutating routes inherit the
  trigger by construction.
* **Persister:** a control-plane tokio task (`boot_model::spawn_running_persist`): waits on
  `running_changed`, sleeps the debounce (~2 s, bounded — at most one write per debounce
  window), then composes the running document via the SAME `compose_export_document` the
  export route uses, deserializes + `validate()`s it, renders TOML, and writes
  `active.toml` atomically (same-directory temp file → `fsync` → `rename(2)` → directory
  `fsync`). Any failure — a store fault, a document that does not compose, an I/O error —
  is a `tracing::warn!` and a skipped write; the task never exits on error and nothing else
  is touched (fail-soft). The file I/O rides `spawn_blocking` (the fsync'd write never parks
  the control-plane reactor; the boot-model status route's file read rides `tokio::fs` for
  the same reason). At startup the starting Running state is persisted once (so a stale
  `active.toml` from a previous run can never outlive the run that supersedes it), and at
  graceful teardown — reached on Ctrl-C **and SIGTERM** (`docker stop`/systemd) —
  `finish_running_persist` aborts the task, awaits it, then runs one final best-effort
  persist capturing changes younger than the debounce. Awaiting the task is **not** by itself
  a single-writer guarantee (a `spawn_blocking` write the aborted task started keeps running
  detached on the blocking pool); the guarantee comes from `write_active_serialized` — every
  boot-model file write holds the model's write lock (no interleaving on the deterministic
  temp names; the promote's boot-file write takes the same lock), and `active.toml` writes
  carry a compose-time **ticket** so a detached stale write is skipped once newer content
  landed (content is monotonic, never regressing).
* **Audit-trigger over-approximation, accepted:** some audit entries (e.g. a config-revision
  commit, an alarm ack) do not change the composed document; the debounced persister then
  rewrites an identical `active.toml`. That costs one small atomic file write per ~2 s worst
  case and keeps the trigger a single, future-proof choke point.

### 4. Cold-start resume

`ControlConfig` gains `start: StartMode` (`boot` | `resume`; serde-typed, so an unknown
token fails parse — validated by construction; default `boot`). The **boot** file's policy
decides. On `resume`, the run reads `<config-dir>/.multiview/active.toml`; if it reads,
parses, and validates, that document becomes the starting Running state — with the
**storeless restart-only sections spliced from the BOOT document** (`control`, `placement`,
`walls`, `routing`, `schema_version` have no control store; the boot file is their durable
truth and a restart is exactly when they take effect, so a boot-file `[control] listen` edit
lands on the restart the operator performed instead of losing to the stale machine-written
copy). `salvos` and `tally_profiles` are deliberately NOT in that splice (round 6): they are
**store-backed always-commit running state** (their definition routes are pure control-plane
store edits with no engine command — see the round-6 hardening section), so they resume from
`active.toml` exactly like `sources`/`overlays`, and a boot-file edit to them is picked up
live by the watcher (which resyncs their stores) rather than discarded on every restart. The
spliced document is re-validated; a
combination that no longer validates falls back to boot with the reason surfaced. The engine
is built from the result, the control stores are seeded from it, the export base document is
it, and the ADR-W020 watcher's baseline is it — while the watcher keeps watching the BOOT
path, so an external boot-file edit during a resumed run still hot-applies (the diff is
computed against the resumed baseline). The watcher's **last-observed content is seeded with
the boot-load text** (`with_initial_observed`): a settled observation whose content still
equals it — the unchanged boot file under a resume, or a touch/identical rewrite — is adopted
without applying, so the resumed state is never clobbered by a file that did not actually
change; an edit landing in the boot window differs from that text and still applies (the
ADR-W020 review-M2 semantics hold). A missing/unreadable/invalid `active.toml` falls back to
the boot document with a `tracing::warn!` naming the reason (also surfaced on
`GET /api/v1/config/boot-model` as `resume_fallback`). **Loaded stays the boot snapshot in
both modes** (the model pins it: revert-to-start targets the deliberate cold-start baseline,
not the resumed state).

### 5. `POST /api/v1/config/revert-to-start` (role: write, `Idempotency-Key`, audited)

Running := Loaded, live: compose the current Running document (the export composition),
deserialize it, `ConfigDiff::between(running, loaded)`, and hand the diff to the one
`apply_document_diff` — synthetic source adds/edits/removals ride
`UpsertSource`/`RemoveSource` on the bounded bus, a layout/cells delta rides the shared
resolve+solve+Class-1 gate and `ApplyLayout`, stores resync to the Loaded values (audited
under the requesting principal), and restart-only sections are reported honestly in the
response's `restart_only` (in the common boot-start case the engine never adopted those
sections' drift, so reverting their stores actually re-converges doc and engine; after a
resume with a different canvas the Class-2 hold applies exactly as in ADR-W020). The
response is `202` with the per-section summary. An empty diff returns `202` with
`reverted: false` and applies nothing. The response is **shed-aware**: `reverted: true` with
the full summary only when every engine command landed on the bounded bus; a shed apply
answers `202` with `reverted: false`, the `shed` count, the partial summary, and raises the
`config-file-apply-incomplete` warning with revert-specific remediation. A shed revert
applies **nothing durable**: the stores are rolled back to the pre-revert Running document,
so the retry's `diff(running, loaded)` is non-empty again and re-runs the whole (idempotent)
revert — without the rollback the first pass's store resync would leave the retry an
empty-diff no-op while the engine still ran the un-reverted state. A revert that completes
(or an honest empty-diff no-op) clears the revert-raised warning instance via a latch on the
boot model — never the watcher's own instance of the same code. A composition failure
(`422`) releases the `Idempotency-Key` reservation so a corrected retry with the same key
actually runs. The ADR-W020 watcher's *file* baseline is deliberately untouched — its
baseline tracks the last applied **file** content (W020 semantics; API edits already moved
Running without moving it), and the latched `config-file-requires-restart` warning stays
honest because the file still differs from what the engine adopted.

### 6. `POST /api/v1/config/promote` (role: write, `Idempotency-Key`, audited, UI-confirmed)

Write the current Running document to the **boot file path**, server-side: compose →
deserialize → `validate()` → `to_toml()` → `expect_write()` on the installed watch handle
(ADR-W020 — this is the seam's first and intended caller; the token is **content-paired**, so
an unrelated external edit landing inside the settle window is still applied, never adopted)
→ atomic write to the boot path (on the blocking pool, under the boot model's write lock) →
`confirm_server_write()` → a config-versioning commit (target **`boot`**, the promoted JSON
document, the principal, message `promote running configuration to boot`) → audit. Returns
`200` with the written path, byte count, and the committed revision id. With no watcher
installed (store-only deployments) the suppression step is skipped — there is nothing to
suppress. A **failed write releases the banked token** (a leaked token would silently adopt a
later real edit carrying the same content) and releases the idempotency reservation; a
**successful write is confirmed as landed**: once a landed write is superseded by a different
settled content it can never be the next settled observation, so the watcher drains its
token — a stale token must never eat a much later real edit restoring the same bytes
(`git checkout`, editor undo); an announcement never confirmed keeps the in-flight semantics
(ADR-W020) and still suppresses exactly its content when it finally settles. The atomic write
**preserves the destination's mode**, applied before the content lands (a chmod-600 boot file
stays 600 and its secrets never sit at the umask default even transiently), and the boot path
is canonicalized at startup so a symlinked config is promoted at its real file — the rename
replaces the file, never the symlink.

### 7. Observability: `GET /api/v1/config/boot-model` (role: read)

A small read-only endpoint backing the UI indicator: whether a boot model exists
(`modeled`), the boot path, the `start` policy, whether this run `resumed` (+
`resume_fallback` reason), the per-section divergence of Running from **Loaded**
(`diverged_from_loaded`, computed by the same pure `ConfigDiff` — exact, cheap at human
poll rates) and from the **current boot file** (`diverged_from_boot_file`, `null` +
`boot_file_error` when the file is unreadable/invalid), and the last successful
`active.toml` write time. The UI shows section **names**, not a count: the originally
floated "Boot + N changes" count is not cheaply truthful (the audit ring is bounded and one
logical apply records several entries), while the per-section diff is exact and more
actionable — this is the "divergence boolean + why" fallback the model allows, strengthened
to per-section names.

**UI (follow-up lane, DEV-D3-adjacent):** the SettingsPage configuration area gains a "Boot
configuration" card with the divergence lines, **Revert to start** (destructive styling +
confirm dialog) and **Promote to boot** (confirm dialog). The SPA work is sequenced as a
separate re-author lane (this lane is the API + engine surface); the endpoints ship complete
and OpenAPI-documented here, so the card is purely additive. All strings will be
Lingui-wrapped.

### 8. Scope and isolation

Everything in this ADR is control-plane tenancy on tokio: composition reads read-mostly
stores, persistence writes files, applies ride the existing bounded non-blocking bus and the
drop-oldest publisher. The render thread never sees any of it (inv #1/#10). Like the
ADR-W020 watcher, the boot model exists only when the run serves a control plane from a
config file; `--ticks` smoke runs and store-only deployments honestly report `modeled:
false` and refuse the actions with a problem document.

## Reconciliation with current `main`

This decision is re-authored on `main` @ `b7528267`; the following facts were verified before
porting:

* **Persistence is genuinely required.** `ConfigVersionStore` is `InMemoryConfigVersionStore`
  only (no sqlx persistence; never constructed persistent in `multiview-cli`), and
  `AppState.base_document` is populated *from* the resolved config at startup. Neither
  survives a process restart, so resume cannot read from them — the on-disk `active.toml`
  snapshot is the resume source. `loaded.toml` is the forensic/cold-start baseline on disk.
* **`compose_export_document` already exists** (`routes/config.rs`) and composes the running
  document from `base_document` + the live stores; Running persistence and the divergence
  endpoint reuse it (no duplicate composition).
* **`config_watch` currently lives in `multiview-cli`** (with the ADR-W020 `expect_write`,
  the M1 shed-retry, and the apply-incomplete warning). It moves to `multiview-control` with
  a cli re-export shim, reconciling the current module — not the older parallel-stack copy.
* **`load_validated` returns only `MultiviewConfig` today**; it grows to return the raw boot
  text too (the watcher's initial observed content + the resume window semantics).
* **`config_watch::spawn` has no `with_initial_observed` today**; it gains one so a resumed
  run does not reapply the unchanged boot file.
* **`bind_and_serve` lives in `multiview-cli/src/control.rs`**; it and `AppState` gain the
  optional `boot_model` and the `running_changed` `Notify`.

## Alternatives considered

* **Trigger persistence from every mutating call site** — rejected: ~30 call sites today
  and a guarantee that future routes forget one. The audit recorder is the one funnel every
  successful mutation already passes through.
* **Reuse the in-memory config-versioning store / `base_document` as the resume source** —
  rejected on evidence: both are in-memory and do not survive a restart (see Reconciliation),
  so neither can resume a crashed process; a durable on-disk snapshot is required.
* **Reuse `POST /config/{target}/rollback` instead of a distinct `revert-to-start`** —
  rejected: rollback is a document-history operation (go to revision N of one tracked
  document); revert-to-start is a whole-Running-state operator capability (undo the show's
  live drift to the cold-start baseline) applied live through the engine. Different intent,
  different surface; they coexist (see §1).
* **Watch `active.toml` for changes** — rejected outright: it is machine-written state, not
  an authoring surface; watching it would loop the writer into the reader.
* **Persist Running as raw JSON of the stores** — rejected: `active.toml` must be a valid
  boot-shaped document (`load_from_toml` + `validate` round-trip) so resume can treat it
  exactly like a config file; the export composition already guarantees that shape.
* **A "Loaded follows resume" variant (revert returns to the resumed state)** — rejected:
  the model pins Loaded to the Boot snapshot; the deliberate cold-start baseline is the
  revert target an operator reasons about mid-show.
* **An exact "N changes" counter** — rejected for honesty (see §7): bounded audit ring +
  multi-entry applies make N unreliable; the per-section diff is exact and cheaper to trust.
* **Promote via the client (export → upload)** — exists already (ADR-W015) and stays; the
  server-side promote removes the lossy manual hop and is the `expect_write()` seam's
  designed consumer.

## Consequences

* A power cut no longer loses the show state: `start = "resume"` brings the process back to
  the last persisted Running state within the debounce window, while `loaded.toml` always
  preserves the cold-start baseline for forensics and revert.
* The `config_watch` module (and its `ConfigWatchHandle`) now lives in `multiview-control`;
  `multiview-cli` re-exports it. `AppState` grows `boot_model`, `running_changed`, and the
  installed watch handle; `bind_and_serve` accepts the optional boot model.
* Three new endpoints join the OpenAPI document (`revert-to-start`, `promote`,
  `boot-model`); the SPA client regen + the Settings card are a sequenced follow-up lane.
* `active.toml` fidelity equals export fidelity by construction: anything the export
  composition does not capture (e.g. a bare `swap` re-point that only mutates the engine's
  working binding) is missing from Running persistence too — one composition to fix, both
  surfaces improve (known ADR-W015/W019 limitation, unchanged here).
* Sections without control stores (`control`, `placement`, `walls`, `routing`,
  `schema_version`) cannot hot-revert; revert reports them `restart_only` and the indicator
  keeps naming them until restart. `salvos` and `tally_profiles` DO have control stores
  (round 6), so a revert re-converges their stores to Loaded (and `active.toml` follows), but
  the engine adopts a salvo/tally-profile DEFINITION change only on the next arm/take, so the
  section is still reported `restart_only` for the running recall — store reverted, recall on
  next use.

## Cross-vendor panel hardening

The 3-lens review surfaced five defects on this design; the fixes are part of the
implementation:

* **Secret state-file writes are exclusive and fail-closed (MAJOR-A).** `loaded.toml`/
  `active.toml` carry the composed Running config with secrets intact (resume needs the real
  credentials). `write_atomic` creates the temp via `tempfile::NamedTempFile::new_in` —
  `O_EXCL` + an **unpredictable** name + `0600` on Unix — then clamps the final mode to a
  `0600` floor (stricter if the destination is), `fsync`s, atomically renames (`persist`),
  and `fsync`s the directory. That defeats both attacks a deterministic `.<name>.tmp` name
  allowed: a pre-existing world-readable temp inode (the secrets are never written into it)
  and an attacker-planted symlink at the temp path (it is never opened/followed).
  `create_state_dir` creates the `.multiview` dir `0700` and **fails closed** when an
  existing one is group/world-writable or owned by another uid (`rustix::process::geteuid`,
  a safe wrapper) — refusing to persist there, which never takes output off air (the output
  clock is untouched).
* **Persistence reflects only ADOPTED state — a lock-ordered adopted snapshot (MAJOR-B).**
  The consistency invariant is **`active.toml` == the running configuration the engine has
  ADOPTED**. It is guaranteed structurally, not by a counter (the round-3 generation gate had
  two defects: over-adoption — an unrelated landed mutation could advance a global `adopted`
  past a prior shed's generation; and a missing happens-before — the gate atomics were not
  ordered with the store mutation behind a different lock, so a racing persister could compose
  mid-mutation).

  The live resource stores deliberately hold **requested** state, not adopted state: ADR-W018
  pins that a shed/non-live REST mutation still **commits the store** and answers `2xx` +
  `X-Multiview-Apply: restart` (the stored doc is the durable config-as-code truth; it applies
  on restart). So `active.toml` cannot be composed from the live store — it is composed from a
  separate **adopted snapshot** (`BootModel.last_adopted`, a `MultiviewConfig`):

  - **One lock for mutate+adopt+persist.** The approved `config_mutation_lock` is extended to
    cover EVERY live mutation — the file-watch apply AND every REST mutation
    (sources/overlays/layout) — each holding it across its whole validate → store-write →
    submit → (snapshot-update | mark-diverged) sequence. `persist_running_now` acquires the
    **same** lock before reading the snapshot. `try_submit` is the bounded, non-blocking
    drop-oldest push (ADR-W008), so the hold is brief (the same shape as `promote` holding the
    lock across its `spawn_blocking` write) and never touches the output clock (inv #1/#10).
  - **The snapshot advances by per-section ADOPTED deltas, never by recomposing the live store
    and never wholesale.** It starts as the startup running config. A section's delta is applied
    to the snapshot **only when the engine adopted that specific section LIVE** (its command
    landed): an adopted source upsert/remove (`adopt_source`/`unadopt_source`); an adopted
    overlay upsert/remove (`adopt_overlay`/`unadopt_overlay`); an adopted layout
    (`adopt_layout` — sets the snapshot's canvas + layout + cells from the working-layout body).
    A shed, restart-only, or non-live change applies **nothing**. The file-watch path runs the
    SAME per-section adoption inside `apply_document_diff` (which already classifies each section
    live-landed vs restart-only): it adopts only the sections it applied live — never the whole
    requested document — so a restart-only/non-live file edit (e.g. an `outputs` change, or a
    non-synthetic source that only applies on restart) never enters `active.toml`. Because only
    the specific landed delta is applied, an unrelated landed mutation can never adopt a prior
    shed's change.
  - `persist_running_now` composes from the store WITHOUT validating, then OVERRIDES every
    snapshot-backed section (sources, overlays, **canvas/layout/cells**) from the snapshot,
    validates the result, and writes it. It never writes the live store directly.

  **Complete-coverage proof — DERIVED from the routes + the file-watch, not hand-enumerated
  (round 6).** Every section `compose_document_unredacted` writes is classified by *how it
  mutates at runtime*, which must agree across THREE places: the compose source, the resume
  splice (`multiview-cli/src/boot.rs::splice_storeless_sections`), and the file-watch
  changed-section handling. The classes:
  *live-sheddable* (a REST/file mutation submits an engine command that can `EngineBusy` — MUST be
  snapshot-backed per landed delta, composed from the snapshot); *always-commit store-backed* (a
  pure control-plane store edit with **no** engine command — the store IS the adopted state by
  construction, composed straight from the store, resumed from `active.toml`, never spliced);
  *restart-only store-backed* (store-backed but the engine only adopts on the next start, which IS
  the resume — store == adopted, composed from the store); *static* (no store, no REST write —
  carried verbatim from the immutable `base_document` and spliced from boot on resume); or
  *runtime* (engine-runtime state, not composed into `active.toml` at all).

  | Section(s) composed | Runtime mutation (route → engine command?) | Class | active.toml compose source | Resume | File-watch |
  |---|---|---|---|---|---|
  | `sources` | `POST/PUT/DELETE /sources/{id}` → `UpsertSource`/`RemoveSource` (synthetic) — **sheddable** | live-sheddable | adopted snapshot (`adopt_source`/`unadopt_source`) | from active.toml | `apply_source_changes` (submit + adopt on land) |
  | `overlays` | `POST/PUT/DELETE /overlays/{id}` → `UpsertOverlay`/`RemoveOverlay` — **sheddable** | live-sheddable | adopted snapshot (`adopt_overlay`/`unadopt_overlay`) | from active.toml | `apply_overlay_changes` (submit + adopt on land — round 6 / F2, mirrors REST) |
  | `canvas` + `layout` + `cells` | `PUT /layouts/{id}` → `ApplyLayout` — **sheddable** | live-sheddable | adopted snapshot (`adopt_layout`) | from active.toml | `apply_layout_change` (submit + adopt on land) |
  | `salvos` | `PUT/DELETE /salvos/{id}` → **no engine command** (arm/take/cancel submit recall cmds, never the definition) | always-commit store-backed | **the store** (round 6 / F1) | from active.toml (NOT spliced) | `resync_salvos` (store reseed; restart-pending recall) |
  | `tally_profiles` | `PUT/DELETE /tally/profiles/{id}` → **no engine command** (override submits a runtime cmd, never the profile) | always-commit store-backed | **the store** (round 6 / F1) | from active.toml (NOT spliced) | `resync_tally_profiles` (store reseed) |
  | `outputs` | `POST/PUT/DELETE /outputs/{id}` → none (output reconfig is restart) | restart-only store-backed | the store | from active.toml | `resync_store` (restart-pending) |
  | `probes`, `devices`, `sync_groups` | their CRUD routes → none | restart-only store-backed | the store | from active.toml | `resync_store` (restart-pending) |
  | `audio` (audio-routing) | `PUT /audio-routing` → none (defers to restart) | restart-only store-backed | the store | from active.toml | `resync_audio` (restart-pending) |
  | `schema_version`, `control`, `placement`, `walls`, `routing` (the `[routing]` config block) | no store, no REST write (`/routing/{kind}/take` submits only runtime `Route*` crosspoints) | static | verbatim from `base_document` | **spliced from boot** | restart-pending only |
  | tally lamp state, salvo arm/take, routing crosspoints | engine-runtime only | runtime | n/a — not composed | n/a | n/a |

  No live-sheddable section is unbacked; every always-commit/restart-only section is composed from
  its store (store == adopted by construction); and the compose source, the resume splice, and the
  file-watch handling agree per section. A runtime-mutable section that was store-backed in one
  place but static in another (round 6: `salvos`/`tally_profiles` were composed verbatim, spliced
  from boot, and not file-watch-resynced — so a runtime edit was LOST on resume) re-introduces the
  unadopted/lost-state leak; the derived table is the compiler-adjacent check against that.

  **Adversarial self-check (all four sequences):**
  - *seq-1 over-adoption (shed `DELETE /sources/in_a` → drain → unrelated source/overlay upsert
    lands).* The shed delete applies nothing to the snapshot (the engine still runs `in_a`); the
    later upsert applies only its own add. `active.toml` keeps `in_a` and gains the new resource. ✓
  - *seq-2 mid-mutation race.* Every mutation + the persist hold the one `config_mutation_lock`,
    so the persister blocks until a mutation fully resolves and reads a settled snapshot. ✓
  - *seq-3 layout leak (landed `PUT /layouts/working` → SHED `apply-layout`).* The PUT commits the
    working-layout store (requested), but the `ApplyLayout` sheds, so `adopt_layout` is NOT called
    — the snapshot keeps the previously-adopted canvas/layout/cells, and persist OVERRIDES those
    from the snapshot, so `active.toml` keeps the adopted layout and gains nothing unadopted. ✓
  - *seq-4 watcher over-adopt (a file-watch apply carrying a restart-only/non-live change).* The
    file-watch `apply_document_diff` adopts only the sections it applies LIVE; a restart-only
    section (e.g. `outputs`, or a non-synthetic source) is reseeded into the store but applies
    nothing to the snapshot, so `active.toml` never captures the unadopted change. ✓
* **Promote/watch concurrency is closed (MAJOR-C).** (C1) `bind_and_serve` installs the
  config-file watch handle into `AppState` BEFORE the router serves any request, so a
  promote in the startup window always finds the suppression seam. (C2/C3) `promote` and
  `revert-to-start` hold one shared `config_mutation_lock` across their whole
  compose → write/apply → commit critical section (composing AFTER acquiring it), so two
  promotes cannot interleave the suppression token and a revert cannot mutate Running
  between a promote's compose and its commit.
* **The ADR's claims are made true (MINOR-D).** Graceful teardown runs on `SIGTERM` as well
  as Ctrl-C (`docker stop` / systemd), and the boot path is canonicalized at startup so a
  symlinked config is promoted at its real target file.

### Round-6 under-adoption fixes (the re-panel after round 5)

Round 4 missed `layout`; round 5 closed `layout` but the re-panel found three more
runtime-mutable sections whose ADOPTED state `active.toml` still failed to capture. The lesson:
**do not hand-enumerate the composed sections — DERIVE the classification from the routes and the
file-watch**, because a misclassification hides wherever the compose source, the resume splice,
and the file-watch handling silently disagree. The round-6 coverage table above is that derivation
(every `state.<store>` with a runtime mutation path, classified, with all three call sites checked
to agree). The three fixes:

* **F1 — `salvos` + `tally_profiles` were lost on resume (correctness).** Their definition routes
  (`PUT/DELETE /api/v1/salvos/{id}`, `PUT/DELETE /api/v1/tally/profiles/{id}`) are pure
  control-plane store edits with **no** engine command (the salvo arm/take/cancel and the tally
  override submit *runtime recall/lamp* commands, never the definition), so the store IS the
  adopted state. But round 5 took both VERBATIM from `base_document`, spliced them from boot on
  resume, and never file-watch-resynced them — three places all treating them as *static*. They
  are now *always-commit store-backed*, exactly like `outputs`: seeded from the config in
  `seed_resources`, composed FROM the store in `compose_document_unredacted`, audited on every
  definition mutation (so the one persist choke point fires), reseeded by the file-watch
  (`resync_salvos`/`resync_tally_profiles`), and resumed from `active.toml` (removed from
  `splice_storeless_sections`). No snapshot backing is needed — a pure store edit cannot shed, so
  store == adopted by construction.
* **F2 — the file-watch `overlays` branch diverged from the REST overlay routes (concurrency).**
  The REST routes submit `UpsertOverlay`/`RemoveOverlay` and adopt per landed delta on an
  overlay-capable run, but round 5's watcher handled the `overlays` changed-section as a
  restart-only store reseed — so a live overlay edit arriving through the watched boot file never
  entered `active.toml`. The watcher now applies overlays through `apply_overlay_changes`, which
  mirrors the REST path exactly: with a live overlay capability it submits per-overlay
  `UpsertOverlay`/`RemoveOverlay` and adopts/unadopts each LANDED delta (a shed counts toward the
  M1 retry and keeps the section restart-pending); with no capability it stays honest restart-only.
  Watcher, REST, and the snapshot now agree.
* **F3 — the shed-REST-source test was a tautology (rule 19).** It only filled the bus and
  asserted a value that was never submitted was absent. It now ACTUALLY performs the recolor over
  the full bus (committing the store and shedding the engine command) and asserts the store
  committed, so it genuinely exercises the shed-REST path the adopted-snapshot gate guards.

### Round-7 fix — overlay retry lost-update (the re-panel after round 6)

The round-6 `apply_overlay_changes` was correct on a clean apply but had a **lost-update on a
shed retry (concurrency)**: it computed the overlay delta from the **mutated control store** and
`resync_store`d the store to `next` BEFORE the retry resolved. On a shed (`apply_change` →
`Retry`) the file baseline does not advance, but the next retry saw the store already == `next`,
submitted NO `UpsertOverlay`/`RemoveOverlay`, falsely reported `all_landed`, cleared
restart/incomplete, and advanced the baseline — so the shed overlay edit was **silently dropped**
(never delivered to the engine, never adopted). A shed became a false success.

The fix mirrors the source path exactly: the overlay delta is now **baseline-derived**. A new
`OverlayChange` enum + `diff_overlays` populate `ConfigDiff.overlays` in
`ConfigDiff::between(baseline, next)` — the same shape and the same stable-across-retries property
`SourceChange`/`diff_sources` already had. `apply_overlay_changes` consumes `diff.overlays` (never
`state.overlays.list()`), so on a shed retry the diff is recomputed from the *un-advanced
baseline* against `next` and the command is re-submitted until it lands. The store reseed still
runs last (the UI mirror follows the file) but no longer feeds the delta. `apply_layout_change`
was audited and is already baseline-derived (it derives the working-layout body from `next` and
gates on `diff.layout_changed`/`diff.canvas_signal_changed`, recomputed from the baseline each
retry; `adopt_layout` uses `next`), so it had no equivalent bug. Two deterministic tests cover the
add and the remove directions (fill bus → file edit → assert not-yet-adopted while shed-pending →
drain → assert re-delivered on retry). META: a live-sheddable file-watch section MUST derive its
retry delta from the file baseline (the `ConfigDiff`), never from a control store the same apply
mutates — a store-derived delta self-erases on retry.

### Task #130 — a pure equal-z reorder is a detected delta (the re-panel residual)

The round-8 correctness re-panel left one **non-blocking** residual: a pure **reorder** of
overlays/sources that keeps every document identical and every `z` equal was **invisible** to the
id-keyed diff. `diff_overlays`/`diff_sources` key on `id`, so a permutation of the same id-set
produces an EMPTY `Added`/`Changed`/`Removed` list — yet **declaration order is the equal-`z`
draw-order tie-break** (the overlay stack's `OverlayStack::draw_order` sorts with a **stable**
`sort_by_key(|l| l.z)`, so equal-z layers blend in insertion order — see
`crates/multiview-overlay/src/layer.rs` and its `stack_z_sort_is_stable_for_equal_z` test; source
declaration order is the software build's `enumerate()`-indexed test-pattern palette index). A
config-file-watch reorder of equal-z overlays was therefore a **silent lost update**.

`ConfigDiff` gains two boolean signals, `sources_reordered` / `overlays_reordered` (accounted for
in `is_empty()`), computed by a pure `common_ids_reordered` helper that compares the
**present-in-both** id subsequence of each document in its own declaration order: adds/removes are
excluded (so an add or a remove *alone* is never a reorder), a genuine permutation of the survivors
always differs, and it is **orthogonal to `Changed`** (a reorder that also edits a survivor's
document reports both). The two legs are classified **differently**, by what each actually affects at
runtime — detection, application, and classification are kept consistent (no dead detection, no false
Class-1):

* **Overlay reorder = Class-1 (live, hot/seamless).** Declaration order is the equal-`z` draw-order
  tie-break, which the engine renders live (below). The order-sensitive `overlays` `changed_sections`
  entry (`running_overlays != next_overlays`, by `Vec` position) is deliberately **kept**, so the
  existing `apply_overlay_changes` still reseeds the overlay store + submits the live re-blend.
* **Source reorder = Class-2 (restart-pending).** Source declaration order has **no live engine
  seam**: it sets only the cold-start test-pattern palette index (`run.rs` `enumerate()` over the boot
  file's sources), while cells bind by `input_id` and the live source stores are id-keyed (`list()`
  is id-sorted), so the order is not even representable in the store/`active.toml`. The watcher's
  `apply_document_diff` reports `sources` **restart-pending** on `diff.sources_reordered` and submits
  **no** command — there is deliberately **no** live `ReorderSources` (it would be theater); it takes
  effect on the next restart, which re-reads the boot file's order. Both reorder flags are surfaced in
  the boot-model `diverged_sections` (so a pure reorder is a named divergence, never a silent no-op).

**The live engine re-blend (the Class-1 promise made true).** Detecting the reorder is necessary but
not sufficient: `Command::UpsertOverlay` updates the engine's working-config mirror **in place by
id** (`crates/multiview-cli/src/control.rs`), so re-submitting upserts for unchanged-content overlays
cannot move their positions — the equal-`z` tie-break is the mirror's insertion order, and the bake
consumer's stable `sort_by_key(|l| l.z)` preserves it. A dedicated **`Command::ReorderOverlays {
order: Vec<String> }`** carries the full desired id sequence; the engine drain's `reorder_overlays`
re-sequences its working mirror to that order (a pure permutation — nothing added, removed, or
edited) and republishes the lock-free `OverlaySet` with a bumped generation, so the bake consumer
re-derives the new draw order at the next frame — a clean frame-boundary (Class-1) transition, no
restart. An order already matching is a no-op (no generation bump), so a shed retry re-submitting the
same order cannot thrash the consumer. Both triggers drive it: the file-watch `apply_overlay_changes`
submits `ReorderOverlays` when `diff.overlays_reordered` (the `order` is the file's id sequence,
baseline-derived and stable across shed retries, submitted after any add/remove so the set is
complete before it is re-sequenced); the command path carries it as a first-class `Command` for any
submitter (e.g. a future bulk-reorder REST endpoint). On an overlay-capable run a landed reorder
leaves nothing restart-pending (genuinely Class-1); a no-capability run is honestly restart-only (no
live overlay seam at all), and a restart adopts the new order from the reseeded store. The reorder
changes neither which overlays exist nor their content, so it needs no adopted-snapshot change (the
ADR-W024 snapshot tracks content, adopted per-id by the upsert/remove path).
