//! Minimal safe wrapper over the Linux `adjtimex(2)` **clock-discipline read**.
//!
//! The wall-clock reference badge needs to know whether the host's system clock
//! is actually disciplined by NTP/chrony and how well it is locked. Linux exposes
//! that through `adjtimex(2)`: with `modes == 0` it is a **read-only** query that
//! returns the kernel's clock-discipline state (the `STA_*` status bits, the
//! `TIME_*` clock state as the return code, and the estimated / maximum error).
//!
//! There is no safe-wrapper crate that exposes `adjtimex` (neither `rustix` nor
//! the workspace's other deps), so this tiny leaf crate owns the **single**
//! `unsafe` FFI call behind a safe, `Option`-returning API. It is the only place
//! in the workspace — besides the libav-owning `multiview-ffmpeg` — that relaxes
//! `unsafe_code` from `forbid` to `deny`; the one `unsafe` block carries a
//! `// SAFETY:` justification. `multiview-engine` stays `forbid(unsafe_code)` and
//! consumes the safe snapshot.
//!
//! ## Live-gated
//!
//! The *Locked* result is meaningful only on a host actually synchronised by
//! `ntpd`/`chrony`; a CI container typically returns `STA_UNSYNC` / `TIME_ERROR`.
//! So [`read_adjtimex`] is exercised live only where a real synchronised host
//! exists; everywhere else (and on non-Linux) it is a compile-checked `None`. The
//! *classification* of a snapshot is pure and tested in `multiview-engine::sysref`.
//!
//! ## Non-Linux
//!
//! On any non-Linux target the crate still compiles; [`read_adjtimex`] returns
//! `None` (the engine then falls back to its configured assumed status).

/// A normalised, all-nanoseconds snapshot of the kernel NTP clock-discipline
/// state read from `adjtimex(2)`.
///
/// `*_ns` fields are nanoseconds: `est_error_ns` / `max_error_ns` come from the
/// kernel's microsecond `esterror` / `maxerror` (always microseconds), and
/// `offset_ns` is nanoseconds when `STA_NANO` is set in `status_bits`, otherwise
/// converted from the kernel's microsecond `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawNtpStatus {
    /// The raw `timex.status` bit-field (`STA_*`).
    pub status_bits: i32,
    /// The `adjtimex` return code (`TIME_*`): the kernel clock-discipline state.
    pub clock_state_code: i32,
    /// The kernel's estimated error, in nanoseconds (from `esterror`, us → ns).
    pub est_error_ns: i64,
    /// The kernel's maximum error, in nanoseconds (from `maxerror`, us → ns).
    pub max_error_ns: i64,
    /// The current clock offset estimate, in nanoseconds (`offset`, honouring
    /// `STA_NANO`).
    pub offset_ns: i64,
}

/// `STA_NANO`: the `offset` field is in nanoseconds (vs microseconds). Mirrors
/// `<linux/timex.h>`; verified against `libc::STA_NANO` in this crate's tests.
pub const STA_NANO: i32 = 0x2000;

/// Read the kernel NTP clock-discipline state via a **read-only** `adjtimex`
/// query, returning a normalised snapshot, or `None` if the read is unavailable
/// (non-Linux target, or the syscall reported an error / undefined state).
///
/// The call uses `modes == 0`: it adjusts nothing, only queries — safe to call
/// repeatedly off the hot path.
#[cfg(target_os = "linux")]
#[must_use]
// reason: this is the crate's sole `unsafe` — a single read-only `adjtimex`
// syscall behind this safe `Option`-returning API. It is exactly the FFI the
// crate exists to isolate so `multiview-engine` stays `forbid(unsafe_code)`.
#[allow(unsafe_code)]
pub fn read_adjtimex() -> Option<RawNtpStatus> {
    // `adjtimex` with a zeroed `timex` (modes == 0) is a pure read. We zero the
    // struct and let the kernel fill it.
    //
    // SAFETY: `buf` is a live, properly-aligned, zero-initialised `libc::timex`
    // owned on this stack frame for the whole call; `adjtimex` only reads `modes`
    // (which we set to 0 — no adjustment) and writes the other fields. We pass a
    // valid `*mut` to that single allocation and use the result only after the
    // call returns. No memory is shared, retained, or freed across the boundary.
    let (ret, buf) = unsafe {
        let mut buf: libc::timex = std::mem::zeroed();
        let ret = libc::adjtimex(std::ptr::addr_of_mut!(buf));
        (ret, buf)
    };
    if ret < 0 {
        // Syscall error (e.g. EPERM in a restricted sandbox): no usable reading.
        return None;
    }

    // `timex.status` is `c_int` (always `i32`); take it as-is.
    let status_bits = buf.status;
    // `adjtimex` returns the TIME_* clock state as a small non-negative code.
    let clock_state_code = ret;

    // esterror / maxerror are documented as microseconds regardless of STA_NANO.
    let est_error_ns = micros_to_nanos(clong_to_i64(buf.esterror));
    let max_error_ns = micros_to_nanos(clong_to_i64(buf.maxerror));

    // offset is nanoseconds iff STA_NANO is set, otherwise microseconds.
    let raw_offset = clong_to_i64(buf.offset);
    let offset_ns = if status_bits & STA_NANO != 0 {
        raw_offset
    } else {
        micros_to_nanos(raw_offset)
    };

    Some(RawNtpStatus {
        status_bits,
        clock_state_code,
        est_error_ns,
        max_error_ns,
        offset_ns,
    })
}

/// On non-Linux targets there is no `adjtimex`; the read is always unavailable.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn read_adjtimex() -> Option<RawNtpStatus> {
    None
}

/// A kernel `c_long` time field as `i64`. Multiview targets only 64-bit Linux
/// (`x86_64` + `aarch64` — see CLAUDE.md "Platforms"), where `c_long` *is* `i64`, so
/// this is the identity (a `static_assertions`-style guard pins that: the cast in
/// the assignment compiles only while `c_long == i64`). No `as` cast, no
/// fallibility.
#[cfg(target_os = "linux")]
const fn clong_to_i64(value: libc::c_long) -> i64 {
    // `c_long` is `i64` on every 64-bit Linux target; this binding is the identity
    // and fails to compile if that ever stops holding (a compile-time tripwire).
    value
}

/// Convert microseconds to nanoseconds, saturating rather than overflowing on an
/// implausibly large error estimate.
#[cfg(target_os = "linux")]
const fn micros_to_nanos(micros: i64) -> i64 {
    micros.saturating_mul(1_000)
}
