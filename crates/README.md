# `crates/` — Rust workspace

> [!NOTE]
> **Early scaffold — implementation in progress.** This is the real Multiview engine workspace, at an
> early stage. The crate boundaries are established (per the
> [canonical crate map](../docs/architecture/conventions.md#3-canonical-crate-map)) and the
> workspace compiles (`cargo check`/`clippy`/`fmt` green), but most crate bodies are trait/type
> **stubs** with `todo!()`/`NotImplemented` placeholders. Code is being built out against the
> documented contracts in [`../docs/`](../docs/); until a piece is implemented, the docs are the spec.

## What exists today

- The **workspace shape** and crate boundaries match `docs/architecture/conventions.md` §3.
- `multiview-core` defines the foundational **types/traits** (frame, color, time, layout, errors).
- All hardware/FFI/GPU/web integration is **feature-gated and not yet implemented**; the default
  build is pure-Rust so `cargo check` is green with no native dependencies.

## The contracts these crates are built against

| For… | See |
|------|-----|
| The system architecture | [`../docs/architecture/`](../docs/architecture/) |
| Why things are the way they are | [`../docs/decisions/`](../docs/decisions/) (ADRs) |
| Deep, cited design research | [`../docs/research/`](../docs/research/) |
| The API & web UI design | [`../docs/api/`](../docs/api/), [`../docs/web/`](../docs/web/) |
| The implementation plan | [`../docs/roadmap.md`](../docs/roadmap.md) |
