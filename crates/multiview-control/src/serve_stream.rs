//! Shutdown-aware, guard-owning transport wrapper for the control-plane serve loop
//! (SEC-14 #126 R2 / [ADR-W031](../../../docs/decisions/ADR-W031.md)).
//!
//! [`TrackedStream`] wraps an accepted connection stream (a `TcpStream`, or a
//! `TlsStream` **after** its handshake) so that:
//!
//! * it **owns** the accept-level
//!   [`ConnectionGuard`](crate::limits::ConnectionGuard), so the population-cap slot
//!   (global + per-IP) is held for the connection's *real* life — including after an
//!   HTTP/1 **Upgrade**. hyper moves the wrapped IO into its `Upgraded`, so the guard
//!   rides into the (detached) WebSocket task and releases only when that socket
//!   finally closes. Without this the guard would drop the instant `serve_connection`
//!   returns — which is at the upgrade handshake, not the WebSocket's end — so
//!   sequential WebSocket upgrades would silently bypass the per-IP population cap
//!   (the F1 escape).
//!
//! * it is **shutdown-aware**: once the serve loop's shutdown `watch` flips (or its
//!   sender drops), reads return EOF and writes fail. A live upgraded WebSocket is a
//!   detached task the drain [`JoinSet`](tokio::task::JoinSet) does not track, so it
//!   cannot be aborted at the ceiling; instead it observes the shutdown *through its
//!   own transport* and drains cooperatively (the [`run_ws_session`] loop selects on
//!   `socket.recv()`, which then returns end-of-stream). serve() does **not**
//!   synchronously await that detached task — the drain is prompt-and-cooperative, not
//!   awaited.
//!
//! The read and write halves each own an **independent** shutdown waiter (a `watch`
//! receiver clone + its own wait-future), so a **parked** read *and* a **parked** write
//! each wake the instant shutdown is signalled — regardless of which half was polled
//! last, and even if the two halves are ever driven from different tasks (an
//! `io::split`). Each waiter owns its receiver (moved in), so its registration persists
//! across polls (a fresh `changed()` per poll would deregister on drop and miss the
//! signal). A single shared waiter would let whichever half polled last overwrite the
//! other's waker slot and miss the flip — a lost wakeup on the drain path, which for the
//! inv#10 drain guarantee we make impossible by construction rather than rely on nothing
//! ever splitting the stream.
//!
//! [`run_ws_session`]: crate::realtime

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;

use crate::limits::ConnectionGuard;

/// The error a shutdown-draining [`TrackedStream`] fails writes with, so a connection
/// (or upgraded socket) blocked on a write unblocks and tears down at drain rather than
/// waiting on a peer that will never read again.
fn shutdown_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "control-plane serve loop is shutting down",
    )
}

/// A boxed future that resolves once the serve loop signals shutdown (the `watch` value
/// flips to `true`, or its sender drops). Owns its [`watch::Receiver`] (moved in), so its
/// waker registration persists across polls.
type ShutdownWaiter = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A connection-stream wrapper that owns the accept-level admission guard (so it rides
/// an HTTP/1 upgrade into the detached WebSocket task) and cooperatively drains on the
/// serve loop's shutdown signal. See the module docs.
pub(crate) struct TrackedStream<S> {
    /// The wrapped transport (a `TcpStream`, or a `TlsStream` post-handshake).
    inner: S,
    /// The accept-level admission guard, held for this stream's whole life — including
    /// after an upgrade, since hyper moves this wrapper into its `Upgraded`. `None` when
    /// no population cap is configured. Released by `Drop` (RAII).
    _guard: Option<ConnectionGuard>,
    /// Shutdown waiter for the **read** half. Independent of the write half's waiter so a
    /// parked read wakes on the shutdown flip regardless of which half was polled last —
    /// robust even if the stream is `io::split`. See the module docs.
    shutdown_read: ShutdownWaiter,
    /// Shutdown waiter for the **write** half. Independent of the read half's waiter.
    shutdown_write: ShutdownWaiter,
    /// Latches once the **read** half has observed shutdown: a completed waiter must not
    /// be polled again, and every subsequent read is EOF.
    draining_read: bool,
    /// Latches once the **write** half has observed shutdown: a completed waiter must not
    /// be polled again, and every subsequent write is an error.
    draining_write: bool,
}

/// Build a [`ShutdownWaiter`] that resolves the first time `shutdown` is observed
/// signalled (the value flips to `true`) or its sender drops (`serve()` gone, which
/// `wait_for` surfaces as `Err`) — either way the connection must drain. The future owns
/// the receiver (moved in), so its waker registration persists across polls.
fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) -> ShutdownWaiter {
    Box::pin(async move {
        let _ = shutdown.wait_for(|signalled| *signalled).await;
    })
}

/// Poll one direction's shutdown `waiter`, latching `draining` once it fires, and report
/// whether that direction is now draining. Registers `cx`'s waker (via the waiter's owned
/// receiver) so a caller parked on the inner transport for this direction wakes the
/// instant shutdown is signalled. Never polls the completed waiter twice — the `draining`
/// latch guards it.
fn poll_shutdown_latch(
    waiter: &mut ShutdownWaiter,
    draining: &mut bool,
    cx: &mut Context<'_>,
) -> bool {
    if *draining {
        return true;
    }
    if waiter.as_mut().poll(cx).is_ready() {
        *draining = true;
    }
    *draining
}

impl<S> TrackedStream<S> {
    /// Wrap `inner`, taking ownership of the admission `guard` and arming shutdown
    /// awareness on `shutdown`. The read and write halves each get an **independent**
    /// waiter (two `watch` receiver clones), so a parked read and a parked write each
    /// wake on the shutdown flip regardless of which half was polled last — see the
    /// module docs.
    pub(crate) fn new(
        inner: S,
        guard: Option<ConnectionGuard>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            inner,
            _guard: guard,
            shutdown_read: wait_for_shutdown(shutdown.clone()),
            shutdown_write: wait_for_shutdown(shutdown),
            draining_read: false,
            draining_write: false,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TrackedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // On shutdown, signal EOF (`Ready(Ok(()))` without filling `buf`): hyper — or,
        // post-upgrade, the WebSocket reading through this transport — sees
        // end-of-stream and drains. The read half's own waiter registers the waker, so a
        // read otherwise parked on `inner` still wakes the instant shutdown fires.
        if poll_shutdown_latch(&mut this.shutdown_read, &mut this.draining_read, cx) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TrackedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if poll_shutdown_latch(&mut this.shutdown_write, &mut this.draining_write, cx) {
            return Poll::Ready(Err(shutdown_error()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if poll_shutdown_latch(&mut this.shutdown_write, &mut this.draining_write, cx) {
            return Poll::Ready(Err(shutdown_error()));
        }
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if poll_shutdown_latch(&mut this.shutdown_write, &mut this.draining_write, cx) {
            return Poll::Ready(Err(shutdown_error()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Closing the write half is *itself* the drain action for that half, so it
        // deliberately does NOT consult a shutdown waiter: short-circuiting it (an error)
        // would defeat its purpose — at drain we *want* the FIN / TLS close_notify to go
        // out. It delegates to the inner transport's `poll_shutdown`, which per the
        // `AsyncWrite` contract MAY return `Pending` (TLS `close_notify` backpressure, or a
        // peer that has stopped reading); a waiter cannot help, since finishing that flush /
        // half-close is exactly what we would be waiting on. That is safe because the drain
        // path never *awaits* a parking `poll_shutdown`: the upgraded WebSocket drains by
        // read-EOF → break → drop (the socket closes on `Drop`, not via a graceful
        // `poll_shutdown`; see `run_ws_session`), and every tracked (non-upgraded)
        // connection is `JoinSet`-tracked and aborted + dropped at the drain ceiling
        // (R2-F1/F3, #201). A `Pending` here is thus bounded by the drain machinery, not by
        // an escape in this fn.
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // test-only: `expect` on a `watch::send` whose receivers are provably alive (rule 20)

    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::watch;

    use super::TrackedStream;

    /// A waker that records whether it was woken, so a synchronous test can assert that a
    /// *parked* direction's waker actually fired on the shutdown flip.
    struct FlagWaker(AtomicBool);

    impl Wake for FlagWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// An inner transport that always parks (`Pending`) and never registers a waker of
    /// its own — so the only shutdown wakeup a test can observe is the one
    /// [`TrackedStream`] registers through its own shutdown waiter(s).
    struct PendingIo;

    impl AsyncRead for PendingIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// inv#10 drain robustness: the read and write halves must observe shutdown through
    /// **independent** waker slots. Park the read half (waker R), then poll the write half
    /// last (waker W): a single shared shutdown future would leave only W registered, so
    /// the flip would never wake the parked reader — a lost wakeup that leaves the read
    /// half blocked past shutdown (a connection outliving `serve`, the exact class of bug
    /// the F1 upgrade-drain fix closes). Both directions must wake, regardless of which
    /// was polled last.
    #[test]
    fn read_and_write_wake_independently_on_shutdown() {
        let (tx, rx) = watch::channel(false);
        let mut stream = TrackedStream::new(PendingIo, None, rx);

        let read_flag = Arc::new(FlagWaker(AtomicBool::new(false)));
        let write_flag = Arc::new(FlagWaker(AtomicBool::new(false)));
        let waker_r = Waker::from(read_flag.clone());
        let waker_w = Waker::from(write_flag.clone());
        let mut cx_r = Context::from_waker(&waker_r);
        let mut cx_w = Context::from_waker(&waker_w);

        // Park the read half first (registers R), then poll the write half last
        // (registers W). No shutdown yet, so both must be Pending.
        let mut buf = [0u8; 8];
        let mut rb = ReadBuf::new(&mut buf);
        assert!(
            Pin::new(&mut stream)
                .poll_read(&mut cx_r, &mut rb)
                .is_pending(),
            "read must park before shutdown"
        );
        assert!(
            Pin::new(&mut stream)
                .poll_write(&mut cx_w, b"x")
                .is_pending(),
            "write must park before shutdown"
        );

        // Flip shutdown. A single shared waker slot keeps only W (polled last); per-
        // direction waiters keep both R and W.
        tx.send(true).expect("receivers are held inside the stream");

        assert!(
            read_flag.0.load(Ordering::SeqCst),
            "parked read half was not woken on shutdown (shared-waker lost wakeup)"
        );
        assert!(
            write_flag.0.load(Ordering::SeqCst),
            "parked write half was not woken on shutdown"
        );

        // Per-direction drain semantics: read now yields EOF, write now errors.
        let mut buf2 = [0u8; 8];
        let mut rb2 = ReadBuf::new(&mut buf2);
        let read_res = Pin::new(&mut stream).poll_read(&mut cx_r, &mut rb2);
        assert!(
            matches!(read_res, Poll::Ready(Ok(()))),
            "shutdown read must be Ready(Ok) EOF, got {read_res:?}"
        );
        assert_eq!(rb2.filled().len(), 0, "shutdown read must be a 0-byte EOF");

        let write_res = Pin::new(&mut stream).poll_write(&mut cx_w, b"x");
        assert!(
            matches!(write_res, Poll::Ready(Err(ref e)) if e.kind() == io::ErrorKind::BrokenPipe),
            "shutdown write must be a BrokenPipe error, got {write_res:?}"
        );
    }
}
