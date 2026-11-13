// Copyright (c) 2020 h3 authors
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit
// persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included
// in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.
//
// Vendored from h3-quinn 0.0.10 (src/lib.rs) and ported to moq-noq 1.3.5;
// the edits are listed in crates/multiview-h3/PROVENANCE.md.

//! QUIC transport for `h3` backed by moq-noq (a port of h3-quinn 0.0.10).
//!
//! Server-side request streams carry the header-block guard
//! ([`crate::guard`]) and the per-request header deadline: both act on the
//! receive path before h3 buffers anything.
//!
//! Server-side, every peer stream is classified by its first varint before
//! h3 sees it, reading at most [`HEAD_MAX`] bytes into a fixed buffer. A
//! stream still unclassified at the header deadline is failed (bidi:
//! `H3_REQUEST_REJECTED`; uni: `H3_STREAM_CREATION_ERROR`); one timer for
//! the earliest such deadline is registered with the accept task, so a
//! silent stream is retired while the accept loop is parked. Of the peer's
//! unidirectional streams only the control stream is handed to h3, behind
//! the [`ControlGuard`] (per-frame caps) and two connection deadlines
//! (SETTINGS arrival, partial-frame age) whose timer is also polled from
//! `poll_accept_recv`. QPACK encoder/decoder streams are read and discarded
//! by the adapter (the server advertises a zero dynamic table, so a peer
//! has almost nothing legitimate to say there) up to [`QPACK_STREAM_MAX`]
//! bytes; push and unknown stream types are stopped with
//! `H3_STREAM_CREATION_ERROR` at classification.

pub mod datagram;

use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{self, ready, Poll},
    time::Duration,
};

use tokio::time::{Instant, Sleep};

use bytes::{Buf, Bytes};
use futures_util::{
    stream::{self},
    Stream, StreamExt,
};
use h3::{
    error::Code,
    proto::varint::VarInt as H3VarInt,
    quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf},
};
use noq::{ReadError, VarInt};

use crate::counters::Counters;
use crate::guard::{ControlGuard, HeaderGuard, Verdict};

/// State shared by the two halves of one stream: the reset code the receive
/// guard asks the send half to apply, and the request-concurrency permit
/// (released when both halves are gone).
#[derive(Debug, Default)]
pub(crate) struct StreamShared {
    abort: AtomicU64,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl StreamShared {
    fn with_permit(permit: Option<tokio::sync::OwnedSemaphorePermit>) -> Arc<Self> {
        Arc::new(Self {
            abort: AtomicU64::new(0),
            _permit: permit,
        })
    }
}

/// `BoxStream` with the `Sync` bound h3's types need.
type BoxStreamSync<'a, T> = Pin<Box<dyn Stream<Item = T> + Sync + Send + 'a>>;

type BiResult = Result<(noq::SendStream, noq::RecvStream), noq::ConnectionError>;
type UniResult = Result<noq::SendStream, noq::ConnectionError>;
type AcceptUniResult = Result<noq::RecvStream, noq::ConnectionError>;

/// Server-side receive-path limits applied to every accepted bidirectional
/// stream (request streams and WebTransport streams alike; the guard steps
/// aside for a WebTransport stream after its first varint).
#[derive(Clone, Debug)]
pub struct GuardConfig {
    /// The h3 `max_field_section_size`; the guard rejects any non-DATA frame
    /// declaring more than this plus [`crate::guard::FRAMING_SLACK`].
    pub max_field_section_size: u64,
    /// Time from stream accept until the first frame (the request HEADERS)
    /// must be complete.
    pub header_deadline: Duration,
    /// From connection start until the peer's SETTINGS frame is complete;
    /// on expiry the connection is closed with `H3_MISSING_SETTINGS`.
    pub settings_deadline: Duration,
    /// How long one control-stream frame may stay incomplete; on expiry the
    /// connection is closed with `H3_EXCESSIVE_LOAD`.
    pub control_frame_deadline: Duration,
    /// Where rejections are counted.
    pub counters: Arc<Counters>,
}

/// Bytes the classifier reads before deciding (two varints of at most 8
/// bytes each: the stream type / frame type and the session id / length).
/// No per-stream buffer larger than this is held before classification.
pub const HEAD_MAX: usize = 16;

/// Bytes a peer may send over the life of its QPACK encoder or decoder
/// stream. The server advertises `SETTINGS_QPACK_MAX_TABLE_CAPACITY` 0 (h3
/// 0.0.8 never sends it, so it is 0), so the peer's encoder can only set
/// capacity 0 and its decoder has nothing to acknowledge.
pub const QPACK_STREAM_MAX: usize = 64;

/// Chunks read per QPACK stream per [`Connection::poll_qpack`] call. A peer
/// delivering the stream as many minimum-size QUIC `STREAM` frames (one
/// byte each) would otherwise make a single accept poll loop up to the
/// lifetime cap in one-byte steps; this bounds that work to a small
/// constant per call regardless of how the peer fragments its writes.
const QPACK_CHUNKS_PER_POLL: usize = 4;

/// HTTP/3 unidirectional stream types the adapter handles.
const UNI_CONTROL: u64 = 0x00;
const UNI_QPACK_ENCODER: u64 = 0x02;
const UNI_QPACK_DECODER: u64 = 0x03;

/// The first bytes of a stream, at most [`HEAD_MAX`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HeadBuf {
    buf: [u8; HEAD_MAX],
    len: usize,
}

impl HeadBuf {
    /// Room left (the read limit for the next classification read).
    pub(crate) fn room(&self) -> usize {
        HEAD_MAX.saturating_sub(self.len)
    }

    /// Appends `data`; bytes beyond the room are dropped and reported.
    pub(crate) fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.room());
        if let (Some(dst), Some(src)) = (self.buf.get_mut(self.len..self.len + n), data.get(..n)) {
            dst.copy_from_slice(src);
            self.len += n;
        }
        data.len() - n
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or_default()
    }
}

/// Connection-level control-stream state shared by the adapter's
/// connection (which owns the timer) and the control stream's guard.
#[derive(Debug, Default)]
struct ControlWatch {
    settings_done: std::sync::atomic::AtomicBool,
    /// When the current partially received control frame started.
    frame_started: std::sync::Mutex<Option<Instant>>,
    /// The accept task, woken when a partial frame starts so the deadline
    /// timer is armed although the control stream itself stays pending.
    waker: std::sync::Mutex<Option<task::Waker>>,
}

impl ControlWatch {
    fn frame_started(&self) -> Option<Instant> {
        *crate::webtransport::unpoison(self.frame_started.lock())
    }

    fn set_frame_started(&self, in_frame: bool) {
        let mut started = crate::webtransport::unpoison(self.frame_started.lock());
        match (in_frame, *started) {
            (true, None) => {
                *started = Some(Instant::now());
                drop(started);
                if let Some(waker) = crate::webtransport::unpoison(self.waker.lock()).take() {
                    waker.wake();
                }
            }
            (false, Some(_)) => *started = None,
            _ => {}
        }
    }
}

/// The connection deadlines (server side).
struct ControlState {
    watch: Arc<ControlWatch>,
    started: Instant,
    settings_deadline: Duration,
    frame_deadline: Duration,
    timer: Option<Pin<Box<Sleep>>>,
}

/// The guard on the peer's control stream.
struct CtlGuard {
    guard: ControlGuard,
    watch: Arc<ControlWatch>,
    conn: noq::Connection,
    counters: Arc<Counters>,
}

impl CtlGuard {
    /// Feeds control-stream bytes; on an oversized frame closes the
    /// connection with `H3_EXCESSIVE_LOAD` and returns the error for h3.
    fn check(&mut self, data: &[u8]) -> Result<(), ConnectionErrorIncoming> {
        if let Verdict::Reject {
            frame_type,
            declared,
        } = self.guard.feed(data)
        {
            Counters::inc(&self.counters.control_stream_rejections);
            tracing::debug!(frame_type, declared, "control frame over its cap");
            let code = Code::H3_EXCESSIVE_LOAD.value();
            self.conn.close(to_varint(code), b"control frame too large");
            return Err(ConnectionErrorIncoming::ApplicationClose { error_code: code });
        }
        if self.guard.first_frame_complete() {
            self.watch.settings_done.store(true, Ordering::Release);
        }
        self.watch.set_frame_started(self.guard.in_frame());
        Ok(())
    }
}

/// A QPACK stream the adapter drains (server side).
struct QpackStream {
    recv: RecvStream,
    encoder: bool,
    seen: usize,
}

/// Arms `timer` for `at` (or disarms it) and polls it. True when it fired.
fn poll_timer(
    timer: &mut Option<Pin<Box<Sleep>>>,
    at: Option<Instant>,
    cx: &mut task::Context<'_>,
) -> bool {
    let Some(at) = at else {
        *timer = None;
        return false;
    };
    let sleep = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
    if sleep.deadline() != at {
        sleep.as_mut().reset(at);
    }
    sleep.as_mut().poll(cx).is_ready()
}

/// A QUIC connection backed by noq; implements [`quic::Connection`].
pub struct Connection {
    conn: noq::Connection,
    incoming_bi: BoxStreamSync<'static, BiResult>,
    opening_bi: Option<BoxStreamSync<'static, BiResult>>,
    incoming_uni: BoxStreamSync<'static, AcceptUniResult>,
    opening_uni: Option<BoxStreamSync<'static, UniResult>>,
    guard: Option<GuardConfig>,
    router: Option<Arc<dyn StreamRouter>>,
    /// Bidi streams accepted from noq whose first varint is still unknown.
    /// Bounded by the QUIC `max_concurrent_bidi_streams` (each entry holds
    /// one open peer stream) and by the header deadline.
    pending_bi: Vec<PendingBi>,
    /// Uni streams whose type varint is still unknown (same bounds).
    pending_uni: Vec<PendingUni>,
    /// One timer for the earliest pending-stream deadline.
    pending_timer: Option<Pin<Box<Sleep>>>,
    /// Control-stream deadlines (server side).
    control: Option<ControlState>,
    /// The peer's QPACK streams (server side; at most one of each kind).
    qpack: Vec<QpackStream>,
}

/// What the server does with peer-initiated streams before h3 sees them.
pub trait StreamRouter: Send + Sync + 'static {
    /// A request stream (first frame is not 0x41): return the concurrency
    /// permit, or `None` to reset the stream with `H3_REQUEST_REJECTED`.
    fn admit_request(&self) -> Option<tokio::sync::OwnedSemaphorePermit>;
    /// A WebTransport bidirectional stream for `session_id` (the CONNECT
    /// stream id); the router owns it from here on.
    fn webtransport_bi(&self, session_id: u64, send: noq::SendStream, recv: RecvStream);
    /// A WebTransport unidirectional stream for `session_id`.
    fn webtransport_uni(&self, session_id: u64, recv: RecvStream);
}

struct PendingBi {
    send: Option<noq::SendStream>,
    recv: RecvStream,
    head: HeadBuf,
    /// The header deadline (server side).
    deadline: Option<Instant>,
}

struct PendingUni {
    recv: RecvStream,
    head: HeadBuf,
    deadline: Option<Instant>,
}

/// Stream type of a WebTransport unidirectional stream.
const WEBTRANSPORT_UNI: u64 = 0x54;

/// Result of looking at the first bytes of a stream.
enum Head {
    /// More bytes needed.
    Incomplete,
    /// Not WebTransport: hand to h3 with these bytes pushed back.
    H3,
    /// WebTransport for this session id; `rest` follows the header.
    WebTransport { session_id: u64, rest: Bytes },
    /// The stream failed (reset by the peer, or by the receive guard) before
    /// it could be classified: drop it.
    Failed,
}

fn classify(head: &[u8], wt_type: u64) -> Head {
    let Some((ty, ty_len)) = crate::varint::decode(head) else {
        return Head::Incomplete;
    };
    if ty != wt_type {
        return Head::H3;
    }
    let Some((session_id, id_len)) = crate::varint::decode(head.get(ty_len..).unwrap_or_default())
    else {
        return Head::Incomplete;
    };
    let rest = head.get(ty_len + id_len..).unwrap_or_default();
    Head::WebTransport {
        session_id,
        rest: Bytes::copy_from_slice(rest),
    }
}

/// What a peer unidirectional stream is, by its type varint.
enum UniKind {
    /// WebTransport (classified by [`classify`]).
    WebTransport,
    /// The HTTP/3 control stream: to h3, behind the control guard.
    Control,
    /// A QPACK stream (`true`: encoder): drained by the adapter.
    Qpack(bool),
    /// Push (never valid toward a server) or an unknown/grease type.
    Other,
}

fn uni_kind(ty: u64) -> UniKind {
    match ty {
        WEBTRANSPORT_UNI => UniKind::WebTransport,
        UNI_CONTROL => UniKind::Control,
        UNI_QPACK_ENCODER => UniKind::Qpack(true),
        UNI_QPACK_DECODER => UniKind::Qpack(false),
        _ => UniKind::Other,
    }
}

/// Reads the next classification bytes (at most the head buffer's room)
/// into `head`. `Ready(Ok(true))`: the peer finished the stream.
fn poll_head(
    recv: &mut RecvStream,
    head: &mut HeadBuf,
    cx: &mut task::Context<'_>,
) -> Poll<Result<bool, ()>> {
    match recv.poll_data_max(cx, head.room().max(1)) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Ok(Some(chunk))) => {
            // `room() >= 1` whenever classification is still incomplete
            // (two varints fit in HEAD_MAX), so nothing is dropped here.
            let _ = head.push(&chunk);
            Poll::Ready(Ok(false))
        }
        Poll::Ready(Ok(None)) => Poll::Ready(Ok(true)),
        Poll::Ready(Err(_)) => Poll::Ready(Err(())),
    }
}

impl Connection {
    /// Wraps a noq connection without receive guards (client side).
    #[must_use]
    pub fn new(conn: noq::Connection) -> Self {
        Self::build(conn, None)
    }

    /// Wraps a noq connection; every accepted bidirectional stream gets the
    /// header-block guard and the header deadline, the control stream its
    /// guard and deadlines (server side).
    #[must_use]
    pub fn with_guard(conn: noq::Connection, guard: GuardConfig) -> Self {
        Self::build(conn, Some(guard))
    }

    fn build(conn: noq::Connection, guard: Option<GuardConfig>) -> Self {
        let control = guard.as_ref().map(|g| ControlState {
            watch: Arc::new(ControlWatch::default()),
            started: Instant::now(),
            settings_deadline: g.settings_deadline,
            frame_deadline: g.control_frame_deadline,
            timer: None,
        });
        Self {
            conn: conn.clone(),
            incoming_bi: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_bi().await, conn))
            })),
            opening_bi: None,
            incoming_uni: Box::pin(stream::unfold(conn, |conn| async {
                Some((conn.accept_uni().await, conn))
            })),
            opening_uni: None,
            guard,
            router: None,
            pending_bi: Vec::new(),
            pending_uni: Vec::new(),
            pending_timer: None,
            control,
            qpack: Vec::new(),
        }
    }

    /// Installs the router that diverts WebTransport streams and admits
    /// request streams (server side).
    #[must_use]
    pub fn with_router(mut self, router: Arc<dyn StreamRouter>) -> Self {
        self.router = Some(router);
        self
    }

    fn stream_deadline(&self) -> Option<Instant> {
        self.guard
            .as_ref()
            .map(|g| Instant::now() + g.header_deadline)
    }

    /// Fails every pending stream whose header deadline passed and keeps
    /// one timer, registered with `cx`, for the earliest remaining one: a
    /// silent stream is retired even while the accept loop is parked.
    fn expire_pending(&mut self, cx: &mut task::Context<'_>) {
        for _ in 0..4 {
            let now = Instant::now();
            let mut i = 0;
            while i < self.pending_bi.len() {
                if self
                    .pending_bi
                    .get(i)
                    .and_then(|e| e.deadline)
                    .is_some_and(|d| d <= now)
                {
                    let mut entry = self.pending_bi.swap_remove(i);
                    if let Some(g) = &self.guard {
                        Counters::inc(&g.counters.header_deadline_rejections);
                    }
                    let code = Code::H3_REQUEST_REJECTED.value();
                    if let Some(mut send) = entry.send.take() {
                        let _ = send.reset(to_varint(code));
                    }
                    entry.recv.stop_final(code);
                } else {
                    i += 1;
                }
            }
            let mut i = 0;
            while i < self.pending_uni.len() {
                if self
                    .pending_uni
                    .get(i)
                    .and_then(|e| e.deadline)
                    .is_some_and(|d| d <= now)
                {
                    let mut entry = self.pending_uni.swap_remove(i);
                    if let Some(g) = &self.guard {
                        Counters::inc(&g.counters.header_deadline_rejections);
                    }
                    entry
                        .recv
                        .stop_final(Code::H3_STREAM_CREATION_ERROR.value());
                } else {
                    i += 1;
                }
            }
            let earliest = self
                .pending_bi
                .iter()
                .filter_map(|e| e.deadline)
                .chain(self.pending_uni.iter().filter_map(|e| e.deadline))
                .min();
            if !poll_timer(&mut self.pending_timer, earliest, cx) {
                return;
            }
        }
        // The timer keeps firing without an entry expiring (clock skew):
        // poll again soon instead of spinning here.
        cx.waker().wake_by_ref();
    }

    /// The connection deadlines: SETTINGS arrival and the age of a partial
    /// control frame. Closes the connection and returns its error on
    /// expiry; otherwise the timer for the next one is registered with `cx`.
    fn poll_control_deadlines(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Result<(), ConnectionErrorIncoming> {
        let Some(control) = self.control.as_mut() else {
            return Ok(());
        };
        {
            let mut waker = crate::webtransport::unpoison(control.watch.waker.lock());
            if !waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *waker = Some(cx.waker().clone());
            }
        }
        let settings_at = (!control.watch.settings_done.load(Ordering::Acquire))
            .then(|| control.started + control.settings_deadline);
        let frame_at = control
            .watch
            .frame_started()
            .map(|t| t + control.frame_deadline);
        let at = match (settings_at, frame_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if !poll_timer(&mut control.timer, at, cx) {
            return Ok(());
        }
        let now = Instant::now();
        let (code, counter) = if settings_at.is_some_and(|t| t <= now) {
            (Code::H3_MISSING_SETTINGS, "settings")
        } else {
            (Code::H3_EXCESSIVE_LOAD, "frame")
        };
        if let Some(g) = &self.guard {
            Counters::inc(&g.counters.control_stream_rejections);
        }
        tracing::debug!(deadline = counter, "control stream deadline expired");
        self.conn.close(to_varint(code.value()), b"control stream");
        Err(ConnectionErrorIncoming::ApplicationClose {
            error_code: code.value(),
        })
    }

    /// Drains the peer's QPACK streams (at most [`QPACK_STREAM_MAX`] bytes
    /// each over their life, else the connection is closed). Reads at most
    /// [`QPACK_CHUNKS_PER_POLL`] chunks per stream per call: a peer that
    /// delivers its stream as many minimum-size frames cannot make one
    /// accept poll do work proportional to the receive window. If the
    /// budget is used up without the stream going pending, this wakes the
    /// task and returns so other accept-path work (and other streams) get a
    /// turn; the stream's own waker, already registered by the last
    /// `poll_data_max` call, brings this one back too.
    fn poll_qpack(&mut self, cx: &mut task::Context<'_>) -> Result<(), ConnectionErrorIncoming> {
        let mut i = 0;
        let mut budget_exhausted = false;
        while i < self.qpack.len() {
            let Some(entry) = self.qpack.get_mut(i) else {
                break;
            };
            let mut chunks = 0;
            let ended = loop {
                if chunks >= QPACK_CHUNKS_PER_POLL {
                    budget_exhausted = true;
                    break false;
                }
                let room = QPACK_STREAM_MAX.saturating_sub(entry.seen) + 1;
                match entry.recv.poll_data_max(cx, room) {
                    Poll::Pending => break false,
                    Poll::Ready(Ok(Some(chunk))) => {
                        chunks += 1;
                        if let Some(g) = &self.guard {
                            Counters::inc(&g.counters.qpack_chunks_read);
                            Counters::max(
                                &g.counters.qpack_max_chunks_per_poll,
                                u64::try_from(chunks).unwrap_or(u64::MAX),
                            );
                        }
                        entry.seen = entry.seen.saturating_add(chunk.len());
                        if entry.seen > QPACK_STREAM_MAX {
                            let code = if entry.encoder {
                                Code::QPACK_ENCODER_STREAM_ERROR
                            } else {
                                Code::QPACK_DECODER_STREAM_ERROR
                            };
                            if let Some(g) = &self.guard {
                                Counters::inc(&g.counters.control_stream_rejections);
                            }
                            self.conn.close(to_varint(code.value()), b"qpack stream");
                            return Err(ConnectionErrorIncoming::ApplicationClose {
                                error_code: code.value(),
                            });
                        }
                    }
                    Poll::Ready(Ok(None) | Err(_)) => break true,
                }
            };
            if ended {
                // Keep the kind registered (a second stream of the same kind
                // is still an error); only release the QUIC stream.
                entry.recv.stop_final(Code::H3_NO_ERROR.value());
            }
            i += 1;
        }
        if budget_exhausted {
            if let Some(g) = &self.guard {
                Counters::inc(&g.counters.qpack_budget_yields);
            }
            cx.waker().wake_by_ref();
        }
        Ok(())
    }

    /// Polls every pending bidi stream once; returns a request stream ready
    /// for h3 if one resolved.
    fn poll_pending_bi<B: Buf>(&mut self, cx: &mut task::Context<'_>) -> Option<BidiStream<B>> {
        let router = self.router.clone()?;
        let mut i = 0;
        while i < self.pending_bi.len() {
            let Some(entry) = self.pending_bi.get_mut(i) else {
                break;
            };
            // Every entry is polled (registering its waker) on every call.
            let outcome = loop {
                match classify(entry.head.as_slice(), crate::guard::WEBTRANSPORT_BIDI) {
                    Head::Incomplete => {}
                    other => break Some(other),
                }
                match poll_head(&mut entry.recv, &mut entry.head, cx) {
                    Poll::Pending => break None,
                    Poll::Ready(Ok(false)) => {}
                    // FIN before the head: h3 handles (and errors) it.
                    Poll::Ready(Ok(true)) => break Some(Head::H3),
                    Poll::Ready(Err(())) => break Some(Head::Failed),
                }
            };
            let Some(outcome) = outcome else {
                i += 1;
                continue;
            };
            let mut entry = self.pending_bi.swap_remove(i);
            let head = Bytes::copy_from_slice(entry.head.as_slice());
            match outcome {
                Head::WebTransport { session_id, rest } => {
                    entry.recv.guard = None;
                    entry.recv.unread(rest);
                    if let Some(send) = entry.send.take() {
                        router.webtransport_bi(session_id, send, entry.recv);
                    }
                }
                Head::Failed => {
                    let code = entry.recv.abort.abort.load(Ordering::Acquire);
                    let code = if code == 0 {
                        Code::H3_REQUEST_CANCELLED.value()
                    } else {
                        code
                    };
                    if let Some(mut send) = entry.send.take() {
                        let _ = send.reset(to_varint(code));
                    }
                }
                Head::H3 | Head::Incomplete => {
                    entry.recv.unread(head);
                    let Some(send) = entry.send.take() else {
                        continue;
                    };
                    if let Some(permit) = router.admit_request() {
                        let shared = StreamShared::with_permit(Some(permit));
                        entry.recv.abort = Arc::clone(&shared);
                        return Some(BidiStream {
                            send: SendStream::new(send, shared),
                            recv: entry.recv,
                        });
                    }
                    let mut send = send;
                    let _ = send.reset(to_varint(Code::H3_REQUEST_REJECTED.value()));
                    entry.recv.stop_final(Code::H3_REQUEST_REJECTED.value());
                }
            }
        }
        None
    }

    /// Polls every pending uni stream once; returns the control stream for
    /// h3 when it resolved. Other stream kinds are routed (WebTransport),
    /// drained (QPACK) or stopped (push, unknown).
    fn poll_pending_uni(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Result<Option<RecvStream>, ConnectionErrorIncoming> {
        let Some(router) = self.router.clone() else {
            return Ok(None);
        };
        let mut i = 0;
        while i < self.pending_uni.len() {
            let Some(entry) = self.pending_uni.get_mut(i) else {
                break;
            };
            let outcome = loop {
                if let Some((ty, _)) = crate::varint::decode(entry.head.as_slice()) {
                    match uni_kind(ty) {
                        UniKind::WebTransport => {
                            match classify(entry.head.as_slice(), WEBTRANSPORT_UNI) {
                                Head::Incomplete => {}
                                other => break Some(Ok(other)),
                            }
                        }
                        // Client side (no guard): everything else to h3.
                        _ if self.guard.is_none() => break Some(Ok(Head::H3)),
                        kind => break Some(Err(kind)),
                    }
                }
                match poll_head(&mut entry.recv, &mut entry.head, cx) {
                    Poll::Pending => break None,
                    Poll::Ready(Ok(false)) => {}
                    // Closed or reset before the header: tolerated, dropped
                    // (RFC 9114 section 6.2).
                    Poll::Ready(Ok(true) | Err(())) => break Some(Ok(Head::Failed)),
                }
            };
            let Some(outcome) = outcome else {
                i += 1;
                continue;
            };
            let mut entry = self.pending_uni.swap_remove(i);
            match outcome {
                Ok(Head::WebTransport { session_id, rest }) => {
                    entry.recv.unread(rest);
                    router.webtransport_uni(session_id, entry.recv);
                }
                Ok(Head::H3) if self.guard.is_none() => {
                    let head = Bytes::copy_from_slice(entry.head.as_slice());
                    entry.recv.unread(head);
                    return Ok(Some(entry.recv));
                }
                Ok(Head::Failed | Head::H3 | Head::Incomplete) => entry.recv.fin = true,
                Err(UniKind::Control) => {
                    let head = Bytes::copy_from_slice(entry.head.as_slice());
                    if let (Some(control), Some(g)) = (&self.control, &self.guard) {
                        let mut guard = CtlGuard {
                            guard: ControlGuard::new(),
                            watch: Arc::clone(&control.watch),
                            conn: self.conn.clone(),
                            counters: Arc::clone(&g.counters),
                        };
                        // The head bytes bypass `poll_data`'s guard (they
                        // come back as the prefix): feed them here.
                        guard.check(&head)?;
                        entry.recv.ctl = Some(guard);
                    }
                    entry.recv.unread(head);
                    return Ok(Some(entry.recv));
                }
                Err(UniKind::Qpack(encoder)) => {
                    if self.qpack.iter().any(|q| q.encoder == encoder) {
                        let code = Code::H3_STREAM_CREATION_ERROR;
                        self.conn
                            .close(to_varint(code.value()), b"second qpack stream");
                        return Err(ConnectionErrorIncoming::ApplicationClose {
                            error_code: code.value(),
                        });
                    }
                    let seen = entry.head.as_slice().len().saturating_sub(1);
                    self.qpack.push(QpackStream {
                        recv: entry.recv,
                        encoder,
                        seen,
                    });
                    // `poll_qpack` already ran in this pass, and the bytes
                    // after the head may all be buffered already, so the
                    // stream's own waker will not fire again: re-poll now
                    // or the drain (and its cap) would wait for unrelated
                    // activity.
                    cx.waker().wake_by_ref();
                }
                Err(UniKind::Other | UniKind::WebTransport) => {
                    // RFC 9114 section 6.2: abort reading unknown types with
                    // H3_STREAM_CREATION_ERROR (a push stream toward a
                    // server is not valid either).
                    entry
                        .recv
                        .stop_final(Code::H3_STREAM_CREATION_ERROR.value());
                }
            }
        }
        Ok(None)
    }

    /// The underlying noq connection.
    #[must_use]
    pub fn inner(&self) -> &noq::Connection {
        &self.conn
    }
}

/// The internal error behind `StreamErrorIncoming::Unknown` for conditions
/// this adapter raises itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterError {
    /// The receive guard failed the stream with this h3 code.
    Rejected(u64),
    /// The stream handle is a placeholder with no QUIC stream behind it.
    Detached,
    /// A write was issued while a previous one was still buffered (an h3
    /// trait misuse).
    NotReady,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(code) => write!(f, "stream rejected by the receive guard ({code:#x})"),
            Self::Detached => f.write_str("detached stream handle"),
            Self::NotReady => f.write_str("send stream is not ready"),
        }
    }
}

impl StdError for AdapterError {}

impl<B> quic::Connection<B> for Connection
where
    B: Buf,
{
    type RecvStream = RecvStream;
    type OpenStreams = OpenStreams;

    fn poll_accept_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        loop {
            if let Some(ready) = self.poll_pending_bi(cx) {
                return Poll::Ready(Ok(ready));
            }
            self.expire_pending(cx);
            let (send, recv) = match ready!(self.incoming_bi.poll_next_unpin(cx)) {
                Some(res) => res.map_err(convert_connection_error)?,
                None => return Poll::Ready(Err(closed_stream_source())),
            };
            let abort = StreamShared::with_permit(None);
            let mut recv = RecvStream::new(recv, Arc::clone(&abort));
            if let Some(guard) = &self.guard {
                recv.guard = Some(RecvGuard::new(guard));
            }
            if self.router.is_none() {
                return Poll::Ready(Ok(Self::BidiStream {
                    send: SendStream::new(send, abort),
                    recv,
                }));
            }
            let deadline = self.stream_deadline();
            self.pending_bi.push(PendingBi {
                send: Some(send),
                recv,
                head: HeadBuf::default(),
                deadline,
            });
        }
    }

    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        loop {
            self.poll_control_deadlines(cx)?;
            self.poll_qpack(cx)?;
            if let Some(ready) = self.poll_pending_uni(cx)? {
                return Poll::Ready(Ok(ready));
            }
            self.expire_pending(cx);
            let recv = match ready!(self.incoming_uni.poll_next_unpin(cx)) {
                Some(res) => res.map_err(convert_connection_error)?,
                None => return Poll::Ready(Err(closed_stream_source())),
            };
            let recv = RecvStream::new(recv, StreamShared::with_permit(None));
            if self.router.is_none() {
                return Poll::Ready(Ok(recv));
            }
            let deadline = self.stream_deadline();
            self.pending_uni.push(PendingUni {
                recv,
                head: HeadBuf::default(),
                deadline,
            });
        }
    }

    fn opener(&self) -> Self::OpenStreams {
        OpenStreams::new(self.conn.clone())
    }
}

/// The streams `unfold` never ends, so `None` cannot happen; it is reported
/// as an internal error instead of a panic.
fn closed_stream_source() -> ConnectionErrorIncoming {
    ConnectionErrorIncoming::InternalError("noq stream source ended".into())
}

fn convert_connection_error(e: noq::ConnectionError) -> ConnectionErrorIncoming {
    match e {
        noq::ConnectionError::ApplicationClosed(application_close) => {
            ConnectionErrorIncoming::ApplicationClose {
                error_code: application_close.error_code.into_inner(),
            }
        }
        noq::ConnectionError::TimedOut => ConnectionErrorIncoming::Timeout,
        error => ConnectionErrorIncoming::Undefined(Arc::new(error)),
    }
}

fn to_varint(code: u64) -> VarInt {
    VarInt::from_u64(code).unwrap_or(VarInt::MAX)
}

fn to_stream_id(id: noq::StreamId) -> StreamId {
    StreamId::from(H3VarInt::from_u64(u64::from(id)).unwrap_or(H3VarInt::MAX))
}

fn open_bi_stream(conn: &noq::Connection) -> BoxStreamSync<'static, BiResult> {
    Box::pin(stream::unfold(conn.clone(), |conn| async {
        Some((conn.open_bi().await, conn))
    }))
}

fn open_uni_stream(conn: &noq::Connection) -> BoxStreamSync<'static, UniResult> {
    Box::pin(stream::unfold(conn.clone(), |conn| async {
        Some((conn.open_uni().await, conn))
    }))
}

fn poll_open_bi<B: Buf>(
    slot: &mut Option<BoxStreamSync<'static, BiResult>>,
    conn: &noq::Connection,
    cx: &mut task::Context<'_>,
) -> Poll<Result<BidiStream<B>, StreamErrorIncoming>> {
    let bi = slot.get_or_insert_with(|| open_bi_stream(conn));
    let (send, recv) = match ready!(bi.poll_next_unpin(cx)) {
        Some(res) => res.map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: convert_connection_error(e),
        })?,
        None => {
            return Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: closed_stream_source(),
            }))
        }
    };
    let abort = StreamShared::with_permit(None);
    Poll::Ready(Ok(BidiStream {
        send: SendStream::new(send, Arc::clone(&abort)),
        recv: RecvStream::new(recv, abort),
    }))
}

fn poll_open_uni<B: Buf>(
    slot: &mut Option<BoxStreamSync<'static, UniResult>>,
    conn: &noq::Connection,
    cx: &mut task::Context<'_>,
) -> Poll<Result<SendStream<B>, StreamErrorIncoming>> {
    let uni = slot.get_or_insert_with(|| open_uni_stream(conn));
    let send = match ready!(uni.poll_next_unpin(cx)) {
        Some(res) => res.map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: convert_connection_error(e),
        })?,
        None => {
            return Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: closed_stream_source(),
            }))
        }
    };
    Poll::Ready(Ok(SendStream::new(send, StreamShared::with_permit(None))))
}

impl<B> quic::OpenStreams<B> for Connection
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        poll_open_bi(&mut self.opening_bi, &self.conn, cx)
    }

    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        poll_open_uni(&mut self.opening_uni, &self.conn, cx)
    }

    fn close(&mut self, code: Code, reason: &[u8]) {
        self.conn.close(to_varint(code.value()), reason);
    }
}

/// Stream opener backed by a noq connection; implements [`quic::OpenStreams`].
pub struct OpenStreams {
    conn: noq::Connection,
    opening_bi: Option<BoxStreamSync<'static, BiResult>>,
    opening_uni: Option<BoxStreamSync<'static, UniResult>>,
}

impl OpenStreams {
    /// An opener for `conn`.
    #[must_use]
    pub fn new(conn: noq::Connection) -> Self {
        Self {
            conn,
            opening_bi: None,
            opening_uni: None,
        }
    }
}

impl<B> quic::OpenStreams<B> for OpenStreams
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        poll_open_bi(&mut self.opening_bi, &self.conn, cx)
    }

    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        poll_open_uni(&mut self.opening_uni, &self.conn, cx)
    }

    fn close(&mut self, code: Code, reason: &[u8]) {
        self.conn.close(to_varint(code.value()), reason);
    }
}

impl Clone for OpenStreams {
    fn clone(&self) -> Self {
        Self::new(self.conn.clone())
    }
}

/// noq-backed bidirectional stream; implements [`quic::BidiStream`].
pub struct BidiStream<B>
where
    B: Buf,
{
    send: SendStream<B>,
    recv: RecvStream,
}

impl<B: Buf> BidiStream<B> {
    /// A placeholder that reports stream id `id` but has no QUIC stream
    /// behind it: reads end immediately and writes fail. The server uses it
    /// to release h3's per-request bookkeeping for a stream h3 counted but
    /// that never becomes an h3 request (a rejected stream or a WebTransport
    /// stream), see `server::release_request_slot`.
    #[must_use]
    pub fn detached(id: StreamId) -> Self {
        let abort = StreamShared::with_permit(None);
        Self {
            send: SendStream {
                stream: None,
                id,
                writing: None,
                abort: Arc::clone(&abort),
            },
            recv: RecvStream {
                stream: None,
                id,
                prefix: None,
                fin: true,
                guard: None,
                ctl: None,
                abort,
            },
        }
    }

    /// Pushes already-read bytes back to the front of the receive side.
    pub fn unread(&mut self, bytes: Bytes) {
        self.recv.unread(bytes);
    }

    /// Splits into the send and receive halves.
    #[must_use]
    pub fn into_parts(self) -> (SendStream<B>, RecvStream) {
        (self.send, self.recv)
    }

    /// Fails both directions with `code` (`STOP_SENDING` + `RESET_STREAM`).
    pub fn abort(&mut self, code: Code) {
        quic::RecvStream::stop_sending(&mut self.recv, code.value());
        quic::SendStream::reset(&mut self.send, code.value());
    }
}

impl<B> quic::BidiStream<B> for BidiStream<B>
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type RecvStream = RecvStream;

    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

impl<B: Buf> quic::RecvStream for BidiStream<B> {
    type Buf = Bytes;

    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        let res = ready!(self.recv.poll_data(cx));
        if res.is_err() {
            // Unsplit (the request is still being resolved): fail the send
            // side now rather than on its next use.
            self.send.apply_abort();
        }
        Poll::Ready(res)
    }

    fn stop_sending(&mut self, error_code: u64) {
        self.recv.stop_sending(error_code);
    }

    fn recv_id(&self) -> StreamId {
        self.recv.recv_id()
    }
}

impl<B> quic::SendStream<B> for BidiStream<B>
where
    B: Buf,
{
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_ready(cx)
    }

    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_finish(cx)
    }

    fn reset(&mut self, reset_code: u64) {
        self.send.reset(reset_code);
    }

    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        self.send.send_data(data)
    }

    fn send_id(&self) -> StreamId {
        self.send.send_id()
    }
}

impl<B> quic::SendStreamUnframed<B> for BidiStream<B>
where
    B: Buf,
{
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        self.send.poll_send(cx, buf)
    }
}

/// The guard state on a server-side receive stream.
struct RecvGuard {
    guard: HeaderGuard,
    /// The header deadline as a fixed instant, measured from stream accept
    /// (set at construction, never recomputed). `None` once the first frame
    /// (the request HEADERS) is complete: from then on no deadline applies
    /// to the stream (a request body, a WebTransport CONNECT stream that
    /// lives as long as its session). #382: clearing only the `Sleep` let
    /// the next pending read re-create it from this past instant, which
    /// reset every CONNECT stream `header_deadline` after accept.
    deadline_at: Option<Instant>,
    /// The `Sleep` backing `deadline_at`, created lazily on the first
    /// `poll_data_max` call rather than at construction: while the stream
    /// sits in `pending_bi`/awaiting the request semaphore, nothing polls
    /// this guard (the pending phase is covered by the connection-level
    /// `pending_timer` instead), so eagerly arming a `Sleep` here for every
    /// accepted bidi stream (up to 128 per connection) would be dead
    /// weight. Cleared once the header guard clears its own deadline
    /// (first frame complete) or the guard is dropped entirely.
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    counters: Arc<Counters>,
}

impl RecvGuard {
    fn new(config: &GuardConfig) -> Self {
        Self {
            guard: HeaderGuard::new(config.max_field_section_size),
            deadline_at: Some(Instant::now() + config.header_deadline),
            deadline: None,
            counters: Arc::clone(&config.counters),
        }
    }

    /// Polls the deadline, creating its `Sleep` on first use. `deadline_at`
    /// in the past (as in a unit test) fires on this first poll.
    fn poll_deadline(&mut self, cx: &mut task::Context<'_>) -> bool {
        let Some(at) = self.deadline_at else {
            return false;
        };
        let sleep = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
        sleep.as_mut().poll(cx).is_ready()
    }

    /// The request HEADERS are complete: no deadline applies any more.
    fn clear_deadline(&mut self) {
        self.deadline_at = None;
        self.deadline = None;
    }
}

/// noq-backed receive stream; implements [`quic::RecvStream`].
pub struct RecvStream {
    stream: Option<noq::RecvStream>,
    id: StreamId,
    /// Bytes already read (by the server's stream classifier) and pushed
    /// back; returned before anything else. Already seen by the guard.
    prefix: Option<Bytes>,
    /// The peer finished the stream and every byte was returned.
    fin: bool,
    guard: Option<RecvGuard>,
    /// The control-stream guard (the peer's control stream, server side).
    ctl: Option<CtlGuard>,
    /// Shared with the send half: a non-zero value is the h3 code the send
    /// half must reset with (set when the guard fails the stream).
    abort: Arc<StreamShared>,
}

impl RecvStream {
    pub(crate) fn new(stream: noq::RecvStream, abort: Arc<StreamShared>) -> Self {
        Self {
            id: to_stream_id(stream.id()),
            stream: Some(stream),
            prefix: None,
            fin: false,
            guard: None,
            ctl: None,
            abort,
        }
    }

    /// Stops the stream with `code` and marks it done (no further stop on
    /// drop).
    fn stop_final(&mut self, code: u64) {
        if let Some(stream) = self.stream.as_mut() {
            let _ = stream.stop(to_varint(code));
        }
        self.fin = true;
    }

    /// Pushes bytes back to the front of the stream (at most one chunk; a
    /// second call prepends to the first).
    pub fn unread(&mut self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        self.prefix = Some(match self.prefix.take() {
            None => bytes,
            Some(rest) => {
                let mut joined = Vec::with_capacity(bytes.len() + rest.len());
                joined.extend_from_slice(&bytes);
                joined.extend_from_slice(&rest);
                Bytes::from(joined)
            }
        });
    }

    /// The QUIC stream and any pushed-back bytes, for a WebTransport stream
    /// leaving h3. The guard is dropped with it.
    pub fn into_raw(mut self) -> (Option<noq::RecvStream>, Option<Bytes>) {
        self.fin = true;
        (self.stream.take(), self.prefix.take())
    }

    /// True once the peer's FIN was reached.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.fin
    }

    /// Resolves when the peer resets the stream (`Some(code)`) or the stream
    /// otherwise ends (`None`).
    ///
    /// # Errors
    ///
    /// The connection was lost.
    pub async fn received_reset(&mut self) -> Result<Option<u64>, noq::ResetError> {
        if self.fin {
            return Ok(None);
        }
        match self.stream.as_mut() {
            Some(stream) => Ok(stream.received_reset().await?.map(VarInt::into_inner)),
            None => Ok(None),
        }
    }

    fn reject(&mut self, code: Code) -> StreamErrorIncoming {
        if let Some(stream) = self.stream.as_mut() {
            let _ = stream.stop(to_varint(code.value()));
        }
        self.abort.abort.store(code.value(), Ordering::Release);
        self.guard = None;
        StreamErrorIncoming::Unknown(Box::new(AdapterError::Rejected(code.value())))
    }
}

impl RecvStream {
    /// [`quic::RecvStream::poll_data`] reading at most `max` bytes (the
    /// classifier's bounded read; a pushed-back prefix is returned whole).
    fn poll_data_max(
        &mut self,
        cx: &mut task::Context<'_>,
        max: usize,
    ) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
        let code = self.abort.abort.load(Ordering::Acquire);
        if code != 0 {
            return Poll::Ready(Err(StreamErrorIncoming::Unknown(Box::new(
                AdapterError::Rejected(code),
            ))));
        }
        if let Some(prefix) = self.prefix.take() {
            return Poll::Ready(Ok(Some(prefix)));
        }
        let Some(stream) = self.stream.as_mut() else {
            self.fin = true;
            return Poll::Ready(Ok(None));
        };
        // `read_chunk` is cancel safe and stateless between polls (it only
        // calls noq's `poll_read_chunk`), so a fresh future per poll is
        // equivalent to h3-quinn's stored `ReusableBoxFuture`.
        let polled = {
            let fut = stream.read_chunk(max);
            let mut fut = std::pin::pin!(fut);
            fut.as_mut().poll(cx)
        };
        let chunk = match polled {
            Poll::Ready(res) => res.map_err(convert_read_error_to_stream_error)?,
            Poll::Pending => {
                let expired = self.guard.as_mut().is_some_and(|g| g.poll_deadline(cx));
                if expired {
                    if let Some(g) = &self.guard {
                        Counters::inc(&g.counters.header_deadline_rejections);
                    }
                    return Poll::Ready(Err(self.reject(Code::H3_REQUEST_REJECTED)));
                }
                return Poll::Pending;
            }
        };
        if chunk.is_none() {
            self.fin = true;
        }
        if let (Some(guard), Some(data)) = (self.guard.as_mut(), chunk.as_ref()) {
            match guard.guard.feed(data) {
                Verdict::Continue => {
                    if guard.guard.first_frame_complete() {
                        guard.clear_deadline();
                    }
                }
                Verdict::WebTransport => self.guard = None,
                Verdict::Reject { .. } => {
                    Counters::inc(&guard.counters.header_guard_rejections);
                    return Poll::Ready(Err(self.reject(Code::H3_EXCESSIVE_LOAD)));
                }
            }
        }
        if let (Some(ctl), Some(data)) = (self.ctl.as_mut(), chunk.as_ref()) {
            if let Err(connection_error) = ctl.check(data) {
                return Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error,
                }));
            }
        }
        Poll::Ready(Ok(chunk))
    }
}

impl quic::RecvStream for RecvStream {
    type Buf = Bytes;

    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        self.poll_data_max(cx, usize::MAX)
    }

    fn stop_sending(&mut self, error_code: u64) {
        if let Some(stream) = self.stream.as_mut() {
            let _ = stream.stop(to_varint(error_code));
        }
    }

    fn recv_id(&self) -> StreamId {
        self.id
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        // noq stops an unfinished stream with code 0 on drop, which is not an
        // HTTP/3 code; abandon reading with H3_NO_ERROR instead (RFC 9114
        // section 4.1: a server may stop reading once it has responded).
        if !self.fin {
            if let Some(stream) = self.stream.as_mut() {
                let _ = stream.stop(to_varint(Code::H3_NO_ERROR.value()));
            }
        }
    }
}

fn convert_read_error_to_stream_error(error: ReadError) -> StreamErrorIncoming {
    match error {
        ReadError::Reset(var_int) => StreamErrorIncoming::StreamTerminated {
            error_code: var_int.into_inner(),
        },
        ReadError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ (ReadError::ClosedStream | ReadError::ZeroRttRejected) => {
            StreamErrorIncoming::Unknown(Box::new(error))
        }
    }
}

fn convert_write_error_to_stream_error(error: noq::WriteError) -> StreamErrorIncoming {
    match error {
        noq::WriteError::Stopped(var_int) => StreamErrorIncoming::StreamTerminated {
            error_code: var_int.into_inner(),
        },
        noq::WriteError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ (noq::WriteError::ClosedStream | noq::WriteError::ZeroRttRejected) => {
            StreamErrorIncoming::Unknown(Box::new(error))
        }
    }
}

/// noq-backed send stream; implements [`quic::SendStream`].
pub struct SendStream<B: Buf> {
    stream: Option<noq::SendStream>,
    id: StreamId,
    writing: Option<WriteBuf<B>>,
    abort: Arc<StreamShared>,
}

impl<B> SendStream<B>
where
    B: Buf,
{
    pub(crate) fn new(stream: noq::SendStream, abort: Arc<StreamShared>) -> Self {
        Self {
            id: to_stream_id(stream.id()),
            stream: Some(stream),
            writing: None,
            abort,
        }
    }

    /// Applies a reset requested by the receive guard. Returns the error to
    /// report when one was applied.
    fn apply_abort(&mut self) -> Option<StreamErrorIncoming> {
        let code = self.abort.abort.load(Ordering::Acquire);
        if code == 0 {
            return None;
        }
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.reset(to_varint(code));
        }
        self.writing = None;
        Some(StreamErrorIncoming::Unknown(Box::new(
            AdapterError::Rejected(code),
        )))
    }

    fn stream_mut(&mut self) -> Result<&mut noq::SendStream, StreamErrorIncoming> {
        if let Some(err) = self.apply_abort() {
            return Err(err);
        }
        self.stream
            .as_mut()
            .ok_or_else(|| StreamErrorIncoming::Unknown(Box::new(AdapterError::Detached)))
    }
}

impl<B> quic::SendStream<B> for SendStream<B>
where
    B: Buf,
{
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        if let Some(err) = self.apply_abort() {
            return Poll::Ready(Err(err));
        }
        if let Some(ref mut data) = self.writing {
            let Some(stream) = self.stream.as_mut() else {
                return Poll::Ready(Err(StreamErrorIncoming::Unknown(Box::new(
                    AdapterError::Detached,
                ))));
            };
            while data.has_remaining() {
                let written = ready!(Pin::new(&mut *stream).poll_write(cx, data.chunk()))
                    .map_err(convert_write_error_to_stream_error)?;
                data.advance(written);
            }
        }
        // all data is written
        self.writing = None;
        Poll::Ready(Ok(()))
    }

    fn poll_finish(
        &mut self,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), StreamErrorIncoming>> {
        Poll::Ready(self.stream_mut().and_then(|s| {
            s.finish()
                .map_err(|e| StreamErrorIncoming::Unknown(Box::new(e)))
        }))
    }

    fn reset(&mut self, reset_code: u64) {
        if let Some(stream) = self.stream.as_mut() {
            let _ = stream.reset(to_varint(reset_code));
        }
    }

    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        if let Some(err) = self.apply_abort() {
            return Err(err);
        }
        if self.writing.is_some() {
            // This can only happen if the traits are misused by h3 itself.
            // Close the connection with H3_INTERNAL_ERROR.
            tracing::error!("send_data called while send stream is not ready");
            return Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(
                    "internal error in the http stack".into(),
                ),
            });
        }
        self.writing = Some(data.into());
        Ok(())
    }

    fn send_id(&self) -> StreamId {
        self.id
    }
}

impl<B> quic::SendStreamUnframed<B> for SendStream<B>
where
    B: Buf,
{
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        if self.writing.is_some() {
            // h3-quinn panics here ("a bug in the implementation"); an error
            // fails only this stream.
            return Poll::Ready(Err(StreamErrorIncoming::Unknown(Box::new(
                AdapterError::NotReady,
            ))));
        }
        let stream = match self.stream_mut() {
            Ok(stream) => stream,
            Err(err) => return Poll::Ready(Err(err)),
        };
        match ready!(Pin::new(stream).poll_write(cx, buf.chunk())) {
            Ok(written) => {
                buf.advance(written);
                Poll::Ready(Ok(written))
            }
            Err(err) => Poll::Ready(Err(convert_write_error_to_stream_error(err))),
        }
    }
}

impl<B: Buf> SendStream<B> {
    /// Sets the noq send priority (higher is sent first).
    pub fn set_priority(&self, priority: i32) {
        if let Some(stream) = &self.stream {
            let _ = stream.set_priority(priority);
        }
    }

    /// Resolves when the peer stops the stream or acknowledges all data after
    /// a finish. `Some(code)` is the peer's `STOP_SENDING` code.
    ///
    /// # Errors
    ///
    /// The connection was lost.
    pub async fn stopped(&self) -> Result<Option<u64>, noq::StoppedError> {
        match &self.stream {
            Some(stream) => Ok(stream.stopped().await?.map(VarInt::into_inner)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_buffer_is_capped_at_head_max() {
        let mut head = HeadBuf::default();
        assert_eq!(head.room(), HEAD_MAX);
        assert_eq!(head.push(&[0x41]), 0);
        // A whole chunk offered: only the room is kept, the rest reported.
        assert_eq!(head.push(&[0u8; 4096]), 4096 - (HEAD_MAX - 1));
        assert_eq!(head.as_slice().len(), HEAD_MAX);
        assert_eq!(head.room(), 0);
        assert_eq!(head.push(&[1, 2, 3]), 3);
        assert_eq!(head.as_slice().len(), HEAD_MAX);
    }

    #[test]
    fn qpack_chunk_budget_is_a_small_fixed_constant() {
        // #358 review2 F2: the budget bounds work per `poll_qpack` call to
        // a small constant regardless of how a peer fragments its writes,
        // not "read until pending". It must be smaller than a one-byte-per-
        // frame delivery of the lifetime cap, or it would not bound
        // anything in the worst case the finding describes. Both sides of
        // this are compile-time constants, so the check is evaluated once
        // at compile time instead of asserting a constant at runtime.
        const _: () = assert!(QPACK_CHUNKS_PER_POLL > 0);
        const _: () = assert!(QPACK_CHUNKS_PER_POLL < QPACK_STREAM_MAX);
    }

    fn guard_config() -> GuardConfig {
        GuardConfig {
            max_field_section_size: 16 * 1024,
            header_deadline: Duration::from_secs(1),
            settings_deadline: Duration::from_secs(1),
            control_frame_deadline: Duration::from_secs(1),
            counters: Arc::new(Counters::default()),
        }
    }

    #[test]
    fn fresh_recv_guard_holds_no_sleep() {
        // #358 review2 R2-1: constructing the guard (done for every
        // accepted bidi stream, up to 128 per connection) must not arm a
        // `Sleep` before anything polls it.
        let guard = RecvGuard::new(&guard_config());
        assert!(guard.deadline.is_none());
    }

    #[tokio::test]
    async fn recv_guard_deadline_is_measured_from_accept_and_fires_on_first_poll() {
        // The deadline instant is fixed at construction (from accept time,
        // as before); if that instant is already in the past by the time
        // anything first polls the guard (e.g. the stream sat in
        // `pending_bi` past its deadline before the request task was
        // created), the lazily-created `Sleep` must still fire immediately
        // rather than measuring a fresh `header_deadline` from the first
        // poll.
        let mut guard = RecvGuard::new(&guard_config());
        assert!(guard.deadline.is_none());
        // Move the deadline into the past, as if accept happened well
        // before `header_deadline` ago.
        guard.deadline_at = Some(Instant::now() - Duration::from_secs(10));
        let fired = std::future::poll_fn(|cx| task::Poll::Ready(guard.poll_deadline(cx))).await;
        assert!(fired, "a past deadline must fire on the first poll");
    }

    #[tokio::test]
    async fn a_cleared_deadline_never_fires_again() {
        // #382: once the HEADERS frame is complete the deadline is gone for
        // good, even if its instant has passed and a later read is pending.
        let mut guard = RecvGuard::new(&guard_config());
        guard.deadline_at = Some(Instant::now() - Duration::from_secs(10));
        guard.clear_deadline();
        for _ in 0..3 {
            let fired = std::future::poll_fn(|cx| task::Poll::Ready(guard.poll_deadline(cx))).await;
            assert!(!fired, "a cleared deadline fired");
            assert!(guard.deadline.is_none(), "a cleared deadline re-armed");
        }
    }

    #[test]
    fn two_maximal_varints_classify_within_head_max() {
        let mut bytes = Vec::new();
        crate::varint::encode(crate::guard::WEBTRANSPORT_BIDI, &mut bytes);
        crate::varint::encode(crate::varint::MAX, &mut bytes);
        assert!(bytes.len() <= HEAD_MAX);
        let mut head = HeadBuf::default();
        let _ = head.push(&bytes);
        assert!(matches!(
            classify(head.as_slice(), crate::guard::WEBTRANSPORT_BIDI),
            Head::WebTransport { session_id, .. } if session_id == crate::varint::MAX
        ));
        // A request stream decides on the first varint.
        assert!(matches!(classify(&[0x01], 0x41), Head::H3));
        assert!(matches!(classify(&[0x40], 0x41), Head::Incomplete));
    }
}
