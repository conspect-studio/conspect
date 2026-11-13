//! The header-block guard: a streaming HTTP/3 frame walker on the receive
//! side of every request stream, run BEFORE h3 sees the bytes.
//!
//! Why: h3 0.0.8's `FrameDecoder` buffers a whole non-DATA frame (HEADERS,
//! trailers, or an unknown/grease frame it will skip) of its *declared*
//! length before it decodes anything, and the QPACK decoder allocates each
//! field before checking the running total against
//! `max_field_section_size`. A peer that declares a 1 MiB HEADERS frame and
//! trickles it in would make the server hold up to the stream receive
//! window (1 MiB) per stream before any check runs. The guard reads only the
//! frame type and length varints (at most 16 bytes of state) and rejects a
//! non-DATA frame whose declared length exceeds the cap the moment the
//! length is known, so at most one QUIC chunk beyond the frame header is
//! ever buffered.
//!
//! DATA frame payloads are skipped by counting (the body streams on demand
//! and is never buffered whole). A stream whose first frame type is
//! `0x41` (a WebTransport bidirectional stream) is not HTTP framing after the
//! session id, so the guard passes it through untouched.

use crate::varint;

/// HTTP/3 frame type DATA.
pub const FRAME_DATA: u64 = 0x00;
/// HTTP/3 frame type HEADERS.
pub const FRAME_HEADERS: u64 = 0x01;
/// The WebTransport bidirectional stream signal (draft-ietf-webtrans-http3).
pub const WEBTRANSPORT_BIDI: u64 = 0x41;

/// Framing slack added to `max_field_section_size` when checking the declared
/// HEADERS length: the QPACK prefix and per-field length prefixes take more
/// wire bytes than the RFC 9114 section 4.2.2 field-section size (which adds
/// 32 octets of overhead per field but counts no wire prefixes). A field line
/// costs at most a few varint bytes plus its literal; 1 KiB covers the
/// prefixes of the fields that fit into the cap.
pub const FRAMING_SLACK: u64 = 1024;

/// What the guard says about the bytes fed so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Keep feeding.
    Continue,
    /// The stream is a WebTransport bidirectional stream; stop inspecting.
    WebTransport,
    /// A non-DATA frame declared a length above the limit: fail the stream.
    Reject {
        /// The frame type that was declared.
        frame_type: u64,
        /// The declared payload length.
        declared: u64,
    },
}

/// Streaming walker over HTTP/3 frames. Holds at most 16 bytes.
#[derive(Clone, Debug)]
pub struct HeaderGuard {
    limit: u64,
    /// Partial frame header (type varint + length varint), at most 16 bytes.
    head: [u8; 16],
    head_len: usize,
    /// Payload bytes of the current frame still to skip.
    skip: u64,
    first_frame: bool,
    /// Frames whose header and payload were fully seen.
    frames_complete: u64,
    done: Option<Verdict>,
}

impl HeaderGuard {
    /// A guard that rejects any non-DATA frame declaring more than
    /// `max_field_section_size + FRAMING_SLACK` payload bytes.
    #[must_use]
    pub fn new(max_field_section_size: u64) -> Self {
        Self {
            limit: max_field_section_size.saturating_add(FRAMING_SLACK),
            head: [0; 16],
            head_len: 0,
            skip: 0,
            first_frame: true,
            frames_complete: 0,
            done: None,
        }
    }

    /// The declared-length limit for non-DATA frames.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// True once the first frame (the request HEADERS) was fully received:
    /// the per-request header deadline stops applying from then on.
    #[must_use]
    pub fn first_frame_complete(&self) -> bool {
        self.frames_complete > 0 || self.done == Some(Verdict::WebTransport)
    }

    /// Feeds the next received bytes. Once a verdict other than
    /// [`Verdict::Continue`] is returned it is sticky.
    pub fn feed(&mut self, mut data: &[u8]) -> Verdict {
        if let Some(done) = self.done {
            return done;
        }
        while !data.is_empty() {
            if self.skip > 0 {
                let available = u64::try_from(data.len()).unwrap_or(u64::MAX);
                let step = self.skip.min(available);
                self.skip -= step;
                if self.skip == 0 {
                    self.frames_complete = self.frames_complete.saturating_add(1);
                }
                let step = usize::try_from(step).unwrap_or(data.len());
                data = data.get(step..).unwrap_or_default();
                continue;
            }
            // Accumulate the frame header byte by byte (at most 16 bytes).
            let Some(slot) = self.head.get_mut(self.head_len) else {
                // Unreachable by construction: two varints are at most 16
                // bytes and are consumed as soon as both decode.
                return self.finish(Verdict::Reject {
                    frame_type: 0,
                    declared: u64::MAX,
                });
            };
            let Some((&byte, rest)) = data.split_first() else {
                break;
            };
            *slot = byte;
            self.head_len += 1;
            data = rest;
            let head = self.head.get(..self.head_len).unwrap_or_default();
            let Some((frame_type, type_len)) = varint::decode(head) else {
                continue;
            };
            // The second varint is the frame length, or the session id of a
            // WebTransport stream; either way the deadline holds until it is
            // complete.
            let Some((declared, _)) = varint::decode(head.get(type_len..).unwrap_or_default())
            else {
                continue;
            };
            if self.first_frame && frame_type == WEBTRANSPORT_BIDI {
                return self.finish(Verdict::WebTransport);
            }
            self.first_frame = false;
            self.head_len = 0;
            if frame_type != FRAME_DATA && declared > self.limit {
                return self.finish(Verdict::Reject {
                    frame_type,
                    declared,
                });
            }
            self.skip = declared;
            if declared == 0 {
                self.frames_complete = self.frames_complete.saturating_add(1);
            }
        }
        Verdict::Continue
    }

    fn finish(&mut self, verdict: Verdict) -> Verdict {
        self.done = Some(verdict);
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ty: u64, len: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint::encode(ty, &mut out);
        varint::encode(len, &mut out);
        out
    }

    #[test]
    fn oversized_headers_rejected_on_the_length_varint() {
        let mut guard = HeaderGuard::new(16_384);
        let head = frame(FRAME_HEADERS, 1 << 20);
        // Byte by byte: the verdict lands on the last length byte.
        for (i, b) in head.iter().enumerate() {
            let v = guard.feed(std::slice::from_ref(b));
            if i + 1 < head.len() {
                assert_eq!(v, Verdict::Continue);
            } else {
                assert_eq!(
                    v,
                    Verdict::Reject {
                        frame_type: FRAME_HEADERS,
                        declared: 1 << 20
                    }
                );
            }
        }
    }

    #[test]
    fn data_frames_are_skipped_and_trailers_checked() {
        let mut guard = HeaderGuard::new(100);
        let mut bytes = frame(FRAME_HEADERS, 3);
        bytes.extend_from_slice(b"abc");
        bytes.extend(frame(FRAME_DATA, 1 << 20));
        assert_eq!(guard.feed(&bytes), Verdict::Continue);
        assert!(guard.first_frame_complete());
        assert_eq!(guard.feed(&vec![0u8; 1 << 20]), Verdict::Continue);
        assert_eq!(
            guard.feed(&frame(FRAME_HEADERS, 5000)),
            Verdict::Reject {
                frame_type: FRAME_HEADERS,
                declared: 5000
            }
        );
    }

    #[test]
    fn unknown_frames_are_bounded_too() {
        let mut guard = HeaderGuard::new(100);
        assert!(matches!(
            guard.feed(&frame(0x21, 1 << 30)),
            Verdict::Reject { .. }
        ));
    }

    #[test]
    fn webtransport_stream_passes() {
        let mut guard = HeaderGuard::new(100);
        let mut bytes = Vec::new();
        varint::encode(WEBTRANSPORT_BIDI, &mut bytes);
        varint::encode(4, &mut bytes);
        bytes.extend(frame(FRAME_HEADERS, 1 << 30));
        assert_eq!(guard.feed(&bytes), Verdict::WebTransport);
        assert_eq!(guard.feed(&[1, 2, 3]), Verdict::WebTransport);
    }

    #[test]
    fn at_limit_is_accepted() {
        let mut guard = HeaderGuard::new(100);
        assert_eq!(
            guard.feed(&frame(FRAME_HEADERS, 100 + FRAMING_SLACK)),
            Verdict::Continue
        );
    }
}
