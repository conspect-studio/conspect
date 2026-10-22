//! The always-on announce + browse **driver** (ADR-0051 §2/§5, brief §9.1) —
//! the `mdns` feature only (it needs `tokio` for the timed loop).
//!
//! [`run_announce_loop`] is the off-hot-loop task: it periodically **announces**
//! this machine's signed [`AnnouncePayload`] over the [`MeshTransport`] and
//! **browses** for neighbours, folding each received announcement (untrusted) into
//! the shared [`MeshState`] and aging out peers it has stopped hearing from. It is
//! generic over the transport, so the pure step ([`announce_browse_step`]) is
//! unit-testable **offline** with an in-memory fake; the live mDNS service
//! ([`crate::service::MdnsService`]) is one transport impl.
//!
//! **Today the daemon runs the browse side only.** The `multiview-cli` mesh task
//! (`spawn_mesh_discovery`) polls + folds untrusted announcements and ages out
//! peers, but does **not** announce: signing needs this machine's Ed25519 key and
//! salted fingerprint digests, provisioned by the operator-confirm items O1/O2/O6
//! (conspect brief §14). [`run_announce_loop`] folds the announce side in over the
//! same transport once that material lands.
//!
//! ## Untrusted inventory (ADR-0041 doctrine)
//!
//! A received announcement populates the **untrusted** inventory (digest + claim
//! state) — it is **never** auto-trusted or auto-relayed. The originator-signed
//! summary lets a receiver detect a spoof/tamper *once the peer's key is known*
//! (the O2 key-distribution step, operator-confirm); at discovery a peer is simply
//! recorded untrusted, exactly the SAP/managed-device contract.
//!
//! ## Isolation (invariant #10)
//!
//! Every transport op is best-effort: a failure is logged and the loop carries on.
//! The browse drain is non-blocking; the loop sleeps between rounds and never
//! blocks on the network. The driver holds no engine handle and can never
//! back-pressure the engine — a wedged or failed mesh loop never stalls anything.

use std::time::Duration;

use crate::error::MeshError;
use crate::peer::PeerObservation;
use crate::state::MeshState;
use crate::transport::MeshTransport;

/// The default interval between announce+browse rounds. mDNS announcements repeat
/// on the order of tens of seconds; this cadence keeps neighbours fresh without
/// flooding the segment, and is comfortably under [`crate::peer::PEER_STALE_AFTER`]
/// so a live neighbour is never aged out between rounds.
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// One announce + browse round against `transport`, folding results into `state`
/// at the monotonic instant `now` (for peer aging). Pure with respect to timing:
/// the caller supplies `now` and the announce wire bytes, so the step is
/// deterministic and testable offline.
///
/// 1. Announce `announce_wire` (best-effort; a failure is logged, the round
///    continues — discovery still browses).
/// 2. Drain received announcements, decode each (garbage is logged + skipped),
///    and fold the untrusted observation (digest + claim state) into `state`.
/// 3. Age out peers not seen within the staleness window.
///
/// Returns how many fresh observations were folded this round (for the caller's
/// metrics/tests).
pub fn announce_browse_step<T: MeshTransport + ?Sized>(
    transport: &T,
    state: &MeshState,
    announce_wire: &[u8],
    now: Duration,
) -> usize {
    // 1. Announce (best-effort).
    if let Err(err) = transport.announce(announce_wire) {
        match &err {
            // A structurally-refused announce (over the chunk cap, or a chunk that
            // is not valid UTF-8) recurs every round and silently removes this node
            // from peer discovery — a real fault, logged loud (not a transient
            // blip), so it is observable rather than silent.
            MeshError::AnnounceTooLarge { .. } => {
                tracing::warn!(
                    %err,
                    "mesh announce refused: payload exceeds the TXT chunk cap — this node \
                     is not discoverable until the announce shrinks"
                );
            }
            MeshError::AnnounceNotText { .. } => {
                tracing::warn!(
                    %err,
                    "mesh announce refused: a payload chunk is not valid UTF-8 for an mDNS \
                     TXT value — this node is not discoverable until the announce is fixed"
                );
            }
            // A transient transport failure is genuinely best-effort: log quietly
            // and retry on the next round (never off air).
            _ => {
                tracing::debug!(
                    %err,
                    "mesh announce failed this round (best-effort, never off air)"
                );
            }
        }
    }

    // 2. Browse + fold untrusted observations.
    let mut folded = 0_usize;
    match transport.poll_received() {
        Ok(received) => {
            for announcement in received {
                match announcement.decode() {
                    Ok(payload) => {
                        let Some(key) = payload.peer_key() else {
                            // An announcement with no digest is not adoptable; skip.
                            continue;
                        };
                        state.observe(PeerObservation {
                            key,
                            claim_state: payload.claim_state,
                            observed_at: now,
                        });
                        folded += 1;
                    }
                    Err(err) => {
                        tracing::debug!(%err, "ignoring a malformed mesh announcement");
                    }
                }
            }
        }
        Err(err) => {
            tracing::debug!(%err, "mesh browse poll failed this round (best-effort)");
        }
    }

    // 3. Age out peers we have stopped hearing from.
    state.age_out(now);
    folded
}

/// Run the always-on announce + browse loop forever, sleeping [`ANNOUNCE_INTERVAL`]
/// between rounds. The daemon spawns this with `tokio::spawn`; it returns only
/// when `transport` reports the runtime is gone (it never returns under normal
/// operation). `announce_wire` is a closure producing the **current** signed
/// announce payload bytes (so a lease/claim-state change is re-announced on the
/// next round).
///
/// `monotonic_now` is a closure returning a monotonic [`Duration`] since some
/// fixed start (the daemon supplies `Instant`-derived elapsed time), used only for
/// peer aging — never a system-clock read on this leaf crate.
#[cfg(feature = "mdns")]
pub async fn run_announce_loop<T, W, N>(
    transport: std::sync::Arc<T>,
    state: std::sync::Arc<MeshState>,
    mut announce_wire: W,
    mut monotonic_now: N,
) where
    T: MeshTransport + Send + Sync + 'static,
    W: FnMut() -> Vec<u8> + Send,
    N: FnMut() -> Duration + Send,
{
    loop {
        let wire = announce_wire();
        let now = monotonic_now();
        let _ = announce_browse_step(transport.as_ref(), state.as_ref(), &wire, now);
        tokio::time::sleep(ANNOUNCE_INTERVAL).await;
    }
}
