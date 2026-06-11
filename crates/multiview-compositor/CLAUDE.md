# multiview-compositor — agent notes

The custom GPU compositor: scale + place + **per-tile color convert** + **linear-light blend** +
overlay compositing. wgpu is the baseline; vendor fast paths (`cuda`/`metal`/`vaapi`) are opt-in.

- **Inv #8 — color pipeline order is fixed, NEVER reorder:** detect 4 axes → range-expand →
  YUV→RGB matrix → linearize (EOTF) → primaries convert **in linear** → scale + premultiplied-
  alpha blend **in linear** → OETF → RGB→YUV + range compress → **tag output** → verify with
  ffprobe. Range is handled in-shader exactly once. **Tagging ≠ converting.**
- **Inv #5 — NV12-throughout:** frames stay NV12 (1.5 B/px); **never materialize RGBA per tile**.
  YUV→RGB happens in-shader at tile size, not full-frame.
- **Inv #6:** assume sources are decoded near display size; composite at tile resolution.

GPU output is validated by SSIM/PSNR thresholds, **never bit-exact**; golden-frame tests are
CPU-only. Keep the wgpu path working GPU-free in CI.

Read first: [color-management](../../docs/research/color-management.md) ·
[core-engine §8.2,§13](../../docs/research/core-engine.md) · ADR-C001..C006, ADR-E002.
Map: [codebase-map](../../docs/development/codebase-map.md).
