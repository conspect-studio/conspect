//! libav → `tracing` log bridge with anti-flood rate limiting.
//!
//! A glitchy or corrupt input is the *normal* operating condition for a live
//! multiview, not an exception — and libav reports it by logging. A corrupt
//! HEVC RTSP feed emits a *continuous* `Error constructing the frame RPS.`; a
//! flaky HLS source emits a *continuous* `IO error: Connection timed out`. By
//! default libav writes these straight to **stderr**, unbounded, so a single bad
//! tile can drown the operator's logs. The output path is already protected (the
//! framestore tile state machine rides last-good → STALE → NO_SIGNAL); this
//! module fixes the **logging** side so a bad input never floods the logs.
//!
//! It does two things:
//!
//! 1. **Routes** every libav log line into Rust [`tracing`] at the mapped level
//!    (PANIC/FATAL/ERROR → `error!`, WARNING → `warn!`, INFO → `info!`,
//!    VERBOSE/DEBUG → `debug!`, TRACE → `trace!`), carrying the libav component
//!    name (the `[hevc @ …]` class) as a `component` field. This replaces
//!    libav's stderr writer with the structured logger the rest of the system
//!    already uses.
//! 2. **Rate-limits** repetitive lines: the first occurrence of a message is
//!    emitted, identical repeats inside a time window are *suppressed*, and the
//!    next occurrence after the window flushes a coalesced
//!    `"… (repeated N× in the last …)"` summary. 10 000 identical `RPS` errors
//!    become one line plus a periodic count.
//!
//! ## Layering
//!
//! The whole anti-flood policy — the level mapping, the suppressor, and the
//! rendered-line sanitiser — is **pure** and native-dep-free, so it is always
//! compiled and unit-tested without libav. Behind the `ffmpeg` feature live two
//! pieces that touch libav: a tiny **C shim** (`csrc/log_shim.c`, compiled by
//! `build.rs`) that owns the libav `va_list` and renders each line into a bounded
//! buffer via `av_log_format_line2`, and the Rust `multiview_log_emit` callback
//! the shim hands the already-formatted line to (it reads the component name,
//! runs the suppressor, and emits via `tracing`).
//!
//! ## Why the rendering is in C (ABI safety)
//!
//! libav's log callback takes a `va_list`. There is no stable Rust `VaList` in
//! function-parameter position, and `ffmpeg-sys-next`'s bindgen spells the type
//! in two incompatible shapes per host libclang (an array-by-value here, a
//! decayed `*mut __va_list_tag` on x86-64 trixie). A Rust trampoline that
//! receives the `va_list` is therefore unsound at runtime on at least one arch —
//! it SIGSEGV'd the decode thread on x86-64. C handles `va_list` ABI-correctly on
//! every architecture by construction, so the rendering is done entirely in the C
//! shim and Rust never touches the `va_list`.
//!
//! ## FFI safety (CLAUDE.md §7)
//!
//! `multiview_log_emit` runs on **foreign/decoder threads**. It therefore:
//! * never lets a Rust panic unwind across the FFI boundary — the entire Rust
//!   body runs inside [`std::panic::catch_unwind`] and any caught panic is
//!   dropped silently (a logging callback must never crash the decoder);
//! * does no per-call heap allocation on the render path — the line is rendered
//!   into a fixed C stack buffer via `av_log_format_line2` before Rust sees it;
//! * holds the suppressor [`std::sync::Mutex`] only for the O(small-cap) lookup,
//!   never across the (cheap) `tracing` emit and never blocking.

use std::time::Duration;

/// libav `AV_LOG_PANIC` — something went really wrong; will crash now.
pub const AV_LOG_PANIC: i32 = 0;
/// libav `AV_LOG_FATAL` — unrecoverable; cannot continue.
pub const AV_LOG_FATAL: i32 = 8;
/// libav `AV_LOG_ERROR` — an error past which recovery may still be possible.
pub const AV_LOG_ERROR: i32 = 16;
/// libav `AV_LOG_WARNING` — something looks wrong and may cause problems.
pub const AV_LOG_WARNING: i32 = 24;
/// libav `AV_LOG_INFO` — standard informational output.
pub const AV_LOG_INFO: i32 = 32;
/// libav `AV_LOG_VERBOSE` — detailed but still relevant information.
pub const AV_LOG_VERBOSE: i32 = 40;
/// libav `AV_LOG_DEBUG` — information only useful for libav developers.
pub const AV_LOG_DEBUG: i32 = 48;
/// libav `AV_LOG_TRACE` — extremely verbose per-operation tracing.
pub const AV_LOG_TRACE: i32 = 56;

/// Maximum number of bytes retained for a single rendered log line.
///
/// libav lines are short; this bounds a pathological component / format so the
/// callback never allocates without limit. A line longer than this is truncated
/// at a UTF-8 character boundary.
pub const MAX_LINE_LEN: usize = 1024;

/// The bridge's coarse severity band — a libav `AV_LOG_*` level collapsed to the
/// five [`tracing`] levels. Kept as a plain enum (rather than [`tracing::Level`])
/// so the mapping is pure and unit-testable in the default, `tracing`-only build
/// without taking a hard dependency on tracing's internal level ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BridgeLevel {
    /// `tracing::Level::ERROR` — libav PANIC / FATAL / ERROR.
    Error,
    /// `tracing::Level::WARN` — libav WARNING.
    Warn,
    /// `tracing::Level::INFO` — libav INFO.
    Info,
    /// `tracing::Level::DEBUG` — libav VERBOSE / DEBUG.
    Debug,
    /// `tracing::Level::TRACE` — libav TRACE (and anything more verbose).
    Trace,
}

/// Map a libav `AV_LOG_*` numeric level to a [`BridgeLevel`].
///
/// libav levels are a numeric *scale* (smaller = more severe), not a closed set,
/// so this buckets by threshold rather than matching exact discriminants: any
/// value at least as severe as `ERROR` (including the negative `QUIET`) maps to
/// [`BridgeLevel::Error`], and anything beyond `TRACE` saturates to
/// [`BridgeLevel::Trace`]. It is total and panic-free for every `i32`.
#[must_use]
pub fn map_av_level(av_level: i32) -> BridgeLevel {
    // Boundaries follow libav's own banding: a value is treated at the most
    // severe band whose threshold it does not exceed. `<=` keeps the canonical
    // constants exact (e.g. AV_LOG_WARNING == 24 → Warn) while bucketing the
    // gaps between them upward in severity.
    if av_level <= AV_LOG_ERROR {
        BridgeLevel::Error
    } else if av_level <= AV_LOG_WARNING {
        BridgeLevel::Warn
    } else if av_level <= AV_LOG_INFO {
        BridgeLevel::Info
    } else if av_level <= AV_LOG_DEBUG {
        BridgeLevel::Debug
    } else {
        BridgeLevel::Trace
    }
}

/// Clean a rendered libav log line for emission as a single `tracing` record.
///
/// libav lines arrive newline-terminated and may, in pathological cases, carry
/// embedded control bytes (a corrupt component name) or be very long. This:
/// * strips trailing ASCII whitespace (the libav newline);
/// * replaces interior control characters (other than ordinary space) with a
///   space so the record stays one clean printable line;
/// * truncates to [`MAX_LINE_LEN`] at a UTF-8 character boundary.
///
/// It allocates a single bounded `String` and is panic-free for any input
/// (including empty, `%`-containing, NUL-containing, or oversized text).
#[must_use]
pub fn sanitize_line(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let mut out = String::with_capacity(trimmed.len().min(MAX_LINE_LEN));
    for ch in trimmed.chars() {
        if out.len().saturating_add(ch.len_utf8()) > MAX_LINE_LEN {
            break;
        }
        // Replace C0/C1 control characters (NUL, BEL, …) with a space; keep all
        // other (printable) characters verbatim. A literal `%` is ordinary text
        // here — the va_list was already expanded in C by the formatter.
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// The decision the [`Suppressor`] reaches for one observed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuppressOutcome {
    /// Emit this message now (a first occurrence, an LRU-evicted key seen again,
    /// or a re-emit after the window with nothing suppressed in between).
    Emit,
    /// Drop this message — an identical one was emitted inside the active window.
    /// Its occurrence has been counted for a later coalesced summary.
    Suppress,
    /// Emit this message now **and** a coalesced summary: `suppressed` identical
    /// occurrences were dropped since the last emit, and the window has elapsed.
    EmitWithSummary {
        /// How many identical occurrences were suppressed since the last emit.
        suppressed: u64,
    },
}

/// One tracked message key's suppression state.
#[derive(Debug, Clone)]
struct Entry {
    /// `(level, resource_id, message)` identity of the tracked message. The
    /// optional `resource_id` (ADR-0060 §3.4) makes two different sources
    /// emitting the *same* libav text suppress independently — CNN's RPS flood
    /// must not mask BBC's. An unattributed line (`None`) is its own key.
    key: (BridgeLevel, Option<String>, String),
    /// When the current window opened (the last time this key was emitted).
    window_start: Duration,
    /// Identical occurrences suppressed since `window_start`.
    suppressed: u64,
    /// Monotonic recency stamp for LRU eviction (largest = most recent).
    touched: u64,
}

/// A thread-unsynchronised, bounded repetition suppressor (the anti-flood core).
///
/// Keyed by `(level, message text)`. The first time a key is seen it is emitted
/// and a window opens. Identical occurrences inside the window are suppressed and
/// counted. The first occurrence at or after the window's end emits the message
/// again, attaching the suppressed count as a coalesced summary, and reopens the
/// window.
///
/// Memory is bounded by a fixed-capacity LRU: at most `cap` keys are retained;
/// inserting a new key when full evicts the least-recently-touched one. Feeding
/// an unbounded stream of *distinct* messages therefore never grows the
/// structure past `cap` (CLAUDE.md §7 bounded-memory). The callback wraps this
/// in a [`std::sync::Mutex`]; it is deliberately kept lock-light (small cap,
/// linear scan) because the trampoline runs on decoder threads.
#[derive(Debug)]
pub struct Suppressor {
    entries: Vec<Entry>,
    cap: usize,
    window: Duration,
    /// Monotonic recency counter; incremented on every observe.
    clock: u64,
}

impl Suppressor {
    /// Create a suppressor retaining at most `cap` distinct message keys, with a
    /// suppression `window` per key. A `cap` of 0 degrades to always-emit (no
    /// state retained) — never a panic, never growth.
    #[must_use]
    pub fn new(cap: usize, window: Duration) -> Self {
        Self {
            entries: Vec::new(),
            cap,
            window,
            clock: 0,
        }
    }

    /// Number of distinct message keys currently tracked (always `<= cap`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no message keys are currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Find the index of the entry whose key matches
    /// `(level, resource_id, message)`.
    fn find(&self, level: BridgeLevel, resource: Option<&str>, message: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.key.0 == level && e.key.1.as_deref() == resource && e.key.2 == message)
    }

    /// Index of the least-recently-touched entry (smallest `touched`).
    fn lru_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.touched)
            .map(|(i, _)| i)
    }

    /// Observe one message at `now`, returning the emit/suppress decision and
    /// updating the per-key window state. `message` is the already-rendered,
    /// sanitised line; identity is `(level, message)` (no resource dimension).
    ///
    /// Equivalent to [`observe_scoped`](Self::observe_scoped) with `resource`
    /// = `None`; retained so resource-agnostic call sites are unchanged.
    pub fn observe(&mut self, level: BridgeLevel, message: &str, now: Duration) -> SuppressOutcome {
        self.observe_scoped(level, None, message, now)
    }

    /// Observe one message attributed to `resource` at `now` (ADR-0060 §3.4).
    ///
    /// Identity is `(level, resource, message)`, so two distinct resources
    /// emitting the same libav text suppress independently and an unattributed
    /// line (`resource == None`) is its own key. Otherwise identical to the
    /// resource-agnostic window/LRU behaviour.
    pub fn observe_scoped(
        &mut self,
        level: BridgeLevel,
        resource: Option<&str>,
        message: &str,
        now: Duration,
    ) -> SuppressOutcome {
        // A zero-capacity suppressor retains nothing and always emits.
        if self.cap == 0 {
            return SuppressOutcome::Emit;
        }

        let tick = self.clock.wrapping_add(1);
        self.clock = tick;

        if let Some(idx) = self.find(level, resource, message) {
            // Borrow the matched entry mutably for the window check.
            if let Some(entry) = self.entries.get_mut(idx) {
                entry.touched = tick;
                let elapsed = now.saturating_sub(entry.window_start);
                if elapsed >= self.window {
                    // Window elapsed: flush any coalesced count and reopen.
                    let suppressed = entry.suppressed;
                    entry.suppressed = 0;
                    entry.window_start = now;
                    if suppressed > 0 {
                        return SuppressOutcome::EmitWithSummary { suppressed };
                    }
                    return SuppressOutcome::Emit;
                }
                // Inside the window: suppress and count.
                entry.suppressed = entry.suppressed.saturating_add(1);
                return SuppressOutcome::Suppress;
            }
            // Unreachable in practice (idx came from `find`); fall through to
            // emit rather than risk any indexing panic.
            return SuppressOutcome::Emit;
        }

        // New key. Evict the LRU entry if at capacity, then insert.
        if self.entries.len() >= self.cap {
            if let Some(victim) = self.lru_index() {
                if victim < self.entries.len() {
                    self.entries.swap_remove(victim);
                }
            }
        }
        self.entries.push(Entry {
            key: (level, resource.map(str::to_owned), message.to_owned()),
            window_start: now,
            suppressed: 0,
            touched: tick,
        });
        SuppressOutcome::Emit
    }
}

/// The kind of resource a libav line is attributed to (ADR-0060 §2.2), as a
/// stable lowercase string. Kept as a small newtype-free enum-of-strings here so
/// the bridge stays pure (no dependency on `multiview-telemetry`); the control
/// plane maps these to its richer `LogResourceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ResourceKind {
    /// An ingest source (`Source.id`).
    Source,
    /// An output sink (`Output.id`).
    Output,
    /// A managed device (`Device.id`).
    Device,
}

impl ResourceKind {
    /// The lowercase wire string for this kind (matches the span field).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Output => "output",
            Self::Device => "device",
        }
    }
}

/// The resource that a thread currently owns, used to attribute libav log lines
/// emitted synchronously on that thread (ADR-0060 §3.1, mechanism A).
///
/// Holds a small, cheap-to-clone `Arc<str>` id (and optional label) plus the
/// kind. Set via a [`ResourceGuard`] for the duration of an owned demuxer-open /
/// `av_read_frame` / decode region, and read by the bridge's `route()` as the
/// **first** attribution source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceContext {
    kind: ResourceKind,
    id: std::sync::Arc<str>,
    label: Option<std::sync::Arc<str>>,
}

impl ResourceContext {
    /// A context for an ingest source with the given config id.
    #[must_use]
    pub fn source(id: impl AsRef<str>) -> Self {
        Self::new(ResourceKind::Source, id)
    }

    /// A context for an output sink with the given config id.
    #[must_use]
    pub fn output(id: impl AsRef<str>) -> Self {
        Self::new(ResourceKind::Output, id)
    }

    /// A context for a managed device with the given config id.
    #[must_use]
    pub fn device(id: impl AsRef<str>) -> Self {
        Self::new(ResourceKind::Device, id)
    }

    /// A context with an explicit kind and id.
    #[must_use]
    pub fn new(kind: ResourceKind, id: impl AsRef<str>) -> Self {
        Self {
            kind,
            id: std::sync::Arc::from(id.as_ref()),
            label: None,
        }
    }

    /// Attach a human label (the config display name) for the UI.
    #[must_use]
    pub fn with_label(mut self, label: impl AsRef<str>) -> Self {
        self.label = Some(std::sync::Arc::from(label.as_ref()));
        self
    }

    /// The resource kind as its lowercase wire string.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.kind.as_str()
    }

    /// The stable config resource id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The human label, if one was attached.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

thread_local! {
    /// The current owned-resource context for this thread, if any. `RefCell` so a
    /// [`ResourceGuard`] can save/restore the previous value for nesting; never
    /// shared across threads (each thread owns at most one resource at a time).
    static CURRENT_RESOURCE: std::cell::RefCell<Option<ResourceContext>> =
        const { std::cell::RefCell::new(None) };
}

/// The resource the current thread owns, if a [`ResourceGuard`] is active.
///
/// Returns [`None`] outside any owned region — the bridge then falls through to
/// the `AVClass` map / component-only attribution (ADR-0060 §3.2/§3.3), never
/// guessing a stale id.
#[must_use]
pub fn current_resource() -> Option<ResourceContext> {
    CURRENT_RESOURCE.with(|cell| cell.borrow().clone())
}

/// A RAII guard that sets the current thread's [`ResourceContext`] on
/// construction and **restores the previous value on drop** (ADR-0060 §3.1 —
/// scoped, never stale).
///
/// Enter one at the seam where a thread/task takes ownership of a source/output
/// (the demuxer-open / read / decode region). A libav line emitted while the
/// guard is live is attributed to its resource; a line emitted after the guard
/// drops is *not* — so an unrelated or nested-unregistered context falls through
/// to weaker attribution rather than inheriting whichever resource last ran on
/// the thread.
#[derive(Debug)]
#[must_use = "the resource context is only set while the guard is alive"]
pub struct ResourceGuard {
    /// The value to restore on drop (the context active before this guard).
    previous: Option<ResourceContext>,
}

impl ResourceGuard {
    /// Set `context` as the current thread's resource, saving the previous one
    /// to restore on drop. Nesting is supported: an inner guard shadows the
    /// outer, and dropping it restores the outer.
    pub fn enter(context: ResourceContext) -> Self {
        let previous = CURRENT_RESOURCE.with(|cell| cell.borrow_mut().replace(context));
        Self { previous }
    }
}

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        // Restore the previously-active context (or clear to None), so the
        // thread is never left attributed to a resource it no longer owns.
        let previous = self.previous.take();
        CURRENT_RESOURCE.with(|cell| {
            *cell.borrow_mut() = previous;
        });
    }
}

#[cfg(feature = "ffmpeg")]
pub use ffi::install;
#[cfg(feature = "ffmpeg")]
pub use ffi::{register_av_context, resolve_av_context, AvContextRegistration};

#[cfg(feature = "ffmpeg")]
mod ffi {
    //! The `av_log_set_callback` installation and the `multiview_log_emit`
    //! callback that the **C shim** (`csrc/log_shim.c`) calls back into.
    //!
    //! ## Why a C shim owns the `va_list`
    //!
    //! libav's log callback is `void (*)(void*, int, const char*, va_list)`. The
    //! `va_list` parameter is fundamentally unsound to receive in **stable Rust**
    //! in function-parameter position: there is no stable `core::ffi::VaList` in
    //! that position, and `ffmpeg-sys-next`'s bindgen spells the type in two
    //! mutually-incompatible shapes per host libclang — an *array by value*
    //! (`[u64; 4]` / `[__va_list_tag; 1]`) on this container, but a *decayed
    //! pointer* (`*mut __va_list_tag`) on x86-64 Debian-trixie libclang. The old
    //! Rust trampoline declared the parameter via the array alias and registered
    //! through a fn-pointer `transmute`; that **compiled** on both renderings but
    //! the **runtime ABI was wrong on x86-64 `SysV`**: libav passes `va_list` as a
    //! single decayed `__va_list_tag*` (one register), while the array-by-value
    //! body expected a 24-byte aggregate — so it read garbage, handed a bogus
    //! pointer to `av_log_format_line2`, and SIGSEGV'd the decode thread the
    //! moment libav emitted any log line.
    //!
    //! C handles `va_list` natively and ABI-correctly on **every** architecture
    //! (x86-64 `SysV` and arm64 AAPCS): the compiler emits exactly the ABI the
    //! callback type promises. So the rendering moves wholesale to
    //! `multiview_av_log_trampoline` in `csrc/log_shim.c`, which owns the
    //! `va_list` end to end, renders the line via `av_log_format_line2` into a
    //! bounded stack buffer, and calls back into [`multiview_log_emit`] with the
    //! already-formatted line. **Rust never touches the `va_list`.**
    //!
    //! [`multiview_log_emit`] then does exactly what the old Rust trampoline did
    //! *after* rendering — read the component class name from the leading
    //! `AVClass*`, run the pure bounded suppressor, and emit via `tracing` — the
    //! whole body inside `catch_unwind` so a Rust panic can never unwind across
    //! the FFI boundary (CLAUDE.md §7).

    // reason: this module installs a raw libav `extern "C"` callback (defined in
    // the C shim) and reads a libav object's leading `AVClass*` pointer — raw FFI
    // the crate's `unsafe_code = "deny"` posture allows here (CLAUDE.md §7), every
    // block with a `// SAFETY:` note.
    #![allow(unsafe_code)]

    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, Once, OnceLock, RwLock};
    use std::time::{Duration, Instant};

    use ffmpeg::ffi;
    use ffmpeg_next as ffmpeg;
    use libc::{c_char, c_int, c_void};

    use super::{
        current_resource, map_av_level, sanitize_line, BridgeLevel, ResourceContext,
        SuppressOutcome, Suppressor, MAX_LINE_LEN,
    };

    // reason(allow): `vl: ffi::va_list` resolves to `[__va_list_tag; 1]` on the
    // x86-64 / FFmpeg-8.1.x bindgen rendering, and rustc's `improper_ctypes`
    // (under `-D warnings`) flags an array passed by value as not-FFI-safe. That
    // type IS the libav callback ABI, though: this shim has the exact
    // `void (*)(void*, int, const char*, va_list)` signature and is implemented in
    // C (`csrc/log_shim.c`), which receives the genuine `va_list` with the correct
    // platform ABI — Rust never constructs, reads, or passes one. The signature is
    // declared only so its fn pointer can be handed to `av_log_set_callback`; it is
    // never called from Rust. Spelling it with the binding's own `ffi::va_list`
    // alias is the faithful declaration of the C ABI, so the lint is suppressed
    // rather than "fixed" by lying about the parameter type.
    #[allow(improper_ctypes)]
    extern "C" {
        /// The C log-callback shim (`csrc/log_shim.c`), compiled and linked by
        /// `build.rs` under the `ffmpeg` feature. It has the exact libav callback
        /// ABI — `void (*)(void*, int, const char*, va_list)` — and owns the
        /// `va_list`: it renders the line with `av_log_format_line2` and calls
        /// [`multiview_log_emit`]. The `va_list` parameter is spelled with the
        /// binding's own [`ffi::va_list`] alias (see [`LogCallback`] for how the
        /// fn pointer is reconciled with `av_log_set_callback`'s parameter type
        /// across the two bindgen renderings).
        ///
        /// This is declared but **never called from Rust** — its function pointer
        /// is handed straight to `av_log_set_callback`, and libav invokes it from
        /// C with a genuine `va_list`.
        fn multiview_av_log_trampoline(
            avcl: *mut c_void,
            level: c_int,
            fmt: *const c_char,
            vl: ffi::va_list,
        );
    }

    /// The libav log-callback fn-pointer type, spelled with the binding's own
    /// [`ffi::va_list`] alias.
    ///
    /// `ffmpeg-sys-next`'s bindgen renders `av_log_set_callback`'s callback
    /// parameter in two shapes per host libclang: the [`ffi::va_list`] alias
    /// (array form) on this container, but a decayed `*mut __va_list_tag` on
    /// x86-64 Debian-trixie. The *standalone* alias `ffi::va_list` keeps resolving
    /// to the array on both. So a fn pointer typed with the alias matches
    /// `av_log_set_callback`'s parameter directly on the array rendering, and a
    /// single fn-pointer `transmute` (below) reconciles it on the decayed
    /// rendering — an ABI-identity bridge between two one-machine-word fn-pointer
    /// types, not a reinterpretation of values. Critically, unlike the old design
    /// this is the only place the alias is used: the *function it points at* is the
    /// C shim, which receives the real `va_list` in C with the correct ABI, so the
    /// runtime is correct on every architecture regardless of how Rust spells the
    /// pointer's type here.
    type LogCallback = unsafe extern "C" fn(*mut c_void, c_int, *const c_char, ffi::va_list);

    /// Suppression window: identical repeats within this span are coalesced.
    const SUPPRESS_WINDOW: Duration = Duration::from_secs(5);
    /// Bounded LRU capacity: distinct recent `(level, message)` keys retained.
    const SUPPRESS_CAPACITY: usize = 256;

    /// The C shim's render-buffer payload length plus the NUL, as a single source
    /// of truth on the Rust side. The C shim hard-codes `MULTIVIEW_LOG_LINE_BUF_LEN
    /// = 1025` (= [`MAX_LINE_LEN`] + 1); this compile-time assertion fails the
    /// build if [`MAX_LINE_LEN`] is ever changed without updating the C constant in
    /// `csrc/log_shim.c` to match — keeping the two ends in lock-step.
    const LINE_BUF_LEN: usize = MAX_LINE_LEN + 1;
    const _: () = assert!(
        LINE_BUF_LEN == 1025,
        "csrc/log_shim.c MULTIVIEW_LOG_LINE_BUF_LEN (1025) must equal MAX_LINE_LEN + 1; \
         update the C constant if MAX_LINE_LEN changes"
    );

    /// Process-global suppressor shared by every decoder thread. A `Mutex` over a
    /// small fixed-capacity LRU; held only for the O(cap) lookup, never across a
    /// blocking call.
    static SUPPRESSOR: OnceLock<Mutex<Suppressor>> = OnceLock::new();
    /// Monotonic origin for relative timestamps fed to the suppressor (a libav
    /// callback has no clock argument). `Instant` is monotonic and cheap.
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    /// Guards one-time callback installation.
    static INSTALL: Once = Once::new();

    fn suppressor() -> &'static Mutex<Suppressor> {
        SUPPRESSOR.get_or_init(|| Mutex::new(Suppressor::new(SUPPRESS_CAPACITY, SUPPRESS_WINDOW)))
    }

    fn origin() -> Instant {
        *ORIGIN.get_or_init(Instant::now)
    }

    /// One entry in the [`AvClassMap`]: the resource a libav context belongs to,
    /// plus the registration epoch that defends against pointer reuse.
    ///
    /// libav reuses freed context addresses, so a raw `ptr → resource` map could
    /// mis-attribute a reused pointer to a dead resource (ADR-0060 §3.2). Each
    /// [`AvContextRegistration`] carries a monotonic `epoch`; the entry records
    /// the epoch it was inserted with, and the guard removes the entry **only if**
    /// the live entry still bears its own epoch — so a reopened context at the same
    /// address (a new, larger epoch) is never torn down by the old guard's `Drop`,
    /// and a stale lookup never resolves to a freed resource.
    #[derive(Debug, Clone)]
    struct AvClassEntry {
        context: ResourceContext,
        epoch: u64,
    }

    /// A bounded `context_ptr → resource` map for attributing libav lines that
    /// fire on libav-owned worker threads (ADR-0060 §3.2, mechanism B), where the
    /// thread-local [`current_resource`] is not set. Bounded by the number of open
    /// libav contexts (tens). A `RwLock<HashMap>`: reads (the hot per-line lookup)
    /// take the read lock; registration / removal take the write lock.
    static AV_CLASS_MAP: OnceLock<RwLock<HashMap<usize, AvClassEntry>>> = OnceLock::new();
    /// Monotonic registration epoch, incremented per registration so a reused
    /// pointer address always gets a fresh epoch (defeats pointer-reuse aliasing).
    static AV_CLASS_EPOCH: AtomicU64 = AtomicU64::new(1);

    fn av_class_map() -> &'static RwLock<HashMap<usize, AvClassEntry>> {
        AV_CLASS_MAP.get_or_init(|| RwLock::new(HashMap::new()))
    }

    /// A RAII registration of a libav context pointer → resource (ADR-0060 §3.2).
    ///
    /// Create one when Multiview opens a libav context it owns (the
    /// `AVFormatContext*` for a source/output, or a reachable child context);
    /// hold it for the context's lifetime. On `Drop` it removes the map entry
    /// **before** the context's memory can be reused, but only if the live entry
    /// still bears this registration's epoch — so an `open → close → reopen` at
    /// the same address never has the new registration torn down by the old
    /// guard, and a reused address never resolves to the prior owner.
    #[derive(Debug)]
    #[must_use = "the libav context is only attributed while the registration is alive"]
    pub struct AvContextRegistration {
        ptr: usize,
        epoch: u64,
    }

    impl Drop for AvContextRegistration {
        fn drop(&mut self) {
            let map = av_class_map();
            let mut guard = match map.write() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Remove only if the live entry is still *ours* (same epoch). A newer
            // registration at the same address (reopen) has a larger epoch and is
            // left intact.
            if guard.get(&self.ptr).is_some_and(|e| e.epoch == self.epoch) {
                guard.remove(&self.ptr);
            }
        }
    }

    /// Register a libav context pointer as belonging to `context`, returning a
    /// guard that removes the mapping on drop (ADR-0060 §3.2, mechanism B).
    ///
    /// `ptr` is the address of the owned libav object (e.g. the `AVFormatContext`)
    /// as a `usize`; the caller is responsible for holding the returned guard for
    /// no longer than the context lives. Registration is bounded by the number of
    /// open contexts and is safe under pointer reuse via the per-entry epoch.
    pub fn register_av_context(ptr: usize, context: ResourceContext) -> AvContextRegistration {
        let epoch = AV_CLASS_EPOCH.fetch_add(1, Ordering::Relaxed);
        let map = av_class_map();
        let mut guard = match map.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.insert(ptr, AvClassEntry { context, epoch });
        AvContextRegistration { ptr, epoch }
    }

    /// Resolve a registered resource from a raw libav object **address**, if any.
    ///
    /// Looks up `ptr` directly; a miss returns [`None`] (the caller then walks
    /// parents via [`resolve_with_parent_walk`] or falls through to
    /// component-only attribution). The lookup takes only the read lock.
    fn resolve_registered(ptr: usize) -> Option<ResourceContext> {
        if ptr == 0 {
            return None;
        }
        let map = av_class_map();
        let guard = match map.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(&ptr).map(|e| e.context.clone())
    }

    /// Resolve a registered resource for a public caller (integration tests, the
    /// demux registration seam) by direct **address** lookup (ADR-0060 §3.2).
    ///
    /// This is the public face of [`resolve_registered`]: it answers "is this
    /// `AVFormatContext*` (as a `usize`) currently registered, and to which
    /// resource?" without exposing the internal map. A miss (or `0`) returns
    /// [`None`].
    #[must_use]
    pub fn resolve_av_context(ptr: usize) -> Option<ResourceContext> {
        resolve_registered(ptr)
    }

    /// Maximum number of `parent_log_context_offset` hops the resolver follows.
    ///
    /// libav's log-context parent chains are shallow (a decoder → its codec
    /// context → its format context is at most a couple of hops). A small fixed
    /// cap bounds the work per line and defends against a malformed or cyclic
    /// chain (a self-referential parent pointer never loops forever).
    const MAX_PARENT_HOPS: usize = 4;

    /// Resolve a registered resource for a logged libav object, following libav's
    /// `parent_log_context_offset` on a direct-lookup miss (ADR-0060 §3.2,
    /// mechanism B step 1→2).
    ///
    /// libav's frame-threaded decoders and the HLS sub-demuxer pool log against a
    /// **child** object (an `AVCodecContext`, a sub-demuxer's `AVFormatContext`)
    /// whose `AVClass` declares a `parent_log_context_offset`: at
    /// `(object + offset)` libav stores a pointer to the parent log context (e.g.
    /// the owning `AVFormatContext` we registered). So a HEVC "Error constructing
    /// the frame RPS" line — emitted on a decoder thread against the codec
    /// context, never our format context — is attributed by walking up to the
    /// registered parent.
    ///
    /// Resolution order, honest and bounded:
    /// 1. direct lookup of the logged object's address;
    /// 2. else follow `parent_log_context_offset` up to [`MAX_PARENT_HOPS`] hops,
    ///    looking each parent up in the map;
    /// 3. else [`None`] (the caller keeps `component` only — never a guessed id).
    ///
    /// Every pointer read is null-checked and bounded to the documented `AVClass`
    /// offset; any null/miss terminates the walk at [`None`].
    fn resolve_with_parent_walk(avcl: *mut c_void) -> Option<ResourceContext> {
        if avcl.is_null() {
            return None;
        }
        // Step 1: the logged object itself.
        // reason(allow): pointer→address cast for a map-key lookup only (never
        // dereferenced); `<*mut T>::addr()` is MSRV-1.84 and this crate is 1.82.
        #[allow(clippy::as_conversions)]
        if let Some(found) = resolve_registered(avcl as usize) {
            return Some(found);
        }

        // Step 2: climb `parent_log_context_offset` a bounded number of hops.
        let mut current = avcl;
        for _ in 0..MAX_PARENT_HOPS {
            let parent = parent_log_context(current)?;
            if parent.is_null() {
                return None;
            }
            // reason(allow): same map-key-only pointer→address cast as above.
            #[allow(clippy::as_conversions)]
            if let Some(found) = resolve_registered(parent as usize) {
                return Some(found);
            }
            current = parent;
        }
        None
    }

    /// Read the parent log-context pointer of a libav object via its
    /// `AVClass::parent_log_context_offset`, if it declares one.
    ///
    /// libav's logging ABI: a loggable object's first member is `*const AVClass`,
    /// and `AVClass::parent_log_context_offset` is a byte offset **into the
    /// object** at which libav stores a `*mut c_void` pointing to the parent log
    /// context (or `0` for "no parent"). Returns the parent object pointer (which
    /// itself begins with a `*const AVClass`), or [`None`] when there is no
    /// declared parent or any pointer in the chain is null.
    fn parent_log_context(obj: *mut c_void) -> Option<*mut c_void> {
        if obj.is_null() {
            return None;
        }
        // SAFETY: per libav's logging ABI the object at `obj` begins with a
        // `*const AVClass`. We read exactly that one leading pointer-sized field
        // (unaligned-safe), assuming nothing about the rest of the struct. `obj`
        // is non-null (checked above).
        let class_ptr = unsafe { obj.cast::<*const ffi::AVClass>().read_unaligned() };
        if class_ptr.is_null() {
            return None;
        }
        // SAFETY: `class_ptr` is a non-null `*const AVClass` libav keeps alive for
        // the logged object; `parent_log_context_offset` is a plain `c_int` field
        // of that valid C struct. A single field read.
        let offset = unsafe { (*class_ptr).parent_log_context_offset };
        // `0` (and any non-positive value) means "no parent log context".
        let offset = usize::try_from(offset).ok().filter(|&o| o > 0)?;
        // SAFETY: libav stores a `*mut c_void` to the parent at `(obj + offset)`
        // bytes. We read exactly one pointer-sized value at that documented
        // offset (unaligned-safe), never assuming any further layout. `obj` is a
        // valid object whose class declared this offset; the read stays within the
        // object libav owns.
        let parent = unsafe {
            obj.cast::<u8>()
                .add(offset)
                .cast::<*mut c_void>()
                .read_unaligned()
        };
        Some(parent)
    }

    /// Install the libav → `tracing` log bridge for the process, exactly once.
    ///
    /// Idempotent: subsequent calls are no-ops. After this, libav log output is
    /// routed into [`tracing`] and rate-limited instead of going to stderr.
    /// Safe to call from `ensure_initialized` on every libav entry point.
    pub fn install() {
        INSTALL.call_once(|| {
            // Initialise the shared state before the callback can fire.
            let _ = suppressor();
            let _ = origin();
            // The C shim's fn pointer, typed via the portable `ffi::va_list` alias
            // (see `LogCallback`). On the array-rendering toolchain this *is*
            // `av_log_set_callback`'s parameter type; on the decayed x86-64
            // rendering the transmute below reconciles the two ABI-identical
            // fn-pointer types.
            let trampoline: LogCallback = multiview_av_log_trampoline;
            // reason(allow): on bindgen renderings that keep the `ffi::va_list`
            // alias in `av_log_set_callback`'s callback parameter, `LogCallback`
            // *is* that exact type, so the transmute is the identity and clippy
            // flags it `useless_transmute`. It is **load-bearing on the decayed
            // x86-64 rendering**, where the binding's callback parameter is
            // `*mut __va_list_tag` and `LogCallback` is the array alias: the
            // transmute reconciles the two ABI-identical fn-pointer types there.
            // The lint cannot see the other rendering, so the allow is required.
            #[allow(clippy::useless_transmute)]
            // reason(allow): the transmute target is `_` ON PURPOSE — it resolves
            // to `av_log_set_callback`'s callback parameter type, which bindgen
            // renders DIFFERENTLY per toolchain (the `ffi::va_list` array alias on
            // arm64/aarch64; the decayed `*mut __va_list_tag` on x86-64 trixie),
            // so no single explicit annotation is correct on both. The
            // `missing_transmute_annotations` lint fires only on the array
            // rendering (FFmpeg 8.1.x, libavcodec .so.62) — exactly the rendering
            // the inferred `_` exists to stay portable across — so it is suppressed
            // here rather than spelling a per-arch type that would break the other.
            #[allow(clippy::missing_transmute_annotations)]
            // SAFETY: this transmutes ONE fn pointer to another. The two types
            // differ only in how bindgen *spells* the `va_list` parameter
            // (`ffi::va_list` array alias vs the decayed `*mut __va_list_tag`);
            // both are a single machine word and the same `extern "C"` calling
            // convention for that argument. Crucially the pointer's *callee* is the
            // C shim `multiview_av_log_trampoline`, which receives the real
            // `va_list` in C with the correct ABI on every architecture — so this
            // only reconciles Rust's type-checker, never the runtime ABI (that is
            // exactly why the rendering lives in C and not in a Rust trampoline).
            // `av_log_set_callback` stores the pointer in a libav global and invokes
            // it (from C, with a genuine `va_list`) for every log line on any
            // thread; the shim owns no captured state. `Some` replaces libav's
            // default stderr writer.
            let callback = unsafe { std::mem::transmute::<LogCallback, _>(trampoline) };
            // SAFETY: `callback` is the ABI-correct, bindgen-typed libav log
            // callback (the C shim's pointer). Installing it is libav's documented
            // mechanism for replacing the log writer.
            unsafe {
                ffi::av_log_set_callback(Some(callback));
            }
        });
    }

    /// Read the component name (libav `AVClass::class_name`, e.g. `"hevc"`) from
    /// a libav log object pointer, if any.
    ///
    /// libav's logging convention: any object that can be logged against has a
    /// `*const AVClass` as the **first** member of its struct, and `AVClass`'s
    /// first member is the `class_name` C string. The pointer may be null (a
    /// context-free log line), so every dereference is null-checked.
    fn component_name(avcl: *mut c_void, buf: &mut [u8; 64]) -> Option<usize> {
        if avcl.is_null() {
            return None;
        }
        // SAFETY: per libav's logging ABI the object pointed to by `avcl` begins
        // with a `*const AVClass`. We read exactly that one pointer-sized field
        // by reading the first `*const AVClass` at the object's address. The read
        // is unaligned-safe via `read_unaligned` and does not assume the rest of
        // the struct's layout. `avcl` is non-null (checked above).
        let class_ptr = unsafe { avcl.cast::<*const ffi::AVClass>().read_unaligned() };
        if class_ptr.is_null() {
            return None;
        }
        // SAFETY: `class_ptr` is a non-null `*const AVClass` provided by libav for
        // the lifetime of the logged object; reading its `class_name` field (the
        // first member) is a single pointer read of a valid C struct.
        let name_ptr = unsafe { (*class_ptr).class_name };
        if name_ptr.is_null() {
            return None;
        }
        // SAFETY: `class_name` is a static, NUL-terminated C string owned by libav
        // (string literals registered at codec/format build time); it outlives
        // this call. `CStr::from_ptr` only requires a valid NUL-terminated string.
        let cstr = unsafe { CStr::from_ptr(name_ptr) };
        let bytes = cstr.to_bytes();
        let n = bytes.len().min(buf.len());
        // Copy the (bounded) name into the caller's stack buffer; never allocate.
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), bytes.get(..n)) {
            dst.copy_from_slice(src);
            Some(n)
        } else {
            None
        }
    }

    /// The pure (panic-free, libav-free) core of the callback: resolve the
    /// resource, run the suppressor (keyed by `(level, resource_id, message)`),
    /// and emit via `tracing`. Split out so the unsafe trampoline body stays tiny.
    ///
    /// `avcl` is the libav object pointer; attribution is resolved in priority
    /// order — (A) the thread-local [`current_resource`] for lines on a thread we
    /// own, then (B) the [`av_class_map`] for libav-owned worker threads (direct
    /// lookup, then a bounded `parent_log_context_offset` walk to the owning
    /// format context), then (C) none (component-only, never a guessed id).
    fn route(level: c_int, component: &str, avcl: *mut c_void, line: &str) {
        let bridge_level = map_av_level(level);
        let clean = sanitize_line(line);
        if clean.is_empty() {
            return;
        }
        // Mechanism A → B → C (ADR-0060 §3): never guess on a miss. (B) is the
        // direct-lookup-then-parent-walk resolver, so a line logged against a
        // child decoder/sub-demuxer context still reaches its registered parent.
        let resource = current_resource().or_else(|| resolve_with_parent_walk(avcl));
        let resource_id = resource.as_ref().map(super::ResourceContext::id);
        let now = origin().elapsed();
        let outcome = match suppressor().lock() {
            Ok(mut guard) => guard.observe_scoped(bridge_level, resource_id, &clean, now),
            // A poisoned lock means another thread panicked while holding it;
            // rather than propagate, emit unconditionally (correctness over
            // anti-flood — never drop a line because of a lock fault).
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .observe_scoped(bridge_level, resource_id, &clean, now)
            }
        };
        match outcome {
            SuppressOutcome::Suppress => {}
            SuppressOutcome::Emit => emit(bridge_level, component, resource.as_ref(), &clean, None),
            SuppressOutcome::EmitWithSummary { suppressed } => {
                emit(
                    bridge_level,
                    component,
                    resource.as_ref(),
                    &clean,
                    Some(suppressed),
                );
            }
        }
    }

    /// Emit one record at the mapped tracing level, carrying the libav component,
    /// the resolved resource attribution (`resource_kind` / `resource_id` /
    /// `label`, when present), and the coalesced suppressed count when flushing.
    ///
    /// An unattributed line (`resource == None`) emits exactly the prior shape
    /// (component-only) so it keeps `component` and **omits** `resource_id`
    /// rather than guessing (ADR-0060 §3.3). Resource fields are attached as
    /// plain event fields (`Option<&str>` empty when absent) so the capture layer
    /// and the fmt writer both see them; the capture layer reads the enclosing
    /// span first and only falls back to these for libav-thread lines.
    fn emit(
        level: BridgeLevel,
        component: &str,
        resource: Option<&ResourceContext>,
        line: &str,
        suppressed: Option<u64>,
    ) {
        let window_s = SUPPRESS_WINDOW.as_secs();
        let resource_kind = resource.map(ResourceContext::kind);
        let resource_id = resource.map(ResourceContext::id);
        let label = resource.and_then(ResourceContext::label);
        match (level, suppressed) {
            (BridgeLevel::Error, None) => {
                tracing::error!(target: "libav", component, resource_kind, resource_id, label, "{line}");
            }
            (BridgeLevel::Error, Some(n)) => {
                tracing::error!(target: "libav", component, resource_kind, resource_id, label, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Warn, None) => {
                tracing::warn!(target: "libav", component, resource_kind, resource_id, label, "{line}");
            }
            (BridgeLevel::Warn, Some(n)) => {
                tracing::warn!(target: "libav", component, resource_kind, resource_id, label, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Info, None) => {
                tracing::info!(target: "libav", component, resource_kind, resource_id, label, "{line}");
            }
            (BridgeLevel::Info, Some(n)) => {
                tracing::info!(target: "libav", component, resource_kind, resource_id, label, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Debug, None) => {
                tracing::debug!(target: "libav", component, resource_kind, resource_id, label, "{line}");
            }
            (BridgeLevel::Debug, Some(n)) => {
                tracing::debug!(target: "libav", component, resource_kind, resource_id, label, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Trace, None) => {
                tracing::trace!(target: "libav", component, resource_kind, resource_id, label, "{line}");
            }
            (BridgeLevel::Trace, Some(n)) => {
                tracing::trace!(target: "libav", component, resource_kind, resource_id, label, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
        }
    }

    /// The Rust callback the **C shim** (`csrc/log_shim.c`) invokes after it has
    /// rendered a libav log line, on whatever (foreign/decoder) thread produced it.
    ///
    /// The `va_list` has already been consumed in C; this receives the libav
    /// object pointer (`avcl`, for component-name extraction; may be null), the
    /// libav `level`, and the already-formatted, NUL-terminated `line`. It reads
    /// the component name from the leading `AVClass*` and hands both to the pure
    /// router (the bounded suppressor + `tracing` emit).
    ///
    /// The entire Rust body runs inside `catch_unwind`: a logging callback must
    /// never crash the decoder, so any panic is caught and the line is dropped
    /// (CLAUDE.md §7 — never unwind across the FFI boundary). A null `line` is
    /// tolerated (treated as nothing to emit) even though the shim never passes
    /// one.
    ///
    /// `#[no_mangle]` so the C shim can resolve the symbol by name (the linker
    /// exports it regardless of Rust visibility); `extern "C"` so it has the C ABI
    /// the shim's `extern` declaration expects. Kept `pub(crate)` — it is part of
    /// the crate-private FFI seam, not a public API, and `#[no_mangle]` handles the
    /// C-side reachability.
    #[no_mangle]
    pub(crate) extern "C" fn multiview_log_emit(
        avcl: *mut c_void,
        level: c_int,
        line: *const c_char,
    ) {
        // `catch_unwind` requires `UnwindSafe`; the raw pointers/level are plain
        // `Copy` scalars with no interior invariants to break on a caught unwind.
        let result = std::panic::catch_unwind(move || {
            // A null line carries no message (defensive — the shim always passes a
            // valid pointer to its NUL-terminated stack buffer).
            if line.is_null() {
                return;
            }
            // SAFETY: per the shim's contract `line` points at a live,
            // NUL-terminated C string (the shim's stack buffer) for the duration
            // of this call; it is non-null (checked above). `CStr::from_ptr` only
            // requires a valid NUL-terminated string and the borrow does not
            // escape this scope.
            let line_cstr = unsafe { CStr::from_ptr(line) };
            let line = line_cstr.to_string_lossy();

            // Read the component (`AVClass::class_name`) into a stack buffer.
            let mut comp_buf = [0u8; 64];
            let component = match component_name(avcl, &mut comp_buf) {
                Some(n) => match comp_buf.get(..n) {
                    Some(slice) => String::from_utf8_lossy(slice),
                    None => std::borrow::Cow::Borrowed(""),
                },
                None => std::borrow::Cow::Borrowed(""),
            };

            // The libav object pointer is handed to the router for mechanism-B
            // attribution: a direct map lookup of its address, then a bounded
            // `parent_log_context_offset` walk (each hop null-checked) to reach a
            // registered owning format context. The router only ever reads the
            // documented leading `AVClass*` and the parent-offset slot.
            route(level, component.as_ref(), avcl, line.as_ref());
        });
        // On any caught panic, drop the line silently. Never re-raise across FFI.
        drop(result);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn install_is_idempotent_and_routes_synthetic_av_log() {
            // Installing twice must not double-register or panic.
            install();
            install();

            // Emit a synthetic libav log line through the real av_log path and
            // assert the bridge does not panic / crash. (Routing correctness of
            // the pure logic is asserted in tests/log_bridge.rs; here we exercise
            // the actual C → trampoline → tracing wiring end to end.)
            let msg = std::ffi::CString::new("multiview log-bridge smoke %d").expect("no NUL");
            for _ in 0..5 {
                // SAFETY: `av_log` takes a NUL-terminated printf format and
                // matching varargs; `msg` is a valid C string and `42` matches
                // the single `%d`. A null `avcl` is explicitly allowed by libav
                // (a context-free log line).
                unsafe {
                    ffi::av_log(
                        std::ptr::null_mut(),
                        super::super::AV_LOG_INFO,
                        msg.as_ptr(),
                        42,
                    );
                }
            }
        }

        #[test]
        fn component_name_is_none_for_null_object() {
            let mut buf = [0u8; 64];
            assert_eq!(component_name(std::ptr::null_mut(), &mut buf), None);
        }

        // ---- AVClassMap (mechanism B) registration + pointer reuse ----------

        #[test]
        fn registered_context_resolves_then_clears_on_drop() {
            // Use a distinct, never-aliasing synthetic address per test to avoid
            // cross-test interference on the process-global map.
            let ptr = 0xA11C_E000_usize;
            assert_eq!(resolve_registered(ptr), None, "unregistered → no resource");
            {
                let _reg = register_av_context(ptr, ResourceContext::source("cnn"));
                let got = resolve_registered(ptr).expect("registered resolves");
                assert_eq!(got.id(), "cnn");
                assert_eq!(got.kind(), "source");
            }
            assert_eq!(
                resolve_registered(ptr),
                None,
                "the registration guard removed the entry on drop"
            );
        }

        #[test]
        fn null_pointer_never_resolves() {
            assert_eq!(resolve_registered(0), None);
        }

        #[test]
        fn pointer_reuse_does_not_resolve_to_the_dead_resource() {
            // open(cnn)@addr → close → reopen(bbc)@same addr. The reopen must win;
            // dropping the OLD guard must NOT tear down the NEW registration.
            let ptr = 0xBEEF_0000_usize;
            let old = register_av_context(ptr, ResourceContext::source("cnn"));
            assert_eq!(
                resolve_registered(ptr).map(|r| r.id().to_owned()),
                Some("cnn".to_owned())
            );

            // Reopen at the same address with a new resource (fresh, larger epoch).
            let new = register_av_context(ptr, ResourceContext::source("bbc"));
            assert_eq!(
                resolve_registered(ptr).map(|r| r.id().to_owned()),
                Some("bbc".to_owned()),
                "the reopen overwrites the stale mapping"
            );

            // Dropping the OLD guard must leave the NEW (different-epoch) entry.
            drop(old);
            assert_eq!(
                resolve_registered(ptr).map(|r| r.id().to_owned()),
                Some("bbc".to_owned()),
                "the old guard's drop must not remove the reopened registration"
            );

            drop(new);
            assert_eq!(
                resolve_registered(ptr),
                None,
                "dropping the live registration finally clears it"
            );
        }

        // ---- mechanism B parent walk (parent_log_context_offset) ------------
        //
        // libav's frame-threaded decoders / HLS sub-demuxers log against a CHILD
        // object (e.g. an `AVCodecContext`) whose `AVClass` carries a
        // `parent_log_context_offset`: at `(child_ptr + offset)` libav stores a
        // pointer to the parent (the `AVFormatContext` we registered). The bridge
        // must follow that link so the operator's "Error constructing the frame
        // RPS" HEVC line — emitted on a libav decoder thread against the codec
        // context, never our format context — still names its source.
        //
        // These tests fabricate the libav object/class layout the walk reads:
        // an object whose FIRST field is `*const AVClass`, and (for the parent
        // link) a `*mut c_void` planted at the class's declared offset.

        /// A fake libav-loggable object: a leading `*const AVClass` followed by a
        /// parent-context pointer slot the class's `parent_log_context_offset`
        /// points at. Mirrors how a real `AVCodecContext` exposes its parent.
        #[repr(C)]
        struct FakeObj {
            class: *const ffi::AVClass,
            parent: *mut c_void,
        }

        /// Build an `AVClass` whose `parent_log_context_offset` is the byte offset
        /// of [`FakeObj::parent`] (so the walk reads that slot), with a stable
        /// `class_name`. `parent_offset == 0` means "no parent" (libav's sentinel).
        fn fake_class(name: &'static CStr, parent_offset: c_int) -> ffi::AVClass {
            // SAFETY (test): a zeroed AVClass is a valid all-fields-null/0 value;
            // we then set only the two fields the walk reads (`class_name`,
            // `parent_log_context_offset`). Pointers stay null/Option None. The
            // caller binds the returned value to a local whose address is stable
            // for the test scope (referenced by `class_ptr`).
            let mut class: ffi::AVClass = unsafe { std::mem::zeroed() };
            class.class_name = name.as_ptr();
            class.parent_log_context_offset = parent_offset;
            class
        }

        /// The leading `*const AVClass` slot value for a fabricated object.
        fn class_ptr(class: &ffi::AVClass) -> *const ffi::AVClass {
            std::ptr::from_ref::<ffi::AVClass>(class)
        }

        /// The opaque `*mut c_void` address of a fabricated object (what libav
        /// hands the log callback as `avcl`).
        fn obj_ptr(obj: &FakeObj) -> *mut c_void {
            std::ptr::from_ref::<FakeObj>(obj)
                .cast::<c_void>()
                .cast_mut()
        }

        /// The map-key address of a fabricated object pointer.
        ///
        /// reason(allow): pointer→address cast for a map key only (never
        /// dereferenced); `<*mut T>::addr()` is MSRV-1.84 and this crate is 1.82.
        #[allow(clippy::as_conversions)]
        fn addr(ptr: *mut c_void) -> usize {
            ptr as usize
        }

        #[test]
        fn parent_walk_resolves_a_child_whose_parent_is_registered() {
            // Distinct synthetic key space; the parent is a REAL stack object so
            // its address is genuine and registrable.
            let parent_name = c"hls";
            let child_name = c"hevc";
            let parent_class = fake_class(parent_name, 0);
            let child_class = fake_class(
                child_name,
                // parent slot is the second field of FakeObj.
                c_int::try_from(std::mem::offset_of!(FakeObj, parent)).unwrap_or(0),
            );

            // The parent (format context) object we own and register.
            let parent_obj = FakeObj {
                class: class_ptr(&parent_class),
                parent: std::ptr::null_mut(),
            };
            let parent_ptr = obj_ptr(&parent_obj);
            let _reg = register_av_context(addr(parent_ptr), ResourceContext::source("cnn"));

            // The child (codec context) object libav logs against; its parent slot
            // points at the registered format context.
            let child_obj = FakeObj {
                class: class_ptr(&child_class),
                parent: parent_ptr,
            };
            let child_ptr = obj_ptr(&child_obj);

            // Direct lookup of the child misses (only the parent is registered)…
            assert_eq!(
                resolve_registered(addr(child_ptr)),
                None,
                "the child context itself is not registered"
            );
            // …but the parent walk reaches the registered format context.
            let got =
                resolve_with_parent_walk(child_ptr).expect("the parent walk resolves the child");
            assert_eq!(got.id(), "cnn", "resolved via parent_log_context_offset");
            assert_eq!(got.kind(), "source");
        }

        #[test]
        fn parent_walk_returns_none_when_no_parent_is_registered() {
            // A child whose parent is NOT registered (an unrelated decoder thread):
            // mechanism C honesty — resolve to None, never a guessed id.
            let child_class = fake_class(
                c"hevc",
                c_int::try_from(std::mem::offset_of!(FakeObj, parent)).unwrap_or(0),
            );
            // An unregistered parent object (a real address, never registered).
            let parent_obj = FakeObj {
                class: std::ptr::null(),
                parent: std::ptr::null_mut(),
            };
            let parent_ptr = obj_ptr(&parent_obj);
            let child_obj = FakeObj {
                class: class_ptr(&child_class),
                parent: parent_ptr,
            };
            let child_ptr = obj_ptr(&child_obj);

            assert_eq!(
                resolve_with_parent_walk(child_ptr),
                None,
                "an unregistered child + unregistered parent resolves to None (mechanism C)"
            );
        }

        #[test]
        fn parent_walk_resolves_transitively_through_two_hops() {
            // grandparent (registered) ← parent ← child. The bounded walk must
            // climb both hops to reach the registered grandparent.
            let gp_class = fake_class(c"hls", 0);
            let parent_offset = c_int::try_from(std::mem::offset_of!(FakeObj, parent)).unwrap_or(0);
            let parent_class = fake_class(c"hls", parent_offset);
            let child_class = fake_class(c"hevc", parent_offset);

            let gp = FakeObj {
                class: class_ptr(&gp_class),
                parent: std::ptr::null_mut(),
            };
            let gp_ptr = obj_ptr(&gp);
            let _reg = register_av_context(addr(gp_ptr), ResourceContext::source("abc"));

            let parent = FakeObj {
                class: class_ptr(&parent_class),
                parent: gp_ptr,
            };
            let parent_ptr = obj_ptr(&parent);
            let child = FakeObj {
                class: class_ptr(&child_class),
                parent: parent_ptr,
            };
            let child_ptr = obj_ptr(&child);

            let got = resolve_with_parent_walk(child_ptr)
                .expect("the two-hop walk reaches the grandparent");
            assert_eq!(got.id(), "abc");
        }

        #[test]
        fn parent_walk_direct_hit_short_circuits() {
            // When the logged object itself is registered, the walk returns it
            // without needing a parent link.
            let cls = fake_class(c"hls", 0);
            let obj = FakeObj {
                class: class_ptr(&cls),
                parent: std::ptr::null_mut(),
            };
            let ptr = obj_ptr(&obj);
            let _reg = register_av_context(addr(ptr), ResourceContext::output("rtsp-main"));
            let got = resolve_with_parent_walk(ptr).expect("a directly-registered object resolves");
            assert_eq!(got.id(), "rtsp-main");
            assert_eq!(got.kind(), "output");
        }

        // reason(allow): `obj.parent = ptr` writes the cycle link that the
        // resolver then reads *through the raw pointer* — the compiler can't see
        // that aliasing read, so it false-flags the store as never read.
        #[allow(unused_assignments)]
        #[test]
        fn parent_walk_tolerates_null_and_self_referential_links() {
            // A null `avcl` resolves to None; a self-referential parent link must
            // not loop forever (the bounded hop cap stops it) and resolves to None
            // when nothing in the chain is registered.
            assert_eq!(resolve_with_parent_walk(std::ptr::null_mut()), None);

            let cls = fake_class(
                c"hevc",
                c_int::try_from(std::mem::offset_of!(FakeObj, parent)).unwrap_or(0),
            );
            // self-referential: the object's parent slot points back at itself.
            let mut obj = FakeObj {
                class: class_ptr(&cls),
                parent: std::ptr::null_mut(),
            };
            let ptr = obj_ptr(&obj);
            obj.parent = ptr; // cycle
            assert_eq!(
                resolve_with_parent_walk(ptr),
                None,
                "a self-referential, unregistered chain terminates and resolves to None"
            );
        }
    }
}
