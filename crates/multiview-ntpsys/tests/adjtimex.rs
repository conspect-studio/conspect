//! Tests for the `adjtimex` FFI leaf crate.
//!
//! The pure cross-check pins our re-declared `STA_NANO` constant against `libc`'s
//! (so a kernel-ABI mismatch is caught at build time, not silently misread). The
//! live read is **gated**: a synchronised host is required for a meaningful
//! `Locked` result, and the syscall may be blocked in a sandbox, so the live test
//! only asserts the call does not panic and returns a *structurally* valid
//! snapshot when it returns one — it never asserts `Locked` (CI has no NTP
//! grandmaster).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ntpsys::{read_adjtimex, STA_NANO};

#[cfg(target_os = "linux")]
#[test]
fn sta_nano_matches_libc() {
    // Our re-declared bit must equal the kernel ABI value libc carries.
    assert_eq!(STA_NANO, libc::STA_NANO);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn sta_nano_is_the_kernel_abi_value() {
    // No libc adjtimex off Linux; pin the documented value so a typo is caught.
    assert_eq!(STA_NANO, 0x2000);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn read_is_unavailable_off_linux() {
    assert!(read_adjtimex().is_none());
}

/// Live-gated: actually invoke `adjtimex` on Linux. This proves the FFI binding
/// is wired correctly (no panic, no UB) on whatever host runs it. It does NOT
/// assert the clock is synchronised — that needs a real NTP/chrony grandmaster,
/// which CI does not have. Set `MULTIVIEW_NTP_LIVE=1` to require a reading.
#[cfg(target_os = "linux")]
#[test]
fn live_read_does_not_panic_and_is_structurally_valid() {
    let reading = read_adjtimex();
    if let Some(r) = reading {
        // maxerror is never less than esterror in a sane kernel report; both are
        // non-negative once normalised to ns.
        assert!(r.est_error_ns >= 0, "esterror should be non-negative");
        assert!(r.max_error_ns >= 0, "maxerror should be non-negative");
        // The clock-state return code is a small non-negative TIME_* value.
        assert!(
            (0..=5).contains(&r.clock_state_code),
            "unexpected TIME_* code {}",
            r.clock_state_code
        );
    } else if std::env::var_os("MULTIVIEW_NTP_LIVE").is_some() {
        panic!("MULTIVIEW_NTP_LIVE set but adjtimex returned no reading");
    }
    // Otherwise (None, not gated): the syscall was blocked in this sandbox — that
    // is the honest fallback the engine relies on; the test passes.
}
