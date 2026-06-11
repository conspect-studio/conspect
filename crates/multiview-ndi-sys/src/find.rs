//! The safe NDI **finder** handle (ADR-0028 §3): discover NDI sources on the
//! network so a [`crate::recv::NdiReceiver`] can connect to one by name.
//!
//! All `unsafe` is confined here; the consuming crates stay `forbid(unsafe_code)`.
//! [`NdiFinder::current_sources`] copies each discovered source name into an owned
//! [`NdiSourceName`] **before returning**, so callers never hold a pointer into the
//! finder's internal (transient) source array.

use std::ffi::{CStr, CString};

use crate::error::NdiError;
use crate::ffi;
use crate::table::{FindInstance, NdiV6};
use crate::NdiApiTable;

/// An owned NDI source name discovered by an [`NdiFinder`].
///
/// Carries the full `"MACHINE (Source Name)"` string a receiver connects to. Owned
/// (a [`CString`]) so it stays valid after the finder moves on to its next scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdiSourceName {
    name: CString,
}

impl NdiSourceName {
    /// The source name as a UTF-8 string (lossy only if the SDK returned non-UTF-8,
    /// which real NDI names never are).
    #[must_use]
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        self.name.to_string_lossy()
    }
}

/// A safe NDI source **finder** over the resolved v6 function table.
///
/// Construct with [`NdiFinder::create`], then poll [`NdiFinder::current_sources`]
/// (the SDK discovers asynchronously, so a source may take a moment to appear).
/// The SDK handle is released on `Drop`.
pub struct NdiFinder {
    table: NdiV6,
    instance: FindInstance,
}

// SAFETY: an NDI find instance is a heap handle the SDK accesses from one thread at
// a time; transferring ownership across threads is sound and we never share it
// (`!Sync` via the raw-pointer field). The resolved fn pointers are process-lifetime.
#[allow(unsafe_code)]
unsafe impl Send for NdiFinder {}

impl NdiFinder {
    /// Create a finder. `show_local_sources` includes sources on this machine
    /// (needed to discover an in-process sender, e.g. for a loopback).
    ///
    /// # Errors
    /// [`NdiError::Table`] if a required fn pointer is missing;
    /// [`NdiError::NullInstance`] if the runtime refuses the finder.
    // reason: the FFI create call — deref the resolved table fn pointer and the SDK
    // create struct (// SAFETY below).
    #[allow(unsafe_code)]
    pub fn create(table: NdiApiTable, show_local_sources: bool) -> Result<Self, NdiError> {
        let v6 = NdiV6::resolve(table)?;
        // Discovery (mDNS browse) needs the runtime initialised.
        let _ = v6.ensure_initialized();
        let create_t = ffi::NDIlib_find_create_t {
            show_local_sources,
            p_groups: std::ptr::null(),
            p_extra_ips: std::ptr::null(),
        };
        // SAFETY: `v6.find_create_v2` is the resolved create fn pointer; `create_t`
        // is fully initialised with null (default) group/IP filters. Returns an
        // owned instance handle (or null), null-checked below.
        let instance = unsafe { (v6.find_create_v2)(std::ptr::from_ref(&create_t)) };
        if instance.is_null() {
            return Err(NdiError::NullInstance { what: "NDI finder" });
        }
        Ok(Self {
            table: v6,
            instance,
        })
    }

    /// The NDI sources currently known to the finder, each name copied into an
    /// owned [`NdiSourceName`]. Returns an empty vec if none are visible yet.
    // reason: the FFI query — deref the resolved fn pointer and read the returned
    // (finder-owned, transient) source array, copying each name out (// SAFETY below).
    #[allow(unsafe_code)]
    #[must_use]
    pub fn current_sources(&self) -> Vec<NdiSourceName> {
        let mut count: u32 = 0;
        // SAFETY: `find_get_current_sources` is the resolved query fn pointer;
        // `self.instance` is the live finder. It writes the count through `&mut count`
        // and returns a pointer to a finder-owned array of `count` `NDIlib_source_t`
        // valid until the next call on this finder or its destroy — we only read it
        // within this function and copy the names out before returning.
        let sources_ptr = unsafe {
            (self.table.find_get_current_sources)(self.instance, std::ptr::addr_of_mut!(count))
        };
        if sources_ptr.is_null() || count == 0 {
            return Vec::new();
        }
        let mut names = Vec::new();
        for i in 0..count {
            let Ok(idx) = usize::try_from(i) else { break };
            // SAFETY: `idx < count` and the array has `count` elements, so `add(idx)`
            // stays in bounds. Each `p_ndi_name` is a NUL-terminated SDK string valid
            // for this scope; `CStr::from_ptr` + `to_owned` copies it into an owned
            // `CString` before we return (never a pointer into the transient array).
            let name = unsafe {
                let src = &*sources_ptr.add(idx);
                if src.p_ndi_name.is_null() {
                    continue;
                }
                CStr::from_ptr(src.p_ndi_name).to_owned()
            };
            names.push(NdiSourceName { name });
        }
        names
    }
}

impl std::fmt::Debug for NdiFinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NdiFinder")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl Drop for NdiFinder {
    // reason: the FFI destroy call — release the SDK handle exactly once.
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.instance` is the live finder from `create`, destroyed exactly
        // once here. Host-side teardown only.
        unsafe {
            (self.table.find_destroy)(self.instance);
        }
    }
}
