//! ICE-TCP passive candidates on the TCP door (RFC 6544, framed by RFC 4571).
//!
//! A connection whose first two bytes are an RFC 4571 length of 20..=1535 must
//! deliver, within the first-byte deadline, a first frame that is structurally
//! a STUN Binding Request with a `USERNAME` (`<local>:<remote>`). It is then
//! registered in a bounded connection table and starts in **probation**: its
//! frames reach the lane (the consumer, str0m, verifies `MESSAGE-INTEGRITY`),
//! but until the consumer queues at least one outbound frame to that peer (str0m
//! answering an authenticated Binding Request) it may send at most
//! [`PROBATION_MAX_FRAMES`] frames, each at most 1535 bytes and each a STUN
//! message; and if no outbound frame was queued within the probation deadline
//! the connection is closed. After probation frames up to 65 535 bytes flow.
//!
//! Bounded state (invariant #10): one lane queue ([`LANE_QUEUE`], `try_send`,
//! full = drop and count), one per-connection out queue ([`OUT_QUEUE`] frames
//! and [`OUT_QUEUE_BYTES`] bytes, drop-oldest; the lock is held only for
//! push/pop, never across an await), a read buffer of at most one frame, and
//! the table capped by the door's connection cap. Nothing a producer calls ever waits on a peer: a stuck peer
//! is closed by its own writer task.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Notify};

use crate::classify::{canonical, STUN_MAGIC_COOKIE};
use crate::Stats;

/// Depth of the one lane queue all ICE-TCP connections feed.
pub const LANE_QUEUE: usize = 1024;
/// Depth of each connection's outbound queue (frames, drop-oldest).
pub const OUT_QUEUE: usize = 256;
/// Bytes queued per connection at most (drop-oldest beyond it), so 256
/// maximum-size frames cannot pin 16 MiB per connection: the worst case over
/// the door is `max_connections` x 256 KiB.
pub const OUT_QUEUE_BYTES: usize = 256 * 1024;
/// The RFC 4571 maximum payload.
pub const MAX_FRAME: usize = 65_535;
/// The largest frame while a connection is in probation.
pub const PROBATION_MAX_FRAME: usize = 1535;
/// The most inbound frames while a connection is in probation.
pub const PROBATION_MAX_FRAMES: u32 = 8;
/// STUN attribute `USERNAME` (RFC 8489 section 14.3).
const ATTR_USERNAME: u16 = 0x0006;

/// Optional pre-check of a first Binding Request's full `USERNAME`
/// (`"<local ufrag>:<remote ufrag>"`). `false` closes the connection.
pub type UsernameCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// One frame read from an ICE-TCP connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceTcpFrame {
    /// The remote peer, canonical (an IPv4-mapped address unmapped).
    pub peer: SocketAddr,
    /// The accepted socket's local address, unmapped.
    pub local: SocketAddr,
    /// The frame payload (without the length prefix).
    pub payload: Vec<u8>,
}

/// What [`IceTcpSender::send`] did with a frame. Never blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Queued for the connection's writer.
    Queued,
    /// Queued; the oldest queued frame was dropped to make room.
    DroppedOldest,
    /// No connection to that peer (never was, or closed).
    NoConnection,
    /// Larger than the RFC 4571 maximum; not queued.
    TooLarge,
}

/// Why a first frame was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstFrameError {
    /// Not a STUN Binding Request header (type, cookie or length).
    NotBindingRequest,
    /// No `USERNAME` attribute, or one without `:`.
    NoUsername,
    /// The attribute list is malformed.
    Malformed,
}

/// Check that `frame` is structurally a STUN Binding Request (RFC 8489):
/// type `0x0001`, the magic cookie, a message length covering exactly the
/// frame, and a `USERNAME` attribute containing `:`. Returns the username.
///
/// # Errors
/// A [`FirstFrameError`].
pub fn binding_request_username(frame: &[u8]) -> Result<&str, FirstFrameError> {
    let Some(&[t0, t1, l0, l1, c0, c1, c2, c3]) = frame.get(..8) else {
        return Err(FirstFrameError::NotBindingRequest);
    };
    let declared = usize::from(u16::from_be_bytes([l0, l1]));
    if [t0, t1] != [0x00, 0x01]
        || [c0, c1, c2, c3] != STUN_MAGIC_COOKIE
        || declared % 4 != 0
        || declared + 20 != frame.len()
    {
        return Err(FirstFrameError::NotBindingRequest);
    }
    let mut attrs = frame.get(20..).ok_or(FirstFrameError::Malformed)?;
    while !attrs.is_empty() {
        let Some(&[a0, a1, b0, b1]) = attrs.get(..4) else {
            return Err(FirstFrameError::Malformed);
        };
        let kind = u16::from_be_bytes([a0, a1]);
        let len = usize::from(u16::from_be_bytes([b0, b1]));
        let value = attrs.get(4..4 + len).ok_or(FirstFrameError::Malformed)?;
        if kind == ATTR_USERNAME {
            let name = std::str::from_utf8(value).map_err(|_| FirstFrameError::NoUsername)?;
            return if name.contains(':') {
                Ok(name)
            } else {
                Err(FirstFrameError::NoUsername)
            };
        }
        let padded = (len + 3) & !3;
        attrs = attrs.get(4 + padded..).ok_or(FirstFrameError::Malformed)?;
    }
    Err(FirstFrameError::NoUsername)
}

/// Why the deframer refused the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeframeError {
    /// A zero-length frame.
    Empty,
    /// A frame longer than the current maximum.
    TooLarge(usize),
}

/// The RFC 4571 deframer: `len_be16 || payload`, repeated. Holds at most one
/// partial frame (two length bytes plus at most `max` payload bytes).
#[derive(Debug)]
pub struct Deframer {
    max: usize,
    header: [u8; 2],
    header_len: usize,
    want: Option<usize>,
    body: Vec<u8>,
}

impl Deframer {
    /// A deframer accepting frames of at most `max` bytes (capped at 65 535).
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max: max.min(MAX_FRAME),
            header: [0; 2],
            header_len: 0,
            want: None,
            body: Vec::new(),
        }
    }

    /// Change the maximum frame length (probation lifted).
    pub fn set_max(&mut self, max: usize) {
        self.max = max.min(MAX_FRAME);
    }

    /// Bytes buffered for the frame in progress.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.header_len + self.body.len()
    }

    /// Feed `data`, pushing every completed frame onto `out`.
    ///
    /// # Errors
    /// A [`DeframeError`]; the stream must then be closed.
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<Vec<u8>>) -> Result<(), DeframeError> {
        while !data.is_empty() {
            match self.want {
                None => {
                    let take = (2 - self.header_len).min(data.len());
                    let (head, rest) = data.split_at(take);
                    if let Some(slot) = self.header.get_mut(self.header_len..self.header_len + take)
                    {
                        slot.copy_from_slice(head);
                    }
                    self.header_len += take;
                    data = rest;
                    if self.header_len == 2 {
                        let len = usize::from(u16::from_be_bytes(self.header));
                        if len == 0 {
                            return Err(DeframeError::Empty);
                        }
                        if len > self.max {
                            return Err(DeframeError::TooLarge(len));
                        }
                        self.want = Some(len);
                        self.body = Vec::with_capacity(len);
                    }
                }
                Some(len) => {
                    let take = (len - self.body.len()).min(data.len());
                    let (part, rest) = data.split_at(take);
                    self.body.extend_from_slice(part);
                    data = rest;
                    if self.body.len() == len {
                        out.push(std::mem::take(&mut self.body));
                        self.want = None;
                        self.header_len = 0;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Timing of an ICE-TCP connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IceTcpTiming {
    pub(crate) probation: Duration,
    pub(crate) idle: Duration,
    pub(crate) write_timeout: Duration,
}

#[derive(Debug, Default)]
struct OutState {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    overflow_since: Option<Instant>,
}

impl OutState {
    fn pop(&mut self) -> Option<Vec<u8>> {
        let frame = self.frames.pop_front()?;
        self.bytes = self.bytes.saturating_sub(frame.len());
        Some(frame)
    }
}

/// One registered connection, shared by its tasks and the sender.
#[derive(Debug)]
struct Conn {
    out: Mutex<OutState>,
    ready: Notify,
    close: watch::Sender<bool>,
    /// Set once the consumer queued an outbound frame (probation over).
    answered: AtomicBool,
    write_timeout: Duration,
}

impl Conn {
    fn close(&self) {
        self.close.send_replace(true);
    }
}

/// State shared between the door, the connections and the lane.
pub(crate) struct IceShared {
    table: Mutex<HashMap<SocketAddr, Arc<Conn>>>,
    cap: usize,
    tx: mpsc::Sender<IceTcpFrame>,
    rx: Mutex<Option<mpsc::Receiver<IceTcpFrame>>>,
    consumer: AtomicBool,
    pub(crate) check: Mutex<Option<UsernameCheck>>,
    stats: Arc<Stats>,
}

impl std::fmt::Debug for IceShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceShared")
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl IceShared {
    pub(crate) fn new(cap: usize, stats: Arc<Stats>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(LANE_QUEUE);
        Arc::new(Self {
            table: Mutex::new(HashMap::new()),
            cap: cap.max(1),
            tx,
            rx: Mutex::new(Some(rx)),
            consumer: AtomicBool::new(false),
            check: Mutex::new(None),
            stats,
        })
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<SocketAddr, Arc<Conn>>> {
        self.table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn take_lane(self: &Arc<Self>, local_port: u16) -> Option<IceTcpLane> {
        let rx = self
            .rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()?;
        self.consumer.store(true, Ordering::Release);
        Some(IceTcpLane {
            rx,
            sender: IceTcpSender {
                shared: Arc::clone(self),
            },
            local_port,
        })
    }

    fn precheck(&self, username: &str) -> bool {
        let check = self
            .check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        check.is_none_or(|f| f(username))
    }

    /// The number of registered connections.
    pub(crate) fn len(&self) -> usize {
        self.table().len()
    }
}

/// Sends frames to ICE-TCP peers. Clone freely; every call is non-blocking.
#[derive(Clone)]
pub struct IceTcpSender {
    shared: Arc<IceShared>,
}

impl std::fmt::Debug for IceTcpSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IceTcpSender")
    }
}

impl IceTcpSender {
    /// Queue one frame to `peer`. Never blocks: a full queue drops its oldest
    /// frame. A peer whose queue has overflowed continuously for the write
    /// timeout is closed as stuck.
    #[must_use]
    pub fn send(&self, peer: SocketAddr, payload: &[u8]) -> SendOutcome {
        if payload.len() > MAX_FRAME || payload.is_empty() {
            return SendOutcome::TooLarge;
        }
        let Some(conn) = self.shared.table().get(&canonical(peer)).cloned() else {
            return SendOutcome::NoConnection;
        };
        let len = u16::try_from(payload.len()).unwrap_or(u16::MAX);
        let mut frame = Vec::with_capacity(payload.len() + 2);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(payload);
        let now = Instant::now();
        let (outcome, stuck) = {
            let mut out = conn
                .out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut dropped = false;
            while out.frames.len() >= OUT_QUEUE
                || (!out.frames.is_empty() && out.bytes + frame.len() > OUT_QUEUE_BYTES)
            {
                out.pop();
                Stats::bump(&self.shared.stats.ice_tcp_dropped_out);
                dropped = true;
            }
            let outcome = if dropped {
                let since = *out.overflow_since.get_or_insert(now);
                (
                    SendOutcome::DroppedOldest,
                    now.duration_since(since) >= conn.write_timeout,
                )
            } else {
                (SendOutcome::Queued, false)
            };
            out.bytes += frame.len();
            out.frames.push_back(frame);
            outcome
        };
        conn.answered.store(true, Ordering::Release);
        conn.ready.notify_one();
        if stuck {
            Stats::bump(&self.shared.stats.ice_tcp_closed_stuck);
            conn.close();
        }
        outcome
    }

    /// Close the connection to `peer` (the consumer dropped its session).
    /// Returns whether a connection was registered.
    #[allow(clippy::must_use_candidate)]
    pub fn close(&self, peer: SocketAddr) -> bool {
        let conn = self.shared.table().remove(&canonical(peer));
        conn.inspect(|c| c.close()).is_some()
    }

    /// The number of bytes queued to `peer` (tests, telemetry).
    #[must_use]
    pub fn queued_bytes(&self, peer: SocketAddr) -> usize {
        self.shared.table().get(&canonical(peer)).map_or(0, |c| {
            c.out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bytes
        })
    }

    /// The number of frames queued to `peer` (tests, telemetry).
    #[must_use]
    pub fn queued(&self, peer: SocketAddr) -> usize {
        self.shared.table().get(&canonical(peer)).map_or(0, |c| {
            c.out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .frames
                .len()
        })
    }

    /// The number of open ICE-TCP connections.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.shared.len()
    }
}

/// The consumer side of ICE-TCP: inbound frames from every connection, and a
/// sender for replies.
pub struct IceTcpLane {
    rx: mpsc::Receiver<IceTcpFrame>,
    sender: IceTcpSender,
    local_port: u16,
}

impl std::fmt::Debug for IceTcpLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceTcpLane")
            .field("local_port", &self.local_port)
            .finish_non_exhaustive()
    }
}

impl IceTcpLane {
    /// Wait for the next frame. `None` once the door is gone.
    pub async fn recv(&mut self) -> Option<IceTcpFrame> {
        self.rx.recv().await
    }

    /// Take a queued frame without waiting.
    pub fn try_recv(&mut self) -> Option<IceTcpFrame> {
        self.rx.try_recv().ok()
    }

    /// Poll for the next frame.
    pub fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<IceTcpFrame>> {
        self.rx.poll_recv(cx)
    }

    /// The sender for replies.
    #[must_use]
    pub fn sender(&self) -> IceTcpSender {
        self.sender.clone()
    }

    /// The TCP door's port (the passive candidate's port).
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

/// Read the first frame's body (its two length bytes already consumed and
/// classified) before `deadline`.
async fn read_first_frame(
    stream: &mut TcpStream,
    len: u16,
    deadline: tokio::time::Instant,
) -> Option<Vec<u8>> {
    let mut frame = vec![0u8; usize::from(len)];
    match tokio::time::timeout_at(deadline, stream.read_exact(&mut frame)).await {
        Ok(Ok(_)) => Some(frame),
        _ => None,
    }
}

/// Serve one classified ICE-TCP connection to its end (the first frame's
/// length prefix already read). `guard` is the admission slot; it is dropped
/// when this returns.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn serve<G: Send + 'static>(
    shared: Arc<IceShared>,
    mut stream: TcpStream,
    peer: SocketAddr,
    first_len: u16,
    deadline: tokio::time::Instant,
    accepted_at: tokio::time::Instant,
    timing: IceTcpTiming,
    guard: G,
) {
    let stats = Arc::clone(&shared.stats);
    let peer = canonical(peer);
    let reject = || Stats::bump(&stats.ice_tcp_rejected);
    let Some(first) = read_first_frame(&mut stream, first_len, deadline).await else {
        reject();
        return;
    };
    match binding_request_username(&first) {
        Ok(username) if shared.precheck(username) => {}
        _ => {
            reject();
            return;
        }
    }
    if !shared.consumer.load(Ordering::Acquire) {
        // No consumer took the lane: nothing could ever answer.
        reject();
        return;
    }
    let Ok(local) = stream.local_addr().map(canonical) else {
        reject();
        return;
    };
    let (close_tx, mut close_rx) = watch::channel(false);
    let conn = Arc::new(Conn {
        out: Mutex::new(OutState::default()),
        ready: Notify::new(),
        close: close_tx,
        answered: AtomicBool::new(false),
        write_timeout: timing.write_timeout,
    });
    {
        let mut table = shared.table();
        if table.len() >= shared.cap || table.contains_key(&peer) {
            // Keep the first connection from a peer; refuse a second.
            drop(table);
            reject();
            return;
        }
        table.insert(peer, Arc::clone(&conn));
    }
    Stats::bump(&stats.ice_tcp_accepted);
    let deliver = |payload: Vec<u8>| -> bool {
        match shared.tx.try_send(IceTcpFrame {
            peer,
            local,
            payload,
        }) {
            Ok(()) => {
                Stats::bump(&stats.ice_tcp_frames_in);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                Stats::bump(&stats.ice_tcp_dropped_in);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    };
    let (mut reader, writer) = stream.into_split();
    if deliver(first) {
        let writer_task = tokio::spawn(write_loop(
            Arc::clone(&conn),
            writer,
            close_rx.clone(),
            Arc::clone(&stats),
        ));
        read_loop(
            &conn,
            &mut reader,
            &mut close_rx,
            accepted_at + timing.probation,
            timing.idle,
            &stats,
            deliver,
        )
        .await;
        conn.close();
        let _ = writer_task.await;
    }
    // Remove only our own entry (a later connection may reuse the address).
    {
        let mut table = shared.table();
        if table.get(&peer).is_some_and(|c| Arc::ptr_eq(c, &conn)) {
            table.remove(&peer);
        }
    }
    drop(guard);
}

async fn read_loop(
    conn: &Conn,
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    close: &mut watch::Receiver<bool>,
    probation_deadline: tokio::time::Instant,
    idle: Duration,
    stats: &Stats,
    deliver: impl Fn(Vec<u8>) -> bool,
) {
    let mut deframer = Deframer::new(PROBATION_MAX_FRAME);
    let mut probation = true;
    // The first frame was one of the probation frames.
    let mut probation_frames = 1u32;
    let mut buf = vec![0u8; 16 * 1024];
    let mut frames = Vec::new();
    loop {
        if probation && conn.answered.load(Ordering::Acquire) {
            probation = false;
            deframer.set_max(MAX_FRAME);
        }
        let probation_sleep = async {
            if probation {
                tokio::time::sleep_until(probation_deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let read = tokio::select! {
            biased;
            _ = close.wait_for(|c| *c) => return,
            () = probation_sleep => {
                if conn.answered.load(Ordering::Acquire) {
                    continue;
                }
                Stats::bump(&stats.ice_tcp_probation_expired);
                return;
            }
            read = tokio::time::timeout(idle, reader.read(&mut buf)) => read,
        };
        let n = match read {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        let Some(data) = buf.get(..n) else { return };
        // The answer may have been queued while this read was parked.
        if probation && conn.answered.load(Ordering::Acquire) {
            probation = false;
            deframer.set_max(MAX_FRAME);
        }
        if deframer.feed(data, &mut frames).is_err() {
            Stats::bump(&stats.ice_tcp_rejected);
            return;
        }
        for frame in frames.drain(..) {
            if probation && !conn.answered.load(Ordering::Acquire) {
                probation_frames += 1;
                if probation_frames > PROBATION_MAX_FRAMES
                    || !matches!(frame.first(), Some(0x00 | 0x01))
                {
                    Stats::bump(&stats.ice_tcp_rejected);
                    return;
                }
            }
            if !deliver(frame) {
                return;
            }
        }
    }
}

async fn write_loop(
    conn: Arc<Conn>,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut close: watch::Receiver<bool>,
    stats: Arc<Stats>,
) {
    loop {
        let next = {
            let mut out = conn
                .out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let next = out.pop();
            if out.frames.len() < OUT_QUEUE / 2 && out.bytes < OUT_QUEUE_BYTES / 2 {
                out.overflow_since = None;
            }
            next
        };
        let Some(frame) = next else {
            tokio::select! {
                biased;
                _ = close.wait_for(|c| *c) => break,
                () = conn.ready.notified() => continue,
            }
        };
        let write = tokio::select! {
            biased;
            _ = close.wait_for(|c| *c) => break,
            w = tokio::time::timeout(conn.write_timeout, writer.write_all(&frame)) => w,
        };
        match write {
            Ok(Ok(())) => {}
            Ok(Err(_)) => break,
            Err(_) => {
                Stats::bump(&stats.ice_tcp_closed_stuck);
                break;
            }
        }
    }
    conn.close();
    let _ = tokio::time::timeout(Duration::from_millis(100), writer.shutdown()).await;
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    pub(crate) fn binding_request(username: &str) -> Vec<u8> {
        let mut m = vec![0x00, 0x01, 0, 0, 0x21, 0x12, 0xA4, 0x42];
        m.extend_from_slice(&[7; 12]);
        // A PRIORITY attribute first, so the parser walks past one.
        m.extend_from_slice(&[0x00, 0x24, 0x00, 0x04, 1, 2, 3, 4]);
        let name = username.as_bytes();
        m.extend_from_slice(&[0x00, 0x06]);
        m.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
        m.extend_from_slice(name);
        while m.len() % 4 != 0 {
            m.push(0);
        }
        let body = u16::try_from(m.len() - 20).unwrap();
        m[2..4].copy_from_slice(&body.to_be_bytes());
        m
    }

    #[test]
    fn first_frame_checks() {
        let ok = binding_request("abcd:efgh");
        assert_eq!(binding_request_username(&ok), Ok("abcd:efgh"));
        assert_eq!(
            binding_request_username(&binding_request("nocolon")),
            Err(FirstFrameError::NoUsername)
        );
        let mut resp = ok.clone();
        resp[1] = 0x01;
        resp[0] = 0x01; // Binding success response
        assert_eq!(
            binding_request_username(&resp),
            Err(FirstFrameError::NotBindingRequest)
        );
        let mut cookie = ok.clone();
        cookie[4] = 0;
        assert!(binding_request_username(&cookie).is_err());
        let mut longer = ok.clone();
        longer.extend_from_slice(&[0; 4]);
        assert!(binding_request_username(&longer).is_err());
        let mut no_attr = ok[..20].to_vec();
        no_attr[2..4].copy_from_slice(&[0, 0]);
        assert_eq!(
            binding_request_username(&no_attr),
            Err(FirstFrameError::NoUsername)
        );
        let mut bad_len = ok;
        bad_len[22..24].copy_from_slice(&[0xff, 0xff]);
        assert_eq!(
            binding_request_username(&bad_len),
            Err(FirstFrameError::Malformed)
        );
    }

    fn framed(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            out.extend_from_slice(&u16::try_from(p.len()).unwrap().to_be_bytes());
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn deframer_split_and_multi() {
        let stream = framed(&[b"abc", &[9; 300], b"z"]);
        for chunk in 1..stream.len() {
            let mut d = Deframer::new(MAX_FRAME);
            let mut out = Vec::new();
            for part in stream.chunks(chunk) {
                d.feed(part, &mut out).unwrap();
            }
            assert_eq!(out.len(), 3, "chunk {chunk}");
            assert_eq!(out[0], b"abc");
            assert_eq!(out[1], vec![9; 300]);
            assert_eq!(out[2], b"z");
            assert_eq!(d.buffered(), 0);
        }
    }

    #[test]
    fn deframer_refusals() {
        let mut d = Deframer::new(MAX_FRAME);
        assert_eq!(d.feed(&[0, 0], &mut Vec::new()), Err(DeframeError::Empty));
        let mut d = Deframer::new(PROBATION_MAX_FRAME);
        assert_eq!(
            d.feed(&1536u16.to_be_bytes(), &mut Vec::new()),
            Err(DeframeError::TooLarge(1536))
        );
        d.set_max(MAX_FRAME);
        let mut out = Vec::new();
        d.feed(&1536u16.to_be_bytes(), &mut out).unwrap();
        d.feed(&[1; 1536], &mut out).unwrap();
        assert_eq!(out.len(), 1);
        let mut d = Deframer::new(usize::MAX);
        d.feed(&u16::MAX.to_be_bytes(), &mut out).unwrap();
        assert!(d.buffered() <= 2);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]
        #[test]
        fn deframer_never_panics_and_stays_bounded(
            data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096),
            chunk in 1usize..512,
        ) {
            let mut d = Deframer::new(1535);
            let mut out = Vec::new();
            for part in data.chunks(chunk) {
                if d.feed(part, &mut out).is_err() {
                    break;
                }
                proptest::prop_assert!(d.buffered() <= 2 + 1535);
            }
            for f in &out {
                proptest::prop_assert!(!f.is_empty() && f.len() <= 1535);
            }
        }

        #[test]
        fn deframer_round_trips(frames in proptest::collection::vec(proptest::collection::vec(proptest::prelude::any::<u8>(), 1..2000), 0..8), chunk in 1usize..3000) {
            let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
            let stream = framed(&refs);
            let mut d = Deframer::new(MAX_FRAME);
            let mut out = Vec::new();
            for part in stream.chunks(chunk) {
                d.feed(part, &mut out).unwrap();
            }
            proptest::prop_assert_eq!(out, frames);
        }

        #[test]
        fn first_frame_parser_never_panics(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..1600)) {
            let _ = binding_request_username(&data);
        }
    }
}
