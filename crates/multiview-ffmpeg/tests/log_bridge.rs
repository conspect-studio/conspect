//! Pure logic of the libav → `tracing` log bridge (the anti-flood).
//!
//! These tests run in the **default** (pure-Rust, no-libav) build: the AV_LOG
//! level → tracing-level mapping, the repetition suppressor (rate limiter), and
//! the rendered-line sanitiser are all native-dep-free. The actual
//! `av_log_set_callback` wiring is an FFI smoke test behind the `ffmpeg`
//! feature (`tests/log_bridge_ffi.rs`); the load-bearing anti-flood logic is
//! covered exhaustively here.
//!
//! Why this is load-bearing: a corrupt HEVC RTSP feed emits a *continuous*
//! `Error constructing the frame RPS.` and a flaky HLS source emits a
//! *continuous* `IO error: Connection timed out`. Without suppression those go
//! straight to the operator's logs unbounded. Taking bad inputs and staying
//! bulletproof is the product's purpose — a glitchy input must never flood the
//! logs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::doc_markdown
)]

use std::time::Duration;

use multiview_ffmpeg::log_bridge::{
    map_av_level, sanitize_line, BridgeLevel, SuppressOutcome, Suppressor, AV_LOG_DEBUG,
    AV_LOG_ERROR, AV_LOG_FATAL, AV_LOG_INFO, AV_LOG_PANIC, AV_LOG_TRACE, AV_LOG_VERBOSE,
    AV_LOG_WARNING,
};

// ---- (b) AV_LOG level → tracing level mapping ----------------------------

#[test]
fn level_mapping_is_exhaustive_and_correct() {
    // PANIC/FATAL/ERROR collapse to error.
    assert_eq!(map_av_level(AV_LOG_PANIC), BridgeLevel::Error);
    assert_eq!(map_av_level(AV_LOG_FATAL), BridgeLevel::Error);
    assert_eq!(map_av_level(AV_LOG_ERROR), BridgeLevel::Error);
    // WARNING → warn.
    assert_eq!(map_av_level(AV_LOG_WARNING), BridgeLevel::Warn);
    // INFO → info.
    assert_eq!(map_av_level(AV_LOG_INFO), BridgeLevel::Info);
    // VERBOSE/DEBUG → debug.
    assert_eq!(map_av_level(AV_LOG_VERBOSE), BridgeLevel::Debug);
    assert_eq!(map_av_level(AV_LOG_DEBUG), BridgeLevel::Debug);
    // TRACE → trace.
    assert_eq!(map_av_level(AV_LOG_TRACE), BridgeLevel::Trace);
}

#[test]
fn level_mapping_buckets_by_threshold_not_exact_value() {
    // libav levels are a numeric scale; intermediate / future values must bucket
    // by the nearest band, never panic. A value below ERROR (e.g. a custom 4) is
    // still at least as severe as ERROR.
    assert_eq!(map_av_level(4), BridgeLevel::Error);
    // Between ERROR(16) and WARNING(24).
    assert_eq!(map_av_level(20), BridgeLevel::Warn);
    // Between WARNING(24) and INFO(32).
    assert_eq!(map_av_level(28), BridgeLevel::Info);
    // Above TRACE saturates to the most verbose band.
    assert_eq!(map_av_level(1000), BridgeLevel::Trace);
    // Negative (below PANIC, e.g. QUIET=-8) is the most severe band.
    assert_eq!(map_av_level(-8), BridgeLevel::Error);
}

// ---- (a) the suppressor / rate limiter -----------------------------------

#[test]
fn first_occurrence_emits_then_identical_repeats_are_suppressed() {
    // Window comfortably larger than the repeat burst so every repeat is in-window.
    let mut s = Suppressor::new(64, Duration::from_secs(60));
    let t0 = Duration::from_secs(0);

    // First time: emit.
    assert_eq!(
        s.observe(BridgeLevel::Error, "Error constructing the frame RPS.", t0),
        SuppressOutcome::Emit
    );
    // 9999 identical repeats inside the window: all suppressed, none emitted.
    for i in 1..=9999u64 {
        let t = t0 + Duration::from_millis(i); // 1 ms..=9.999 s, all within 60 s
        assert_eq!(
            s.observe(BridgeLevel::Error, "Error constructing the frame RPS.", t),
            SuppressOutcome::Suppress
        );
    }
}

#[test]
fn window_expiry_emits_a_coalesced_repeat_summary() {
    // 2 s window; the burst is delivered at 0.1 ms spacing so all 9998 repeats
    // land inside the first 1 s (well within the window).
    let mut s = Suppressor::new(64, Duration::from_secs(2));
    let msg = "IO error: Connection timed out";

    assert_eq!(
        s.observe(BridgeLevel::Error, msg, Duration::from_secs(0)),
        SuppressOutcome::Emit
    );
    // 9998 suppressed within the window (1 µs..≈10 ms — all < 2 s).
    for i in 1..=9998u64 {
        assert_eq!(
            s.observe(BridgeLevel::Error, msg, Duration::from_micros(i)),
            SuppressOutcome::Suppress
        );
    }
    // The next occurrence AFTER the window has elapsed flushes the coalesced
    // count and re-emits the message itself. 9998 were suppressed since the
    // first emit.
    assert_eq!(
        s.observe(BridgeLevel::Error, msg, Duration::from_secs(3)),
        SuppressOutcome::EmitWithSummary { suppressed: 9998 }
    );
    // After the flush the counter resets: the immediate next repeat is suppressed
    // again (a fresh window started at t=3 s).
    assert_eq!(
        s.observe(BridgeLevel::Error, msg, Duration::from_millis(3100)),
        SuppressOutcome::Suppress
    );
}

#[test]
fn window_expiry_with_no_suppressed_repeats_just_emits() {
    let mut s = Suppressor::new(64, Duration::from_secs(2));
    let msg = "harmless once-off";
    assert_eq!(
        s.observe(BridgeLevel::Warn, msg, Duration::from_secs(0)),
        SuppressOutcome::Emit
    );
    // Same message, but only after the window — and nothing was suppressed in
    // between, so it is a plain re-emit (summary count zero is reported as Emit).
    assert_eq!(
        s.observe(BridgeLevel::Warn, msg, Duration::from_secs(5)),
        SuppressOutcome::Emit
    );
}

#[test]
fn distinct_messages_each_emit_independently() {
    let mut s = Suppressor::new(64, Duration::from_secs(60));
    let t = Duration::from_secs(0);
    assert_eq!(
        s.observe(BridgeLevel::Error, "alpha", t),
        SuppressOutcome::Emit
    );
    assert_eq!(
        s.observe(BridgeLevel::Error, "beta", t),
        SuppressOutcome::Emit
    );
    assert_eq!(
        s.observe(BridgeLevel::Error, "gamma", t),
        SuppressOutcome::Emit
    );
    // Repeat of alpha is now suppressed (still in window).
    assert_eq!(
        s.observe(BridgeLevel::Error, "alpha", t),
        SuppressOutcome::Suppress
    );
}

#[test]
fn same_text_at_different_levels_is_keyed_separately() {
    let mut s = Suppressor::new(64, Duration::from_secs(60));
    let t = Duration::from_secs(0);
    let msg = "ambiguous";
    assert_eq!(s.observe(BridgeLevel::Error, msg, t), SuppressOutcome::Emit);
    // Same text, different level → distinct key → emits.
    assert_eq!(s.observe(BridgeLevel::Warn, msg, t), SuppressOutcome::Emit);
    // Repeat at the original level is suppressed.
    assert_eq!(
        s.observe(BridgeLevel::Error, msg, t),
        SuppressOutcome::Suppress
    );
}

#[test]
fn suppressor_lru_is_bounded_and_never_grows_past_cap() {
    let cap = 8;
    let mut s = Suppressor::new(cap, Duration::from_secs(3600));
    // Feed far more distinct keys than the cap, each only once.
    for i in 0..10_000u64 {
        let msg = format!("distinct message number {i}");
        // First sight of every distinct key emits.
        assert_eq!(
            s.observe(BridgeLevel::Error, &msg, Duration::from_secs(0)),
            SuppressOutcome::Emit
        );
        assert!(
            s.len() <= cap,
            "suppressor grew past cap: len={} cap={}",
            s.len(),
            cap
        );
    }
    assert!(s.len() <= cap);
}

#[test]
fn lru_eviction_lets_an_old_key_re_emit() {
    let cap = 2;
    let mut s = Suppressor::new(cap, Duration::from_secs(3600));
    let t = Duration::from_secs(0);
    // Fill with A, B.
    assert_eq!(s.observe(BridgeLevel::Error, "A", t), SuppressOutcome::Emit);
    assert_eq!(s.observe(BridgeLevel::Error, "B", t), SuppressOutcome::Emit);
    // C evicts the least-recently-used (A).
    assert_eq!(s.observe(BridgeLevel::Error, "C", t), SuppressOutcome::Emit);
    // A was evicted, so it is seen as new again → Emit (not Suppress).
    assert_eq!(s.observe(BridgeLevel::Error, "A", t), SuppressOutcome::Emit);
    assert!(s.len() <= cap);
}

#[test]
fn zero_capacity_suppressor_always_emits_without_panicking() {
    // A pathological cap of 0 must not panic and must degrade to always-emit
    // (no state retained), never grow.
    let mut s = Suppressor::new(0, Duration::from_secs(60));
    for i in 0..100u64 {
        assert_eq!(
            s.observe(BridgeLevel::Error, "same", Duration::from_millis(i)),
            SuppressOutcome::Emit
        );
        assert_eq!(s.len(), 0);
    }
}

// ---- (c) the rendered-line sanitiser -------------------------------------

#[test]
fn sanitize_trims_trailing_newline_and_whitespace() {
    // libav lines arrive newline-terminated; the bridge strips the trailing
    // newline so the tracing record is one clean line.
    assert_eq!(
        sanitize_line("Error constructing the frame RPS.\n"),
        "Error constructing the frame RPS."
    );
    assert_eq!(sanitize_line("trailing spaces   \n"), "trailing spaces");
    assert_eq!(sanitize_line("\r\n"), "");
}

#[test]
fn sanitize_handles_empty_and_percent_containing_messages() {
    // An empty message must not panic.
    assert_eq!(sanitize_line(""), "");
    // A message that already contains `%` (e.g. a pre-rendered percentage) must
    // pass through verbatim — the formatter expands the va_list in C, so by the
    // time we sanitise there is no further format-string interpretation.
    assert_eq!(
        sanitize_line("decode 50% complete\n"),
        "decode 50% complete"
    );
    assert_eq!(
        sanitize_line("%s %d %n raw percents"),
        "%s %d %n raw percents"
    );
}

#[test]
fn sanitize_strips_interior_control_chars_but_keeps_text() {
    // Embedded NUL / control bytes from a corrupt component name must not break
    // the log line; they are replaced, not panicked on.
    let raw = "bad\u{0}component\u{7}name\n";
    let cleaned = sanitize_line(raw);
    assert!(!cleaned.contains('\u{0}'));
    assert!(!cleaned.contains('\u{7}'));
    assert!(cleaned.starts_with("bad"));
    assert!(cleaned.contains("component"));
}

#[test]
fn sanitize_oversized_message_is_bounded() {
    // A pathologically long line is truncated to the bridge's bound and never
    // allocates without limit / panics.
    let huge = "x".repeat(100_000);
    let cleaned = sanitize_line(&huge);
    assert!(
        cleaned.len() <= multiview_ffmpeg::log_bridge::MAX_LINE_LEN,
        "sanitised line {} exceeds MAX_LINE_LEN {}",
        cleaned.len(),
        multiview_ffmpeg::log_bridge::MAX_LINE_LEN
    );
}
