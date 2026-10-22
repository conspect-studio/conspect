//! Resource-scoped behaviour of the libav → `tracing` bridge (ADR-0060 §3).
//!
//! These run in the **default** (pure-Rust, no-libav) build. They cover the two
//! pure, native-dep-free pieces of the attribution work:
//!
//! * the suppressor key gaining a `resource_id` dimension (§3.4) so two
//!   different sources emitting the *same* libav message text suppress
//!   independently — CNN's RPS flood must not mask BBC's;
//! * the scoped, **never-stale** thread-local [`ResourceContext`] (§3.1): a
//!   RAII guard sets the current owned resource on entry to an owned
//!   demuxer/decode region and clears it on exit, so a line emitted outside any
//!   owned region resolves to *no* resource (falls through to component-only),
//!   never inheriting whichever resource last ran on the thread (§3.3 honesty).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::doc_markdown
)]

use std::time::Duration;

use multiview_ffmpeg::log_bridge::{
    current_resource, BridgeLevel, ResourceContext, ResourceGuard, SuppressOutcome, Suppressor,
};

// ---- suppressor: per-resource independence (§3.4) ------------------------

#[test]
fn same_message_from_two_resources_suppresses_independently() {
    let mut s = Suppressor::new(8, Duration::from_secs(5));
    let msg = "Error constructing the frame RPS.";
    let t0 = Duration::from_secs(0);

    // First occurrence for cnn -> emit.
    assert_eq!(
        s.observe_scoped(BridgeLevel::Error, Some("cnn"), msg, t0),
        SuppressOutcome::Emit,
        "cnn's first RPS error emits"
    );
    // First occurrence for bbc with the SAME text -> must ALSO emit (different
    // resource = different key), not be masked by cnn's window.
    assert_eq!(
        s.observe_scoped(BridgeLevel::Error, Some("bbc"), msg, t0),
        SuppressOutcome::Emit,
        "bbc's first RPS error must emit even though cnn just emitted the same text"
    );
    // A repeat for cnn inside the window -> suppressed (cnn's own window).
    assert_eq!(
        s.observe_scoped(BridgeLevel::Error, Some("cnn"), msg, Duration::from_secs(1)),
        SuppressOutcome::Suppress,
        "cnn's repeat is suppressed in cnn's window"
    );
    // A repeat for bbc inside the window -> suppressed independently.
    assert_eq!(
        s.observe_scoped(BridgeLevel::Error, Some("bbc"), msg, Duration::from_secs(1)),
        SuppressOutcome::Suppress
    );
}

#[test]
fn unattributed_and_attributed_same_text_are_distinct_keys() {
    let mut s = Suppressor::new(8, Duration::from_secs(5));
    let msg = "Opening 'x' for reading";
    let t0 = Duration::from_secs(0);
    assert_eq!(
        s.observe_scoped(BridgeLevel::Info, None, msg, t0),
        SuppressOutcome::Emit
    );
    assert_eq!(
        s.observe_scoped(BridgeLevel::Info, Some("cnn"), msg, t0),
        SuppressOutcome::Emit,
        "an attributed line is a distinct key from the unattributed one"
    );
}

#[test]
fn observe_is_equivalent_to_observe_scoped_with_no_resource() {
    // The legacy resource-agnostic `observe` must behave exactly like
    // `observe_scoped(.., None, ..)` so existing call sites are unchanged.
    let mut a = Suppressor::new(4, Duration::from_secs(5));
    let mut b = Suppressor::new(4, Duration::from_secs(5));
    let msg = "same";
    let t = Duration::from_secs(0);
    assert_eq!(
        a.observe(BridgeLevel::Warn, msg, t),
        b.observe_scoped(BridgeLevel::Warn, None, msg, t)
    );
    let t2 = Duration::from_secs(1);
    assert_eq!(
        a.observe(BridgeLevel::Warn, msg, t2),
        b.observe_scoped(BridgeLevel::Warn, None, msg, t2),
        "the second (in-window) observation matches too"
    );
}

// ---- scoped thread-local ResourceContext (§3.1) -------------------------

#[test]
fn no_context_outside_a_guard() {
    assert_eq!(
        current_resource(),
        None,
        "with no guard active, there is no current resource"
    );
}

#[test]
fn guard_sets_and_clears_current_resource() {
    assert_eq!(current_resource(), None);
    {
        let _g = ResourceGuard::enter(ResourceContext::source("cnn").with_label("CNN"));
        let cur = current_resource().expect("inside the guard a resource is current");
        assert_eq!(cur.id(), "cnn");
        assert_eq!(cur.kind(), "source");
        assert_eq!(cur.label(), Some("CNN"));
    }
    assert_eq!(
        current_resource(),
        None,
        "the guard cleared the context on drop — never stale (§3.1)"
    );
}

#[test]
fn nested_guards_restore_the_outer_context() {
    let _outer = ResourceGuard::enter(ResourceContext::source("cnn"));
    assert_eq!(
        current_resource().map(|r| r.id().to_owned()),
        Some("cnn".to_owned())
    );
    {
        let _inner = ResourceGuard::enter(ResourceContext::output("rtsp-main"));
        assert_eq!(
            current_resource().map(|r| r.id().to_owned()),
            Some("rtsp-main".to_owned()),
            "the inner guard shadows the outer"
        );
    }
    assert_eq!(
        current_resource().map(|r| r.id().to_owned()),
        Some("cnn".to_owned()),
        "dropping the inner guard restores the outer context, not None"
    );
}
