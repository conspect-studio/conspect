# Conspect patch record

Vendored base: the published h3 0.0.8 crates.io archive, SHA256
`10872b55cfb02a821b69dc7cf8dc6a71d6af25eb9a79662bec4a9d016056b3be`
(upstream commit `22c1aa3f44d1463cd7644c8f654fffc9a6da305c`, path `h3`).
`LICENSE` is copied unchanged. The workspace `[patch.crates-io]` (and the
`crates/multiview-h3/fuzz` workspace's) selects this path dependency.

Three edits, each marked `conspect patch (issue #403)` in the source, fix a
peer-reachable panic (a truncated DATA frame, then `poll_next` asserted):

1. `src/frame.rs` `FrameStream::poll_data`: end of stream with DATA payload
   still owed returns `FrameStreamError::UnexpectedEnd` (H3_FRAME_ERROR).
   A WebTransport bidi stream keeps its `usize::MAX` (unbounded) sentinel
   instead of decrementing it, so payload then FIN still ends cleanly.
2. `src/frame.rs` `FrameStream::poll_next`: the `remaining_data` `assert!`
   becomes `FrameStreamError::Proto(Malformed)`.
3. `src/buf.rs` `BufList::push_bytes`: a `debug_assert!` becomes an early return.

`README.md`: relative links into the upstream repository (absent here) point
to the same files at the upstream commit, for the repo's link check.

Upstream issue: https://github.com/hyperium/h3/issues/386. Drop this copy and
the `[patch.crates-io]` entry once a released h3 carries the fix. Full audit:
`crates/multiview-h3/PROVENANCE.md`.
