# multiview-compositor — agent notes

Custom GPU compositor: scale + place + per-tile color convert + linear-light blend + overlay. wgpu baseline; `cuda`/`metal`/`vaapi` opt-in.
Inv #8 — color pipeline order is fixed, never reorder: detect axes → range-expand → YUV→RGB → linearize (EOTF) → primaries convert in linear → scale + premultiplied-alpha blend in linear → OETF → RGB→YUV + range compress → tag output → verify with ffprobe. Tagging ≠ converting.
Inv #5 — NV12-throughout: never materialize RGBA per tile; YUV→RGB happens in-shader at tile size.
Inv #6: sources are decoded near display size; composite at tile resolution.
GPU output validated by SSIM/PSNR, never bit-exact; golden-frame tests are CPU-only. Keep the wgpu path GPU-free in CI.
Read first: [color-management](../../docs/research/color-management.md).
