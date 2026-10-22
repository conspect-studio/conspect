# Runbook — devcontainer cargo target dir (per-worktree isolation)

**What & why.** The dev container does **not** set a global `CARGO_TARGET_DIR`.
Cargo therefore uses each checkout's own local `target/`: the root checkout
(`/workspaces/multiview`) builds into `/workspaces/multiview/target`, and **every
git-worktree lane** under `.claude/worktrees/**` builds into **its own**
`<lane>/target`. This is the isolation [`AGENTS.md`](../../AGENTS.md) rules 10 and
11 require, and it is what stops the host load-storms below.

## Symptom (what this prevents)

When several worktree lanes ran `cargo build`/`test`/`clippy` at the same time,
the host melted into a **disk-I/O storm**, not a CPU one:

- 1-minute loadavg spiked to **300–675** while only ~48 tasks were runnable.
- ~600+ processes sat in uninterruptible-sleep (**D-state**, I/O-wait) — one
  observation was **loadavg 675 with ~627 D-state procs**.
- Builds crawled or appeared to hang; unrelated sessions on the same host stalled.

## Root cause

`.devcontainer/devcontainer.json` previously set, in `containerEnv`:

```jsonc
"CARGO_TARGET_DIR": "${containerWorkspaceFolder}/target"
```

`containerEnv` is exported to **every** process in the container (and written into
`/etc/environment`, so every login shell inherits it). A single global value means
cargo ignores each worktree's local `target/` and funnels **all** lanes' builds
into the **one** root dir `/workspaces/multiview/target`. Concurrent builds then:

1. **contend on cargo's target `flock`** and hammer one directory's inodes →
   the D-state I/O storm above; and
2. **link siblings' stale artifacts**, which can **fake a green** build/test run —
   the exact hazard [`AGENTS.md`](../../AGENTS.md) rule 11 warns about.

This directly contradicted rule 10 ("Do **not** override `CARGO_TARGET_DIR` — use
each worktree's local `target/`; it is isolated and deleted with the worktree").

## The fix

**Removed the one `CARGO_TARGET_DIR` line** from `containerEnv` in
[`.devcontainer/devcontainer.json`](../../.devcontainer/devcontainer.json). Nothing
in the repo reads the variable (no `.cargo/config.toml`, no script, CI job, or
xtask), so removing it simply restores cargo's default per-directory behaviour:

- Root checkout → `/workspaces/multiview/target` (**unchanged**).
- Each worktree lane → `<lane>/target` (**isolated**, deleted with the worktree).

The `multiview-target` **named volume** (still mounted at
`${containerWorkspaceFolder}/target` in the same file) is untouched: it keeps the
**root** checkout's `target/` on a fast Docker volume. Worktree `target/` dirs live
on the workspace bind mount, one per lane — which is the isolation we want. A guard
comment in `devcontainer.json` records why the env var must stay unset, so it is
not "helpfully" re-added.

## IMPORTANT — the fix needs a container REBUILD to take effect

`containerEnv` is applied at container **create** time. Editing
`devcontainer.json` does **not** change an already-running container: the old value
persists in the live process environment, in `/etc/environment`, and in every shell
already started. To make the fix live, **Dev Containers: Rebuild Container** (or
`devcontainer up --remove-existing-container`).

### Interim workaround (running container, before a rebuild)

Until the container is rebuilt, **unset** the inherited override so cargo falls back
to its default per-directory `target/` under the worktree — rule-10 compliant (no
override at all), and more robust than re-pointing the variable (it can't
accidentally target the wrong path when run from a subdirectory):

```bash
# run from inside the worktree lane
env -u CARGO_TARGET_DIR cargo build   # (…test / clippy / check)
```

In an agent bash context whose working directory resets between calls, `cd` into the
lane first so cargo's default resolution lands the `target/` under the lane, not the
root checkout:

```bash
cd /workspaces/multiview/.claude/worktrees/<lane> \
  && env -u CARGO_TARGET_DIR cargo build
```

Do **not** `cargo clean` the shared root `target/` to "isolate" a build — that nukes
the cache other co-tenant lanes are using and forces mass rebuilds.

## Verify

**In a running container that still has the old value:**

```bash
printenv CARGO_TARGET_DIR                 # -> /workspaces/multiview/target (pre-rebuild)
grep CARGO_TARGET_DIR /etc/environment    # -> present (pre-rebuild)
```

**After rebuilding the container, the fix is live when both are empty:**

```bash
printenv CARGO_TARGET_DIR                 # -> (no output; unset)
grep CARGO_TARGET_DIR /etc/environment    # -> (no match)
```

Then a build from a worktree lane writes to `<lane>/target`, not the root, and
concurrent lane builds no longer contend on one directory.

## See also

- [`AGENTS.md`](../../AGENTS.md) rules 10 (no `CARGO_TARGET_DIR` override) and 11
  (shared build caches fake a green run).
- [`.devcontainer/devcontainer.json`](../../.devcontainer/devcontainer.json) —
  `containerEnv` (env vars) and the `multiview-target` named volume mount.
- [`docs/operations/devcontainer.md`](../operations/devcontainer.md) — full
  dev-container guide (GPU/VAAPI, volumes, scripts).
