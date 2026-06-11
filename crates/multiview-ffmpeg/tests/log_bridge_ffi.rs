//! FFI integration smoke test for the libav → `tracing` log bridge.
//!
//! Unlike `tests/log_bridge.rs` (which covers the pure anti-flood *logic* in the
//! default build), this drives the **real** libav `av_log` path under the
//! `ffmpeg` feature: synthetic log lines go through libav's C logger → the
//! installed `extern "C"` trampoline → `av_log_format_line2` rendering → the
//! suppressor → `tracing`. A counting subscriber proves that a flood of
//! identical lines is routed once and then suppressed, and that the trampoline
//! never crashes the (foreign-thread) caller.
#![cfg(feature = "ffmpeg")]
// reason: this integration test must call the raw libav `av_log` C entry point
// to drive the real logger → trampoline path it is verifying; there is no safe
// wrapper for *emitting* a synthetic libav log line. The single `unsafe` block
// carries a `// SAFETY:` note. Integration-test files are not covered by the
// crate's `[lints]`, so the allow is restated here.
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ffmpeg_next::ffi;
use tracing::field::{Field, Visit};
use tracing::subscriber::with_default;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// A `tracing` layer that counts events on `target = "libav"` and remembers the
/// last formatted message, so the test can assert routing + suppression.
#[derive(Clone, Default)]
struct CountingLayer {
    count: Arc<AtomicUsize>,
    last_message: Arc<std::sync::Mutex<String>>,
}

struct MessageVisitor<'a> {
    out: &'a mut String,
}

impl Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.out = format!("{value:?}");
        }
    }
}

impl<S> Layer<S> for CountingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "libav" {
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut msg = String::new();
        event.record(&mut MessageVisitor { out: &mut msg });
        if let Ok(mut guard) = self.last_message.lock() {
            *guard = msg;
        }
    }
}

/// Emit one synthetic libav log line at `level` with `text` as a literal format
/// string (no varargs), against a null object (a context-free log line).
fn av_log_line(level: i32, text: &str) {
    let fmt = CString::new(text).expect("no interior NUL in test text");
    // SAFETY: `av_log` takes a NUL-terminated printf format and matching
    // varargs. `fmt` is a valid C string containing no conversion specifiers, so
    // no varargs are consumed; a null `avcl` is an explicitly allowed
    // context-free log line. The pointer is valid for the duration of the call.
    unsafe {
        ffi::av_log(std::ptr::null_mut(), level, fmt.as_ptr());
    }
}

#[test]
fn flood_of_identical_libav_lines_is_routed_once_then_suppressed() {
    // Installing the bridge is idempotent and must not panic.
    multiview_ffmpeg::install_log_bridge();
    multiview_ffmpeg::install_log_bridge();

    let layer = CountingLayer::default();
    let count = Arc::clone(&layer.count);
    let last = Arc::clone(&layer.last_message);
    let subscriber = tracing_subscriber::registry().with(layer);

    with_default(subscriber, || {
        // A unique message text so this test is independent of any other line the
        // process-global suppressor has seen.
        let msg = "multiview-ffmpeg integration flood: corrupt RPS";
        // Drive a flood of 5000 identical INFO lines through the real av_log path.
        for _ in 0..5000 {
            av_log_line(multiview_ffmpeg::AV_LOG_INFO, msg);
        }

        let emitted = count.load(Ordering::Relaxed);
        // Routed: at least the first occurrence reached tracing.
        assert!(emitted >= 1, "expected at least one routed libav record");
        // Suppressed: a 5000-line flood must NOT produce ~5000 records. With a
        // multi-second window and a tight loop, the burst coalesces to a tiny
        // number (the first emit, possibly one window-boundary summary). Assert a
        // hard anti-flood bound well below the input count.
        assert!(
            emitted < 50,
            "anti-flood failed: {emitted} records emitted for 5000 identical lines"
        );

        // The routed message text carried the libav line.
        let seen = last.lock().expect("last-message lock").clone();
        assert!(
            seen.contains("corrupt RPS"),
            "routed record did not carry the libav message, got {seen:?}"
        );
    });
}

#[test]
fn distinct_libav_lines_each_route_independently() {
    multiview_ffmpeg::install_log_bridge();

    let layer = CountingLayer::default();
    let count = Arc::clone(&layer.count);
    let subscriber = tracing_subscriber::registry().with(layer);

    with_default(subscriber, || {
        // Distinct messages → distinct suppressor keys → each emits.
        for i in 0..16 {
            let msg = format!("multiview-ffmpeg distinct integration line {i}");
            av_log_line(multiview_ffmpeg::AV_LOG_WARNING, &msg);
        }
        let emitted = count.load(Ordering::Relaxed);
        assert!(
            emitted >= 16,
            "expected each distinct line routed, got {emitted}"
        );
    });
}

#[test]
fn empty_and_percent_lines_do_not_crash_the_trampoline() {
    multiview_ffmpeg::install_log_bridge();
    let subscriber = tracing_subscriber::registry().with(CountingLayer::default());
    with_default(subscriber, || {
        // A literal `%%` renders to a single `%`; a lone format directive with no
        // varargs is a libav misuse but must not crash our trampoline. We only
        // pass well-formed no-arg formats here (passing `%d` with no vararg is UB
        // at the C level and not something the bridge can defend against), and
        // assert the safe ones route without panicking.
        av_log_line(
            multiview_ffmpeg::AV_LOG_ERROR,
            "100%% rendered literal percent",
        );
        av_log_line(multiview_ffmpeg::AV_LOG_ERROR, "");
        av_log_line(multiview_ffmpeg::AV_LOG_DEBUG, "   ");
    });
}

#[test]
fn varargs_are_expanded_by_the_c_shim_va_list_path() {
    // This is the direct regression test for the x86-64 ingest-thread SIGSEGV.
    // libav emits real log lines with `va_list` varargs (e.g. counts, sizes,
    // addresses). The old Rust trampoline received the `va_list` *by value* and
    // read garbage on x86-64 SysV — handing a bogus pointer to
    // `av_log_format_line2`, which dereferenced null and crashed the decode
    // thread. With rendering moved into the C shim (which owns the `va_list`
    // ABI-correctly), a `%d`/`%s` line is expanded and routed intact. We assert
    // the *expanded* text (not the format string) reaches tracing — proving the
    // shim consumed the varargs correctly rather than crashing or mis-reading.
    multiview_ffmpeg::install_log_bridge();

    let layer = CountingLayer::default();
    let last = Arc::clone(&layer.last_message);
    let subscriber = tracing_subscriber::registry().with(layer);

    with_default(subscriber, || {
        // A unique sentinel so this is independent of the process-global
        // suppressor's prior state, plus two real varargs: an int and a string.
        let fmt = CString::new("multiview shim vararg sentinel n=%d tag=%s").expect("no NUL");
        let tag = CString::new("RPS").expect("no NUL");
        // SAFETY: `av_log` takes a NUL-terminated printf format and matching
        // varargs. `fmt` has exactly one `%d` (matched by the `7` int) and one
        // `%s` (matched by `tag.as_ptr()`, a valid C string outliving the call);
        // a null `avcl` is an explicitly allowed context-free log line.
        unsafe {
            ffi::av_log(
                std::ptr::null_mut(),
                multiview_ffmpeg::AV_LOG_WARNING,
                fmt.as_ptr(),
                7,
                tag.as_ptr(),
            );
        }

        let seen = last.lock().expect("last-message lock").clone();
        // The expanded values must be present — the C shim consumed the va_list.
        assert!(
            seen.contains("n=7"),
            "the int vararg was not expanded by the C shim, got {seen:?}"
        );
        assert!(
            seen.contains("tag=RPS"),
            "the string vararg was not expanded by the C shim, got {seen:?}"
        );
    });
}
