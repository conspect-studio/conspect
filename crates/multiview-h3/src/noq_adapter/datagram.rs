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
// Vendored from h3-quinn 0.0.10 (src/datagram.rs) and ported to moq-noq
// 1.3.5; the edits are listed in crates/multiview-h3/PROVENANCE.md.

//! h3-datagram traits for the noq connection.
//!
//! Kept for API parity with h3-quinn. The WebTransport session in this crate
//! reads and writes datagrams on the noq connection directly instead,
//! because h3-datagram 0.0.2's `Datagram::encode` never copies the encoded
//! quarter stream id into its output (it always sends a zero id).

use std::task::{ready, Poll};

use bytes::{Buf, Bytes};
use futures_util::{stream, StreamExt};
use h3_datagram::datagram::EncodedDatagram;
use h3_datagram::quic_traits::{
    DatagramConnectionExt, RecvDatagram, SendDatagram, SendDatagramErrorIncoming,
};
use h3_datagram::ConnectionErrorIncoming;
use noq::SendDatagramError;

use super::{closed_stream_source, convert_connection_error, BoxStreamSync, Connection};

/// Sends datagrams over a QUIC connection.
pub struct SendDatagramHandler {
    conn: noq::Connection,
}

impl<B: Buf> SendDatagram<B> for SendDatagramHandler {
    fn send_datagram<T: Into<EncodedDatagram<B>>>(
        &mut self,
        data: T,
    ) -> Result<(), SendDatagramErrorIncoming> {
        let mut buf: EncodedDatagram<B> = data.into();
        self.conn
            .send_datagram(buf.copy_to_bytes(buf.remaining()))
            .map_err(convert_send_datagram_error)
    }
}

/// Receives datagrams over a QUIC connection.
pub struct RecvDatagramHandler {
    datagrams: BoxStreamSync<'static, Result<Bytes, noq::ConnectionError>>,
}

impl RecvDatagram for RecvDatagramHandler {
    type Buffer = Bytes;
    fn poll_incoming_datagram(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Result<Self::Buffer, ConnectionErrorIncoming>> {
        Poll::Ready(match ready!(self.datagrams.poll_next_unpin(cx)) {
            Some(res) => res.map_err(convert_connection_error),
            None => Err(closed_stream_source()),
        })
    }
}

impl<B: Buf> DatagramConnectionExt<B> for Connection {
    type SendDatagramHandler = SendDatagramHandler;
    type RecvDatagramHandler = RecvDatagramHandler;

    fn send_datagram_handler(&self) -> Self::SendDatagramHandler {
        SendDatagramHandler {
            conn: self.conn.clone(),
        }
    }

    fn recv_datagram_handler(&self) -> Self::RecvDatagramHandler {
        RecvDatagramHandler {
            datagrams: Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.read_datagram().await, conn))
            })),
        }
    }
}

fn convert_send_datagram_error(error: SendDatagramError) -> SendDatagramErrorIncoming {
    match error {
        SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled => {
            SendDatagramErrorIncoming::NotAvailable
        }
        SendDatagramError::TooLarge => SendDatagramErrorIncoming::TooLarge,
        SendDatagramError::ConnectionLost(e) => {
            SendDatagramErrorIncoming::ConnectionError(convert_connection_error(e))
        }
    }
}
