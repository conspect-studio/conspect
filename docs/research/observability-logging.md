> **Design brief — Observability & Structured Logging.** Authoritative research/design
> record for resource-scoped structured logging and libav log correlation. Produced for the
> 2026-06-13 operator feature-request intake. This is a **design for an unbuilt feature** —
> sections describing current Multiview behaviour are verified against the code, and every
> reference to *existing* code names a real, verified path (`file:line`). Forward-looking
> statements use "should"/"proposed"/"would".

> **Vendor posture.** This brief builds on **open** observability standards only — the Rust
> `tracing` data model, the OpenTelemetry logs/trace-correlation specification (OpenTelemetry
> `trace_id`/`span_id`, carried on the wire as the W3C Trace Context `traceparent` `trace-id` /
> `parent-id`), RFC 5424 syslog, the systemd journal native-protocol fields,
> and the published FFmpeg/libav logging API (`av_log_set_callback`, `AVClass`). No proprietary
> log schema is copied or implied. See [CODE_OF_CONDUCT.md](../../CODE_OF_CONDUCT.md).

# Multiview — Observability & Structured Logging

**Area:** Telemetry / cross-cutting (`multiview-telemetry`, `multiview-ffmpeg`,
`multiview-input`, `multiview-output`, `multiview-engine`, `multiview-control`,
`multiview-events`)
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered
waves.
**Drives:** [ADR-0060](../decisions/ADR-0060.md) (structured, resource-scoped logging taxonomy
+ libav log routing/correlation).
**Extends:** [ADR-R009](../decisions/ADR-R009.md) (telemetry/SLI surface), the BUILT libav→
`tracing` bridge (`crates/multiview-ffmpeg/src/log_bridge.rs`), the BUILT subscriber builder
(`crates/multiview-telemetry/src/tracing_init.rs`), and the BUILT `Topic::Logs` event lane
(`crates/multiview-events/src/topic.rs:57`).
**Relates to:** [ADR-MV001](../decisions/ADR-MV001.md) (X.733 alarm engine — alarms are the
*lifecycle* layer above the log *records* designed here), [ADR-0052](../decisions/ADR-0052.md)
(the two-pipe consent/retention boundary the local log buffer must respect), and sibling intake
briefs [system-stats-attribution.md](system-stats-attribution.md) (per-resource stats share the
same resource-id vocabulary) and [webui-operability-gaps.md](webui-operability-gaps.md) (the
log-tail UI surface).
**Backlog:** `OBS-*` in
[`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> **The defect in one sentence.** A live multiview emits thousands of libav log lines — HLS
> opens, reconnects, decode errors — but today every one of them carries only the libav
> *component* (`hevc`, `hls`), **never which source, output, or layout it belongs to**, so the
> operator reading `Opening 'https://…/cnn.m3u8' for reading` cannot tell *which tile* is
> reconnecting. This brief proposes a resource-scoped span hierarchy plus a libav→resource
> correlation map so every log line — ours and libav's — names the source / output / layout it
> came from, without ever putting logging on or behind the output clock.

---

## 0. Headlines

1. **The bridge exists; the *context* does not.** `multiview-ffmpeg`'s `log_bridge.rs` already
   routes every libav line into `tracing` with a `component` field and a bounded anti-flood
   suppressor (`log_bridge.rs:14-24`, `:559-593`). What is missing is the **resource identity**:
   `component="hls"` tells you it is *an* HLS demuxer, not *which source*. This brief adds the
   missing dimension; it does **not** rebuild the bridge.

2. **Resource-scoped spans are the spine.** Every unit of work runs inside a `tracing` span
   carrying the stable resource id — `run` → (`source{id}` | `output{id}` | `layout{id}` |
   `program`) — so **all** logs emitted under that work (ours *and*, via §3, libav's) inherit
   the id as a structured field. This is the idiomatic `tracing` mechanism (a child event
   inherits its parent span's fields), not a new logging framework.

3. **libav→resource correlation is the hard part, and it is solvable.** A libav callback only
   hands us the object pointer + the component class. The proposal threads our resource id to
   the callback two ways, in priority order: (a) a **task-local resource context** set when a
   source/output owns the calling thread (covers ~all our own libav calls), and (b) an
   **`AVClass` registration map** that resolves the libav context pointer / `item_name` to a
   resource id for lines emitted on libav-owned worker threads. Where neither resolves, the line
   keeps `component` only — honestly, never a wrong id.

4. **Invariant #10 is the first-class constraint.** Logging is **bounded and async and can
   never back-pressure the engine.** The hot path never blocks on a log sink; the suppressor
   lock is held only for an O(cap) lookup off the engine thread (`log_bridge.rs:200-204`); the
   control-API log tail is a **drop-oldest broadcast**, never a queue the engine fills. A wedged
   log consumer loses log lines, never a tick.

5. **Correlation ids, not just text.** Each log record should carry, where known: `run_id`,
   `resource_kind` + `resource_id`, an optional `corr` (the realtime command/job correlation
   key the API already uses for `Topic::Jobs`), and — when an OpenTelemetry exporter is
   configured (opt-in, off by default) — the W3C `trace_id`/`span_id` for trace↔log correlation.

6. **Five surfaces, one record.** The same structured record fans to: stderr/file (the BUILT
   `SubscriberBuilder`), the systemd journal (native structured fields), a bounded control-API
   **log tail** (`GET /api/v1/logs` + `Topic::Logs` stream), the **alarm engine** (MV001 — a
   *recurring* error log becomes an X.733 alarm), and the **consent-independent local buffer**
   (ADR-0052 S5 — diagnostics for the support context-pack, never auto-exported).

7. **Efficiency is bounded by construction.** Span entry attaches a small set of fields (the
   resource ids are `Arc<str>`-backed, so entry clones a refcount, not the string bytes); the
   suppressor is a fixed-cap LRU; the log-tail ring is drop-oldest; the journal/file writers run
   on the subscriber's own thread behind the §5.1 non-blocking appender. No per-line allocation on
   the libav render path (the line is rendered into a C stack buffer before Rust sees it,
   `log_bridge.rs`).

---

## 1. The defect (the operator's libav-log sample)

The operator's running system emits libav lines like this (representative — the exact strings are
libav's, the resource framing is what is *missing*):

```text
[hls @ 0x55f…] Opening 'https://cdn.example/cnn/master.m3u8' for reading
[hls @ 0x55f…] Opening 'https://cdn.example/cnn/720p/seg-4821.ts' for reading
[https @ 0x561…] Opening 'https://cdn.example/bbc/master.m3u8' for reading
[hevc @ 0x57a…] Error constructing the frame RPS.
[AVIOContext @ 0x55f…] Statistics: 1234567 bytes read, 3 seeks
[hls @ 0x563…] Opening 'https://cdn.example/abc/1080p/seg-1190.ts' for reading
```

Today, after the BUILT bridge, those arrive in `tracing` as (verified shape from
`log_bridge.rs:562-592`):

```text
INFO  libav  component="hls"   Opening 'https://cdn.example/cnn/master.m3u8' for reading
ERROR libav  component="hevc"  Error constructing the frame RPS.
```

**What the operator cannot answer from this:**

- *Which source* is the `hevc` RPS error? CNN, BBC, or ABC? They all decode HEVC. The
  `component` is the codec family, not the tile.
- *Which output* (if any) does the `Opening …` line serve — is this an **input** HLS pull or an
  **output** HLS package fetch? Both log through libav's `hls`/`https` classes.
- *Which layout* is on air when this storm of reconnect lines appears? A reconnect during a
  full-screen single-tile layout is a program-affecting event; the same reconnect on an off-air
  source in a 16-tile grid is cosmetic.
- *Are these all one tile flapping*, or three different sources each opening once? The pointer
  (`0x55f…`) differs but is meaningless to a human and unstable across runs.

The framestore tile state machine already *recovers* from all of this (it rides LIVE→STALE→
NO_SIGNAL, [resilience-and-av.md](resilience-and-av.md) §1) and the suppressor already stops the
*flood* (`log_bridge.rs:191-204`). The remaining gap is purely **attribution**: a recovered,
de-flooded log is still useless for triage if it cannot say *which resource*. That is this
brief's whole job.

> **Scope boundary.** This brief is about **log records and their resource context**. The
> *content-aware alarm lifecycle* (raise/clear/ack/roll-up, X.733 severity, SNMP/syslog
> northbound) is [ADR-MV001](../decisions/ADR-MV001.md)'s; §5.3 wires the two together but does
> not duplicate the alarm engine. Per-resource **numeric** stats (bitrate, fps, mbps) are
> [system-stats-attribution.md](system-stats-attribution.md)'s; they share the resource-id
> vocabulary defined in §2.2.

---

## 2. Log taxonomy: span hierarchy + structured fields + correlation ids

### 2.1 The span hierarchy

The proposal is a strict, shallow tree of `tracing` spans. A log event emitted anywhere inside a
span inherits that span's fields, so attribution is automatic for any code that runs inside the
right span.

```text
run{ run_id }                                  ← process-level, opened once at startup
├── source{ resource_kind="source", resource_id, label }     ← per ingest source
│     (ingest open / read / reconnect / decode all run here)
├── output{ resource_kind="output", resource_id, label, transport }  ← per output sink
│     (packager / muxer / push all run here)
├── program{ resource_kind="program" }         ← the protected output core's own work
└── layout{ resource_kind="layout", resource_id }  ← a layout-apply / hot-reconfig span
      (short-lived: wraps an apply_layout / rebind_cell operation)
```

- `run{run_id}` is opened once (a ULID/uuid minted at startup), so cross-restart log analysis can
  group a process's lifetime. It is the parent of everything.
- `source{…}` and `output{…}` are **long-lived** spans, one per declared resource, entered by the
  owning task/thread. `multiview-input` already keys everything by `source_id`
  (`crates/multiview-engine/src/drive.rs:67`, `:327-400`; `crates/multiview-cli/src/pipeline.rs`
  uses `spec.source_id` throughout, e.g. `:2684-2702`), and `multiview-output` already labels by
  `output_id` in display sinks (`crates/multiview-output/src/display/sink.rs:278`,
  `crates/multiview-output/src/display/audio/sink.rs:404-465`). Today these are passed as
  ad-hoc `output = %output_id` **fields on individual log calls**; the proposal promotes them to
  a **span** so they cover *every* line in scope, including libav's (§3).
- `layout{…}` is **short-lived**, wrapping a layout apply / `rebind_cell` (`rebind_cell` is the
  expected seam, e.g. `drive.rs`) so the log of a hot-reconfig names the layout and cell.
  Long-running compositing runs under `program{…}`, not a per-frame layout span (a per-tick span
  would be a #1 hazard — see §4). Because this span is short-lived, a source-reconnect or
  decode-error line emitted *outside* an apply would **not** otherwise name the on-air layout. To
  keep "which layout is on air when this storm appears" answerable (the §1 requirement), the
  proposal samples a coarse **`active_layout_id`** (and, for outputs, `program_layout_id`) from
  engine state — read off-loop, *not* per tick — and attaches it as a plain field on
  source/output/program records where relevant. This is a sampled scalar, never a per-frame layout
  span, so it adds nothing to the output clock (inv #1).
- `program{…}` is the engine's own span. **Per invariant #1, the output-clock loop itself emits
  no per-tick log** (logging in the tick is forbidden, §4); `program{…}` exists so the *rare*
  program-level events (degradation step, supervisor restart) are attributable.

### 2.2 The resource-id vocabulary (shared, stable)

| Field | Type | Source of truth | Notes |
|---|---|---|---|
| `run_id` | string (ULID) | minted at startup in `multiview-cli` | groups a process lifetime |
| `resource_kind` | enum `source\|output\|layout\|program\|device` | — | the discriminator |
| `resource_id` | string | config id (`Source.id`, `Output.id`, `Layout.id`) | **stable across restarts** — the config id, never a pointer or array index |
| `label` | string (optional) | config display label | human name for the UI; never used as a key |
| `transport` | enum (outputs) | output config | `rtsp\|hls\|llhls\|ndi\|rtmp\|srt\|display` |
| `corr` | string (optional) | the realtime command/job correlation key | ties a log to the REST op that caused it (`Topic::Jobs`, `crates/multiview-events/src/topic.rs:60`) |
| `trace_id` / `span_id` | hex (optional) | OpenTelemetry exporter, opt-in | OpenTelemetry `trace_id`/`span_id` (carried on the wire as W3C Trace Context `traceparent` `trace-id`/`parent-id`), off by default (§5.4) |

`resource_id` is the **config id**, deliberately. A pointer (`0x55f…`) is unstable and
meaningless; an array index churns when sources are added/removed. The config id is what the
operator typed, what the API addresses, and what every other surface (alarms, stats, the SPA)
already uses — so one vocabulary spans logs, alarms (MV001), stats, and the UI.

### 2.3 The structured record

Every log record (ours and libav's) should carry: the span-inherited `run_id` /
`resource_kind` / `resource_id` / `label`, the event's own fields, and — for libav lines — the
existing `component` and `repeated` fields (`log_bridge.rs:566`). A line that cannot be attributed
keeps `component` only and **omits** `resource_id` rather than guessing (honesty over a wrong id).

---

## 3. libav log capture & correlation (the hard part)

The BUILT bridge gives us `(level, component, line)` and the libav object pointer `avcl`
(`crates/multiview-ffmpeg/src/log_bridge.rs:616-647`). It already reads `AVClass::class_name`
for `component` (`log_bridge.rs:489-529`). The problem: **the callback runs on libav's own
decoder/IO threads** (`log_bridge.rs:50`), which are *not* inside our `source{…}` span, so the
span-inheritance of §2 does **not** automatically reach libav lines. We need an explicit bridge
from `avcl` (or the calling context) to our `resource_id`. Three mechanisms, applied in priority
order:

### 3.1 Mechanism A — task-local / thread-local resource context (covers our own calls)

Most libav calls Multiview makes (demuxer open, `av_read_frame`, decode flush) run **on a thread
we own for one source/output**. The proposal sets a **`tokio::task_local!` (async) /
`thread_local!` (sync ingest thread) `ResourceContext`** — `{ kind, id, label }` — when that
task/thread takes ownership of a source/output, and the `route()` function in the bridge
(`log_bridge.rs:534-555`) reads it as the *first* attribution source. When libav logs
synchronously from inside our `av_read_frame` call, the callback fires **on our thread**, so the
thread-local is set and the line is correctly attributed with zero per-line cost.

- This is the **primary** path and covers the operator's `Opening '…cnn…m3u8'` case directly:
  the HLS open happens inside the CNN source's ingest thread, which set
  `ResourceContext{ source, "cnn" }` at open time (the `FileSource::open` / `Demuxer::open` seam,
  `crates/multiview-input/src/libav.rs:88-96`).
- It is bounded and wait-free: one small `Arc<str>`-backed value per thread, set when the thread
  adopts the resource, read by the callback without a lock.

> **Scoped, never stale (the wrong-id hazard).** A thread-local that is *set once and left set*
> would misattribute any later libav line that fires on that thread from an **unrelated or nested
> unregistered context** — exactly the failure mode §3.3 forbids. The `ResourceContext` must
> therefore be **scoped to known owned calls** (a RAII guard that sets it on entry to a
> demuxer-open / `av_read_frame` / decode region and **clears it on exit** — `tokio::task_local!`
> scoping for async, a guard-restoring `thread_local!` for the sync ingest thread), so a line
> emitted outside an owned region sees *no* context and falls through to mechanism B/C rather than
> inheriting whichever resource last ran on the thread. Define the per-thread/per-task ownership
> precondition explicitly at each seam; never apply a thread-local that is not provably current.

### 3.2 Mechanism B — `AVClass` registration map (covers libav-owned worker threads)

Some libav lines are emitted from **libav-spawned** threads (frame-threaded decoders, the HLS
sub-demuxer pool) where our thread-local is not set. For these, the only handle is `avcl`. The
proposal maintains a process-global, bounded **`AVClassMap`**: when Multiview opens a libav
context for a resource (the `AVFormatContext*` for source `cnn`), it records
`context_ptr → resource_id` in a small concurrent map (`arc_swap`/`RwLock` over a bounded
`HashMap`), removed on close via `Drop`. The callback resolves attribution by walking from `avcl`:

1. If `avcl` itself is a registered context, use its `resource_id`.
2. Else follow libav's **`parent_log_context_offset`** (verified: `AVClass` exposes a parent-
   context pointer offset so a decoder can point at its `AVCodecContext`/`AVFormatContext`
   parent — FFmpeg `AVClass` reference) to reach the owning format context, and look *that* up.
3. Else read `AVClass::item_name` (the instance-name function, when libav provides one) as a weak
   secondary label — never as the `resource_id`, only as supplementary text.

This is **best-effort and honest**: a context we did not open (rare) resolves to no `resource_id`
and keeps `component` only. The map is bounded by the number of open libav contexts (tens), so it
is trivially within the memory budget.

> **Registration lifetimes & pointer reuse.** A raw `context_ptr` key is only sound while the
> context lives: after `Drop`, the allocator can hand the *same* address to a *different* libav
> context, so a naive `ptr → resource_id` map could mis-attribute a reused pointer to a dead
> resource. The map must therefore: **(i)** name exactly which concrete objects are registered
> (the `AVFormatContext*` we open per source/output, and — where reachable — the child
> `AVIOContext`/protocol/HLS sub-demuxer contexts resolved via `parent_log_context_offset`);
> **(ii)** remove an entry on `Drop` **before** the context's memory can be reused, ordered so no
> lookup observes a freed pointer mapped to a stale id; and **(iii)** carry a per-entry
> generation/epoch (or registration-time guard) so a reused address never resolves to the prior
> owner — a mismatch falls through to `component`-only. Tests must cover nested HLS, protocol, and
> frame-threaded-decoder contexts and a deliberate open→close→reopen pointer-reuse case. This is
> the hardest part of the feature; attribution is treated as best-effort and falls back to
> unattributed whenever ownership is not provable.

> **FFI honesty.** Walking `parent_log_context_offset` is another leading-pointer read of a libav
> struct, exactly the pattern the bridge already uses for `class_name` (`log_bridge.rs:500-519`,
> with `// SAFETY:` notes). It is null-checked at every hop and never assumes layout beyond the
> documented offset; an unresolvable walk falls through to `component`-only. No new `unsafe`
> surface beyond the bounded pointer reads the bridge already performs.

### 3.3 Mechanism C — fall through to component-only (never a wrong id)

When neither A nor B resolves (a context-free line, `avcl == NULL`, `log_bridge.rs:497`; or an
unregistered context), the record keeps the BUILT `component` field and **omits** `resource_id`.
This is the load-bearing honesty rule: an unattributed line is labelled *unattributed*, never
mis-attributed to whichever source happens to be in a thread-local. Tests assert that a line from
a deliberately-unregistered context carries no `resource_id`.

### 3.4 Putting it together

The bridge's `route()` (`log_bridge.rs:534`) gains one step before emit: resolve attribution
(A → B → C), then attach `resource_kind`/`resource_id`/`label` as fields alongside the existing
`component`. The suppressor key (`log_bridge.rs:259`) should be **extended to include the
resolved `resource_id`** so two different sources emitting the *same* libav message text are
suppressed independently — otherwise CNN's RPS error would mask BBC's. (Today the key is
`(level, message)`; the proposal makes it `(level, resource_id, message)`.) The cap stays fixed;
the LRU eviction (`log_bridge.rs:248-254`) keeps memory bounded regardless of source count.

---

## 4. Levels, sampling, rate-limit, bounded ring (invariant #10)

Logging is on the **best-effort** side of the isolation boundary. Every mechanism here is bounded
and asynchronous; none can stall the engine.

### 4.1 The hot path emits no per-tick log

The output-clock loop (`out_pts = f(tick)`, invariant #1) must do bounded O(1) work per tick. A
log call on the tick path — even a suppressed one — is a #1 hazard (formatting cost, a contended
subscriber, a blocking writer). **Rule:** the decode→composite→encode→mux tick path and the
output clock emit **no** `tracing` events per frame. Per-tick observability is **numeric metrics**
(counters/gauges sampled off-loop, [system-stats-attribution.md](system-stats-attribution.md)),
not logs. Rare program events (degradation step, supervisor action) log under `program{…}` and are
inherently low-rate. The gate for this rule is an **architectural boundary test/lint**, not just a
text grep: a `tracing::`-macro grep over the named tick modules is a cheap first pass but is *not*
sufficient on its own — logging can hide behind a local/re-exported macro, a helper fn, a `log`
facade call, or a dependency callback. The real gate is a boundary check that the named tick /
output-clock functions reach no logging facade and no sink call, plus the bounded/non-blocking
tests of §4.4/§5.1.

### 4.2 Level mapping (already BUILT) + per-resource verbosity

The libav→`tracing` level map is BUILT and total (`log_bridge.rs:103-127`): PANIC/FATAL/ERROR →
`error!`, WARNING → `warn!`, INFO → `info!`, VERBOSE/DEBUG → `debug!`, TRACE → `trace!`. The
proposal adds **per-resource filter directives**: because the resource id is a span field, the
`EnvFilter` grammar the subscriber already uses (`tracing_init.rs`) can scope verbosity
(e.g. raise one flapping source to `debug` without drowning the rest). Default level stays `info`
(`tracing_init.rs`). Two caveats to verify before implementation: a span-field directive only
matches events the subscriber sees *inside* the relevant span (libav lines attributed via §3 carry
the field, so they match; events emitted before the span is entered do not); and per-resource
verbosity is most useful if it can change at runtime — that needs a reloadable filter
(`tracing_subscriber::reload`) with a defined behaviour for an invalid/unknown `resource_id`
directive (reject the update, keep the prior filter), which §8 leaves as an open question.

### 4.3 Rate-limit / anti-flood (BUILT, extended)

The bounded LRU suppressor is BUILT (`log_bridge.rs:191-308`): first occurrence emits, identical
repeats inside a 5 s window are suppressed and counted, the next occurrence after the window emits
a coalesced `"… (repeated N× in the last 5s)"` summary (`log_bridge.rs:566`). §3.4 extends the key
with `resource_id` so per-source floods are independent. Cap = 256 keys (`log_bridge.rs:409`),
window = 5 s — both fixed, so an unbounded stream of distinct messages never grows past `cap`
(`log_bridge.rs:199-204`). This is the existing inv-#10 guarantee for the libav path; the
proposal preserves it.

### 4.4 The bounded log-tail ring (the new isolation surface)

The control-API log tail (§5.2) must **never** be a channel the engine fills. The proposal adds a
`tracing` **layer** that mirrors each emitted record into a **bounded drop-oldest ring**
(fixed capacity, e.g. last N=2000 records) and onto a `tokio::broadcast` for live streaming —
exactly the `Topic::Logs` lossy-lane pattern. Properties:

- **Drop-oldest, never block.** A full ring evicts its oldest record; a slow `GET /logs` reader or
  a wedged WebSocket subscriber loses old lines, never stalls a writer. The engine is not even on
  this path (it does not log per-tick), but the rule holds for every emitter.
- **No lock shared with the engine.** The ring's lock is internal to the log layer; the engine
  holds none of it. The §6 buffer and this ring are the only retained log state, both bounded.
- **Conflation is unnecessary** for logs (they are already rate-limited at the source by §4.3),
  unlike the 30 Hz meter lane — but the *transport* shape (drop-oldest broadcast) is identical to
  the proven `AudioMeters`/`Topic::Logs` lanes.

---

## 5. Surfaces: file/journal/stdout + control-API log tail + alarm tie-in

One structured record fans to five sinks. The first is BUILT; the rest are additive layers.

### 5.1 File / journal / stdout (BUILT subscriber + a journal layer)

`multiview-telemetry`'s `SubscriberBuilder` (`tracing_init.rs:42-115`) already writes formatted
lines to stderr/stdout with `EnvFilter` and no-ANSI defaults suited to log files/journald
(`tracing_init.rs:109-114`). The proposal adds an **optional journal layer** that emits the
structured fields as **native systemd journal fields** (`RESOURCE_ID=`, `RESOURCE_KIND=`,
`COMPONENT=`, `RUN_ID=`) so `journalctl RESOURCE_ID=cnn` filters by source — the structured-log
payoff a flat string cannot give. Stdout/stderr stays the default for containers; the journal
layer is opt-in for systemd deployments. Both consume the *same* record; no second formatting.

> **Non-blocking appender (inv #10).** A plain `tracing` stderr/file/journald writer is *not*
> inherently non-blocking — a synchronous `write()` to a full pipe, a slow disk, or a stalled
> journald socket can block the emitting thread. To keep logging strictly off the back-pressure
> path, every sink must sit behind a **bounded, lossy non-blocking appender** (e.g.
> `tracing_appender::non_blocking` with `Lossy(true)`, or an equivalent bounded drop-oldest
> worker), so a wedged sink **drops log records, never stalls a writer**. Engine/output-owned
> threads must **never** write a sink synchronously; the overflow policy (drop-oldest, count the
> drops) is stated and tested. This is the §5.1/§7 expression of the same drop-oldest rule the
> log-tail ring (§4.4) follows.

### 5.2 Control-API log tail (`GET /api/v1/logs` + `Topic::Logs`)

`Topic::Logs` ("Structured log tail") **already exists** in the events crate
(`crates/multiview-events/src/topic.rs:57`) but has no producer. The proposal wires it:

- `GET /api/v1/logs?resource_id=&kind=&level=&since=&limit=` returns the bounded ring's recent
  records as JSON (RFC 9457 problem docs on error; the established `multiview-control` contract).
  Filtering by `resource_id`/`kind`/`level` is the operator's triage entry point — "show me
  everything for source `cnn` at `warn` and above."
- A live tail streams new records on `Topic::Logs` over the existing WebSocket (`/api/v1/ws`) /
  SSE fallback, drop-oldest (§4.4).
- IPv6-first: examples and any bind are dual-stack `[::]`, bracketed literals
  (`wss://[2001:db8::10]/api/v1/ws`), per [conventions §10](../architecture/conventions.md).

This is the operator's direct fix: the same CNN reconnect storm, in the UI, filtered to
`resource_id=cnn`, de-flooded, with the libav lines attributed.

### 5.3 Alarm tie-in (MV001 — records feed the lifecycle)

[ADR-MV001](../decisions/ADR-MV001.md)'s alarm engine owns the **lifecycle** (X.733 severity,
dwell/hysteresis, raise/clear/ack, roll-up, SNMP/syslog/webhook northbound). Logs are the
**evidence layer beneath it**: a *recurring* attributed error log (e.g. source `cnn` emitting
`error` libav lines past a dwell threshold) is exactly the kind of signal that should **raise an
X.733 alarm** scoped to that `resource_id`. The proposal does **not** re-implement the alarm
lifecycle; it ensures every log record carries the `resource_id` the alarm engine needs to scope
an alarm, and notes that the suppressor's coalesced repeat-count (`repeated = N`,
`log_bridge.rs:566`) is a natural dwell input. The X.733 severity vocabulary (Critical / Major /
Minor / Warning / Cleared) is MV001's; logs map to it only at the alarm boundary, not per record.

### 5.4 OpenTelemetry export (opt-in, off by default)

For sites that run an OTel collector, an **optional** `tracing-opentelemetry` layer would attach
the OpenTelemetry `trace_id`/`span_id` to every record (correlated on the wire via the W3C Trace
Context `traceparent` `trace-id`/`parent-id`) and export spans/logs, giving trace↔log correlation
(navigate from a span to its logs). This is **off by default** — no new mandatory dependency, no
data leaves the box unless the operator configures a collector. It must respect the ADR-0052
boundary: OTel export is *operator-configured local/remote diagnostics*, **not** the Aperim
telemetry pipe, and is governed by neither the licensing heartbeat nor the product-telemetry
consent toggle. The Aperim two-pipe model and an operator's own OTel collector are different
planes (see §6).

---

## 6. Privacy / consent boundary vs the Conspect two-pipe model (ADR-0052)

[ADR-0052](../decisions/ADR-0052.md) pins two **never-co-mingled** Aperim-bound pipes: a mandatory
licensing heartbeat (salted digests only) and an opt-in daily product-telemetry pipe (aggregated,
anonymised counters). Logs designed here are a **third, distinct thing** and must not be conflated
with either:

- **Logs are local-first operator diagnostics.** The bounded ring (§4.4), the file/journal output
  (§5.1), and the **consent-independent local buffer** (ADR-0052 S5 — the rolling metrics buffer
  for the support context-pack) are the operator's own data on the operator's own machine. Turning
  the **product-telemetry** pipe off does **not** stop local logging — exactly the consent-
  independence ADR-0052 §3 requires. A test should assert local log capture survives telemetry
  opt-out.
- **Logs never auto-egress to Aperim.** No log line enters the daily telemetry pipe. Logs reach
  Aperim **only** inside a **redacted** support context-pack (ADR-0052 §10 / ADR-0053), and only
  on **explicit local operator approval** of a data request. Until then they never leave the box.
- **Data minimisation in the record itself.** Media URLs are a privacy surface — the operator's
  sample shows full `https://cdn.example/cnn/master.m3u8` URLs in libav lines. The proposal:
  (a) keeps full URLs in the **local** file/journal/ring (the operator needs them to debug); but
  (b) when a record is selected into a **context-pack** for egress, URLs and query strings are
  **redacted to host + resource_id** (the `resource_id` is the non-sensitive handle that makes the
  pack useful without leaking credentials embedded in stream URLs). This mirrors ADR-0052 §8's
  "never transmit raw media URLs" rule.
  - **Local logs are themselves a credential surface.** A full `https://…?token=…` URL left in the
    file/journal/ring is readable by other system users or scraped by host log tooling even though
    it never egresses. The proposal therefore: ships a **configurable local URL-redaction mode**
    (default-safe: strip query strings / userinfo from URLs in the local file and ring, with an
    explicit operator-debug "raw" mode that retains them for deep triage), and writes the log file
    with **restrictive default permissions** (`0600`/owner-only) so the default local posture does
    not leak tokens. The journal inherits journald's own access controls.
- **An operator's own OTel collector (§5.4) is not the Aperim pipe.** Exporting to a collector the
  operator runs is their diagnostic choice on their plane; it is documented as distinct from the
  Aperim telemetry consent so "opt out of telemetry" is never mis-sold as "logs stop working."

---

## 7. Efficiency budget (standing review)

- **Hot path (engine tick):** **zero** log events per tick by rule (§4.1); attribution adds
  nothing to the loop because the loop does not log. The gate is the §4.1 architectural boundary
  test/lint (no logging-facade or sink call reachable from the named tick/output-clock functions),
  not a `tracing::` grep alone.
- **libav callback (decoder/IO threads, off the engine loop):** per line — one thread-local read
  (§3.1) **or** one bounded-map lookup + ≤2 null-checked pointer hops (§3.2); the line is already
  rendered into a C stack buffer before Rust sees it (`log_bridge.rs:54-56`), so **no per-line
  heap allocation** on the render path. The suppressor lock is held for an O(cap≤256) scan only
  (`log_bridge.rs:200-204`). Extending the suppressor key with `resource_id` (§3.4) adds one
  `Arc<str>` clone per *new* key, not per line.
- **Memory:** bounded everywhere — suppressor LRU = 256 keys (fixed, `log_bridge.rs:409`); the
  log-tail ring = N≈2000 records drop-oldest (§4.4); the `AVClassMap` = O(open libav contexts)
  (tens); the local diagnostics buffer = ADR-0052 S5's bounded, auto-pruned buffer. No structure
  grows with uptime or source count beyond its cap.
- **CPU:** span entry/exit attaches/pops a small fixed field set (resource ids are `Arc<str>`, so
  the cost is a refcount clone, not a string copy — not literally `Copy`, but allocation-free per
  entry); field inheritance is a subscriber concern off the producer's critical path; the
  journal/file writers run on the subscriber thread behind the non-blocking appender.
- **I/O:** file/journal/stdout writes go through a **bounded, lossy non-blocking appender** (§5.1)
  so a slow/stalled sink drops records rather than blocking the emitter — a plain `tracing` writer
  is not inherently non-blocking, so this appender is mandatory, not assumed; the API tail is a
  drop-oldest broadcast (never a blocking sink); OTel export (off by default) batches off-thread.
- **GPU:** none — logging never touches the compositor or GPU memory.

Invariants re-asserted: **#1** — no per-tick logging, no log call on the output clock; **#10** —
every log surface is bounded drop-oldest/async, the engine awaits no log consumer and shares no
log lock; **#5/#6/#8** — untouched (logging is orthogonal to the pixel pipeline). IPv6-first for
the log-tail API surface (§5.2).

---

## 8. Open questions

1. **Thread-local vs task-local granularity (§3.1).** Ingest threads are sync (`thread_local!`
   fits); some libav work may run inside tokio tasks (`tokio::task_local!`). The exact split
   depends on the as-built threading of each source type — proposed: both, with a single
   `current_resource()` accessor the bridge calls. Confirm against the input crate's final
   threading model before wiring.
2. **`parent_log_context_offset` portability (§3.2).** The parent-context walk is documented in
   the FFmpeg `AVClass` API, but the offset's reliability across the libav versions Multiview
   ships against (and across frame-threaded decoders) needs a hardware-validated check on the
   deploy ffmpeg, per the project's "validate ingest fixes on the deploy ffmpeg version" rule.
   Fall-through to `component`-only (§3.3) makes a miss harmless, but the *coverage* is worth
   measuring.
3. **Suppressor key cardinality (§3.4).** Adding `resource_id` to the key multiplies distinct keys
   by source count; with 256 cap and many sources, eviction churn could re-emit a flood. Proposed
   mitigation: scale `cap` with declared resource count (still fixed at build, still bounded), or
   a two-level cap (per-resource sub-cap). Pick after measuring real source counts.
4. **Log-tail retention vs the support buffer.** Is the §4.4 API ring the *same* store as the
   ADR-0052 S5 local diagnostics buffer, or a separate shorter one? Proposed: separate — the API
   ring is short (live triage), the S5 buffer is longer and feeds context-packs — but they share
   the record shape. Confirm with the Conspect owner.
5. **OTel as a default-off feature flag.** Which Cargo feature gates `tracing-opentelemetry`
   (`otel`?), and does it pull a deny-clean dependency closure? Needs a `cargo deny` pass before
   adoption (off by default, so it never touches the CI-green baseline).
6. **Redaction policy for URLs in context-packs (§6).** Host + `resource_id` is proposed; whether
   the path/segment name is also useful (without the query string / credentials) for debugging
   playlist issues is an operator call.
7. **Device-scoped logs.** Managed devices (`managed-devices.md`) and Cast/display outputs already
   carry ids; should `resource_kind="device"` be a first-class span like source/output? Proposed
   yes, sharing the vocabulary — but sequenced after the source/output/layout core.

---

## 9. Dependency-ordered backlog (OBS-*)

The full backlog with effort + deps lives in
[`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md). Summary
order (each ships wired end-to-end — no core-without-integration split):

1. **OBS-0** (S) — resource-id vocabulary + the `run/source/output/layout/program` span scaffold in
   `multiview-telemetry` + `multiview-core` (§2). *deps: —*
2. **OBS-1** (M) — thread/task-local `ResourceContext` (§3.1) set at the ingest/output ownership
   seams (`crates/multiview-input/src/libav.rs:88-96`; the output sink spawn sites); bridge reads
   it. *deps: OBS-0*
3. **OBS-2** (M) — `AVClassMap` registration + `parent_log_context_offset`/`item_name` resolution
   in the bridge; suppressor key gains `resource_id` (§3.2-§3.4). *deps: OBS-1*
4. **OBS-3** (M) — bounded log-tail ring + `Topic::Logs` producer + `GET /api/v1/logs` + live tail
   (§4.4, §5.2). *deps: OBS-0*
5. **OBS-4** (S) — systemd journal layer with native structured fields (§5.1). *deps: OBS-0*
6. **OBS-5** (S) — alarm tie-in: recurring attributed errors as an MV001 alarm input (§5.3).
   *deps: OBS-2, MV001*
7. **OBS-6** (M) — consent boundary + context-pack URL redaction (§6). *deps: OBS-3, ADR-0052*
8. **OBS-7** (S, opt-in) — `tracing-opentelemetry` export behind an off-by-default feature (§5.4).
   *deps: OBS-0; gated on a `cargo deny` pass*

**Critical path:** OBS-0 → OBS-1 → OBS-2 (the operator's defect is fixed at OBS-2: every libav line
attributed). OBS-3 makes it visible in the UI; OBS-4/5/6/7 widen the surfaces.
