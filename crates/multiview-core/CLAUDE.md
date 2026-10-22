# multiview-core — agent notes

Foundation crate: shared types/traits, no FFI and no native deps — the default GPU-free `cargo check` baseline. `Frame`, `PixelFormat` (NV12 canonical, inv #5), `ColorInfo` (inv #8), clock/`MediaTime` (i64 ns/exact rationals, inv #3 — never float fps), layout/template model, error taxonomy, stage traits.
Dependency rule: `core` ← everything; core depends on nothing in the workspace — do not add a dependency on another `multiview-*` crate here.
Before changing types/traits here, read core-engine §3–§5 — a signature change ripples to every crate.
Keep `#![warn(missing_docs)]` clean; `thiserror` for the `Error` enum; serde unions tagged, never `untagged`.
