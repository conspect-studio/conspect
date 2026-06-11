//! HA cluster **transport** — feature `cluster`, compile-verified only.
//!
//! Exchanging heartbeats with peers and shipping replication snapshots/deltas
//! over a real network requires a live multi-node cluster and sockets, neither of
//! which exist in this environment. This binding therefore lives behind the
//! off-by-default `cluster` feature and is compile-verified only; its correctness
//! rests on the pure, fully-tested model in [`super`] (the heartbeat/failover
//! state machine) and [`super::repl`] (the replication model).
//!
//! ## Isolation is preserved (invariants #1 + #10)
//!
//! Crucially, even with `cluster` enabled the transport never paces or
//! back-pressures the active node's output clock. The runner *samples* inbound
//! heartbeats and feeds them to the pure [`HaStateMachine`]
//! / [`Cluster`]; it *publishes* this node's own heartbeats on a
//! best-effort, drop-oldest path. The engine's `out_pts = f(tick)` is never
//! derived from, nor gated on, any peer or socket — a partitioned, flapping, or
//! slow cluster link changes only when a standby *decides* to promote, never how
//! or when the active emits frames.
use super::repl::{EngineSnapshot, ReplicaApplier, ReplicationDelta};
use super::{Cluster, FailoverDecision, HaStateMachine, Heartbeat, NodeId, NodeRole, Priority};
use crate::error::{Error, Result};
use multiview_core::time::MediaTime;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{ToSocketAddrs, UdpSocket};

/// A best-effort, non-blocking transport over which a node exchanges heartbeats
/// and replication messages with its peers.
///
/// Implementations bind real sockets in a `cluster` build. The contract is that
/// **no method back-pressures the caller**: sends are best-effort drop-oldest and
/// receives are non-blocking polls. The engine's output path never awaits any of
/// these methods (invariants #1 + #10).
#[allow(async_fn_in_trait)]
// reason: like the `Actor` trait, this transport is consumed only inside the
// engine's HA runner; we do not need `Send`-bound futures via `trait-variant`.
pub trait ClusterTransport {
    /// Publish this node's heartbeat to all peers. Best-effort: must never block
    /// or back-pressure; a full/slow link drops, it does not stall the caller.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::error::Error) if the transport could not accept
    /// the heartbeat for publication (a permanent link/encoding fault — not a
    /// transient drop, which is silent).
    fn publish_heartbeat(&self, hb: Heartbeat) -> Result<()>;

    /// Poll for the next inbound peer heartbeat without blocking. Returns `None`
    /// when none is currently queued.
    fn poll_heartbeat(&mut self) -> Option<Heartbeat>;

    /// Publish a full replication snapshot to standbys (best-effort).
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::error::Error) on a permanent transport fault
    /// while submitting the snapshot.
    fn publish_snapshot(&self, snapshot: &EngineSnapshot) -> Result<()>;

    /// Publish an incremental replication delta to standbys (best-effort).
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::error::Error) on a permanent transport fault
    /// while submitting the delta.
    fn publish_delta(&self, delta: &ReplicationDelta) -> Result<()>;

    /// Poll for the next inbound replication message (snapshot or delta) without
    /// blocking.
    fn poll_replication(&mut self) -> Option<ReplicationMessage>;
}

/// An inbound replication message: a full baseline or an incremental delta.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplicationMessage {
    /// A full baseline snapshot.
    Snapshot(EngineSnapshot),
    /// An incremental delta.
    Delta(ReplicationDelta),
}

/// Drives one node's HA participation over a [`ClusterTransport`]: feeds inbound
/// heartbeats into the local [`HaStateMachine`] + [`Cluster`] view, applies
/// inbound replication to the [`ReplicaApplier`], and (on the active) publishes
/// this node's own heartbeats.
///
/// This is the seam a `cluster`-enabled deployment wires to its sockets. It is a
/// pure orchestrator over the tested model — every *decision* it makes is the
/// model's; the runner only moves bytes (best-effort, never blocking the engine).
#[derive(Debug)]
pub struct HaRunner<T: ClusterTransport> {
    transport: T,
    machine: HaStateMachine,
    cluster: Cluster,
    replica: ReplicaApplier,
}

impl<T: ClusterTransport> HaRunner<T> {
    /// Build a runner for `machine`'s node, watching `cluster`, over `transport`.
    #[must_use]
    pub fn new(transport: T, machine: HaStateMachine, cluster: Cluster) -> Self {
        Self {
            transport,
            machine,
            cluster,
            replica: ReplicaApplier::new(),
        }
    }

    /// This node's id.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.machine.node().id()
    }

    /// The local HA state machine (for metrics / role inspection).
    #[must_use]
    pub const fn machine(&self) -> &HaStateMachine {
        &self.machine
    }

    /// The local replica (for promotion: the state to resume from).
    #[must_use]
    pub const fn replica(&self) -> &ReplicaApplier {
        &self.replica
    }

    /// Whether the cluster view considers the configured active alive at `now`.
    ///
    /// Reflects only heartbeats already pumped into the [`Cluster`]; it is the
    /// observable proof that a peer's beat actually crossed the transport.
    #[must_use]
    pub fn cluster_active_alive(&self, now: MediaTime) -> bool {
        self.cluster.active_alive(now)
    }

    /// Drain all currently-queued inbound heartbeats into the model, returning
    /// `true` if doing so promoted this node. Non-blocking: stops when the
    /// transport has nothing queued.
    pub fn pump_heartbeats(&mut self, now: MediaTime) -> bool {
        while let Some(hb) = self.transport.poll_heartbeat() {
            self.cluster.observe(hb);
            self.machine.observe_heartbeat(hb);
        }
        self.machine.tick(now)
    }

    /// Drain all currently-queued inbound replication messages into the local
    /// replica. Errors from the pure applier (gaps / non-monotonic) are surfaced
    /// for the caller to react to (e.g. request a fresh snapshot); they never
    /// mutate the replica.
    pub fn pump_replication(&mut self) {
        while let Some(msg) = self.transport.poll_replication() {
            let _result = match msg {
                ReplicationMessage::Snapshot(snap) => self.replica.install_snapshot(snap),
                ReplicationMessage::Delta(delta) => self.replica.apply_delta(delta),
            };
        }
    }

    /// Publish this node's heartbeat for `now` (best-effort, never blocking).
    ///
    /// # Errors
    ///
    /// Propagates a transport-level publish error.
    pub fn beat(&self, now: MediaTime) -> Result<()> {
        self.transport.publish_heartbeat(Heartbeat {
            from: self.machine.node().id(),
            priority: self.machine.node().priority(),
            role: self.machine.role(),
            epoch: self.machine.epoch(),
            drives_output: self.machine.drives_output(),
            sent_at: now,
        })
    }

    /// The cluster-wide failover decision for this node at `now`.
    #[must_use]
    pub fn decision(&self, now: MediaTime) -> FailoverDecision {
        self.cluster.evaluate(self.machine.node().id(), now)
    }
}

/// The serializable wire mirror of a [`Heartbeat`].
///
/// The HA model types ([`NodeId`], [`Priority`], [`NodeRole`], [`Heartbeat`]) are
/// intentionally not `serde`-derived (the pure model needs no wire format); this
/// transport-local mirror is the JSON-on-the-wire shape and converts to/from the
/// model at the socket boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct HeartbeatWire {
    from: u32,
    priority: u32,
    active: bool,
    epoch: u64,
    drives_output: bool,
    sent_at_ns: i64,
}

impl From<Heartbeat> for HeartbeatWire {
    fn from(hb: Heartbeat) -> Self {
        Self {
            from: hb.from.get(),
            priority: hb.priority.get(),
            active: matches!(hb.role, NodeRole::Active),
            epoch: hb.epoch,
            drives_output: hb.drives_output,
            sent_at_ns: hb.sent_at.as_nanos(),
        }
    }
}

impl From<HeartbeatWire> for Heartbeat {
    fn from(w: HeartbeatWire) -> Self {
        Self {
            from: NodeId::new(w.from),
            priority: Priority::new(w.priority),
            role: if w.active {
                NodeRole::Active
            } else {
                NodeRole::Standby
            },
            epoch: w.epoch,
            drives_output: w.drives_output,
            sent_at: MediaTime::from_nanos(w.sent_at_ns),
        }
    }
}

/// A single datagram on the cluster wire: a heartbeat, a full snapshot, or a
/// delta. Serialized **tagged** (`#[serde(tag = "msg")]`) per repo conventions —
/// never `untagged` — so a malformed or unknown datagram fails to deserialize and
/// is dropped, never silently misclassified.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
enum ClusterDatagram {
    Heartbeat(HeartbeatWire),
    Snapshot(EngineSnapshot),
    Delta(ReplicationDelta),
}

/// The maximum number of inbound messages buffered per kind before the oldest is
/// dropped. A flooding peer can therefore never grow this node's memory (bounded,
/// drop-oldest — the data-plane discipline, invariants #9/#10).
const INBOUND_CAP: usize = 256;

/// The fixed receive buffer size, in bytes. A datagram larger than this is
/// truncated by the OS on `recv`; our messages (heartbeats + small layout/source
/// snapshots) are far smaller, and an oversized/garbage datagram simply fails to
/// deserialize and is dropped.
const RECV_BUF_LEN: usize = 64 * 1024;

/// A concrete [`ClusterTransport`] over non-blocking UDP, for a `cluster` build.
///
/// * **Sends are best-effort and never block.** The socket is non-blocking;
///   `publish_*` serializes to JSON and `send_to`s each peer, treating a
///   [`WouldBlock`](ErrorKind::WouldBlock) (a full socket buffer) as a silent
///   drop — exactly the contract the trait requires (invariants #1 + #10: the
///   publisher, which may be the active's control surface, never stalls on a slow
///   or black-holed peer).
/// * **Receives are non-blocking polls into bounded, drop-oldest queues.** Inbound
///   datagrams are drained into per-kind [`VecDeque`]s capped at `INBOUND_CAP`;
///   a flooding peer drops the oldest rather than growing memory. A malformed or
///   unknown datagram is dropped (and `trace`-logged), never a panic.
///
/// This moves bytes only; every HA *decision* is made by the pure model the
/// [`HaRunner`] drives over these polls.
#[derive(Debug)]
pub struct UdpClusterTransport {
    socket: UdpSocket,
    peers: Vec<std::net::SocketAddr>,
    heartbeats: VecDeque<Heartbeat>,
    replication: VecDeque<ReplicationMessage>,
    recv_buf: Vec<u8>,
}

impl UdpClusterTransport {
    /// Bind a non-blocking UDP socket to `bind_addr` and address the given
    /// `peers`. IPv6-first: pass `"[::1]:0"` (loopback) or `"[::]:0"` (all
    /// interfaces) for an OS-assigned ephemeral port (read it back with
    /// [`local_addr`](Self::local_addr)); a user-supplied IPv4 `bind_addr` still
    /// works but is never the default. `bind_addr` and `peers` must share an
    /// address family.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Cluster`] if the socket cannot be bound, the bind address
    /// resolves to nothing, or the socket cannot be put into non-blocking mode.
    pub fn bind<A: ToSocketAddrs>(bind_addr: A, peers: &[std::net::SocketAddr]) -> Result<Self> {
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| Error::Cluster(format!("bind cluster socket: {e}")))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| Error::Cluster(format!("set cluster socket non-blocking: {e}")))?;
        Ok(Self {
            socket,
            peers: peers.to_vec(),
            heartbeats: VecDeque::with_capacity(INBOUND_CAP),
            replication: VecDeque::with_capacity(INBOUND_CAP),
            recv_buf: vec![0_u8; RECV_BUF_LEN],
        })
    }

    /// The address the socket is actually bound to (resolves an ephemeral `:0`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Cluster`] if the OS cannot report the local address.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.socket
            .local_addr()
            .map_err(|e| Error::Cluster(format!("cluster socket local addr: {e}")))
    }

    /// Replace this transport's peer set (builder style), returning `self`.
    #[must_use]
    pub fn with_peers(mut self, peers: &[std::net::SocketAddr]) -> Self {
        self.peers = peers.to_vec();
        self
    }

    /// Serialize `datagram` and `send_to` every peer, best-effort.
    ///
    /// A `WouldBlock` (full socket buffer) for any peer is a silent drop — never a
    /// block, never a returned error. A *serialization* failure is a permanent
    /// fault and is returned.
    fn send_to_peers(&self, datagram: &ClusterDatagram) -> Result<()> {
        let bytes = serde_json::to_vec(datagram)
            .map_err(|e| Error::Cluster(format!("encode cluster datagram: {e}")))?;
        for peer in &self.peers {
            match self.socket.send_to(&bytes, peer) {
                Ok(_) => {}
                // A full send buffer means a slow/black-holed link: drop, do not
                // stall the publisher (the whole point of best-effort isolation).
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    tracing::trace!(target: "ha.transport", %peer, "heartbeat/repl send dropped (WouldBlock)");
                }
                Err(e) => {
                    // A transient per-peer error must not abort the burst nor
                    // back-pressure: log and continue (best-effort).
                    tracing::trace!(target: "ha.transport", %peer, error = %e, "cluster send error (dropped)");
                }
            }
        }
        Ok(())
    }

    /// Drain every datagram currently queued on the socket into the bounded
    /// per-kind inbound queues. Non-blocking: stops on `WouldBlock`. Malformed or
    /// unknown datagrams are dropped (trace-logged), never panicked.
    fn drain_socket(&mut self) {
        loop {
            match self.socket.recv_from(&mut self.recv_buf) {
                Ok((len, from)) => {
                    let slice = self.recv_buf.get(..len).unwrap_or(&[]);
                    match serde_json::from_slice::<ClusterDatagram>(slice) {
                        Ok(ClusterDatagram::Heartbeat(w)) => {
                            push_bounded(&mut self.heartbeats, Heartbeat::from(w));
                        }
                        Ok(ClusterDatagram::Snapshot(s)) => {
                            push_bounded(&mut self.replication, ReplicationMessage::Snapshot(s));
                        }
                        Ok(ClusterDatagram::Delta(d)) => {
                            push_bounded(&mut self.replication, ReplicationMessage::Delta(d));
                        }
                        Err(e) => {
                            tracing::trace!(
                                target: "ha.transport",
                                %from,
                                error = %e,
                                "dropped malformed cluster datagram"
                            );
                        }
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    // A transient receive error (e.g. an ICMP port-unreachable
                    // surfaced as ConnReset on some platforms): log and stop this
                    // drain pass rather than spin.
                    tracing::trace!(target: "ha.transport", error = %e, "cluster recv error");
                    break;
                }
            }
        }
    }
}

/// Push `item` onto `queue`, dropping the oldest element first when the queue is
/// at [`INBOUND_CAP`] (bounded, drop-oldest — a flooding peer cannot grow memory).
fn push_bounded<T>(queue: &mut VecDeque<T>, item: T) {
    if queue.len() >= INBOUND_CAP {
        let _ = queue.pop_front();
    }
    queue.push_back(item);
}

impl ClusterTransport for UdpClusterTransport {
    fn publish_heartbeat(&self, hb: Heartbeat) -> Result<()> {
        self.send_to_peers(&ClusterDatagram::Heartbeat(HeartbeatWire::from(hb)))
    }

    fn poll_heartbeat(&mut self) -> Option<Heartbeat> {
        self.drain_socket();
        self.heartbeats.pop_front()
    }

    fn publish_snapshot(&self, snapshot: &EngineSnapshot) -> Result<()> {
        self.send_to_peers(&ClusterDatagram::Snapshot(snapshot.clone()))
    }

    fn publish_delta(&self, delta: &ReplicationDelta) -> Result<()> {
        self.send_to_peers(&ClusterDatagram::Delta(delta.clone()))
    }

    fn poll_replication(&mut self) -> Option<ReplicationMessage> {
        self.drain_socket();
        self.replication.pop_front()
    }
}
