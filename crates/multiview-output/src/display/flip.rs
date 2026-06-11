//! The pure flip-loop state machine (ADR-0044 §1): tracks the single
//! in-flight commit a CRTC allows and which mailbox sequence is on glass, so
//! the loop body reduces to three small decisions — commit, conflate, or do
//! nothing (KMS repeats the framebuffer for free).
//!
//! `EBUSY` from a nonblocking atomic commit means the kernel already has a
//! flip pending: the machine marks itself in-flight **without** advancing the
//! committed sequence, so the same latest frame is the retry candidate after
//! the pending flip drains. Nothing is ever queued and nothing ever spins.

/// Per-CRTC commit state: at most one commit in flight, newest frame wins.
#[derive(Debug, Default)]
pub struct FlipDriver {
    in_flight: bool,
    last_committed: u64,
}

impl FlipDriver {
    /// A fresh driver: nothing in flight, nothing committed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the loop should commit the mailbox frame stamped `latest_seq`
    /// now: only when no commit is in flight **and** the frame is strictly
    /// newer than what was last committed (no new frame ⇒ no commit).
    #[must_use]
    pub fn wants_commit(&self, latest_seq: u64) -> bool {
        !self.in_flight && latest_seq > self.last_committed
    }

    /// A nonblocking commit of the frame stamped `seq` was accepted: it is
    /// now the single in-flight commit.
    pub fn on_commit_submitted(&mut self, seq: u64) {
        self.in_flight = true;
        self.last_committed = seq;
    }

    /// The commit failed `EBUSY`: the kernel has an unaccounted-for flip
    /// pending. Become in-flight **without** advancing the committed
    /// sequence — the same latest frame retries after the next flip event.
    pub fn on_commit_busy(&mut self) {
        self.in_flight = true;
    }

    /// A page-flip completion arrived: the pipe can take the next commit.
    pub fn on_flip_complete(&mut self) {
        self.in_flight = false;
    }

    /// Whether a commit is currently in flight.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }
}
