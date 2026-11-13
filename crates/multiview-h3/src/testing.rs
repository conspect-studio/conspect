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
        }
        let mut close = vec![5u8];
        close.extend(crate::capsule::encode_close(9, "done"));
        capsule(&close);
    }
}
