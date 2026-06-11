> **Design brief — Realtime API.** Authoritative research/design record backing the implementation. Produced by a verification-hardened multi-agent research workflow (2026-06-02). Canonical crate/API naming lives in [docs/architecture](../architecture/). ADRs derived from this brief are in [docs/decisions](../decisions/).

---

# Multiview Realtime / Eventing API — Authoritative Brief

**Status:** Proposed (lead-merged from 3 area designs) · **Date:** 2026-06-02 · **Owning crate:** `multiview-control` (axum REST + WS) · **Shared types crate:** `multiview-events`

This brief merges (1) the event model, (2) transport/auth/resilience/backpressure, and (3) documentation + typed client + web integration into one design. It is subordinate to and consistent with `core-engine.md` (control = Tokio/axum, data plane = dedicated threads), `resilience-av.md` (bulletproof output invariant, tile state machine §1.3, audio metering SPSC ring §5), and `efficiency.md` ("bound every queue to depth 1–3, drop-oldest; unbounded queues are the OOM failure mode").

---

## 1. Architecture: REST = commands, WS = events/state, SSE = degraded fallback

Multiview has two complementary control-plane surfaces, both served by the same axum router in `multiview-control` under `/api/v1`:

- **REST (OpenAPI 3.1, served at `/docs` via Scalar):** synchronous **commands / CRUD** — create/patch/delete inputs, tiles, outputs, layouts; `apply` config; trigger session restart. Long-running work returns `202 Accepted` + a `correlationId` (a.k.a. `corr`); the result streams on the realtime channel.
- **Realtime WebSocket (PRIMARY, AsyncAPI 3.0, served at `/docs/events`):** bidirectional **events / state** — full snapshot on connect then deltas, tile state-machine transitions, input events, audio meters, output status, alerts, layout/config changes, log tail, job progress, and WHEP preview signaling.
- **SSE (FALLBACK, one-way):** identical envelope, server→client only, for proxies/MITM that strip the WS `Upgrade`. Cannot carry client subscribes or WHEP signaling; preview degrades to "unavailable (degraded transport)" or a separate plain-HTTP WHEP endpoint.

**Load-bearing invariant (non-negotiable):** the realtime layer is strictly best-effort and **physically incapable of back-pressuring the engine**. Every event originates from engine-owned `tokio::watch` (latest-wins state) / `broadcast` (fan-out events) channels; the engine **never awaits a client**. A slow consumer can only ever lose or coalesce *its own* messages, or be disconnected — never stall or OOM the compositor/encoder/output core. This mirrors the engine's own "slow consumer degrades to dropped/stale, never a stall" rule.

There is exactly **one fan-out core** with **two wire encoders** (`WsJson`/`WsBinary` and `SseText`): subscription, sequencing, snapshot, conflation, and resume logic are transport-agnostic; only framing differs.

---

## 2. Versioned message envelope (single wire frame, both directions)

Every message — events, control frames, WHEP signaling — uses ONE envelope so a single parse/validate/route path serves WS and SSE.

```jsonc
{
  "v":    1,                 // u16 envelope schema MAJOR. Client rejects unknown major.
  "t":    "tile.state",      // dotted event type; selects the `data` schema (discriminator)
  "topic":"tiles",           // subscription routing key; control frames use "$control"
  "id":   "tile:big",        // optional resource scope (tile/input/output/job id)
  "seq":  184213,            // u64 per-connection monotonic resume cursor (gaps = drops)
  "ts":   920451123456,      // i64 engine monotonic ns (same clock family as output PTS)
  "corr": "req_5f2a",        // optional correlation id echoing a REST command / job
  "data": { /* typed payload selected by t */ }
}
```

- **Rust:** `struct Envelope<T>{v,t,topic,id,seq,ts,corr,data:T}` with an internally-tagged `EventPayload` enum (`#[serde(tag="t", content="data")]`) → renders as a JSON-Schema `oneOf` with a `const` discriminator → a perfect TypeScript discriminated union for exhaustive `switch` handling.
- **Control frames** (`$hello`/`$subscribe`/`$subscribed`/`$unsubscribe`/`$snapshot`/`$resume`/`$resync`/`$lag`/`$ping`/`$pong`/`$error`) reuse the same envelope with `topic="$control"`.
- **Binary fast-path:** high-rate meter frames MAY use a compact CBOR/MessagePack/fixed-LE body (same envelope shape, `t="audio.meter"`) when the client negotiates subprotocol `multiview.bin.v1`. JSON is the canonical *documented* form; the AsyncAPI schema describes the decoded shape and notes the binary `contentType`.
- **Versioning policy:** additive event types/fields → minor (clients ignore unknown `t`/fields); breaking → bump `v` major. `hello.server_v` advertises supported majors; the server may speak multiple majors during a migration window. The negotiated subprotocol `multiview.v1` makes the wire major explicit.

### Key control schemas

`$hello` (first server frame after auth):
```json
{ "v":1,"t":"$hello","topic":"$control","seq":0,"ts":920000000000,
  "data":{ "session_id":"s_8f3a91c2","server_v":[1],"heartbeat_ms":15000,
    "min_rate_hz":1,"max_rate_hz":30,"default_rate_hz":10,
    "clock_epoch_unix_ns":1717329600000000000,"replay_ring":1024,
    "auth":{ "sub":"operator:troy","scopes":["realtime:read","preview:signal"] } } }
```

`$subscribe` → `$subscribed`:
```jsonc
// client -> server
{"v":1,"t":"$subscribe","topic":"$control","seq":1,"ts":920000111111,
 "data":{"topics":["tiles","audio.meters"],"ids":["tile:big","input:cam1"],"rate_hz":25,"since_seq":184250}}
// server -> client (one ack per topic, then the snapshot)
{"v":1,"t":"$subscribed","topic":"$control","seq":42,"ts":920000112000,
 "data":{"topic":"audio.meters","effective_rate_hz":25,"snapshot_seq":43}}
```

`$resume` / `$resync` / `$lag` (reconnect + overflow):
```jsonc
// client reconnect (after $hello)
{"v":1,"t":"$resume","topic":"$control","seq":0,"ts":920500000000,"data":{"session_id":"s_8f3a91c2","last_seq":184250}}
// server: gap too large -> fresh snapshot
{"v":1,"t":"$resync","topic":"$control","seq":1,"ts":920500001000,"data":{"reason":"seq_evicted","resubscribe":["tiles","outputs","alerts"]}}
// server: THIS connection's queue overflowed -> client should re-snapshot affected topic
{"v":1,"t":"$lag","topic":"$control","seq":184980,"ts":920500050000,"data":{"topic":"audio.meters","dropped_n":143,"action":"conflated"}}
```

---

## 3. Event taxonomy

Topics are deliberately **coarse (group-level)** subscription units carrying fine `t` event types; fine-grained scoping is the `ids` filter, not more topics. High-rate volume is isolated to `audio.meters` (and optionally fast `tile.fps`) so conflation machinery applies only there.

| Event type `t` | Topic | Trigger | Payload (key fields) | Rate / policy |
|---|---|---|---|---|
| `system.health` | `system` | livez/readyz/startup change | `{livez,readyz,startup}` | low / lossless |
| `system.gpu` | `system` | device-lost, rebuild, encoder-recycle | `{event,device,detail}` | low / lossless |
| `system.degradation` | `system` | adaptation-ladder step (per efficiency.md) | `{step,reason}` | low / lossless |
| `capabilities.snapshot` / `.changed` | `capabilities` | startup / probe change | per-(stage×backend×codec) + per-output matrix | low / lossless |
| `input.added` / `.removed` | `inputs` | source add/remove | `{id,kind,uri}` | low / lossless |
| `input.connection` | `inputs` | connected/disconnected/reconnecting/connecting | `{id,state,attempt}` | low / lossless |
| `input.format` | `inputs` | mid-stream codec/res/fps/pixfmt/color change | `{id,codec,w,h,fps,pixfmt}` | low / lossless |
| `input.supervision` | `inputs` | backoff / circuit-breaker Closed/Open/Half-Open | `{id,breaker,backoff_ms}` | low / lossless |
| `input.error` | `inputs` | read/decode error | `{id,error}` | low / lossless |
| `tile.state` | `tiles` | LIVE/STALE/RECONNECTING/NO_SIGNAL transition (resilience-av §1.3) | `{from,to,input,trigger,showing,since_ts}` | low / lossless |
| `tile.fps` | `tiles` | measured fps update | `{id,fps}` | mid / conflate (opt-in fast) |
| `tile.bound` / `.unbound` | `tiles` | input↔tile binding change | `{id,input}` | low / lossless |
| `output.status` | `outputs` | running/error/starting/migrating | `{state,migration}` | low / lossless |
| `output.bitrate` | `outputs` | measured bitrate sample | `{bitrate_bps}` | mid / conflate |
| `output.clients` | `outputs` | consumer connect/disconnect | `{clients}` | low / lossless |
| `output.validity` | `outputs` | SLO probe (gaps/PTS-monotonic/freeze) | `{gaps,pts_monotonic,...}` | low / lossless |
| `audio.meter` | `audio.meters` | per-input/track peak/RMS/clip | `{track,peak_db[],rms_db[],clip,overflow,sampled_hz}` | **HIGH / conflate→sample 10–30 Hz** |
| `audio.loudness` | `audio.loudness` | M/S/I/LRA/dBTP update | `{track,m,s,i,lra,dbtp}` | ~10 Hz / conflate |
| `alert.raised` / `.cleared` / `.updated` | `alerts` | condition raised/cleared | `{key,severity,category,source,title,detail,active}` | low / lossless |
| `layout.changed` | `layout` | resolved DrawQuad diff or full layout | diff or full layout | low / lossless |
| `layout.preview` / `.transition` | `layout` | Preview→Program cue / Cut/Crossfade | `{kind,progress}` | low / lossless |
| `config.changed` / `.applied` / `.rejected` | `config` | operator applies/validates config | `{section,schema_version,by,result}` | low / lossless |
| `log.line` | `logs` | tracing tail | `{level,target,fields,span}` | rate-limited / drop-oldest + marker |
| `job.accepted` / `.progress` / `.result` / `.failed` | `jobs` | long REST command lifecycle (corr) | `{phase,pct,message}` / result | low / lossless |
| `preview.offer` / `.answer` / `.ice` / `.closed` | `preview` | WHEP signaling | `{session,sdp}` / `{session,candidate}` | best-effort, **never dropped by meter policy** |

### Sample event schemas (JSON)

`tile.state`:
```json
{"v":1,"t":"tile.state","topic":"tiles","id":"tile:small1","seq":184213,"ts":920451123456,
 "data":{"from":"RECONNECTING","to":"NO_SIGNAL","input":"input:ndi3","trigger":"nosignal_timeout","showing":"signal_lost_slate","since_ts":920451123456}}
```

`audio.meter` (high-rate, conflated/sampled; numeric only, never audio):
```json
{"v":1,"t":"audio.meter","topic":"audio.meters","id":"input:cam1","seq":901244,"ts":920451140000,
 "data":{"track":0,"peak_db":[-6.2,-7.1],"rms_db":[-18.4,-19.0],"clip":false,"overflow":false,"sampled_hz":25}}
```

`output.status`:
```json
{"v":1,"t":"output.status","topic":"outputs","id":"output:ll_hls_main","seq":52310,"ts":920451200000,
 "data":{"state":"running","bitrate_bps":5980000,"clients":12,"migration":null,"last_validity":{"gaps":0,"pts_monotonic":true}}}
```

`alert.raised` (dedupe key so the same condition coalesces):
```json
{"v":1,"t":"alert.raised","topic":"alerts","id":"alert:enc_recycle_main","seq":7781,"ts":920451300000,
 "data":{"key":"encoder.recycle:output:rtsp_main","severity":"warning","category":"encoder","source":"output:rtsp_main","title":"Encoder recycled behind hot standby","detail":"NVENC INVALID_DEVICE threshold; recycled, output continuous","active":true}}
```

`job.progress` (correlated to the originating REST call via `corr`):
```json
{"v":1,"t":"job.progress","topic":"jobs","id":"job:apply_8821","corr":"req_5f2a","seq":33120,"ts":920451400000,
 "data":{"phase":"prewarming_inputs","pct":60,"message":"input:cam4 connected, filling jitter buffer"}}
```

---

## 4. Snapshot-then-delta model

On `subscribe` the server immediately sends one `$snapshot` per topic: full current state of every resource in that topic (optionally `ids`-filtered) plus the `seq`/`ts` it is current as of. Every later message on that topic is a **DELTA**. Therefore **snapshot ⊕ ordered deltas = current truth**, with no REST polling.

Snapshots are cheap and non-blocking because the engine already maintains latest-state in `watch` channels (the bulletproof design keeps last-good state per tile/output anyway). The snapshot is a read of `watch::Receiver::borrow()` values, never a request the engine must service synchronously.

- **Race-free boundary:** capture the broadcast subscription / `borrow_and_update()` + `changed()` *before* serializing the snapshot, so no transition is lost in the window between snapshot and first delta.
- **Delta granularity:** high-churn topics (tiles/outputs) → field-level patches; `config`/`layout` → structural DrawQuad diff, or an embedded fresh snapshot above a size threshold.
- **Ordering:** within a topic, `seq` is monotonic and deltas are causally ordered after their snapshot. **No global cross-topic order** (each topic streams from its own channel); clients reconcile via `ts`. Changes that must be atomic across resources (e.g. a layout change + the tile states it implies) are emitted as a single coupled delta.

```json
{"v":1,"t":"$snapshot","topic":"tiles","seq":43,"ts":920000112500,
 "data":{"as_of_seq":43,"tiles":[
   {"id":"tile:big","state":"LIVE","input":"input:cam1","fps":50.0,"since_ts":910000000000},
   {"id":"tile:small1","state":"NO_SIGNAL","input":"input:ndi3","fps":0.0,"since_ts":918200000000,"reason":"nosignal_timeout"}]}}
```

---

## 5. Subscription model

Client controls its own view with control frames; the server filters server-side so unwanted events never enter the connection's queue.

- `subscribe{topics[], ids?[], rate_hz?, since_seq?}` — `ids` restricts to a resource subset; `rate_hz` requests a max cadence (only meaningful for high-rate topics; server **clamps** to `[min_rate_hz, max_rate_hz]` and reports effective); `since_seq` does subscribe+resume in one shot.
- `unsubscribe{topics[]}` and `set_rate{topic, rate_hz}` (e.g. operator opens a meter panel → 25 Hz; closes it → 5 Hz or unsubscribe).
- **Per-topic rate** is implemented in the per-connection sender as a *conflating sampler*: a latest-value slot per `(topic,id)` flushed at the clamped cadence — 1000 Hz engine meters → 10–30 Hz wire is pure coalescing (older values overwritten, never queued).
- **Access control hook:** auth scopes gate which topics/ids a client may subscribe to (e.g. `realtime:logs` for `logs`, `realtime:write` for `cmd`, `preview:signal` for `preview`).
- **Default UI subscriptions on connect:** `inputs, tiles, outputs, alerts, layout, config, system, capabilities` at default rate; `audio.meters` only when a meter panel is visible; `logs` only when the log view is open.

---

## 6. Auth handshake (browsers cannot set WS headers)

Server supports all three; **the one-time ticket is the recommended/default path for browsers.** Validation happens **before `on_upgrade`** so failures are debuggable HTTP responses, not silent socket closes.

1. **One-time ticket (default):** authenticated `POST /api/v1/realtime/ticket` (normal bearer/cookie) → `{ticket, expires_in:30, bound_to:{ip,origin}, ws_url}`. Ticket is a random 256-bit single-use token (short-TTL in-memory map, or signed/HMAC stateless), bound to user/scopes + IP + Origin. Browser connects `wss://…/api/v1/realtime?ticket=…&last_seq=N`. Server consumes the ticket atomically on upgrade. Keeps long-lived tokens out of URLs/logs/history/Referer.
2. **Subprotocol token:** `new WebSocket(url, ['multiview.v1','multiview.token.'+jwt])`; server reads `Sec-WebSocket-Protocol`, validates the JWT (same signer/scopes as REST), and **MUST echo back exactly one** non-secret subprotocol (`multiview.v1`) or the browser silently closes (classic gotcha — integration-tested). Logging risk: token may land in proxy logs; documented, ticket preferred.
3. **Cookie (same-origin UI):** session cookie + **mandatory strict `Origin` allow-list check** in the upgrade handler (WS is NOT subject to CORS → CSWSH risk without it).

**API/non-browser clients** can send `Authorization: Bearer` directly on the upgrade. **SSE** uses the Authorization header or the same ticket query param. All paths converge on one `AuthContext{principal, scopes}` shared with the REST identity system. Failure closes with WS code **4401** (auth required) / **4403** (forbidden scope) before any data. First server frame is always `$hello`.

---

## 7. Resilience: heartbeat, reconnect, resume

- **Heartbeat:** server sends a WS `Ping` (and an app-level `$ping` envelope, so proxies and SSE both work) every ~15 s; no `Pong`/frame within ~10 s → close `1011`. SSE sends a comment heartbeat (`: ping\n\n`) on the same cadence. Also enforce a handshake auth deadline and a read-idle timeout.
- **Auto-reconnect (documented client contract, shipped in the typed client):** exponential backoff + full jitter (base 0.5 s, ×2, cap 15 s) mirroring the engine's `backon` usage. Treat close `1008`/`4401` as "mint a new ticket then retry" (not blind retry); `1013` (try-later, sent during overload/restart) as "back off harder".
- **Resume via seq / Last-Event-ID:** the server keeps a **bounded per-session/per-topic replay ring** (e.g. last ~1024 deltas, drop-oldest) of already-serialized envelopes. On reconnect the client presents its last `seq`: WS via `$resume{session_id,last_seq}` (or `?last_seq=`); SSE via the standard `Last-Event-ID` header (the SSE `id:` field carries `seq`). Three outcomes:
  - **Gap replayable** (`last_seq` in the ring): replay missed deltas in order, then resume live — no discontinuity.
  - **Gap too large** (`last_seq` evicted, or `session_id` unknown / server restarted): send `$resync{reason}` + a fresh `$snapshot`, new seq baseline. **The UI MUST treat resync as a full state REBUILD, not a merge** (else stale local state leaks across reconnects).
  - **Normal** (`last_seq` == last sent): nothing to replay.
- **Self-healing overflow:** when this connection's bounded queue drops deltas, the server proactively emits `$lag{topic,dropped_n}`, which triggers a targeted re-snapshot of the affected topic.
- **Memory bounds:** per-session/per-topic rings sized by churn; **high-rate meters are excluded from the replay ring entirely** (latest-only, re-snapshotable, not worth replaying). Sessions expire after a TTL (~30 s) to reclaim ring/state for gone clients.

---

## 8. Backpressure / conflation model (the load-bearing core)

A three-stage isolation funnel; the engine NEVER awaits a client.

1. **Engine publishes** into channels it owns: `tokio::watch` (latest-wins, snapshot-able state — tile state, output status, capabilities, config, layout, and a single `watch::<MeterSnapshot>`) and `tokio::broadcast` (discrete fan-out streams — alerts, log lines, format changes, job progress, tile-state transitions, WHEP-out). `watch::send`/`send_replace` overwrites latest and never blocks; `broadcast::send` never blocks and surfaces slow consumers as `RecvError::Lagged(n)`. Sync data-plane producers bridge via `flume`.
2. **Per-connection session pump** (one tokio task per WS/SSE client) `select!`s over its subscribed receivers, applies conflation + meter sampling, assigns `seq`, and `try_send`s into stage 3. It **converts `Lagged(n)` directly into `$lag` + re-snapshot** — the single most error-prone spot, unit-tested per topic.
3. **Per-connection bounded mpsc** (e.g. 256 frames, `thingbuf`/bounded `tokio::mpsc`) to the socket writer — the ONLY queue a client can fill. Policy on overflow: STATE → conflate (latest-wins); METERS → conflate (latest-wins); ALERTS/JOBS/CONFIG/tile-state → lossless, emit a compact gap marker + re-snapshot rather than block; LOG TAIL → drop-oldest + "logs dropped: N" marker. Persistent wedge → close `1011`/`4408` and let the client reconnect+resume. **One slow client = one disconnect, never a stalled engine.**

**Hard code-review rule:** the pump's hot branches use `try_send` and never `.await` a full queue; **no realtime future is ever `.await`ed on a data-plane thread.**

### High-rate meter sampling (10–30 Hz)
The audio meter DSP thread (already isolated per resilience-av §5: reads a lock-free SPSC drop-oldest ring, never on the media thread) publishes the newest full meter vector into the single `watch::<MeterSnapshot>` (intermediate values dropped for free). Each connection's pump runs a `tokio::time::interval` at the negotiated rate (default 20 Hz, clamp 10–30) and on each tick reads `*watch.borrow()` and sends ONE compact frame (preferably binary fixed-LE: per-track f32 + clip/overflow bitflags). Wire rate is decoupled from production rate; a backlogged client just gets the latest sample next tick. dBTP/true-peak only for tracks the client actually subscribed to (true-peak is ~2.5–3× costlier — cap it to displayed tracks). **Conflation lives in the per-connection task, never the audio thread** — tapping meters synchronously per-subscriber would reintroduce backpressure toward audio and violate the invariant.

---

## 9. WHEP preview signaling over the channel

Live preview uses WHEP (WebRTC-HTTP Egress Protocol); its SDP offer/answer + trickle ICE ride the SAME WebSocket (one authenticated, heartbeated, resumable socket for the whole UI) under the `preview` topic, on its **own bounded best-effort lane with independent metrics**, keyed by `preview_id`/`session`:

```jsonc
// client -> server
{"v":1,"t":"preview.offer","topic":"preview","id":"pv_7","data":{"session":"pv_7","sdp":"v=0...m=video..."}}
// server -> client
{"v":1,"t":"preview.answer","topic":"preview","id":"pv_7","seq":12001,"data":{"session":"pv_7","sdp":"v=0...a=ice-ufrag:..."}}
// both sides trickle: preview.ice{session,candidate}; teardown: preview.stop / preview.closed{session,reason}
```

- It is the **only** topic with meaningful client→server payloads (everything else stays commands-over-REST). **Signaling is correctness-critical, not best-effort-droppable** — it must NOT share the meter conflation/drop policy; isolate it on its own lane (a dedicated `/api/v1/realtime/preview` channel is the option if isolation needs grow; AsyncAPI models both).
- The preview encode is a separate, capacity-budgeted encoder session (counts against the session caps in efficiency.md) and is fully decoupled from program output — a preview client churning never touches the bulletproof core. A resilient reconnect must re-establish or explicitly tear down in-flight PeerConnections to avoid orphaned encoder sessions.
- **SSE-fallback clients** (no upstream channel) get a separate standards-compliant plain-HTTP WHEP endpoint (`POST offer → 201 answer + Location for ICE/DELETE`) or preview is disabled.
- Note: WebRTC media *output* is out of v1 engine scope (core-engine.md), but *preview* WHEP is in scope and the envelope/topic model is forward-compatible if WHEP output is later promoted to a first-class `outputs` kind.

---

## 10. Documentation, typed client, web reconciliation

**Single source of truth:** every wire message is a Rust type in `multiview-events` deriving `serde::{Serialize,Deserialize}` + `schemars::JsonSchema`. The SAME types feed `utoipa` (OpenAPI 3.1) and the AsyncAPI 3.0 generator, so a field added once shows up in both docs and both generated clients — docs cannot drift from the wire.

- **AsyncAPI 3.0** (event-driven analog of OpenAPI) describes the WS server (ticket/subprotocol/cookie `securitySchemes`), the SSE server (modeled as a receive-only channel), every channel/topic + `$control` + `preview`, operations (send/receive), and the envelope message as a `oneOf` over payloads. Generated by an `xtask` (`gen-asyncapi`) using `asyncapi-rust` (v0.2) with a documented `serde_json` post-process to inject the WS `bindings` (method GET, ticket/last_seq query schema) that the crate does not yet emit; validated with `npx @asyncapi/cli validate`. **Fallback** if proc-macro coverage is thin: assemble the AsyncAPI document by hand from `schemars::schema_for!(T)` — same source of truth, more code.
- **Interactive docs hub** served from the binary (rust-embed, no internet): `GET /docs` = Scalar over `/openapi.json` (REST/commands); `GET /docs/events` = `@asyncapi/react-component` (UMD bundle, zero build step) over `/asyncapi.json` (events/state). A landing page explains the split and cross-links (each mutating REST endpoint says "subscribe to topic X / event Y for the result").
- **In-browser WS "Try it" console** beside `/docs/events` (<200 lines, vanilla): token field, Connect (using the documented subprotocol handshake), live auto-scrolling type-filtered envelope log with seq, and a send box pre-filled from AsyncAPI `send` examples (subscribe, command, WHEP offer). "Show meter frames" off by default so it isn't flooded. Doubles as the manual resume/Last-Event-ID smoke test.
- **Typed clients (CI-generated, build fails on git diff):** Modelina emits TS interfaces + the envelope discriminated union from `asyncapi.json` (types only). A thin **hand-written** `MultiviewRealtimeClient` (TS + Rust) owns the lifecycle (subprotocol auth, ping/pong, backoff reconnect, Last-Event-ID/seq resume, snapshot-vs-delta dispatch) typed BY the generated models, exposing `client.on(type, handler)` with full narrowing and `client.send(op, payload)` checked against `send` messages. (Generated WS *runtimes* are avoided — they fight the conflation/resume semantics.)

**Web app (React + TanStack Query) reconciliation:**
- A single `<RealtimeProvider>` instantiates the client once. The connect **SNAPSHOT seeds caches** via `queryClient.setQueryData(['tiles'|'outputs'|'layout'|'config'|'alerts'], …)` → existing `useQuery` hooks render instantly with no HTTP. Set high `staleTime` / disable `refetchOnWindowFocus` (WS is the source of truth; REST GETs are cold-start/fallback only).
- Subsequent **DELTAS** route through one envelope-`type`→reducer map that updates the matching query key immutably (adding an event = adding one exhaustiveness-checked case). This delivers **multi-operator sync** — operator A's layout/config change arrives as a delta into operator B's cache.
- **High-rate meters BYPASS Query** entirely → a zustand/ref store consumed by the canvas on rAF (latest-wins; ballistics/peak-hold client-side). State events → Query cache (low-rate, correctness-critical); meter events → ref store (high-rate, best-effort).
- **Optimistic mutations reconciled by `corr`/mutationId:** REST mutation sends `X-Mutation-Id`; `onMutate` applies optimistically; the echoed WS delta carrying the same id is treated as **confirmation, not a re-apply** (prevents double-apply/flicker); `onError` rolls back; `onSettled` invalidates as the safety net. Long jobs stream progress on the same `corr`.

---

## 11. Realtime data flow (Mermaid)

```mermaid
flowchart LR
  subgraph Data["Data Plane (dedicated threads — NEVER awaits a client)"]
    ENG["Compositor / Encoder / Output core"]
    SUP["Per-input supervisor (state machine)"]
    AUD["Audio meter DSP thread (SPSC drop-oldest ring)"]
    LOGT["tracing layer"]
  end

  subgraph Chan["Engine-owned channels (non-blocking sends)"]
    W["watch (latest-wins): tile/output/config/layout + MeterSnapshot"]
    B["broadcast: alerts, logs, jobs, input/format events, WHEP-out"]
  end

  ENG -->|send_replace| W
  SUP -->|send_replace / send| W
  SUP -->|send| B
  AUD -->|send_replace| W
  LOGT -->|send sampled| B

  subgraph Conn["Per-connection session pump (one tokio task per client)"]
    SEL["select! over subscribed watch.changed / broadcast.recv"]
    CONF["conflate state + sample meters @ clamped rate_hz"]
    SEQ["assign seq + replay ring (per session/topic)"]
    MPSC["bounded mpsc (256) — try_send, drop/conflate/close"]
  end

  W -.borrow/changed.-> SEL
  B -.recv / Lagged(n) -> $lag+resnapshot.-> SEL
  SEL --> CONF --> SEQ --> MPSC

  MPSC --> WR["Writer: WsJson | WsBinary | SseText + Ping/Pong"]
  WR <-->|WS upgrade w/ ticket/subprotocol| BR["Browser / API client"]
  WR -->|SSE id:=seq, Last-Event-ID resume| BR
  BR <-->|WHEP offer/answer/ice (own bounded lane)| WR
  BR -->|REST commands /api/v1 (corr)| ENG
```

---

## 12. Concrete Rust / axum implementation notes

- **Route handler (validate ticket/origin BEFORE upgrade):**
  ```rust
  async fn realtime_ws(ws: WebSocketUpgrade, ticket: WsTicket, State(rt): State<RealtimeState>) -> impl IntoResponse {
      ws.protocols(["multiview.v1"]).on_upgrade(move |sock| session(sock, ticket.auth, rt))
  }
  ```
  `WsTicket` is an axum extractor resolving in the handler pre-`on_upgrade` (TTL + single-use consume + Origin/IP match). Keep the ticket store behind a trait (in-proc map now; Redis later for multi-node).
- **Inside `session`:** `let (mut tx, mut rx) = sock.split();` then spawn two halves under a `tokio_util::CancellationToken` + `TaskTracker` (already used by the supervisor) for clean teardown on disconnect/shutdown.
  - **Writer task:** owns the per-conn bounded mpsc receiver, the Ping `interval`, the pong-deadline; `tokio::select!`s over {mpsc recv, ping tick, pong deadline, cancel}; serializes via the chosen `WireEncoder` and `tx.send`s.
  - **Pump task:** `tokio::select!`s over each subscribed `watch::Receiver::changed()`, each `broadcast::Receiver::recv()` (handle `Lagged(n)` → `$lag` + re-snapshot), the meter `interval` tick, and `rx.next()` for inbound (subscribe/set_rate, WHEP, client pings/acks); assigns `seq`, conflates, `try_send`s (never awaits a full queue).
- **SSE:** reuse the pump via `axum::response::Sse<impl Stream>` adapting the same `OutFrame` stream to `event:`/`id:`/`data:` lines (`id:` = `seq`); require HTTP/2 (per-origin connection cap; SSE is UTF-8 text only so meters are base64/JSON — extra reason WS is preferred for the high-rate lane).
- **Channels:** `tokio::watch` for snapshot-able latest-wins state (incl. single `MeterSnapshot` watch); `tokio::broadcast` for fan-out event streams; per-conn bounded `tokio::mpsc`/`thingbuf` (`try_send`) as the only client-fillable queue; `flume` to bridge sync data-plane producers; `backon` for client reconnect backoff.
- **Crate placement:** lives in `multiview-control`; wire types in a shared no-deps `multiview-events` crate (the contract shared by engine, REST, WS, AsyncAPI generator, and codegen). `xtask` emits the combined OpenAPI + AsyncAPI + JSON-Schema bundle.
- **Metrics (bound cardinality — never label by connection id):** aggregate counters `dropped_meters`, `lagged_events`, `mpsc_full_disconnects`, `active_connections`, `resyncs`.
- **Close codes (documented for clients):** 1000 normal, 1008/4401 auth, 4403 forbidden scope, 1011/4408 server/backpressure, 1013 try-later.
- **Testability:** (a) every example envelope is a CI fixture validated against the generated schema (serde round-trip + JSON-Schema); (b) a conformance harness drives subscribe→snapshot→delta→disconnect→resume asserting seq monotonicity, gap-replay correctness, re-snapshot on overflow, rate clamping; (c) a **backpressure test** attaches a deliberately-stalled consumer and asserts ZERO effect on engine tick/output-validity SLO and bounded memory — the load-bearing automated proof; (d) fuzz the envelope/control parser (cargo-fuzz + arbitrary); (e) integration-test the subprotocol-echo handshake.

---

## 13. Top risks (carried forward, with mitigations)

- **Resync = rebuild, not merge** — short resume is lossless; long disconnect / server restart forces full re-snapshot. UI MUST discard local state on `$resync`; document + test or stale state leaks.
- **No cross-topic ordering** — couple atomic multi-resource changes into one delta; otherwise reconcile via `ts`. Wrong granularity = transient UI inconsistency.
- **`Lagged(n)` handling** is the most error-prone spot — must convert to `$lag` + re-snapshot, unit-tested per topic, or events drop silently.
- **CSWSH** — cookie auth MUST manually allow-list `Origin` (WS bypasses CORS); prefer ticket for browsers.
- **Token-in-URL leakage** — use short-TTL single-use IP/origin-bound ticket; never log full query strings.
- **WHEP must not share meter drop policy** — isolate on its own lane (signaling is correctness-critical).
- **Spec drift** — CI regenerates OpenAPI/AsyncAPI/TS and fails on any git diff; no hand-editing JSON specs.
- **Replay-ring memory** = O(connections × ring × envelope); bound connection count + ring size; exclude meters from the ring.
- **Binary meter frames vs JSON-documented schema** — document the binary `contentType` explicitly so clients don't misparse.
- **Multi-node/HA** — in-proc ticket store + in-process rings don't span replicas; design behind traits now (single-engine today, low priority, called out).
