/*
 * multiview-ffmpeg — libav log-callback C shim.
 *
 * libav's `av_log_set_callback` installs a callback of type
 *
 *     void (*)(void *avcl, int level, const char *fmt, va_list vl)
 *
 * The `va_list` parameter is the footgun: under the x86-64 SysV ABI a
 * `va_list` is `__va_list_tag[1]`, which decays to a single `__va_list_tag*`
 * pointer in one argument register at the call site — but a Rust trampoline
 * that spells that parameter via bindgen's `va_list` *alias* may be compiled to
 * receive a 24-byte `[__va_list_tag; 1]` value, reading garbage and handing a
 * bogus pointer to `av_log_format_line2`, which then dereferences null and
 * SIGSEGVs the decoder thread the instant libav emits any line. There is no
 * stable Rust `VaList` in function-parameter position, and the alias type
 * differs per architecture and per bindgen rendering, so the boundary cannot be
 * made ABI-correct in pure stable Rust.
 *
 * C, by contrast, handles `va_list` natively and correctly on every
 * architecture (x86-64 SysV and arm64 AAPCS alike): the compiler emits the
 * exact ABI the C callback type promises, and `av_log_format_line2` receives a
 * genuine `va_list`. This shim therefore *owns* the `va_list` end to end — it
 * renders the message into a fixed stack buffer with `av_log_format_line2` and
 * hands the already-formatted, NUL-terminated line plus the raw `avcl` object
 * pointer back to Rust. Rust never touches the `va_list`.
 *
 * The Rust side (`log_bridge.rs`) provides `multiview_log_emit`, which maps the
 * level, extracts the component name from `avcl`, runs the bounded anti-flood
 * suppressor, and emits via `tracing`. That callback is panic-safe
 * (`catch_unwind`) and null-tolerant, so this shim may call it unconditionally.
 */

#include <libavutil/log.h>
#include <stdarg.h>
#include <stddef.h>

/*
 * Rust-side callback (see `log_bridge.rs`). Receives the libav object pointer
 * (for component-name extraction; may be NULL), the libav level, and the
 * already-rendered, NUL-terminated log line. It must not be called with a NULL
 * `line`; `line` always points at this shim's stack buffer below.
 */
extern void multiview_log_emit(void *avcl, int level, const char *line);

/*
 * Render-buffer size in bytes: the line payload plus the terminating NUL. Kept
 * deliberately small and fixed so the callback never allocates and the stack
 * footprint is bounded (CLAUDE.md §7 bounded-memory on the data plane). Must
 * stay in sync with `LOG_SHIM_LINE_BUF_LEN` asserted on the Rust side.
 */
#define MULTIVIEW_LOG_LINE_BUF_LEN 1025

/*
 * The libav log callback. Installed via `av_log_set_callback` from Rust.
 *
 * `print_prefix` is initialised to 1 so `av_log_format_line2` prepends libav's
 * `[component @ 0x…]` prefix into the rendered line, matching libav's default
 * presentation; the Rust side additionally surfaces the component as a
 * structured field via `avcl`.
 */
void multiview_av_log_trampoline(void *avcl, int level, const char *fmt,
                                 va_list vl)
{
    /* A NULL format string carries no message — nothing to render. */
    if (fmt == NULL) {
        return;
    }

    char line[MULTIVIEW_LOG_LINE_BUF_LEN];
    int print_prefix = 1;

    /*
     * av_log_format_line2 consumes `vl` exactly once and writes at most
     * line_size-1 characters plus a terminating NUL. We ignore the return
     * value (the would-have-been length) and rely on the guaranteed NUL.
     */
    av_log_format_line2(avcl, level, fmt, vl, line,
                        (int)sizeof(line), &print_prefix);

    /* Defensive: guarantee NUL-termination before handing the line to Rust. */
    line[sizeof(line) - 1] = '\0';

    multiview_log_emit(avcl, level, line);
}
