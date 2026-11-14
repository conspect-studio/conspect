//! Harness entry points shared by the fuzz targets and the unit tests
//! (feature `fuzzing`; always compiled under `cfg(test)`).

use crate::capsule::{Capsule, CapsuleParser, MAX_REASON};
use crate::guard::{HeaderGuard, Verdict, FRAME_DATA, WEBTRANSPORT_BIDI};
use crate::varint;

/// Splits `data` into chunks: the first byte of each step picks the next
/// chunk length (1..=64), so a fuzzer controls the split.
fn chunks(data: &[u8]) -> Vec<&[u8]> {
    let Some((&seed, mut rest)) = data.split_first() else {
        return Vec::new();
    };
    let mut step = usize::from(seed % 64) + 1;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let n = step.min(rest.len());
        let (head, tail) = rest.split_at(n);
        out.push(head);
        rest = tail;
        step = (step * 7 + 3) % 64 + 1;
    }
    out
}

/// `header_guard` target: feeds arbitrary bytes in arbitrary chunks, and
/// checks against an independent whole-buffer walk that the guard never
/// accepts a non-DATA frame declared longer than its limit.
///
/// # Panics
///
/// When the guard diverges from the reference walk (a fuzz finding).
pub fn header_guard(data: &[u8]) {
    let limit = 64;
    let mut guard = HeaderGuard::new(limit);
    let cap = guard.limit();
    let mut verdict = Verdict::Continue;
    let mut fed = Vec::new();
    for chunk in chunks(data) {
        verdict = guard.feed(chunk);
        fed.extend_from_slice(chunk);
        if verdict != Verdict::Continue {
            break;
        }
    }
    // Reference walk over everything fed.
    let mut pos = 0usize;
    let mut first = true;
    let expected = loop {
        let rest = fed.get(pos..).unwrap_or_default();
        let Some((ty, a)) = varint::decode(rest) else {
            break Verdict::Continue;
        };
        let Some((len, b)) = varint::decode(rest.get(a..).unwrap_or_default()) else {
            break Verdict::Continue;
        };
        if first && ty == WEBTRANSPORT_BIDI {
            break Verdict::WebTransport;
        }
        first = false;
        if ty != FRAME_DATA && len > cap {
            break Verdict::Reject {
                frame_type: ty,
                declared: len,
            };
        }
        let Some(next) = usize::try_from(len)
            .ok()
            .and_then(|l| pos.checked_add(a + b)?.checked_add(l))
        else {
            break Verdict::Continue;
        };
        pos = next;
    };
    assert_eq!(verdict, expected, "guard diverged from the reference walk");
}

/// `capsule` target: arbitrary bytes in arbitrary chunks through the
/// capsule parser; it never panics and every CLOSE it yields carries a
/// bounded reason.
///
/// # Panics
///
/// When a reason exceeds its bound (a fuzz finding).
pub fn capsule(data: &[u8]) {
    let mut parser = CapsuleParser::new();
    let mut out = Vec::new();
    for chunk in chunks(data) {
        if parser.feed(chunk, &mut out).is_err() {
            break;
        }
    }
    for c in out {
        if let Capsule::Close { reason, .. } = c {
            // Lossy decoding may expand invalid bytes to U+FFFD (3 bytes).
            assert!(reason.len() <= MAX_REASON * 3);
        }
    }
}

/// In-memory `h3::quic` transport: one client request stream carrying the
/// fuzz bytes (in chunks) and one valid client control stream; everything
/// the server sends is discarded.
mod mem {
    use std::collections::VecDeque;
    use std::task::{Context, Poll};

    use bytes::{Buf, Bytes};
    use h3::error::Code;
    use h3::proto::varint::VarInt;
    use h3::quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf};

    fn id(n: u32) -> StreamId {
        StreamId::from(VarInt::from_u32(n))
    }

    pub(super) struct Recv {
        chunks: VecDeque<Bytes>,
        id: StreamId,
        /// End with FIN after the chunks (the request), or stay open
        /// (the control stream, whose end is a connection error).
        fin: bool,
    }

    impl quic::RecvStream for Recv {
        type Buf = Bytes;
        fn poll_data(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
            match self.chunks.pop_front() {
                Some(chunk) => Poll::Ready(Ok(Some(chunk))),
                None if self.fin => Poll::Ready(Ok(None)),
                None => Poll::Pending,
            }
        }
        fn stop_sending(&mut self, _: u64) {
            self.chunks.clear();
        }
        fn recv_id(&self) -> StreamId {
            self.id
        }
    }

    pub(super) struct Send {
        id: StreamId,
    }

    impl<B: Buf> quic::SendStream<B> for Send {
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
            Poll::Ready(Ok(()))
        }
        fn send_data<T: Into<WriteBuf<B>>>(&mut self, data: T) -> Result<(), StreamErrorIncoming> {
            let mut data: WriteBuf<B> = data.into();
            data.advance(data.remaining());
            Ok(())
        }
        fn poll_finish(&mut self, _: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
            Poll::Ready(Ok(()))
        }
        fn reset(&mut self, _: u64) {}
        fn send_id(&self) -> StreamId {
            self.id
        }
    }

    impl<B: Buf> quic::SendStreamUnframed<B> for Send {
        fn poll_send<D: Buf>(
            &mut self,
            _: &mut Context<'_>,
            buf: &mut D,
        ) -> Poll<Result<usize, StreamErrorIncoming>> {
            let n = buf.remaining();
            buf.advance(n);
            Poll::Ready(Ok(n))
        }
    }

    pub(super) struct Bidi {
        send: Send,
        recv: Recv,
    }

    impl quic::RecvStream for Bidi {
        type Buf = Bytes;
        fn poll_data(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
            self.recv.poll_data(cx)
        }
        fn stop_sending(&mut self, code: u64) {
            self.recv.stop_sending(code);
        }
        fn recv_id(&self) -> StreamId {
            self.recv.id
        }
    }

    impl<B: Buf> quic::SendStream<B> for Bidi {
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
            quic::SendStream::<B>::poll_ready(&mut self.send, cx)
        }
        fn send_data<T: Into<WriteBuf<B>>>(&mut self, data: T) -> Result<(), StreamErrorIncoming> {
            self.send.send_data(data)
        }
        fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
            quic::SendStream::<B>::poll_finish(&mut self.send, cx)
        }
        fn reset(&mut self, code: u64) {
            quic::SendStream::<B>::reset(&mut self.send, code);
        }
        fn send_id(&self) -> StreamId {
            self.send.id
        }
    }

    impl<B: Buf> quic::BidiStream<B> for Bidi {
        type SendStream = Send;
        type RecvStream = Recv;
        fn split(self) -> (Send, Recv) {
            (self.send, self.recv)
        }
    }

    pub(super) struct Opener {
        next_uni: u32,
    }

    impl<B: Buf> quic::OpenStreams<B> for Opener {
        type BidiStream = Bidi;
        type SendStream = Send;
        fn poll_open_bidi(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Result<Bidi, StreamErrorIncoming>> {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError("no server bidi".into()),
            }))
        }
        fn poll_open_send(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Result<Send, StreamErrorIncoming>> {
            let n = self.next_uni;
            self.next_uni = self.next_uni.saturating_add(4);
            Poll::Ready(Ok(Send { id: id(n) }))
        }
        fn close(&mut self, _: Code, _: &[u8]) {}
    }

    pub(super) struct Conn {
        request: Option<Bidi>,
        control: Option<Recv>,
        opener: Opener,
    }

    impl Conn {
        pub(super) fn new(chunks: Vec<Bytes>) -> Self {
            // Client control stream: type 0x00, then an empty SETTINGS frame.
            let control = Recv {
                chunks: VecDeque::from([Bytes::from_static(&[0x00, 0x04, 0x00])]),
                id: id(2),
                fin: false,
            };
            Self {
                request: Some(Bidi {
                    send: Send { id: id(0) },
                    recv: Recv {
                        chunks: chunks.into(),
                        id: id(0),
                        fin: true,
                    },
                }),
                control: Some(control),
                opener: Opener { next_uni: 3 },
            }
        }
    }

    impl<B: Buf> quic::OpenStreams<B> for Conn {
        type BidiStream = Bidi;
        type SendStream = Send;
        fn poll_open_bidi(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<Result<Bidi, StreamErrorIncoming>> {
            quic::OpenStreams::<B>::poll_open_bidi(&mut self.opener, cx)
        }
        fn poll_open_send(
            &mut self,
            cx: &mut Context<'_>,
        ) -> Poll<Result<Send, StreamErrorIncoming>> {
            quic::OpenStreams::<B>::poll_open_send(&mut self.opener, cx)
        }
        fn close(&mut self, _: Code, _: &[u8]) {}
    }

    impl<B: Buf> quic::Connection<B> for Conn {
        type RecvStream = Recv;
        type OpenStreams = Opener;
        fn poll_accept_recv(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Result<Recv, ConnectionErrorIncoming>> {
            match self.control.take() {
                Some(control) => Poll::Ready(Ok(control)),
                None => Poll::Pending,
            }
        }
        fn poll_accept_bidi(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Result<Bidi, ConnectionErrorIncoming>> {
            match self.request.take() {
                Some(request) => Poll::Ready(Ok(request)),
                None => Poll::Pending,
            }
        }
        fn opener(&self) -> Opener {
            Opener { next_uni: 1003 }
        }
    }
}

/// Polls `fut` with a no-op waker until it is ready or `budget` polls pass
/// (the in-memory transport never wakes; everything it has is ready).
fn run_bounded<F: std::future::Future>(fut: F, budget: usize) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    for _ in 0..budget {
        if let std::task::Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return Some(out);
        }
    }
    None
}

/// `h3_request` target: drives the REAL h3 server request path (settings
/// exchange, `accept`, `resolve_request`, the conversion to the `Site`
/// request and the streamed [`crate::RequestBody`]) over arbitrary client
/// request-stream bytes, behind the same header-block guard the noq
/// adapter applies. Never panics; never hands the site a request with a
/// pseudo-header or without HTTP/3 version.
///
/// # Panics
///
/// When a converted request breaks those properties (a fuzz finding).
pub fn h3_request(data: &[u8]) {
    let _ = h3_request_body_len(data);
}

/// [`h3_request`], returning the body length the site would have read when
/// the request resolved (for the unit tests).
///
/// # Panics
///
/// As [`h3_request`].
#[must_use]
pub fn h3_request_body_len(data: &[u8]) -> Option<usize> {
    use http_body_util::BodyExt;

    let settings = crate::H3Settings::default();
    // The guard runs on the receive path first, exactly as in the adapter:
    // bytes after a rejection never reach h3.
    let mut guard = HeaderGuard::new(settings.max_field_section_size);
    let mut chunks = Vec::new();
    for chunk in self::chunks(data) {
        match guard.feed(chunk) {
            Verdict::Reject { .. } | Verdict::WebTransport => return None,
            Verdict::Continue => chunks.push(bytes::Bytes::copy_from_slice(chunk)),
        }
    }
    let conn = mem::Conn::new(chunks);
    let fut = async move {
        let mut h3conn: h3::server::Connection<mem::Conn, bytes::Bytes> =
            settings.builder().build(conn).await.ok()?;
        let resolver = h3conn.accept().await.ok()??;
        let (req, stream) = resolver.resolve_request().await.ok()?;
        let (_send, recv) = stream.split();
        let req = crate::server::site_request(req, crate::RequestBody::new(recv));
        assert_eq!(req.version(), http::Version::HTTP_3);
        assert!(req.headers().keys().all(|k| !k.as_str().starts_with(':')));
        let mut body = req.into_body();
        let mut total = 0usize;
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else { break };
            if let Ok(data) = frame.into_data() {
                total = total.saturating_add(data.len());
            }
        }
        drop(h3conn);
        Some(total)
    };
    run_bounded(fut, 10_000).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_seeds() {
        let seeds: [&[u8]; 6] = [
            b"",
            &[0, 1, 0x40, 0x80],
            &[3, 0x01, 0x80, 0x10, 0x00, 0x00],
            &[1, 0x41, 0x04, 0x01, 0xff],
            &[7, 0x00, 0x05, 1, 2, 3, 4, 5, 0x01, 0x02, 9, 9],
            &[2, 0x68, 0x43, 0x05, 0, 0, 0, 7, b'x'],
        ];
        for seed in seeds {
            header_guard(seed);
            capsule(seed);
            h3_request(seed);
        }
        // A well-formed GET with a body and trailers through the real path.
        let mut get = vec![9u8, 0x01, 0x10, 0x00, 0x00, 0xd1, 0xd7, 0xc1, 0x50, 9];
        get.extend_from_slice(b"localhost");
        get.extend_from_slice(&[0x00, 0x03, b'a', b'b', b'c']);
        assert_eq!(h3_request_body_len(&get), Some(3));
        let mut close = vec![5u8];
        close.extend(crate::capsule::encode_close(9, "done"));
        capsule(&close);
    }

    /// Issue #403: the CI fuzzer's `h3_request` crash input. A GET whose
    /// HEADERS are followed by a DATA frame declaring 22 bytes and then FIN:
    /// h3 0.0.8 returned "no more data" with the payload still owed, and the
    /// trailers poll asserted (`frame.rs:57`). It must end the body with an
    /// error, never panic.
    #[test]
    fn issue_403_truncated_data_frame_does_not_panic() {
        // base64 `JAEI+/dQATjR9/EAFg==` (also fuzz/regressions/h3_request).
        let repro: [u8; 13] = [
            0x24, 0x01, 0x08, 0xfb, 0xf7, 0x50, 0x01, 0x38, 0xd1, 0xf7, 0xf1, 0x00, 0x16,
        ];
        h3_request(&repro);
        // The resolved request's body yields no data (it errors).
        assert_eq!(h3_request_body_len(&repro), Some(0));

        // The same shape on a well-formed GET: a DATA frame cut short after
        // some payload, and one with no payload at all, each then FIN.
        for tail in [&[0x00, 0x05, b'a', b'b'][..], &[0x00, 0x16][..]] {
            let mut get = vec![9u8, 0x01, 0x10, 0x00, 0x00, 0xd1, 0xd7, 0xc1, 0x50, 9];
            get.extend_from_slice(b"localhost");
            get.extend_from_slice(tail);
            h3_request(&get);
        }
    }
}
