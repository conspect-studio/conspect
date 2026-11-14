//! Off-clock media-player roll for a stinger AUTO (ADR-0112 §5).
//!
//! The switcher tick driver must ask a media player to cue and play when a
//! stinger AUTO starts, without ever waiting for it (invariants #1 and #10).
//! [`PlayerRollSender::roll`] is the only clock-side call: a lookup of the
//! player among the ids registered at boot (at most [`MAX_ROLL_PLAYERS`]) and
//! one `fetch_or` of its bit into a single preallocated [`AtomicU64`] of
//! pending players. That is the whole queue: no allocation, no lock, no
//! syscall, no wait, and a bounded footprint whatever the worker does. A roll
//! for a player whose bit is still pending coalesces into it and is counted;
//! a roll for an unregistered player is dropped and counted.
//!
//! Rationale (review SA-1): `std::sync::mpsc::SyncSender::try_send` is not
//! lock-free; its bounded channel wakes a receiver through a mutex-protected
//! waker list, so a worker descheduled while registering or unregistering in
//! `recv()` can stall the tick inside `try_send`. The tick therefore never
//! wakes the worker either: no `Thread::unpark` (its futex wake is a syscall
//! with kernel-side bucket locking). The worker polls the mask with
//! [`std::thread::park_timeout`] every [`ROLL_POLL`], swaps it to zero and
//! runs the caller's roll callback for each set bit. The poll interval is far
//! below a one-frame pre-roll, the smallest a valid stinger has. A callback
//! that blocks or panics stalls or loses only that worker; the clock keeps
//! its cadence and the take keeps its schedule, because nothing on the clock
//! depends on the player.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_core::switcher::SourceRef;

/// Players one roll mask can address: one bit each in an `AtomicU64`.
/// Players registered beyond this are never rolled (counted as dropped).
pub const MAX_ROLL_PLAYERS: usize = 64;

/// How often the worker polls the pending mask.
pub const ROLL_POLL: Duration = Duration::from_millis(2);

/// State shared by the clock-side senders and the worker.
#[derive(Debug)]
struct Shared {
    /// Bit `i` set: player `i` has a roll pending.
    pending: AtomicU64,
    sent: AtomicU64,
    coalesced: AtomicU64,
    dropped: AtomicU64,
}

/// The clock-side handle: a fixed-size, wait-free roll request mask.
#[derive(Debug, Clone)]
pub struct PlayerRollSender {
    players: Arc<[Arc<str>]>,
    shared: Arc<Shared>,
    /// Liveness of the sender set: the worker exits once only it holds this.
    _liveness: Arc<()>,
}

/// The worker side of a roll mask: takes pending players without ever
/// touching the clock. Production drives it from [`PlayerRollSender::spawn`];
/// a test may hold one and never drain it.
#[derive(Debug)]
pub struct PlayerRollReceiver {
    shared: Arc<Shared>,
    senders: std::sync::Weak<()>,
    taken: u64,
}

impl PlayerRollReceiver {
    /// Take the lowest pending player index, if any; never waits.
    pub fn try_recv(&mut self) -> Option<usize> {
        if self.taken == 0 {
            self.taken = self.shared.pending.swap(0, Ordering::AcqRel);
        }
        if self.taken == 0 {
            return None;
        }
        let index = self.taken.trailing_zeros();
        self.taken &= self.taken - 1;
        usize::try_from(index).ok()
    }

    /// Whether every sender has been dropped.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.senders.strong_count() == 0
    }
}

impl PlayerRollSender {
    /// A sender and its receiver for `players`, without a worker: the caller
    /// drains the receiver itself (tests). Production uses [`Self::spawn`].
    #[must_use]
    pub fn channel(players: impl IntoIterator<Item = String>) -> (Self, PlayerRollReceiver) {
        let players: Arc<[Arc<str>]> = players.into_iter().map(Arc::from).collect();
        if players.len() > MAX_ROLL_PLAYERS {
            tracing::warn!(
                players = players.len(),
                limit = MAX_ROLL_PLAYERS,
                "more media players than the stinger roll mask addresses; the rest never roll"
            );
        }
        let shared = Arc::new(Shared {
            pending: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let senders = Arc::new(());
        let receiver = PlayerRollReceiver {
            shared: Arc::clone(&shared),
            senders: Arc::downgrade(&senders),
            taken: 0,
        };
        (
            Self {
                players,
                shared,
                _liveness: senders,
            },
            receiver,
        )
    }

    /// Spawn the worker that calls `roll(player)` for every pending roll.
    /// A panicking callback is contained per roll; the worker exits when
    /// every sender is dropped.
    ///
    /// # Errors
    /// The OS refused to start the worker thread; there is then no roll
    /// (a stinger still takes on schedule, drawing its clip only if fresh).
    pub fn spawn(
        players: impl IntoIterator<Item = String>,
        roll: impl Fn(&str) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (sender, mut rx) = Self::channel(players);
        let ids = Arc::clone(&sender.players);
        std::thread::Builder::new()
            .name("stinger-roll".into())
            .spawn(move || loop {
                while let Some(index) = rx.try_recv() {
                    if let Some(id) = ids.get(index) {
                        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| roll(id)));
                        if outcome.is_err() {
                            tracing::warn!(player = %id, "stinger player roll panicked; skipped");
                        }
                    }
                }
                if rx.is_closed() {
                    break;
                }
                // Polled, never unparked by the tick (see the module docs).
                std::thread::park_timeout(ROLL_POLL);
            })?;
        Ok(sender)
    }

    /// Ask the worker to roll `player`, without waiting. Returns whether a
    /// new roll became pending; a roll already pending for the same player
    /// coalesces into it (counted in `coalesced`), an unregistered player is
    /// dropped (counted in `dropped`). Allocation-free, lock-free and
    /// wait-free: a bounded id scan and one `fetch_or`.
    #[allow(clippy::must_use_candidate)]
    // reason: called for its effect on the clock; the result only informs
    // tests (a coalesced or dropped roll is already counted).
    pub fn roll(&self, player: &SourceRef) -> bool {
        let bit = self
            .players
            .iter()
            .take(MAX_ROLL_PLAYERS)
            .position(|id| id.as_ref() == player.as_str())
            .and_then(|index| u32::try_from(index).ok())
            .and_then(|index| 1_u64.checked_shl(index));
        let Some(bit) = bit else {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let before = self.shared.pending.fetch_or(bit, Ordering::AcqRel);
        if before & bit == 0 {
            self.shared.sent.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            self.shared.coalesced.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Rolls that became pending so far.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.shared.sent.load(Ordering::Relaxed)
    }

    /// Rolls folded into an already-pending roll of the same player.
    #[must_use]
    pub fn coalesced(&self) -> u64 {
        self.shared.coalesced.load(Ordering::Relaxed)
    }

    /// Rolls dropped so far (unregistered player, or beyond
    /// [`MAX_ROLL_PLAYERS`]).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
}
