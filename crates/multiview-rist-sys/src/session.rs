//! Runtime-loaded librist **sender** session with the link-stats callback
//! (ADR-0095 Tier-1 / RIST-5; `session` feature).
//!
//! This is the direct-librist egress transport that *has* statistics: it owns a
//! `rist_ctx` (a sender), parses the `rist://…` URL into a peer config via
//! librist's own `rist_parse_address2` (so the typed [`multiview_config`] URL
//! lowering is the single source of truth and we never hand-mirror the large,
//! version-sensitive `rist_peer_config` struct), creates the peer, registers
//! `rist_stats_callback_set`, starts the context, and writes the **same** encoded
//! MPEG-TS packets every other push sink fans out (inv #7). The stats callback
//! runs on librist's own thread and only ever **publishes** a decoded
//! [`RistLinkSample`] to a bounded drop-oldest channel — it never blocks, never
//! allocates beyond the sample, and never unwinds across the FFI boundary
//! (safety rules §4, inv #10).
//!
//! ## Honest boundary
//!
//! Only the **sender** is built here (leaf-sized: a sink consuming already-encoded
//! packets, replacing no shared data-plane transport). A direct-librist
//! **receiver** with stats owns the receive+demux loop (a new `Source`) and is a
//! larger, Tier-2-shaped change — deliberately **not** built; the
//! [`crate::decode_stats`] already handles the receiver-flow shape so the model
//! is complete and ready for that follow-up.
//!
//! librist is **never linked at build time** and never vendored: the `.so`
//! (`librist.so.4` / `librist.so`) is `dlopen`-resolved at run time. A build with
//! the `session` feature still compiles and links with no librist present; only a
//! run that opens a session needs the runtime library.

#![allow(unsafe_code)] // The runtime-load FFI boundary lives in this module.

use std::ffi::{c_int, c_void, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use libloading::Library;

use crate::raw::RawStats;
use crate::{decode_stats, DecodeError};
use multiview_telemetry::rist::RistLinkSample;

/// Opaque librist handles — we never read their fields, so they stay anonymous
/// pointers (no fragile layout mirror).
#[repr(C)]
struct RistCtx {
    _private: [u8; 0],
}
#[repr(C)]
struct RistPeer {
    _private: [u8; 0],
}
#[repr(C)]
struct RistPeerConfig {
    _private: [u8; 0],
}

/// The librist `rist_profile` enum value (simple=0, main=1, advanced=2).
pub type RistProfileValue = c_int;

/// An error opening or driving a librist sender session.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The librist shared library could not be `dlopen`-resolved at run time.
    #[error("librist runtime could not be loaded: {0}")]
    Load(String),
    /// A required librist symbol was absent from the loaded library.
    #[error("librist runtime is missing the `{0}` symbol")]
    MissingSymbol(&'static str),
    /// A librist call returned a non-zero (error) status.
    #[error("librist `{call}` failed with status {status}")]
    Call {
        /// The librist function that failed.
        call: &'static str,
        /// The non-zero status it returned.
        status: c_int,
    },
    /// The `rist://` URL contained an interior NUL and could not be a C string.
    #[error("rist url is not a valid C string (interior NUL)")]
    InvalidUrl,
}

/// The librist functions this session resolves at load time (the hand fn-table,
/// ADR-0028). Each is the verbatim librist 0.2.x C signature.
struct RistApi {
    sender_create: unsafe extern "C" fn(*mut *mut RistCtx, c_int, u32, *mut c_void) -> c_int,
    parse_address2: unsafe extern "C" fn(*const c_char, *mut *mut RistPeerConfig) -> c_int,
    peer_create:
        unsafe extern "C" fn(*mut RistCtx, *mut *mut RistPeer, *const RistPeerConfig) -> c_int,
    peer_config_free2: unsafe extern "C" fn(*mut *mut RistPeerConfig) -> c_int,
    stats_callback_set: unsafe extern "C" fn(
        *mut RistCtx,
        c_int,
        Option<unsafe extern "C" fn(*mut c_void, *const RawStats) -> c_int>,
        *mut c_void,
    ) -> c_int,
    start: unsafe extern "C" fn(*mut RistCtx) -> c_int,
    sender_data_write: unsafe extern "C" fn(*mut RistCtx, *const RistDataBlock) -> c_int,
    destroy: unsafe extern "C" fn(*mut RistCtx) -> c_int,
}

/// `struct rist_data_block` (verbatim librist 0.2.x field order). Only the
/// leading fields `rist_sender_data_write` reads (`payload`/`payload_len`/
/// `ts_ntp`/`virt_src_port`) are populated; the trailing receiver/output fields
/// are zeroed but **present in full** so the struct ABI-matches librist's exactly
/// (a short struct could let librist read or write past our allocation).
#[repr(C)]
struct RistDataBlock {
    payload: *const c_void,
    payload_len: usize,
    ts_ntp: u64,
    virt_src_port: u16,
    virt_dst_port: u16,
    // Receiver-populated / output fields — not used by the sender write, but
    // present so the struct ABI-matches `struct rist_data_block` byte-for-byte.
    peer: *mut c_void,
    flow_id: u32,
    seq: u64,
    flags: u32,
    /// `struct rist_ref *ref` — librist's internal refcount handle (output only).
    rist_ref: *mut c_void,
}

/// The shared callback context handed to librist's stats callback as `void *arg`.
///
/// Holds the link id (for labelling the decoded sample) and the bounded sender
/// the callback publishes to. `Arc`-shared so the callback context outlives a
/// brief overlap on session teardown; the callback only ever **sends** (it never
/// reads engine state), so it cannot back-pressure anything (inv #10).
struct CallbackCtx {
    link_id: String,
    tx: SyncSender<RistLinkSample>,
}

/// A live librist sender session: an owned `rist_ctx` with the stats callback
/// firing decoded [`RistLinkSample`]s onto [`stats`](RistSenderSession::stats).
///
/// Dropping the session destroys the librist context (`rist_destroy`), which
/// stops the stats thread; the callback context is then released.
pub struct RistSenderSession {
    // Field order matters for drop: the library must outlive the ctx it created.
    ctx: *mut RistCtx,
    api: RistApi,
    // Kept alive for the session's lifetime so the resolved fn pointers stay
    // valid (they point into this mapped library).
    _lib: Library,
    // Kept alive so librist's callback `arg` pointer stays valid until destroy.
    _cb: Arc<CallbackCtx>,
    stats_rx: Receiver<RistLinkSample>,
}

// SAFETY: the librist ctx is driven only from the owning thread (writes) plus
// librist's internal stats thread (which we never touch the ctx from); the
// session itself is moved, not shared, across threads. The raw pointers are
// owned, not aliased. `Send` is sound because no `&` to the ctx escapes.
unsafe impl Send for RistSenderSession {}

impl RistSenderSession {
    /// Open a librist sender session for `link_id`, sending to the `rist://` URL
    /// `url`, with `profile` and a stats reporting `interval_ms`.
    ///
    /// `url` is the already-lowered `rist://…?…` AVIO-style URL (the typed
    /// [`multiview_config`] options resolved, secret injected) — librist's own
    /// `rist_parse_address2` parses it, so URL semantics stay identical to the
    /// Tier-0 `FFmpeg` path.
    ///
    /// # Errors
    /// [`SessionError`] if the runtime cannot be loaded, a symbol is missing, the
    /// URL is invalid, or any librist call fails.
    pub fn open(
        link_id: impl Into<String>,
        url: &str,
        profile: RistProfileValue,
        interval_ms: c_int,
    ) -> Result<Self, SessionError> {
        let lib = load_librist()?;
        let api = RistApi::resolve(&lib)?;
        let link_id = link_id.into();

        // A bounded, drop-oldest-on-full channel: the stats thread must never
        // block (inv #10). `sync_channel(N)` blocks when full, so the callback
        // uses `try_send` and drops on `Full` — a lagging consumer just misses a
        // sample, never stalls the librist thread.
        let (tx, stats_rx) = std::sync::mpsc::sync_channel::<RistLinkSample>(8);
        let cb = Arc::new(CallbackCtx { link_id, tx });

        // Create the sender ctx.
        let mut ctx: *mut RistCtx = ptr::null_mut();
        // SAFETY: `sender_create` is the resolved librist symbol; we pass a valid
        // out-pointer for the ctx, the profile int, flow_id 0 (librist assigns),
        // and a null logging-settings pointer (librist uses its global default).
        let rc =
            unsafe { (api.sender_create)(ptr::from_mut(&mut ctx), profile, 0, ptr::null_mut()) };
        if rc != 0 || ctx.is_null() {
            return Err(SessionError::Call {
                call: "rist_sender_create",
                status: rc,
            });
        }
        // From here, any early return drops `session`, whose `Drop` destroys the
        // ctx — so a failed peer/start path never leaks the librist context.
        let session = Self {
            ctx,
            api,
            _lib: lib,
            _cb: Arc::clone(&cb),
            stats_rx,
        };

        // Register the stats callback BEFORE start so no sample is missed.
        let arg = Arc::as_ptr(&cb).cast::<c_void>().cast_mut();
        // SAFETY: `ctx` is the just-created non-null sender ctx; `stats_trampoline`
        // is an `extern "C"` fn with the exact librist callback signature; `arg`
        // points at the `Arc<CallbackCtx>` we keep alive for the session lifetime
        // (`_cb`), so it is valid for every callback invocation.
        let rc = unsafe {
            (session.api.stats_callback_set)(session.ctx, interval_ms, Some(stats_trampoline), arg)
        };
        if rc != 0 {
            return Err(SessionError::Call {
                call: "rist_stats_callback_set",
                status: rc,
            });
        }

        // Parse the URL into a peer config and create the peer.
        let c_url = CString::new(url).map_err(|_| SessionError::InvalidUrl)?;
        let mut cfg: *mut RistPeerConfig = ptr::null_mut();
        // SAFETY: `parse_address2` takes a NUL-terminated C string and an
        // out-pointer it allocates; `c_url` is valid for the call and `cfg` is a
        // valid out-slot.
        let rc = unsafe { (session.api.parse_address2)(c_url.as_ptr(), ptr::from_mut(&mut cfg)) };
        if rc != 0 || cfg.is_null() {
            return Err(SessionError::Call {
                call: "rist_parse_address2",
                status: rc,
            });
        }
        let mut peer: *mut RistPeer = ptr::null_mut();
        // SAFETY: `ctx` is the live sender ctx, `peer` a valid out-slot, `cfg` the
        // librist-allocated peer config from `parse_address2`.
        let rc = unsafe { (session.api.peer_create)(session.ctx, ptr::from_mut(&mut peer), cfg) };
        // Free the config regardless: librist copies what it needs in peer_create.
        // SAFETY: `cfg` is the librist-allocated config; `peer_config_free2` takes
        // its address and nulls it. Freeing after peer_create is the documented
        // ownership (peer_create does not take ownership of the config).
        unsafe {
            let _ = (session.api.peer_config_free2)(ptr::from_mut(&mut cfg));
        }
        if rc != 0 || peer.is_null() {
            return Err(SessionError::Call {
                call: "rist_peer_create",
                status: rc,
            });
        }

        // Start the context (begins the connection + stats reporting thread).
        // SAFETY: `ctx` is the live, peer-configured sender ctx.
        let rc = unsafe { (session.api.start)(session.ctx) };
        if rc != 0 {
            return Err(SessionError::Call {
                call: "rist_start",
                status: rc,
            });
        }

        Ok(session)
    }

    /// Write one encoded MPEG-TS payload to the RIST flow (inv #7: the same
    /// packets fanned to every other sink).
    ///
    /// # Errors
    /// [`SessionError::Call`] if `rist_sender_data_write` returns negative.
    pub fn write(&self, payload: &[u8]) -> Result<(), SessionError> {
        let block = RistDataBlock {
            payload: payload.as_ptr().cast::<c_void>(),
            payload_len: payload.len(),
            ts_ntp: 0,
            virt_src_port: 0,
            virt_dst_port: 0,
            peer: ptr::null_mut(),
            flow_id: 0,
            seq: 0,
            flags: 0,
            rist_ref: ptr::null_mut(),
        };
        // SAFETY: `ctx` is the live sender ctx; `block` is a fully-initialised
        // `rist_data_block` whose `payload`/`payload_len` reference the caller's
        // slice, valid for the duration of this synchronous call (librist copies
        // the payload into its buffer before returning).
        let rc = unsafe { (self.api.sender_data_write)(self.ctx, ptr::from_ref(&block)) };
        if rc < 0 {
            return Err(SessionError::Call {
                call: "rist_sender_data_write",
                status: rc,
            });
        }
        Ok(())
    }

    /// Drain any link-stats samples the callback has published since the last
    /// poll (non-blocking; bounded). The producer loop calls this off the data
    /// plane and forwards each sample to the telemetry surface.
    #[must_use]
    pub fn drain_stats(&self) -> Vec<RistLinkSample> {
        self.stats_rx.try_iter().collect()
    }
}

impl Drop for RistSenderSession {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            // SAFETY: `ctx` is the live librist ctx we created and have not yet
            // destroyed; `rist_destroy` stops the stats thread and frees it. After
            // this no further callback fires, so releasing `_cb` afterwards is
            // safe. We null the pointer to make a double-drop a no-op.
            unsafe {
                let _ = (self.api.destroy)(self.ctx);
            }
            self.ctx = ptr::null_mut();
        }
    }
}

/// librist's stats callback (`extern "C"`, runs on librist's thread).
///
/// Decodes the C stats blob and **try-sends** the sample on the bounded channel,
/// dropping on a full channel rather than blocking (inv #10). It never panics
/// across the FFI boundary: every fallible step is matched, and a decode error
/// or a full channel simply skips the sample. Returns 0 (librist ignores the
/// return but expects an int).
unsafe extern "C" fn stats_trampoline(arg: *mut c_void, stats: *const RawStats) -> c_int {
    // SAFETY: `arg` is the `Arc<CallbackCtx>` pointer we registered and keep alive
    // for the session lifetime; `stats` is the librist-owned stats container valid
    // for the callback duration. We only read them; we never free `stats` (librist
    // owns it) and never drop the `Arc` here (we borrow it via a raw ref).
    let ctx: &CallbackCtx = unsafe {
        match arg.cast::<CallbackCtx>().as_ref() {
            Some(c) => c,
            None => return 0,
        }
    };
    let stats_ref: &RawStats = unsafe {
        match stats.as_ref() {
            Some(s) => s,
            None => return 0,
        }
    };
    // Stamp `since` with a monotonic-ish nanosecond clock; the engine re-derives
    // ordering, so a coarse process-relative timestamp is sufficient here.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let since_ns = i64::try_from(nanos).unwrap_or(i64::MAX);
    match decode_stats(stats_ref, ctx.link_id.clone(), since_ns) {
        Ok(sample) => {
            // Drop-oldest semantics: try_send, ignore a full channel (a lagging
            // consumer misses a sample — the librist thread is never blocked).
            let _ = ctx.tx.try_send(sample);
        }
        Err(DecodeError::UnknownStatsType(_)) => {
            // A newer/foreign stats type: skip it, never guess the union arm.
        }
    }
    0
}

/// `dlopen` the host librist, trying the SONAME variants in order.
fn load_librist() -> Result<Library, SessionError> {
    // SAFETY: `Library::new` `dlopen`s the named shared object. The names are
    // fixed string literals (no attacker-controlled path); loading librist runs
    // only its normal initialisers. We try the versioned SONAME first (what is
    // actually installed) then the dev symlink.
    for name in ["librist.so.4", "librist.so", "librist.so.4.4.0"] {
        if let Ok(lib) = unsafe { Library::new(name) } {
            return Ok(lib);
        }
    }
    Err(SessionError::Load(
        "could not dlopen librist.so.4 / librist.so".to_owned(),
    ))
}

impl RistApi {
    /// Resolve every required librist symbol from the loaded library into the
    /// flat fn-table (read once; ADR-0028).
    fn resolve(lib: &Library) -> Result<Self, SessionError> {
        // SAFETY: each `get` resolves a symbol whose Rust type we declare to match
        // the librist 0.2.x C signature verbatim (verified against the installed
        // librist headers). The returned `Symbol` borrows `lib`; we copy the bare
        // fn pointer out and keep `lib` mapped for the session lifetime so the
        // pointers stay valid.
        unsafe {
            // libloading's `get` reads the name as a C string, so each must be
            // NUL-terminated (mirrors `multiview-ndi-sys`'s `b"…\0"`).
            Ok(Self {
                sender_create: *resolve(lib, b"rist_sender_create\0")?,
                parse_address2: *resolve(lib, b"rist_parse_address2\0")?,
                peer_create: *resolve(lib, b"rist_peer_create\0")?,
                peer_config_free2: *resolve(lib, b"rist_peer_config_free2\0")?,
                stats_callback_set: *resolve(lib, b"rist_stats_callback_set\0")?,
                start: *resolve(lib, b"rist_start\0")?,
                sender_data_write: *resolve(lib, b"rist_sender_data_write\0")?,
                destroy: *resolve(lib, b"rist_destroy\0")?,
            })
        }
    }
}

/// Resolve a single symbol, mapping a missing symbol to a typed error.
///
/// # Safety
/// The declared `T` must match the C symbol's ABI signature. `name` must be a
/// NUL-terminated byte string (libloading reads it as a C string).
unsafe fn resolve<'lib, T>(
    lib: &'lib Library,
    name: &'static [u8],
) -> Result<libloading::Symbol<'lib, T>, SessionError> {
    // SAFETY: caller guarantees `T` matches the symbol ABI; `name` is a
    // NUL-terminated byte string naming a librist export.
    unsafe {
        lib.get(name).map_err(|_| {
            // Trim the trailing NUL so the error names the symbol cleanly.
            let sym = std::str::from_utf8(name)
                .unwrap_or("<symbol>")
                .trim_end_matches('\0');
            SessionError::MissingSymbol(static_name(sym))
        })
    }
}

/// Map a resolved symbol name back to a `&'static str` for the error (the names
/// are a fixed, known set).
fn static_name(sym: &str) -> &'static str {
    match sym {
        "rist_sender_create" => "rist_sender_create",
        "rist_parse_address2" => "rist_parse_address2",
        "rist_peer_create" => "rist_peer_create",
        "rist_peer_config_free2" => "rist_peer_config_free2",
        "rist_stats_callback_set" => "rist_stats_callback_set",
        "rist_start" => "rist_start",
        "rist_sender_data_write" => "rist_sender_data_write",
        "rist_destroy" => "rist_destroy",
        _ => "<librist symbol>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live FFI roundtrip (RIST-5 capstone): `dlopen` the real librist, resolve
    /// every symbol, open a sender to a loopback listener, write a TS-sized
    /// payload, and drain the stats channel — proving the whole binding (the
    /// hand fn-table, the peer-config parse, the stats callback registration, the
    /// data write) works against the installed librist with no panic and no UB.
    /// `#[ignore]`: needs `librist.so` present (the CI box / the deploy image
    /// built `--enable-librist`).
    ///
    /// ```text
    /// cargo test -p multiview-rist-sys --features session --lib -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires librist.so present (run on a librist-equipped host)"]
    fn live_sender_session_opens_writes_and_drains_stats() {
        // A high loopback port so a transient listener is not required for the
        // sender to construct (a RIST sender connects lazily / tolerates no peer).
        let session = RistSenderSession::open("live-test", "rist://[::1]:8099", 0, 250)
            .expect("librist present: a sender session opens against loopback");

        // Writing a TS-aligned payload must not error or panic (librist buffers it).
        let payload = [0_u8; 1316];
        session
            .write(&payload)
            .expect("a sender data write succeeds");

        // Draining stats is non-blocking and bounded; it may be empty this early
        // (the stats interval is 250 ms and we do not sleep), which is fine — the
        // point is that the binding round-trips without UB.
        let _ = session.drain_stats();

        // Dropping the session destroys the librist ctx cleanly (no panic, no
        // double-free); the test ending exercises `Drop`.
    }

    #[test]
    fn static_name_is_stable_for_known_symbols() {
        assert_eq!(static_name("rist_start"), "rist_start");
        assert_eq!(static_name("rist_destroy"), "rist_destroy");
        assert_eq!(static_name("nope"), "<librist symbol>");
    }
}
