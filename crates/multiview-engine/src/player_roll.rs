//! Off-clock media-player roll for a stinger AUTO (ADR-0112 §5).
//!
//! The switcher tick driver must ask a media player to cue and play when a
//! stinger AUTO starts, without ever waiting for it (invariants #1 and #10).
//! [`PlayerRollSender::roll`] is the only clock-side call: a lookup of the
//! player among the ids registered at boot and one `try_send` of its index on
//! a bounded channel. No allocation, no lock, no wait: a full or closed queue
//! drops the roll and counts it. A dedicated worker thread receives the index
//! and runs the caller's roll callback (the CLI submits `Cue` then `Play` to
//! the player's transport mailbox). A callback that blocks or panics stalls
//! or loses only that worker; the clock keeps its cadence and the take keeps
//! its schedule, because nothing on the clock depends on the player.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use multiview_core::switcher::SourceRef;

/// Pending rolls the worker may lag behind by before a roll is dropped.
pub const PLAYER_ROLL_CAPACITY: usize = 4;

/// The clock-side handle: a bounded, non-blocking roll request queue.
#[derive(Debug, Clone)]
pub struct PlayerRollSender {
    players: Arc<[Arc<str>]>,
    tx: SyncSender<usize>,
    sent: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl PlayerRollSender {
    /// A sender and its receiver for `players`, without a worker: the caller
    /// drains the receiver itself (tests). Production uses [`Self::spawn`].
    #[must_use]
    pub fn channel(players: impl IntoIterator<Item = String>) -> (Self, Receiver<usize>) {
        let (tx, rx) = sync_channel(PLAYER_ROLL_CAPACITY);
        let players: Arc<[Arc<str>]> = players.into_iter().map(Arc::from).collect();
        (
            Self {
                players,
                tx,
                sent: Arc::new(AtomicU64::new(0)),
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// Spawn the worker that calls `roll(player)` for every queued roll.
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
        let (sender, rx) = Self::channel(players);
        let ids = Arc::clone(&sender.players);
        std::thread::Builder::new()
            .name("stinger-roll".into())
            .spawn(move || {
                while let Ok(index) = rx.recv() {
                    if let Some(id) = ids.get(index) {
                        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| roll(id)));
                        if outcome.is_err() {
                            tracing::warn!(player = %id, "stinger player roll panicked; skipped");
                        }
                    }
                }
            })?;
        Ok(sender)
    }

    /// Ask the worker to roll `player`, without waiting. Returns whether the
    /// request was queued; an unregistered player or a full/closed queue is
    /// dropped and counted. Allocation-free and wait-free.
    #[allow(clippy::must_use_candidate)]
    // reason: called for its effect on the clock; the result only informs
    // tests (a drop is already counted in `dropped`).
    pub fn roll(&self, player: &SourceRef) -> bool {
        let Some(index) = self
            .players
            .iter()
            .position(|id| id.as_ref() == player.as_str())
        else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        match self.tx.try_send(index) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Rolls queued so far.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Rolls dropped so far (unregistered player, full or closed queue).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
