# ADR-I002: GPU compositor — wgpu behind an off-by-default `wgpu` feature; WGSL shaders naga-validated GPU-free + SSIM/PSNR-gated at runtime

- **Status:** Accepted
- **Area:** Implementation Build-out
- **Date:** 2026-06-03
- **Source brief:** [color-management.md](../research/color-management.md), [core-engine.md](../research/core-engine.md), [efficiency.md](../research/efficiency.md)
- **Realizes:** Invariant #5 (NV12-throughout), Invariant #8 (fixed color order); see [ADR-0005](ADR-0005.md), [ADR-C003](ADR-C003.md), [ADR-C004](ADR-C004.md), [ADR-E002](ADR-E002.md), [ADR-R009](ADR-R009.md)

## Decision

The GPU compositor (`multiview-compositor`) ships a wgpu-backed implementation, but during the build-out it is kept behind an **OFF-BY-DEFAULT `wgpu` feature**. The default `cargo build`/`cargo check` therefore stays pure-Rust, `cargo deny`-clean, and fast, with the wgpu graphics stack (and its large transitive dependency graph) compiled only when the feature is requested. The compositor's pixel math lives in WGSL shaders that implement the canonical fixed color order (Invariant #8) over NV12 surfaces (Invariant #5): range-expand → YUV→RGB matrix → linearize → primaries-convert in linear → scale + premultiplied-alpha blend in linear → OETF → RGB→YUV → range-compress. Two verification tiers gate the shaders: (1) **GPU-free** — every WGSL shader is parsed and validated with `naga` without any GPU, but only under the off-by-default `wgpu` feature (e.g. `cargo test -p multiview-compositor --features wgpu`); this validation is genuinely GPU-free yet is **NOT** exercised by the default CI job, which builds default features only (`cargo test --workspace`), and would become a default-CI gate only if/when the `wgpu` feature (or that test) is wired into CI; (2) **runtime** — GPU output on hardware runners is compared against the CPU reference compositor and must meet **SSIM ≥ 0.98 and PSNR ≥ 40 dB**, never bit-exact (per ADR-R009's GPU testing tier).

## Rationale

Keeping `wgpu` opt-in during the build-out preserves the single most valuable property of the default build: it compiles with no native/GPU deps, passes `cargo deny`, and is fast enough for the inner-loop CI that every other crate depends on (the pure-Rust trait/type layer is the CI enabler). The compositor is nonetheless real, not a stub: the WGSL shaders encode the exact, non-reorderable color pipeline from the color-management brief, and operate NV12-throughout so no per-tile RGBA is ever materialized. Validating shaders with `naga` gives high-signal feedback (parse + type + binding errors) with zero GPU on commodity hardware — though under the off-by-default `wgpu` feature, so it is not yet part of the default CI job (see the GPU-free tier above) — while SSIM/PSNR thresholds are the correct acceptance bar for floating-point GPU math that will never be bit-identical to a scalar CPU reference — exactly the testing tier ADR-R009 mandates.

## Open follow-up

Conventions §3 lists `wgpu` as the **default** compositor backend (`wgpu` "(default)" / "wgpu baseline"). The off-by-default placement here is therefore a deliberate, temporary divergence for the build-out, with a tracked follow-up to flip `wgpu` on by default once the trade-off is paid down. The trade-off to weigh at flip time: turning `wgpu` on by default adds meaningful CI build time and pulls the wgpu graphics stack into the default `cargo deny` advisory/license graph (which must stay clean). Until then, conventions remains the source of truth for the *intended* default and this ADR records the *current* state plus the reason.

## Alternatives considered

- **`wgpu` on by default now** — matches conventions §3 but slows the inner-loop CI and enlarges the default deny graph before the surrounding scaffold is settled; deferred to the tracked follow-up rather than rejected.
- **FFmpeg/GStreamer compositing filters** — already rejected in ADR-0005 (no per-cell fit/crop, vendor-uneven, ties to a foreign memory model); used only as a reference.
- **Bit-exact GPU↔CPU comparison** — rejected: GPU floating-point and rounding differ from a scalar reference; SSIM/PSNR thresholds are the correct gate (ADR-R009).
- **Materializing per-tile RGBA** — rejected: violates Invariant #5; YUV→RGB happens in-shader at tile size.

## Consequences

- Default CI does not exercise GPU code paths, and it does not run the `naga` shader validation either (that test is gated by the off-by-default `wgpu` feature). On the no-GPU path, shader correctness rests on `naga` validation **when built with `--features wgpu`** plus the CPU reference compositor; full GPU verification runs only on GPU-tagged runners.
- The `wgpu` feature must keep the deny graph clean on its own so that flipping it to default later is a configuration change, not a license/advisory cleanup.
- The CPU reference compositor is load-bearing: it is both the golden-frame oracle and the SSIM/PSNR baseline, so it must track the same fixed color order as the WGSL shaders.
- When `wgpu` becomes default (per the follow-up), this ADR is superseded by reconvergence with conventions §3; until then conventions documents intent and this ADR documents the as-built state.
