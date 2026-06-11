//! Per-connection monotonic sequence cursors.
//!
//! Every frame carries a per-connection (per-topic stream) monotonic `seq`
//! (ADR-RT002/RT003). A gap in observed `seq` means deltas were dropped; the
//! cursor is the resume key a reconnecting client presents (`$resume` /
//! `Last-Event-ID`). This module owns the [`Seq`] newtype and the
//! [`SeqCounter`] that guarantees strictly increasing issuance.
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A per-connection monotonic resume cursor (the envelope `seq` field).
///
/// Equality/ordering are by the underlying `u64`. The first frame on a
/// connection (`$hello`) uses [`Seq::ZERO`]; issued frames strictly increase.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// The baseline sequence (carried by `$hello`).
    pub const ZERO: Self = Self(0);

    /// Construct a sequence from a raw `u64`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw `u64` value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next sequence, or [`None`] on `u64` overflow.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The count of frames strictly between `self` and a later `other`
    /// (`other - self - 1`), i.e. how many were dropped if `other` is the next
    /// frame actually observed. Returns [`None`] if `other` does not strictly
    /// follow `self`.
    #[must_use]
    pub const fn gap_to(self, other: Self) -> Option<u64> {
        match other.0.checked_sub(self.0) {
            Some(0) | None => None,
            Some(diff) => Some(diff - 1),
        }
    }
}

/// Issues a strictly increasing stream of [`Seq`] values for one connection
/// (or one per-topic stream).
///
/// The first value issued is [`Seq::ZERO`]; each subsequent call advances by
/// one. Reaching [`u64::MAX`] is not a normal runtime condition, so
/// [`SeqCounter::issue`] returns [`Error::SeqOverflow`] rather than wrapping or
/// panicking.
#[derive(Debug, Clone)]
pub struct SeqCounter {
    next: Option<Seq>,
}

impl Default for SeqCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl SeqCounter {
    /// A fresh counter whose first issued value is [`Seq::ZERO`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: Some(Seq::ZERO),
        }
    }

    /// A counter that resumes issuing **after** `last` — its next value is
    /// `last + 1`. Returns [`Error::SeqOverflow`] if `last` is already
    /// [`u64::MAX`].
    ///
    /// # Errors
    ///
    /// [`Error::SeqOverflow`] when `last == u64::MAX` (no successor exists).
    pub fn resuming_after(last: Seq) -> Result<Self> {
        let next = last.next().ok_or(Error::SeqOverflow)?;
        Ok(Self { next: Some(next) })
    }

    /// The value the next [`SeqCounter::issue`] call will yield, if any remain.
    #[must_use]
    pub const fn peek(&self) -> Option<Seq> {
        self.next
    }

    /// Issue the next sequence, advancing the counter.
    ///
    /// Returned values are strictly increasing across calls.
    ///
    /// # Errors
    ///
    /// [`Error::SeqOverflow`] once the counter is exhausted at [`u64::MAX`].
    pub fn issue(&mut self) -> Result<Seq> {
        let current = self.next.ok_or(Error::SeqOverflow)?;
        // Advance; if there is no successor the counter is now exhausted and a
        // later call returns the overflow error.
        self.next = current.next();
        Ok(current)
    }
}
