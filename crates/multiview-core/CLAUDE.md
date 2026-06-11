# multiview-core — agent notes

Foundation crate: shared types/traits with **no FFI and no native deps** — the default
GPU-free `cargo check` baseline. `Frame`, `PixelFormat` (**NV12 is canonical**, inv #5),
`ColorInfo` (the 4 color axes, inv #8), clock/`MediaTime` (i64 ns / exact rationals — inv #3,
**never float fps**), layout/template model, error taxonomy, and the stage traits
(`Source`, `Sink`, `Decoder`, `Encoder`, `Compositor`, `Backend`).

**Dependency rule:** `core` ← everything; **core depends on nothing in the workspace.** Do not
add a dependency on another `multiview-*` crate here — that creates a cycle.

**Before changing types/traits here**, read core-engine §3–§5 — a signature change ripples to
every crate. Keep `#![warn(missing_docs)]` clean; use `thiserror` for the `Error` enum; serde
unions are tagged (`#[serde(tag = "kind")]`), **never `untagged`**.

Depth: [core-engine](../../docs/research/core-engine.md) ·
[conventions §5 invariants, §9 conventions](../../docs/architecture/conventions.md). Map:
[codebase-map](../../docs/development/codebase-map.md).
