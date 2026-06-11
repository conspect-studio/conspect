//! Library surface for the Multiview developer-automation `xtask` crate.
//!
//! The binary ([`main`](../main/index.html)) is a thin CLI dispatcher; the
//! reusable, unit-testable logic lives here. Today that is the [`iptv`]
//! test-source selection tool used by the `soak-iptv` task.
#![warn(missing_docs)]

pub mod iptv;
