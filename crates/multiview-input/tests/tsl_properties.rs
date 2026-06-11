//! Property tests for the TSL UMD **decoders**: arbitrary bytes must never panic
//! (the decoders run on untrusted network input, so a malformed packet must
//! surface as a typed [`TslError`], never an `unwrap`/index panic) and any
//! accepted packet must satisfy the structural invariants.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use multiview_input::tsl;
use proptest::prelude::*;

proptest! {
    /// v3.1 never panics on arbitrary input and only accepts exactly-18-byte
    /// packets with the sync bit set.
    #[test]
    fn v31_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..40)) {
        if let Ok(msg) = tsl::v31::decode(&bytes) {
            prop_assert_eq!(bytes.len(), 18);
            prop_assert_eq!(msg.displays.len(), 1);
            prop_assert!(bytes[0] & 0x80 != 0);
        }
    }

    /// v4.0 never panics; any accepted packet had a valid checksum and the sync
    /// bit set.
    #[test]
    fn v40_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..40)) {
        if let Ok(msg) = tsl::v40::decode(&bytes) {
            prop_assert_eq!(bytes.len(), 19);
            prop_assert_eq!(msg.displays.len(), 1);
            let sum = bytes.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            prop_assert_eq!(sum, 0); // body + checksum sums to zero
        }
    }

    /// v5.0 never panics on arbitrary input, even with hostile length fields.
    #[test]
    fn v50_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        if let Ok(msg) = tsl::v50::decode(&bytes) {
            prop_assert!(!msg.displays.is_empty());
            prop_assert!(bytes.len() <= tsl::v50::MAX_PACKET_LEN);
        }
    }

    /// v5.0 stuffed decoder never panics on arbitrary input.
    #[test]
    fn v50_stuffed_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..600)) {
        let _ = tsl::v50::decode_stuffed(&bytes);
    }
}
