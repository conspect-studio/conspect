# Audio native crash: 2026-11-09

## Incident evidence supplied by operations

The deployed candidate was `12ea65696dbc2f2c9baa264e7c422b8349bb4363`.
At 17:40:48 AEDT the `multiview-audio` thread (TID 3943656) of process
3943514 received SIGSEGV. The reported fault address was `0x7d`, in
`libavformat.so.62.12.101`, at library offset `0x63762`.
Systemd restarted the service. These facts do not identify the failing function.
The preceding cnn-usa HLS extension refusal is correlation, not a demonstrated
cause. Do not disable that source as a substitute for a fix.

## Verified wrapper defect

`Demuxer::stream_parameters` promised an owned codec-parameter snapshot but
returned `ffmpeg-next`'s stream-associated `Parameters`. That object retains an
`Rc` of the native input destructor. Holding it beyond `Demuxer` delayed
`avformat_close_input` until after `Demuxer` freed its boxed interrupt deadline.
The delayed close therefore retained a raw callback pointer to freed Rust state.

The fix deep-copies the parameters before return. A Linux native regression
creates a PCM WAV, keeps the returned parameters, drops the interrupt-enabled
demuxer, and checks that its native file descriptor closes immediately. The
parameters must still construct a valid audio decoder after the input closes.
This regression fails before the fix and passes after it.

This defect is not yet the proven cause of this incident. The normal
`AudioFileDecoder` construction passes parameters into
`StreamAudioDecoder::new`; `ffmpeg-next` copies them into an independent codec
context and releases the temporary parameters before the constructor returns.
The direct `Demuxer` field order is correct: input precedes deadline, so the
input normally closes first. Its open-error paths also close the context while
the deadline remains alive. `ensure_initialized` uses process-wide `Once`;
no network deinitialization calls exist in the repository's Rust source.
No HLS reload defect is established by these observations.

## Host symbol and instruction evidence

Read-only SSH inspection found the loaded library at
`/opt/multiview-ffmpeg/lib/libavformat.so.62.12.101`, build ID
`fee7baeffd7ec41a03cf55e01bc5945d39ba22d1`, SHA256
`df02e80d4a57c66eb706029d8e2b4c8a02416e4f923cbe4b50f2109c40596fc0`.
The library is stripped. `addr2line -f -C` reports the nearest exported
`av_probe_input_buffer`, with `??:?`; this is not a reliable function name.

The exact instruction at `0x63762` is `cmpl $0x2,0x28(%rbx)`. Immediately before
it, the code subtracts playlist fields at offsets `0x11b8 - 0x1188`, compares the
signed result against the count at `0x11a0`, loads an indexed pointer from the
array at `0x11a8`, and checks the pointer for null. It has no negative-index
check. This matches the inlined `current_segment` lookup and SAMPLE-AES
`seg->key_type` test in `hls_read_packet`, not the Rust interrupt callback.
The reported fault address `0x7d` implies a segment pointer of `0x55` at this
instruction, assuming the supplied library offset is correct.

[Upstream FFmpeg n8.1.1 HLS source](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n8.1.1/libavformat/hls.c)
shows `current_segment` calculating `cur_seq_no - start_seq_no` and rejecting
only indices at or above `n_segments`. A negative index can therefore read
before the segment array. A reload that advances `start_seq_no` beyond
`cur_seq_no` is a concrete candidate mechanism. The matching machine code is
strong evidence for this failing block; the actual index and reload sequence
remain inference without a core or a focused reproducer. The wrapper fix above
must not be presented as a fix for this HLS block.

Host core configuration: `core_pattern=core`, service `LimitCORE=infinity`,
`LimitCORESoft=0`. This soft limit normally suppresses core output. Neither
`/home/operations/mv/core` nor `core.3943514` exists. The matching build-ID debug
object is absent. `gdb` is unavailable; `addr2line` and `objdump` are available.
Current service PID remained 317237 with `NRestarts=1`. No host changes occurred.

## Missing incident evidence

Resolve `0x63762` against the exact host library, not the local build. The local
native tests use libavformat 61.7.103, which cannot symbolize the host's version
62.12.101 or prove that version's HLS behavior. Use read-only `addr2line -f -C`
against the actual loaded library. If the object lacks symbols, an unresolved
answer is not a function identification. Preserve the library build ID and
inspect the instruction with an existing `gdb` or `objdump` if available.
Do not install tools, attach to the healthy process, restart, or deploy.

Read `/proc/sys/kernel/core_pattern` and systemd's `LimitCORE` and
`LimitCORESoft` through read-only commands. The absence of `coredumpctl` does
not establish that no core exists. If an existing core and `gdb` are available,
inspect only thread backtraces and the faulting instruction with the exact
executable and library. Do not print arbitrary memory, HTTP URLs, request
headers, environment variables, or source tokens. An actual symbolized frame,
register snapshot, and native HLS reproducer remain unavailable.

## Output-clock isolation requirement (R3)

A SIGSEGV in any native input thread terminates the entire process. Neither a
Rust panic boundary nor a per-input thread can keep the output clock alive in
that case. A durable guarantee needs process isolation for native input work.
The output process must own the fixed-cadence clock and last-good/fallback
frames; replaceable input worker processes must publish through bounded,
nonblocking IPC with explicit buffer ownership. The output tick must never
wait for an input worker, worker shutdown, or IPC lock. Re-stamp from the output
tick counter after every worker failure and reconnect.

This candidate does not change that architecture or claim SIGSEGV containment.
A separate reviewed R3 design and chaos/soak test must kill a native worker,
prove exactly one valid output frame per tick with contiguous timestamps, and
prove bounded resource use and recovery. That work must precede any claim that
native input crashes cannot stop output.
