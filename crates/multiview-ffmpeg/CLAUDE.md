# multiview-ffmpeg — agent notes (ALL RAW libav* FFI LIVES HERE)

Safe RAII wrappers over libav* — demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe transfer/map. Off-by-default `ffmpeg` feature; overrides `unsafe_code` to `deny` (kept in sync with workspace lints by hand — nothing checks it).
Every `unsafe` block carries a `// SAFETY:` comment.
Context wrappers are `Send + !Sync` (or Mutex-guarded); expose a frame only after the decoder yields ownership.
`extern "C"` callbacks run on threads Rust does not own — keep them allocation-light, `catch_unwind` at the boundary, never let a panic unwind into C. A pooled buffer's `Drop` may only `try_send`, never block/spawn/join.
Licensing: default build stays LGPL-clean — `scale_cuda`, never `scale_npp`; `gpl-codecs` is opt-in only.
Read first: [data-plane-safety §4](../../docs/architecture/data-plane-safety.md).
