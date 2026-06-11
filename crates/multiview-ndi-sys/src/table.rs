//! The resolved NDI v6 function table (ADR-0028 §1).
//!
//! `NDIlib_v6_load` returns a versioned, append-only table whose leading slots
//! `bindgen` renders as anonymous unions (`__bindgen_anon_N`, each carrying one
//! function pointer under its current **and** deprecated name). Reading those
//! indices is fragile and `unsafe`, so we do it **exactly once** here:
//! [`NdiV6::resolve`] reads each needed slot, null-checks its pointer, and stores
//! the bare `unsafe extern "C" fn` in a flat, stably-named struct. Nothing else in
//! the workspace ever touches `__bindgen_anon_N`. Because v6_3 is append-only, an
//! SDK bump never renumbers what we read — and if one ever did, only this file
//! changes (the single source of the anon-union index knowledge).

// The whole resolved table is built up-front (ADR-0028 §1: read the anon-union
// slots ONCE). The send/recv/find handles consume most fields; `version` is read
// only by the in-crate live test (so it is unused in a non-test build). Allowing
// dead-code for this one module keeps the full resolved table intact without a
// per-field attribute.
#![allow(dead_code)]

use crate::ffi;
use crate::NdiApiTable;

/// SDK send-instance handle (opaque pointer owned by the runtime).
pub(crate) type SendInstance = ffi::NDIlib_send_instance_t;
/// SDK receive-instance handle.
pub(crate) type RecvInstance = ffi::NDIlib_recv_instance_t;
/// SDK finder-instance handle.
pub(crate) type FindInstance = ffi::NDIlib_find_instance_t;

/// The resolved NDI v6 function table: bare, non-null function pointers with our
/// own stable names, extracted once from the `bindgen` table at load time.
///
/// Field signatures are copied verbatim from the `bindgen`-generated ABI (derived
/// from the licensed header), so they cannot drift from the real SDK.
///
/// `Copy`: every field is a bare `fn` pointer (itself `Copy`), so the resolved
/// table is a cheap value the safe handles ([`crate::send`]/`recv`/`find`) each
/// hold by value — no shared ownership or lifetime threading needed. The pointers
/// stay valid only while the owning [`crate::NdiRuntime`] (the loaded `Library`)
/// is alive; each handle documents that it must not outlive that runtime.
#[derive(Clone, Copy)]
pub(crate) struct NdiV6 {
    pub(crate) version: unsafe extern "C" fn() -> *const std::os::raw::c_char,
    pub(crate) initialize: unsafe extern "C" fn() -> bool,

    pub(crate) find_create_v2:
        unsafe extern "C" fn(*const ffi::NDIlib_find_create_t) -> FindInstance,
    pub(crate) find_get_current_sources:
        unsafe extern "C" fn(FindInstance, *mut u32) -> *const ffi::NDIlib_source_t,
    pub(crate) find_destroy: unsafe extern "C" fn(FindInstance),

    pub(crate) send_create: unsafe extern "C" fn(*const ffi::NDIlib_send_create_t) -> SendInstance,
    pub(crate) send_send_video_v2:
        unsafe extern "C" fn(SendInstance, *const ffi::NDIlib_video_frame_v2_t),
    pub(crate) send_send_video_async_v2:
        unsafe extern "C" fn(SendInstance, *const ffi::NDIlib_video_frame_v2_t),
    pub(crate) send_send_audio_v3:
        unsafe extern "C" fn(SendInstance, *const ffi::NDIlib_audio_frame_v3_t),
    pub(crate) send_destroy: unsafe extern "C" fn(SendInstance),

    pub(crate) recv_create_v3:
        unsafe extern "C" fn(*const ffi::NDIlib_recv_create_v3_t) -> RecvInstance,
    pub(crate) recv_capture_v3: unsafe extern "C" fn(
        RecvInstance,
        *mut ffi::NDIlib_video_frame_v2_t,
        *mut ffi::NDIlib_audio_frame_v3_t,
        *mut ffi::NDIlib_metadata_frame_t,
        u32,
    ) -> ffi::NDIlib_frame_type_e,
    pub(crate) recv_free_video_v2:
        unsafe extern "C" fn(RecvInstance, *const ffi::NDIlib_video_frame_v2_t),
    pub(crate) recv_free_audio_v3:
        unsafe extern "C" fn(RecvInstance, *const ffi::NDIlib_audio_frame_v3_t),
    pub(crate) recv_destroy: unsafe extern "C" fn(RecvInstance),
}

impl NdiV6 {
    /// Ensure the NDI runtime is initialised (idempotent; the SDK ref-counts
    /// `initialize`/`destroy`). Returns whether this CPU is supported. Sending
    /// works without it, but **discovery** (mDNS advertise/browse for sources)
    /// requires it, so every safe handle calls it on construction.
    // reason: one FFI call into the resolved `initialize` fn pointer (// SAFETY).
    #[allow(unsafe_code)]
    pub(crate) fn ensure_initialized(&self) -> bool {
        // SAFETY: `self.initialize` is the resolved `NDIlib_initialize` fn pointer
        // (process-lifetime). It takes no arguments, has no preconditions, and is
        // safe to call repeatedly — the SDK ref-counts init/destroy. We leave it
        // initialised for the process lifetime (the owning runtime lives that long).
        unsafe { (self.initialize)() }
    }

    /// Resolve the function table from the loaded [`NdiApiTable`].
    ///
    /// Reads each required `__bindgen_anon_N` slot exactly once and unwraps its
    /// (non-null by SDK contract) function pointer; a missing pointer is a typed
    /// [`TableError`] rather than a panic. The borrowed table is only **read**
    /// here (each `Option<fn>` value is copied out); no SDK function is called.
    ///
    /// # Errors
    /// [`TableError::MissingFn`] if any required function pointer is null.
    // reason: the one `unsafe` boundary of this module — deref the loaded table
    // pointer + read its anonymous-union function-pointer slots (// SAFETY below).
    #[allow(unsafe_code)]
    pub(crate) fn resolve(table: NdiApiTable) -> Result<Self, TableError> {
        // SAFETY: `table.v6()` is the process-lifetime NDIlib_v6 (= v6_3) table
        // returned by `NDIlib_v6_load`, valid for the owning `NdiRuntime`'s life.
        // Each field is read from its documented anonymous-union slot (the two
        // member names alias the same pointer). We only COPY the `Option<fn>`
        // values out; no function is invoked, and no pointer is dereferenced
        // beyond the table struct itself.
        let slots = unsafe {
            let t = &*table.v6();
            Slots {
                version: t.__bindgen_anon_3.version,
                initialize: t.__bindgen_anon_1.initialize,
                find_create_v2: t.__bindgen_anon_6.find_create_v2,
                find_get_current_sources: t.__bindgen_anon_44.find_get_current_sources,
                find_destroy: t.__bindgen_anon_7.find_destroy,
                send_create: t.__bindgen_anon_9.send_create,
                send_send_video_v2: t.__bindgen_anon_51.send_send_video_v2,
                send_send_video_async_v2: t.__bindgen_anon_52.send_send_video_async_v2,
                send_send_audio_v3: t.__bindgen_anon_97.send_send_audio_v3,
                send_destroy: t.__bindgen_anon_10.send_destroy,
                recv_create_v3: t.__bindgen_anon_85.recv_create_v3,
                recv_capture_v3: t.__bindgen_anon_102.recv_capture_v3,
                recv_free_video_v2: t.__bindgen_anon_48.recv_free_video_v2,
                recv_free_audio_v3: t.__bindgen_anon_103.recv_free_audio_v3,
                recv_destroy: t.__bindgen_anon_24.recv_destroy,
            }
        };
        Ok(Self {
            version: req(slots.version, "version")?,
            initialize: req(slots.initialize, "initialize")?,
            find_create_v2: req(slots.find_create_v2, "find_create_v2")?,
            find_get_current_sources: req(
                slots.find_get_current_sources,
                "find_get_current_sources",
            )?,
            find_destroy: req(slots.find_destroy, "find_destroy")?,
            send_create: req(slots.send_create, "send_create")?,
            send_send_video_v2: req(slots.send_send_video_v2, "send_send_video_v2")?,
            send_send_video_async_v2: req(
                slots.send_send_video_async_v2,
                "send_send_video_async_v2",
            )?,
            send_send_audio_v3: req(slots.send_send_audio_v3, "send_send_audio_v3")?,
            send_destroy: req(slots.send_destroy, "send_destroy")?,
            recv_create_v3: req(slots.recv_create_v3, "recv_create_v3")?,
            recv_capture_v3: req(slots.recv_capture_v3, "recv_capture_v3")?,
            recv_free_video_v2: req(slots.recv_free_video_v2, "recv_free_video_v2")?,
            recv_free_audio_v3: req(slots.recv_free_audio_v3, "recv_free_audio_v3")?,
            recv_destroy: req(slots.recv_destroy, "recv_destroy")?,
        })
    }
}

/// The raw `Option<fn>` slots read out of the table in one `unsafe` step, before
/// each is checked for null. Mirrors [`NdiV6`] field-for-field.
struct Slots {
    version: Option<unsafe extern "C" fn() -> *const std::os::raw::c_char>,
    initialize: Option<unsafe extern "C" fn() -> bool>,
    find_create_v2: Option<unsafe extern "C" fn(*const ffi::NDIlib_find_create_t) -> FindInstance>,
    find_get_current_sources:
        Option<unsafe extern "C" fn(FindInstance, *mut u32) -> *const ffi::NDIlib_source_t>,
    find_destroy: Option<unsafe extern "C" fn(FindInstance)>,
    send_create: Option<unsafe extern "C" fn(*const ffi::NDIlib_send_create_t) -> SendInstance>,
    send_send_video_v2:
        Option<unsafe extern "C" fn(SendInstance, *const ffi::NDIlib_video_frame_v2_t)>,
    send_send_video_async_v2:
        Option<unsafe extern "C" fn(SendInstance, *const ffi::NDIlib_video_frame_v2_t)>,
    send_send_audio_v3:
        Option<unsafe extern "C" fn(SendInstance, *const ffi::NDIlib_audio_frame_v3_t)>,
    send_destroy: Option<unsafe extern "C" fn(SendInstance)>,
    recv_create_v3:
        Option<unsafe extern "C" fn(*const ffi::NDIlib_recv_create_v3_t) -> RecvInstance>,
    recv_capture_v3: Option<
        unsafe extern "C" fn(
            RecvInstance,
            *mut ffi::NDIlib_video_frame_v2_t,
            *mut ffi::NDIlib_audio_frame_v3_t,
            *mut ffi::NDIlib_metadata_frame_t,
            u32,
        ) -> ffi::NDIlib_frame_type_e,
    >,
    recv_free_video_v2:
        Option<unsafe extern "C" fn(RecvInstance, *const ffi::NDIlib_video_frame_v2_t)>,
    recv_free_audio_v3:
        Option<unsafe extern "C" fn(RecvInstance, *const ffi::NDIlib_audio_frame_v3_t)>,
    recv_destroy: Option<unsafe extern "C" fn(RecvInstance)>,
}

/// Unwrap a resolved `Option<fn>`, mapping `None` to a typed error (no panic).
fn req<F>(slot: Option<F>, name: &'static str) -> Result<F, TableError> {
    slot.ok_or(TableError::MissingFn(name))
}

/// A required NDI function pointer was absent from the loaded runtime table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableError {
    /// The named v6 function pointer was null in the loaded runtime table.
    MissingFn(&'static str),
}

impl std::fmt::Display for TableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableError::MissingFn(name) => {
                write!(
                    f,
                    "NDI runtime table is missing the {name} function pointer"
                )
            }
        }
    }
}

impl std::error::Error for TableError {}

#[cfg(test)]
mod tests {
    use super::NdiV6;
    use crate::NdiRuntime;
    use std::ffi::CStr;

    /// Live keystone check (NDI-L1): resolve **every** `__bindgen_anon_N` slot the
    /// flat table names against the real licensed runtime. `resolve` succeeding
    /// proves all 13 indices point at populated (non-null) slots — a wrong index
    /// would land on an unpopulated slot and surface as `TableError::MissingFn`
    /// rather than silent UB. We then call `version` *through the flat struct* to
    /// confirm the resolved pointer is the real function. `#[ignore]`: needs a
    /// resolvable NDI runtime (run on the SDK-equipped box).
    ///
    /// ```text
    /// cargo test -p multiview-ndi-sys --features bindings table::tests -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a resolvable NDI runtime (libndi_advanced.so.6 / libndi.so.6)"]
    #[allow(unsafe_code)]
    fn resolve_maps_every_slot_on_hardware() {
        let runtime = NdiRuntime::load().expect("an NDI runtime should be resolvable on this host");
        let table =
            NdiV6::resolve(runtime.api_table()).expect("every required v6 slot resolves non-null");

        // SAFETY: `table.version` is the resolved `NDIlib_version` pointer, owned by
        // the still-live `runtime` (its `Library` stays mapped for this scope). Per
        // the SDK it takes no arguments, has no preconditions, and returns a pointer
        // to a process-static NUL-terminated string.
        let text = unsafe {
            CStr::from_ptr((table.version)())
                .to_str()
                .expect("the version string is valid UTF-8")
        };
        println!("NDI runtime version via the resolved NdiV6 table: {text}");
        assert!(
            text.to_ascii_uppercase().contains("NDI"),
            "the version string identifies NDI: {text}"
        );
    }
}
