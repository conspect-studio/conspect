# multiview-rist-sys — agent notes

Direct librist (BSD-2-Clause) FFI leaf for RIST link stats; the sole `unsafe` boundary on this path, so `multiview-output` stays `forbid(unsafe_code)`.
Runtime `dlopen` only (`librist.so.4`/`librist.so`) — never linked at build time or vendored; the default build compiles/links with no librist present.
`unsafe_code = "deny"` here (not `forbid`); every `unsafe` block carries `// SAFETY:`. C-ABI structs in `raw.rs` are hand-mirrored from librist 0.2.x — keep byte-exact.
Stats callback runs on librist's own thread: only `try_send`s on a bounded drop-oldest channel — never blocks, allocates, or unwinds across the FFI boundary.
Only the **sender** session is built; the receiver is a separate future crate. Never fabricate a stat.
Read first: [ADR-0095](../../docs/decisions/ADR-0095.md).
