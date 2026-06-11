//! Minimal safe wrapper over the Linux **i915 PMU per-engine busy-ns counter**.
//!
//! On Intel GPUs the per-engine encoder/decoder utilisation that NVML exposes
//! directly (and that DRM `fdinfo` exposes only per-process) is published by the
//! kernel as a **whole-device** counter through the i915 *performance monitoring
//! unit* (PMU): `perf_event_open(2)` on the i915 PMU `type` with an engine-class
//! `config` returns a file descriptor whose `read(2)` yields a monotonically
//! increasing **busy-nanoseconds** total for that engine class
//! ([gpu-monitoring §1, §2.1](../../../docs/research/gpu-monitoring-and-scheduling.md),
//! [I1][I2]). A busy *fraction* is the delta of two reads over the wall interval
//! between them — exactly the two-snapshot diff the AMD fdinfo path uses.
//!
//! `perf_event_open` is a raw syscall that needs `unsafe`, and `libc` 0.2 does
//! not carry a `perf_event_attr` struct, so this tiny leaf crate declares the
//! minimal `#[repr(C)]` attribute layout itself and owns the counter lifecycle
//! behind a safe, `Option`-returning API. The three `unsafe` blocks are exactly
//! that lifecycle and nothing more — the `perf_event_open` syscall the crate
//! exists to isolate, the counter `read(2)`, and the fd `close(2)` on `Drop` —
//! each with its own `// SAFETY:` justification. It is the only place in the
//! workspace — besides the libav-owning `multiview-ffmpeg` and the
//! `adjtimex`-owning `multiview-ntpsys` — that relaxes `unsafe_code` from
//! `forbid` to `deny`. `multiview-hal` stays `forbid(unsafe_code)` and consumes
//! the safe snapshot.
//!
//! ## Pure, always-compiled core
//!
//! The engine-class config encoding ([`engine_busy_config`]) and the
//! busy-ns → fraction two-snapshot diff ([`busy_fraction`]) are pure, have no
//! native deps, and are unit-tested on fixture counters on **every** target. The
//! live counter ([`I915PmuCounter`]) is `cfg(target_os = "linux")`.
//!
//! ## Live-gated
//!
//! Opening a counter needs a real i915 device **and** `perf_event_open`
//! permission (`CAP_PERFMON` / a low enough `perf_event_paranoid`); a CI
//! container typically lacks both. So [`I915PmuCounter::open`] returns `None` on
//! any error — non-Linux target, no i915 PMU, `EACCES`/`EPERM`, or a short read —
//! never a panic and never a block. The live `perf_event_open` test is gated; the
//! pure encoding and diff are tested everywhere.
//!
//! ## Non-Linux
//!
//! On any non-Linux target the crate still compiles; [`I915PmuCounter::open`]
//! returns `None` (the probe then leaves the Intel per-engine fields unknown).

/// i915 engine class for the **Video** engine (VCS) — the bitstream
/// encode/decode block. Mirrors `I915_ENGINE_CLASS_VIDEO` in
/// `<drm/i915_drm.h>`; verified against the kernel ABI value in this crate's
/// tests.
pub const ENGINE_CLASS_VIDEO: u8 = 2;

/// i915 engine class for the **Video Enhance** engine (VECS) — the
/// scaling/format/colour-conversion block used alongside decode. Mirrors
/// `I915_ENGINE_CLASS_VIDEO_ENHANCE`.
pub const ENGINE_CLASS_VIDEO_ENHANCE: u8 = 3;

/// i915 engine class for the **Render/3D** engine (RCS) — compositor pressure.
/// Mirrors `I915_ENGINE_CLASS_RENDER`.
pub const ENGINE_CLASS_RENDER: u8 = 0;

/// `I915_SAMPLE_BUSY`: the per-engine *busy-ns* sample type. Bits `0..4` of the
/// PMU `config`. Mirrors `enum drm_i915_pmu_engine_sample`.
const SAMPLE_BUSY: u64 = 0;

/// `I915_PMU_SAMPLE_BITS` (4) — the sample type occupies the low 4 bits.
const SAMPLE_BITS: u64 = 4;

/// `I915_PMU_CLASS_SHIFT` = `SAMPLE_BITS + I915_PMU_SAMPLE_INSTANCE_BITS`
/// (`4 + 8 = 12`) — the engine class is shifted up past the 8-bit instance.
const CLASS_SHIFT: u64 = 12;

/// Encode an i915 PMU `config` for the **busy-ns** counter of one engine
/// `(class, instance)`.
///
/// This is the `I915_PMU_ENGINE_BUSY(class, instance)` macro from
/// `<drm/i915_drm.h>`:
/// `(class << 12) | (instance << 4) | I915_SAMPLE_BUSY`. The result is the
/// `perf_event_attr.config` passed to `perf_event_open` on the i915 PMU `type`.
///
/// Pure and total — no syscall, no allocation. Unit-tested against the kernel
/// macro's expansion for the video engines.
#[must_use]
pub fn engine_busy_config(class: u8, instance: u8) -> u64 {
    (u64::from(class) << CLASS_SHIFT) | (u64::from(instance) << SAMPLE_BITS) | SAMPLE_BUSY
}

/// Derive an engine busy **fraction** (`0.0..=1.0`) from two i915 PMU busy-ns
/// reads `interval_ns` wall-nanoseconds apart.
///
/// `(later_ns - earlier_ns) / interval_ns`, clamped to `0.0..=1.0`. Returns
/// `None` when the interval is non-positive (a divide guard) or when the counter
/// went backwards (a reset/wrap — unknown this tick, never a fabricated zero).
/// Mirrors the DRM-fdinfo two-snapshot diff in `multiview-hal`'s sysfs path so
/// the Intel PMU term folds into the same `enc/dec_util_frac`. Never panics.
#[must_use]
pub fn busy_fraction(earlier_ns: u64, later_ns: u64, interval_ns: u64) -> Option<f32> {
    if interval_ns == 0 {
        return None;
    }
    let delta = later_ns.checked_sub(earlier_ns)?;
    Some(ratio_to_unit_f32(delta, interval_ns))
}

/// `numerator / denominator` as an `f32` clamped to `0.0..=1.0`, via a lossless
/// `u64 -> f64` widening (no `as` casts — busy-ns and interval-ns are both well
/// below `2^53` for any sane poll cadence). `denominator` is `> 0` by the
/// caller's guard.
fn ratio_to_unit_f32(numerator: u64, denominator: u64) -> f32 {
    let numerator = u64_to_f64(numerator);
    let denominator = u64_to_f64(denominator);
    let frac = (numerator / denominator).clamp(0.0, 1.0);
    f64_unit_to_f32(frac)
}

/// Lossless `u64 -> f64` widening for ns counts (`< 2^53`), avoiding `as`.
fn u64_to_f64(value: u64) -> f64 {
    u32::try_from(value).map_or_else(
        |_| {
            let high = u32::try_from((value >> 32) & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            let low = u32::try_from(value & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            high * 4_294_967_296.0 + low
        },
        f64::from,
    )
}

/// Narrow an `f64` already in `0.0..=1.0` to `f32` without an `as` cast, by
/// mapping onto the exact `2^24` integer grid (f32's integer-exactness bound)
/// and recovering it through `TryFrom`/`f32::from`. Carries full f32 precision
/// for a unit fraction.
fn f64_unit_to_f32(value: f64) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let scaled = (value * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return 1.0;
    }
    // 0 < scaled < 2^24 and integer-valued (`.round()`), so the f64 -> integer
    // conversion is exact and in range via the bit-reading integer path.
    let ticks = u32::try_from(f64_trunc_to_u64(scaled)).unwrap_or(0);
    f32_from_u24(ticks) / 16_777_216.0_f32
}

/// Truncate a finite, non-negative `f64` to its integer part as a `u64`, reading
/// the IEEE-754 fields (exact for any integer below `2^53`; our domain is
/// `< 2^24`). Avoids an `as` cast.
fn f64_trunc_to_u64(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
    if exponent >= 52 {
        significand << (exponent - 52)
    } else {
        significand >> (52 - exponent)
    }
}

/// Exact `u32 -> f32` for values `<= 2^24` (the unit-fraction grid), composing
/// two `u16` halves each lossless via `f32::from`. Avoids an `as` cast.
fn f32_from_u24(value: u32) -> f32 {
    let high = u16::try_from((value >> 16) & 0xFFFF).map_or(f32::INFINITY, f32::from);
    let low = u16::try_from(value & 0xFFFF).map_or(f32::INFINITY, f32::from);
    high * 65_536.0_f32 + low
}

#[cfg(target_os = "linux")]
pub use self::linux::{read_pmu_type, I915PmuCounter, I915_PMU_SYSFS_GLOB};

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    /// The sysfs glob under which the kernel publishes each i915 PMU's dynamic
    /// `type` number (one `i915_<pci-bus-id>` directory per GPU, e.g.
    /// `/sys/devices/i915_0000_00_02.0/type`; a single-GPU box may also expose a
    /// plain `/sys/devices/i915/type`). The caller resolves the concrete path
    /// for a device and passes it to [`read_pmu_type`].
    pub const I915_PMU_SYSFS_GLOB: &str = "/sys/devices/i915*/type";

    /// Read the dynamic i915 PMU `type` number from a sysfs `type` file.
    ///
    /// The kernel assigns the i915 PMU a *dynamic* perf type id (it is not a
    /// fixed `PERF_TYPE_*`); it is published as a decimal integer in
    /// `/sys/devices/i915<...>/type`. Returns `None` if the file is absent or its
    /// contents are not a `u32` — graceful absence, never a panic. Plain
    /// `std::fs`; no `unsafe`.
    #[must_use]
    pub fn read_pmu_type(type_path: &Path) -> Option<u32> {
        let raw = std::fs::read_to_string(type_path).ok()?;
        raw.trim().parse().ok()
    }

    /// The minimal `perf_event_attr` layout this crate sets, matching the kernel
    /// ABI prefix. `libc` 0.2 does not carry this struct, so we declare exactly
    /// the fields we use and zero the rest; the kernel reads `size` to know how
    /// much of the (versioned, growable) struct we provided, so a zeroed tail is
    /// the documented backward-compatible request.
    ///
    /// Field order/types follow `<linux/perf_event.h>` `struct perf_event_attr`
    /// up through the `bp_len`/`config2` union — exactly the `PERF_ATTR_SIZE_VER1`
    /// (72-byte) ABI prefix. Every field the i915 busy counter does not use
    /// (`sample_*`, `read_format`, the `disabled/inherit/…` flags bitfield,
    /// `wakeup`, `bp_type`, `config1`/`config2`) is declared explicitly and left
    /// zero; the kernel reads `size` to know how much of the (versioned, growable)
    /// struct we provided, so a fully-zeroed tail past `config` is the documented
    /// backward-compatible request and `size = size_of::<Self>()` is honest.
    #[repr(C)]
    struct PerfEventAttr {
        /// `PERF_TYPE_*` or, for the i915 PMU, the dynamic `type` from sysfs.
        type_: u32,
        /// Size of this struct as the kernel ABI version marker.
        size: u32,
        /// The engine-class busy-ns `config` (`engine_busy_config`).
        config: u64,
        /// `sample_period`/`sample_freq` union — unused (0) for a counting read.
        sample_period_or_freq: u64,
        /// `sample_type` — unused (0); we only `read()` the running count.
        sample_type: u64,
        /// `read_format` — unused (0); a bare `u64` count is returned.
        read_format: u64,
        /// The packed `disabled/inherit/exclude_*/…` bitfield flags as one
        /// `u64`. We leave it 0: counting from open, no exclusion filters.
        flags: u64,
        /// `wakeup_events`/`wakeup_watermark` union — unused (0).
        wakeup: u32,
        /// `bp_type` — unused (0).
        bp_type: u32,
        /// `bp_addr`/`config1` union — unused (0) for the i915 busy counter.
        config1: u64,
        /// `bp_len`/`config2` union — unused (0).
        config2: u64,
    }

    /// An open i915 PMU per-engine busy-ns counter file descriptor.
    ///
    /// Construct with [`I915PmuCounter::open`] (returns `None` on any error);
    /// read the running busy-ns with [`I915PmuCounter::read_busy_ns`]. The fd is
    /// closed on `Drop`. This is the **only** type in the crate that performs the
    /// `unsafe` syscall, behind the safe `Option`-returning API the probe uses.
    #[derive(Debug)]
    pub struct I915PmuCounter {
        fd: libc::c_int,
    }

    impl I915PmuCounter {
        /// Open a busy-ns counter for engine `config` (from
        /// [`super::engine_busy_config`]) on the i915 PMU `pmu_type` (from
        /// [`read_pmu_type`]).
        ///
        /// Returns `None` on any failure — `perf_event_open` denied
        /// (`EACCES`/`EPERM`, the common CI/container case), no such PMU, or an
        /// out-of-range fd — never a panic and never a block. The opened event is
        /// *not* attached to a task (`pid == -1`) and covers all CPUs
        /// (`cpu == 0`, the kernel reports the whole device for the i915 PMU),
        /// with no group leader and no flags.
        #[must_use]
        // reason: the `perf_event_open` syscall this crate exists to isolate — a
        // single FFI call behind this safe `Option`-returning API so
        // `multiview-hal` stays `forbid(unsafe_code)`.
        #[allow(unsafe_code)]
        pub fn open(pmu_type: u32, config: u64) -> Option<Self> {
            // A zeroed attr with `size` set to its own size is the documented
            // backward/forward-compatible request: we set only `type`, `size`,
            // and `config`; every other field stays 0 (count from open, no
            // sampling, no exclusion).
            let size = u32::try_from(std::mem::size_of::<PerfEventAttr>()).ok()?;
            let mut attr: PerfEventAttr = PerfEventAttr {
                type_: pmu_type,
                size,
                config,
                sample_period_or_freq: 0,
                sample_type: 0,
                read_format: 0,
                flags: 0,
                wakeup: 0,
                bp_type: 0,
                config1: 0,
                config2: 0,
            };

            // SAFETY: `perf_event_open(attr, pid, cpu, group_fd, flags)` reads
            // `attr.size` bytes from the `*mut PerfEventAttr` we pass (a live,
            // properly-aligned, fully-initialised stack allocation whose `size`
            // field equals its real byte length) and writes nothing back through
            // it. `pid == -1` + `cpu == 0` is a valid system-wide-on-CPU-0
            // request (the i915 PMU reports the whole device); `group_fd == -1`
            // means no group leader; `flags == 0`. The pointer is used only for
            // the duration of the call. On error the kernel returns a negative
            // value and sets errno; we never dereference a returned fd as a
            // pointer. No memory is shared, retained, or freed across the call.
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_perf_event_open,
                    std::ptr::addr_of_mut!(attr),
                    -1i32, // pid: not attached to a task
                    0i32,  // cpu: the i915 PMU reports the whole device on cpu 0
                    -1i32, // group_fd: no group leader
                    0u64,  // flags
                )
            };
            if raw < 0 {
                // Denied (EACCES/EPERM in a sandbox), no such PMU, or other
                // error: no usable counter. Honest unknown, never a panic.
                return None;
            }
            let fd = libc::c_int::try_from(raw).ok()?;
            Some(Self { fd })
        }

        /// Read the current running busy-nanoseconds total for this engine.
        ///
        /// The i915 PMU busy counter is a monotonically increasing `u64` of
        /// nanoseconds the engine was busy. Returns `None` if the `read(2)` fails
        /// or returns fewer than 8 bytes — never a panic. Caller differences two
        /// reads a known wall interval apart via [`super::busy_fraction`].
        #[must_use]
        // reason: the crate's counter read — a single `read(2)` on the owned
        // perf fd into a fixed 8-byte buffer, behind this safe API.
        #[allow(unsafe_code)]
        pub fn read_busy_ns(&self) -> Option<u64> {
            let mut buf = [0u8; 8];
            // SAFETY: `read(fd, buf, len)` writes at most `len` bytes into `buf`,
            // a live 8-byte stack array we own; `self.fd` is a valid perf-event
            // fd owned by this `I915PmuCounter` for the call's duration. We act
            // on the result only after the call returns and read no more bytes
            // than the kernel reported. No memory is shared or freed.
            let got =
                unsafe { libc::read(self.fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
            // A perf counter read returns exactly the 8-byte `u64` count; a short
            // or failed read is unusable this tick.
            if got != 8 {
                return None;
            }
            Some(u64::from_ne_bytes(buf))
        }
    }

    impl Drop for I915PmuCounter {
        // reason: release the owned perf-event fd; closing is the counterpart of
        // the `perf_event_open` this crate isolates.
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: `self.fd` is a valid fd this `I915PmuCounter` opened and has
            // exclusive ownership of; `Drop` runs once. `close(2)`'s result is
            // intentionally ignored (there is no recovery for a failing close of
            // an fd we are discarding). No memory is touched.
            let _ = unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
/// On non-Linux targets there is no i915 PMU; opening a counter is always
/// unavailable. This stub keeps the crate compiling everywhere while the live
/// path is Linux-only. It mirrors the Linux type's `Debug` so downstream
/// `#[derive(Debug)]` holders compile identically on every target.
#[derive(Debug)]
pub struct I915PmuCounter {
    _private: (),
}

#[cfg(not(target_os = "linux"))]
impl I915PmuCounter {
    /// No i915 PMU off Linux — always [`None`].
    #[must_use]
    pub fn open(_pmu_type: u32, _config: u64) -> Option<Self> {
        None
    }

    /// No counter off Linux — always [`None`].
    #[must_use]
    pub fn read_busy_ns(&self) -> Option<u64> {
        None
    }
}
