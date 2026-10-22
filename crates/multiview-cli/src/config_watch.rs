//! Config-file watch (ADR-W020) — **re-export shim**.
//!
//! The watcher and its diff→apply machinery moved into
//! [`multiview_control::config_watch`] (ADR-W024 §2) so the
//! `POST /api/v1/config/revert-to-start` route — which lives in the control
//! plane — can reach the public `apply_document_diff` under the `cli → control`
//! dependency arrow. The CLI keeps this shim so existing
//! `multiview_cli::config_watch::*` paths (including the ADR-W020 integration
//! tests) compile unchanged.

pub use multiview_control::config_watch::*;
