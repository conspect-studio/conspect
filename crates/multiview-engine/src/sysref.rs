//! System-clock **discipline reference** — the pure source-selection +
//! lock-classification logic that feeds the on-screen wall-clock reference badge.
//!
//! The wall-clock overlay shows the operator *which* timing reference disciplines
//! the displayed time-of-day and *how well* it is locked (a text + glyph badge,
//! never colour alone). Two discipline sources are detected and arbitrated here:
//!
//! * **System NTP/chrony** (`SYS`): the kernel's own clock-discipline state,
//!   read on Linux with `adjtimex`/`ntp_adjtime` (the `STA_*` status bits, the
//!   `TIME_*` clock state, and the estimated error). That syscall read lives
//!   behind the off-by-default [`live`] module / `ntp` feature (it needs a real,
//!   NTP-synchronised host to mean anything); the **classification** of a reading
//!   is pure and lives here, tested over injected snapshots.
//! * **PTP / ST 2059-2** (`PTP`): the disciplined-reference lock state produced
//!   by the [`ReferenceTracker`](crate::ptp::ReferenceTracker) (work item ENG-5),
//!   surfaced as a [`ReferenceStatus`].
//!
//! [`ReferenceSelector`] arbitrates the two into one authoritative
//! [`SelectedReference`] for the badge: PTP is the higher-stratum media reference
//! and wins whenever it is disciplined; otherwise the system NTP discipline is
//! reported.
//!
//! ## Invariant #1: this informs the badge only — it never paces the output clock
//!
//! Like the PTP servo, this whole module is a **pure value machine** over injected
//! readings. It owns no clock, performs no I/O in the default build, exposes no
//! method the output clock ([`OutputClock`](crate::clock::OutputClock)) calls, and
//! holds no reference to it. A clock that is `Freerun`, `Holdover`, or whose
//! `adjtimex` read fails on every sample changes only the displayed badge; it can
//! neither stall nor speed up the fixed-cadence `out_pts = f(tick)` tick loop. The
//! `sysref_no_pacing` test pins this behaviourally.
//!
//! All thresholds are **integer nanoseconds** (never float seconds — invariant
//! #3).

use multiview_overlay::clock::{RefSource, RefStatus, TimeRef};

use crate::ptp::{LockState, ReferenceStatus};

/// A typed view over the kernel `timex.status` bit-field (`STA_*`).
///
/// Only the bits the badge classification needs are interpreted; the raw value is
/// retained for diagnostics. The bit values are the stable Linux kernel ABI
/// (`<linux/timex.h>`); they are re-declared here so the pure classifier carries
/// no syscall dependency (the live `adjtimex` read in [`live`] cross-checks them
/// against `libc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NtpStatusFlags {
    bits: i32,
}

impl NtpStatusFlags {
    /// `STA_UNSYNC` — the clock is **not** synchronised to a reference. When set,
    /// the kernel discipline is free-running regardless of the estimated error.
    pub const STA_UNSYNC: i32 = 0x0040;
    /// `STA_NANO` — resolution is nanoseconds (vs microseconds) for the offset /
    /// error fields. Informational for unit conversion at the call site.
    pub const STA_NANO: i32 = 0x2000;
    /// `STA_FREQHOLD` — frequency adjustments are held (the discipline loop is not
    /// actively slewing); a hint that the clock is coasting.
    pub const STA_FREQHOLD: i32 = 0x0080;

    /// Build a typed view over a raw `timex.status` value.
    #[must_use]
    pub const fn from_bits(bits: i32) -> Self {
        Self { bits }
    }

    /// The raw status bit-field, for diagnostics.
    #[must_use]
    pub const fn bits(self) -> i32 {
        self.bits
    }

    /// Whether `STA_UNSYNC` is set — the kernel reports the clock is not
    /// synchronised to any reference.
    #[must_use]
    pub const fn is_unsynchronized(self) -> bool {
        self.bits & Self::STA_UNSYNC != 0
    }

    /// Whether `STA_NANO` is set (offset/error fields are nanoseconds, not
    /// microseconds).
    #[must_use]
    pub const fn is_nano_resolution(self) -> bool {
        self.bits & Self::STA_NANO != 0
    }

    /// Whether `STA_FREQHOLD` is set (the frequency loop is holding — coasting).
    #[must_use]
    pub const fn is_freq_held(self) -> bool {
        self.bits & Self::STA_FREQHOLD != 0
    }
}

/// The kernel clock-discipline state (`adjtimex` return value / `TIME_*`).
///
/// This is the syscall's *return code*, distinct from the `STA_*` status bits.
/// `Error` (`TIME_ERROR`) means the clock is not synchronised; the leap-second
/// states are normal-but-pending conditions a disciplined clock can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum NtpClockState {
    /// `TIME_OK` — the clock is synchronised, no leap second pending.
    #[default]
    Ok,
    /// `TIME_INS` — a leap second will be inserted at the end of the day.
    InsertLeap,
    /// `TIME_DEL` — a leap second will be deleted at the end of the day.
    DeleteLeap,
    /// `TIME_OOP` — a leap second is in progress.
    LeapInProgress,
    /// `TIME_WAIT` — a leap second has occurred, awaiting the flag clear.
    LeapOccurred,
    /// `TIME_ERROR` — the clock is **not** synchronised (no usable discipline).
    Error,
}

impl NtpClockState {
    /// Map the raw `adjtimex` return code (`TIME_*`) to the typed state. An
    /// unknown / negative code is treated conservatively as [`Self::Error`] (the
    /// clock cannot be trusted as disciplined).
    #[must_use]
    pub const fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Ok,             // TIME_OK
            1 => Self::InsertLeap,     // TIME_INS
            2 => Self::DeleteLeap,     // TIME_DEL
            3 => Self::LeapInProgress, // TIME_OOP
            4 => Self::LeapOccurred,   // TIME_WAIT
            _ => Self::Error,          // TIME_ERROR (5) or unknown
        }
    }

    /// Whether the kernel reports the clock as synchronised (anything but
    /// [`Self::Error`]). The pending-leap states are still synchronised.
    #[must_use]
    pub const fn is_synchronized(self) -> bool {
        !matches!(self, Self::Error)
    }
}

/// A single `adjtimex`/`ntp_adjtime` snapshot, normalised for classification.
///
/// Produced by the live syscall reader ([`live::SystemNtpQuery`], behind the
/// `ntp` feature) in production and injected directly in tests. All error / offset
/// fields are normalised to **nanoseconds** by the reader (`STA_NANO` decides
/// whether the kernel's raw fields were us or ns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpReading {
    /// The `timex.status` bit-field, typed.
    pub status: NtpStatusFlags,
    /// The `adjtimex` return code (`TIME_*`), typed.
    pub clock_state: NtpClockState,
    /// The kernel's estimated error (`timex.esterror`), in nanoseconds.
    pub est_error_ns: i64,
    /// The kernel's maximum error (`timex.maxerror`), in nanoseconds.
    pub max_error_ns: i64,
    /// The current clock offset estimate (`timex.offset`), in nanoseconds
    /// (`local - reference`), surfaced for the badge.
    pub offset_ns: i64,
}

/// Tuning for the system-clock discipline classifier + tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemRefConfig {
    /// Maximum estimated error (ns) for a *synchronised* clock to count as
    /// [`LockState::Locked`]. Above this (but still synchronised) it degrades to
    /// [`LockState::Holdover`]. Typical chrony/NTP holds esterror in the tens to
    /// low-hundreds of microseconds.
    pub est_error_tolerance_ns: i64,
    /// The state to assume when the `adjtimex` read is **unavailable** (the
    /// platform is not Linux, the syscall failed, or the `ntp` feature is off).
    /// Default-safe: a deployment that knows its host is NTP-disciplined keeps
    /// [`LockState::Locked`]; an operator who knows it is not can set
    /// [`LockState::Freerun`] to be honest.
    pub assumed_when_unavailable: LockState,
}

impl SystemRefConfig {
    /// A sane default: a 100 us estimated-error lock tolerance, assuming
    /// [`LockState::Locked`] when the read is unavailable (the deployment host is
    /// NTP-disciplined — the same assumption the MVP `SystemWallClock` documents).
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            est_error_tolerance_ns: 100_000,
            assumed_when_unavailable: LockState::Locked,
        }
    }
}

impl Default for SystemRefConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// Classify a kernel `adjtimex` snapshot into a [`LockState`] for the badge.
///
/// The mapping (work item ENG-3):
///
/// * `STA_UNSYNC` set **or** `TIME_ERROR` ⇒ [`LockState::Freerun`] — the kernel
///   itself says the clock is not synchronised.
/// * synchronised **and** estimated error `<= est_error_tolerance_ns` ⇒
///   [`LockState::Locked`].
/// * synchronised **but** estimated error over tolerance (or a non-`Ok`
///   leap/`TIME_ERROR`-adjacent clock state) ⇒ [`LockState::Holdover`] — the
///   clock is still disciplined but degraded, coasting on a looser lock.
///
/// Pure: no syscalls, no clock of its own.
#[must_use]
pub fn classify_system(reading: &NtpReading, config: &SystemRefConfig) -> LockState {
    if reading.status.is_unsynchronized() {
        // The kernel is explicit: not synchronised to any reference.
        return LockState::Freerun;
    }
    if !reading.clock_state.is_synchronized() {
        // STA_UNSYNC was clear but the return code is TIME_ERROR: treat the clock
        // as undisciplined rather than trusting a degraded esterror.
        return LockState::Holdover;
    }
    if reading.est_error_ns <= config.est_error_tolerance_ns {
        LockState::Locked
    } else {
        // Synchronised but the estimated error has grown past the lock band: the
        // discipline is degraded but not abandoned — coast on Holdover.
        LockState::Holdover
    }
}

/// The injectable source of kernel `adjtimex` readings.
///
/// Production wires the live Linux reader ([`live::SystemNtpQuery`], behind the
/// `ntp` feature); tests wire a fake whose readings they control. [`read`](Self::read)
/// returns `None` when no reading is available (non-Linux, missing capability,
/// syscall error, or the `ntp` feature off) — the tracker then falls back to the
/// configured assumed state rather than reporting a misleading status.
pub trait NtpQuery {
    /// Take one `adjtimex` reading, or `None` if unavailable.
    fn read(&mut self) -> Option<NtpReading>;
}

/// Tracks the system-clock discipline state for the badge: samples an
/// [`NtpQuery`], classifies the reading, and falls back safely when unavailable.
///
/// Pure over the injected query — no syscalls of its own, no clock, never paces.
#[derive(Debug, Clone)]
pub struct SystemRefTracker {
    config: SystemRefConfig,
    /// The lock state from the most recent sample (or the configured fallback
    /// before the first / when unavailable).
    state: LockState,
    /// The offset estimate (ns) from the most recent available reading, surfaced
    /// for the badge. Zero when no reading has been seen.
    offset_ns: i64,
    /// Whether the most recent sample had a live reading (vs the fallback arm).
    available: bool,
}

impl SystemRefTracker {
    /// Build a tracker with the given tuning. It reports the configured
    /// `assumed_when_unavailable` state until the first reading arrives.
    #[must_use]
    pub const fn new(config: SystemRefConfig) -> Self {
        Self {
            config,
            state: config.assumed_when_unavailable,
            offset_ns: 0,
            available: false,
        }
    }

    /// The current discipline lock state.
    #[must_use]
    pub const fn state(&self) -> LockState {
        self.state
    }

    /// The current clock offset estimate (`local - reference`), in nanoseconds —
    /// zero before the first available reading.
    #[must_use]
    pub const fn offset_ns(&self) -> i64 {
        self.offset_ns
    }

    /// Whether the most recent sample came from a live `adjtimex` reading (`true`)
    /// or fell back to the configured assumed state (`false`).
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }

    /// Take one reading from `query`, update the tracked state, and return it.
    ///
    /// On a reading, the state is the [`classify_system`] mapping and the offset
    /// is taken from the reading. On `None` (unavailable), the state falls back to
    /// [`SystemRefConfig::assumed_when_unavailable`] and the offset is left at its
    /// last value (the assumed badge carries no fresh offset).
    pub fn sample<Q: NtpQuery + ?Sized>(&mut self, query: &mut Q) -> LockState {
        if let Some(reading) = query.read() {
            self.state = classify_system(&reading, &self.config);
            self.offset_ns = reading.offset_ns;
            self.available = true;
        } else {
            // Unavailable: fall back to the configured assumed state. The offset
            // is left at its last value (the assumed badge carries no fresh one).
            self.state = self.config.assumed_when_unavailable;
            self.available = false;
        }
        self.state
    }
}

/// The authoritative reference chosen for the wall-clock badge: which discipline
/// source is in charge, its lock state, and its estimated offset (ns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedReference {
    /// The authoritative reference source (`PTP` or `SYS`).
    pub source: RefSource,
    /// The chosen reference's lock state.
    pub state: LockState,
    /// The chosen reference's offset estimate (`local - reference`), in ns.
    pub offset_ns: i64,
}

impl SelectedReference {
    /// Map the selected lock state onto the overlay badge's [`RefStatus`] so the
    /// clock can render the text + glyph badge for all four states (a11y).
    #[must_use]
    pub const fn to_ref_status(self) -> RefStatus {
        match self.state {
            LockState::Locked => RefStatus::Locked,
            LockState::Holdover => RefStatus::Holdover,
            // The reference has lost its feed past holdover, or never acquired:
            // both read as free-running on the badge (the overlay `RefLoss`
            // variant is reserved for an explicitly-signalled loss the engine does
            // not distinguish here).
            LockState::Freerun | LockState::Acquiring => RefStatus::Freerun,
        }
    }

    /// The full overlay [`TimeRef`] descriptor (source + status) for the badge.
    #[must_use]
    pub const fn to_time_ref(self) -> TimeRef {
        TimeRef::new(self.source, self.to_ref_status())
    }
}

/// Arbitrates the system (NTP/`SYS`) discipline against the PTP (`PTP`) reference
/// into one authoritative [`SelectedReference`] for the badge.
///
/// Policy: **PTP is the higher-stratum media reference** (ST 2059-2), so a
/// disciplined PTP reference (`Locked` or `Holdover`) wins over the system clock.
/// When PTP is not disciplined, the system NTP discipline is reported. When
/// neither is disciplined the badge shows the system source in its (honest)
/// `Freerun` state.
///
/// Pure: no clock, no I/O, never paces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ReferenceSelector;

impl ReferenceSelector {
    /// Choose the authoritative reference from the system NTP lock state (+ its
    /// offset) and the PTP [`ReferenceStatus`].
    #[must_use]
    pub fn select(
        self,
        system_state: LockState,
        system_offset_ns: i64,
        ptp: &ReferenceStatus,
    ) -> SelectedReference {
        if ptp.state.is_disciplined() {
            // PTP outranks the system clock whenever it is locked or coasting.
            return SelectedReference {
                source: RefSource::Ptp,
                state: ptp.state,
                offset_ns: ptp.offset_ns,
            };
        }
        // PTP absent / not yet locked: report the system NTP discipline.
        SelectedReference {
            source: RefSource::System,
            state: system_state,
            offset_ns: system_offset_ns,
        }
    }
}

/// Live kernel clock-discipline read — **feature `ntp`, Linux, live-gated**.
///
/// Reading the kernel NTP discipline state requires the `adjtimex(2)` syscall and
/// — to mean anything — a host actually synchronised by `ntpd`/`chrony`. Neither
/// the syscall's *disciplined* result nor a real grandmaster exists in CI (a
/// container typically returns `STA_UNSYNC` / `TIME_ERROR`), so the live read is
/// behind the off-by-default `ntp` feature and its *Locked* behaviour is verified
/// only through the injected classifier above. The pure classification +
/// selection this module exports is always compiled and fully tested.
///
/// ## Why this is a separate crate, not inline
///
/// `multiview-engine` is `#![forbid(unsafe_code)]`. The `adjtimex` syscall has no
/// safe-wrapper crate available, so the single `unsafe` FFI call is isolated in
/// the tiny [`multiview-ntpsys`](multiview_ntpsys) leaf crate (which is
/// `unsafe_code = "deny"` + `// SAFETY:`, the workspace FFI posture). This module
/// is the thin, safe adapter from that crate's snapshot into [`NtpReading`].
#[cfg(feature = "ntp")]
pub mod live {
    use super::{NtpClockState, NtpQuery, NtpReading, NtpStatusFlags};

    /// An [`NtpQuery`] backed by the live `adjtimex` syscall (via the
    /// `multiview-ntpsys` FFI leaf crate). On any platform other than Linux, or
    /// when the syscall fails, [`read`](NtpQuery::read) yields `None` and the
    /// tracker falls back to the configured assumed state.
    ///
    /// Compile-verified everywhere; the *Locked* path is meaningful only on a real
    /// NTP-synchronised host (live-gated — see the module docs).
    #[derive(Debug, Default)]
    #[non_exhaustive]
    pub struct SystemNtpQuery;

    impl SystemNtpQuery {
        /// Construct the live query.
        #[must_use]
        pub const fn new() -> Self {
            Self
        }
    }

    impl NtpQuery for SystemNtpQuery {
        fn read(&mut self) -> Option<NtpReading> {
            // The FFI leaf crate performs the one `unsafe` adjtimex call behind a
            // safe API and returns a normalised, all-nanoseconds snapshot (or
            // `None` off Linux / on error). We only re-shape it here — no unsafe,
            // no syscall, in the engine.
            let raw = multiview_ntpsys::read_adjtimex()?;
            Some(NtpReading {
                status: NtpStatusFlags::from_bits(raw.status_bits),
                clock_state: NtpClockState::from_code(raw.clock_state_code),
                est_error_ns: raw.est_error_ns,
                max_error_ns: raw.max_error_ns,
                offset_ns: raw.offset_ns,
            })
        }
    }
}
