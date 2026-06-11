# HLS + LL-HLS delivery

> Status: design brief (pre-implementation). Source of truth: the Rust code +
> [`conventions.md`](../architecture/conventions.md). Companion decision:
> [ADR-0032](../decisions/ADR-0032.md). Cross-refs: ADR-0006, ADR-0007, ADR-T005,
> [core-engine §9.2](core-engine.md), [streaming-gotchas §4](streaming-gotchas.md).
>
> This brief is verification-hardened: every as-built claim below is cited to a
> file:line and was checked against the code, not assumed.

## 0. Scope & the one load-bearing idea

Multiview's HLS / LL-HLS delivery **bifurcates by latency tier, not by file
type**. The serving boundary is drawn so the engine hot path (invariants #1, #7,
#10) is *never* on a request path. Two artifact classes:

- **Static-frontable** — multivariant (master) playlist, fully-written media
  segments (`.m4s`/`.ts`), and the fMP4 `init.mp4` (EXT-X-MAP target). These are
  write-once-immutable; any dumb file server / object store / CDN fronts them
  correctly *given the right headers*.
- **Ours-to-serve** — the LL-HLS **live media playlist** (re-rendered per part)
  and **blocking-reload** responses (`_HLS_msn` / `_HLS_part` held GETs),
  preload-hint part delivery, and delta playlists. A static file server has no
  "wait until it exists" primitive, so this endpoint is irreducibly a dynamic
  async origin (axum/hyper + `tokio::sync::Notify` per (msn,part)).

**Dual-write rule.** The segmenter *always* writes static files to disk; the
async handler reads those *same* on-disk artifacts and adds only "wait until it
exists." It is **not** a second packager and **not** a second encoder.

## 1. Why (the design forces)

- **Invariant #1 (output never stalls):** delivery is fed by drop-oldest
  channels from the engine; no delivery code blocks an output tick.
- **Invariant #7 (encode-once-mux-many):** one `ProgramEncoder` encodes the
  canvas once and fans owned `EncodedPacket` copies to N mux-only sinks
  (`sink.rs` ProgramEncoder + the cli fan-out). Container choice lives in the
  muxer the segment sink opens — it adds no encode. CMAF lets one fMP4 set serve
  plain HLS + LL-HLS (+ future DASH), packaging the same bytes many ways.
- **Invariant #10 (isolation):** the LL-HLS origin is a *client* of an
  engine-published drop-oldest watch/broadcast snapshot; held HTTP tasks await a
  `watch`/`Notify`, are hard-capped and time-bounded, and can never
  back-pressure the part writer, encoder, or output clock. Mirrors the existing
  `multiview-engine` `isolation.rs` and `multiview-control` realtime pattern, and
  the sink's "a slow one paces only its own consumer, never the engine."

## 2. As-built vs missing (verified in code)

### 2.1 What exists and is correct

- **Text layer ~complete & RFC 8216bis-correct** (`hls/media.rs`): EXT-X-PART
  (with `INDEPENDENT=YES`), EXT-X-PART-INF:PART-TARGET, EXT-X-SERVER-CONTROL
  (CAN-BLOCK-RELOAD / PART-HOLD-BACK / HOLD-BACK / CAN-SKIP-UNTIL),
  EXT-X-PRELOAD-HINT, EXT-X-RENDITION-REPORT, sliding window with
  MEDIA-SEQUENCE/DISCONTINUITY-SEQUENCE eviction (`trim_to_window`, the §6.2.2
  rule: DISCONTINUITY-SEQUENCE +1 only when the *evicted* segment carried a
  discontinuity), `recompute_target_duration`, EXT-X-MAP gated to fMP4,
  `set_finished` (ENDLIST), version auto-bump 7→9 when parts present.
  `master.rs` builds the multivariant playlist. Matching tests exist.
- **The rolling primitives already exist as public methods** on `MediaPlaylist`
  (`push_segment`, `set_window`, `trim_to_window`) — the text layer needs **zero
  changes** to support a live rolling window.
- **Encode-once fan-out** (`sink.rs` ProgramEncoder → N PacketMuxSinks) is real
  and format-agnostic; PTS is re-stamped from the tick counter (inv #3).
- **Reusable header precedent:** `spa.rs` two-tier cache convention
  (`assets/*` ⇒ `public, max-age=31536000, immutable`; else ⇒ `no-cache`).
- **`tower-http` `cors` feature already enabled** in `multiview-control`
  (Cargo.toml) — `CorsLayer` is declared-but-unwired.

### 2.2 What is missing (the real gap)

| Gap | Where | Evidence |
|-----|-------|----------|
| Behaviour layer ≈ 0% | blocking-reload / Notify / `_HLS_msn` / `_HLS_part` appear **only** in doc comments — no serving code | grep over all crates |
| No fMP4/CMAF producer | `SegmentType::Fmp4` is constructed only in tests; live path hardcodes TS | `sink.rs:840` `MediaPlaylist::new(SegmentType::MpegTs)`; `sink.rs:1516` `Muxer::create_as(&path, "mpegts")`; segments named `seg{n}.ts` (`sink.rs:1510`) |
| Playlist written **once at finalize, non-atomically** | not rolling | `pipeline.rs:1795` `std::fs::write(&path, …)` after the sink thread joins |
| Unbounded segment accumulation | grows forever | `SegmentState.done: Vec` only ever `push`ed; `index = self.done.len()` ⇒ monotonic filenames forever |
| No disk pruning | nothing unlinks | zero `remove_file`/`unlink` in `multiview-output` + `multiview-cli` |
| Unbounded `program.ts` injected unconditionally | always-on file sink | `pipeline.rs:3046` `prepend_file_sink` |
| No byte-range parts | `Part` has only `uri`/`duration`/`independent` | `media.rs:52-60`; renderer emits no BYTERANGE |
| No delta-playlist *production* | CAN-SKIP-UNTIL is *advertised* but not honoured | no EXT-X-SKIP/SKIPPED-SEGMENTS render path |
| No PROGRAM-DATE-TIME | `Segment` has no PDT field | `media.rs` render emits none |
| No config knobs | `path`/`codec`/`*_ms`/`gpu_pin`/`audio` only | `schema.rs:573` (Hls), `schema.rs:551` (LlHls); no container/base_url/segment_dir/dvr/serve/cache/cors |
| Static sidecar mis-served | stock nginx, no custom config | `deploy/compose.yaml` mounts the HLS volume read-only on `:8888`; **no** Cache-Control, **no** CORS, **no** Accept-Ranges |

> **Correction to the original premise (folded from review):** stock
> `nginx:1-alpine` already ships `m3u8`→`application/vnd.apple.mpegurl` and
> `ts`→`video/mp2t` in its bundled `mime.types` (since 2014). So the **present**
> header bugs are: *no Cache-Control, no CORS, no Accept-Ranges* — **not** a
> Safari MIME refusal. The MIME gap is real only for **`.m4s`** (absent from
> nginx defaults) and only **after** the CMAF switch lands. Lead with the real
> bugs; treat `.m4s` MIME as a post-CMAF future bug.
>
> **Distinction to keep explicit:** ADR-0007 *documents* CMAF-first, but the
> **as-built default on the live path is MPEG-TS** (`sink.rs:840`);
> `MediaPlaylist::new()` takes the type from the caller — there is no `Default`
> impl defaulting to fMP4. The foundation work must target this real gap.

## 3. Delivery-tier ladder

| Tier | What | Who serves | Latency | Notes |
|------|------|-----------|---------|-------|
| **Tier 0 — Plain HLS, static-fronted (DEFAULT)** | fMP4/CMAF `.m4s` + EXT-X-MAP `init.mp4` segments + sliding-window media playlist + master playlist, written atomically to disk | nginx / traefik / any CDN / object store. **Multiview does ZERO HTTP.** | ~6–30 s | Segments+init immutable 1y; playlists short max-age. Universal safe default; also the DVR/CDN-offload tier. LGPL-clean. |
| **Tier 1 — LL-HLS via Multiview's own async origin** | the *same* fMP4 segments **plus** EXT-X-PART byte-range parts, served by our blocking-reload handler (`_HLS_msn`/`_HLS_part`, EXT-X-PRELOAD-HINT, EXT-X-SKIP deltas) | Multiview axum/hyper origin | ~2–5 s | Concurrency bounded by the handler's connection/held-request budget, **never** by the engine. Serve over HTTP/1.1 **or** HTTP/2; **HTTP/2 preferred** (multiplexes many held GETs, avoids HTTP/1.1 pool exhaustion). **No HTTP/2 server push** (Apple removed it). HTTPS recommended to reach Safari/hls.js `lowLatencyMode`. |
| **Tier 2 — LL-HLS behind an LL-capable CDN** | CDN coalesces held requests + caches msn/part-keyed playlists in front of our origin | CDN → Multiview origin | ~2–5 s at scale | Only path combining low latency *and* mass concurrency. **CDN contract** (below), not code coupled to a vendor. LL-HLS segments+init stay static-cacheable — only the live playlist + held parts need the dynamic origin. |
| *(out of v1)* Sub-second / interactive | WebRTC (WHEP), not HLS | — | <1 s | Deferred per core-engine §11. |

**Tier 2 CDN contract (operator-facing, documented not coded):** the CDN MUST
(a) include the full `_HLS_*` query set (`_HLS_msn`, `_HLS_part`, `_HLS_skip`,
`_HLS_push`, `_HLS_report`) in the **cache key** — else it serves a stale
lower-msn playlist and the live edge stalls; (b) do request-coalescing /
collapse-forwarding; (c) set its origin-read/idle timeout **> the maximum
blocking hold**, which is bounded by ~1 TARGET (segment) duration + margin —
**not** "3× part duration"; an `_HLS_msn`-only request blocks until the next
whole *segment*; (d) speak HTTP/2 to players; (e) serve the live media playlist
with `Cache-Control: max-age=1, must-revalidate` so an imperfect cache
self-heals.

## 4. Serving / isolation model (inv #10)

Strictly one-directional:

engine → segmenter writes part/segment file → fsync → rename(2) (whole files)
       → bumps watch::Sender(latest_msn, latest_part, byte-offsets)
       → fires per-(msn,part) Notify
                         │
held HTTP tasks ─────────┘  await watch (latest-wins / drop-oldest) + Notify
       → on advance, parked GETs that are now satisfiable read the file & complete

Rules the handler MUST obey:

- The segmenter **never `.await`s** an HTTP task.
- **`watch` is the source of truth; `Notify` is a wakeup hint only.** On wake,
  re-read the watched (msn,part,offset) and re-check satisfiability *before*
  re-parking — closes the lost-wakeup race against a fast-advancing edge.
- **Hard count cap** enforced *before* registering a waiter; past the cap, shed
  to `503` (capacity exhaustion).
- **Time-bounded** by the *HTTP task's own* `tokio::time::timeout` (~bounded by
  HOLD-BACK / the max hold), never by anything the engine awaits.
- A request beyond the satisfiable window (msn/part the playlist will never
  reach) → `400 Bad Request` (correct player retry signal). Reserve `503` for
  capacity shedding only.
- **No-trickle part delivery (corrects core-engine §9.2):** hold the GET until
  the *whole* part is durable on disk, then send the complete part as one
  line-speed burst. **Chunked-transfer of in-progress part bytes is forbidden**
  (many CDNs reject CTE for live; Apple permits hold-then-burst). The handler
  never writes the socket incrementally from the encoder.

A slow/hostile/abandoned client consumes only handler task memory + connection
slots. **CI chaos/soak gate (mandatory):** a held-request flood + abandoned
client + slow consumer must not delay a single output tick.

## 5. Format decision (per tier)

- **Plain HLS (Tier 0): default fMP4/CMAF** (`.m4s` + once-served `init.mp4`),
  **not** MPEG-TS. Keep **MPEG-TS opt-in legacy-reach only**, restricted to the
  plain-HLS tier (hls.js can transmux TS via MSE; TS has effectively zero
  native-Safari LL-HLS value).
- **LL-HLS (Tier 1/2): REQUIRE fMP4/CMAF.** RFC 8216bis *permits* TS parts (a
  Partial Segment MUST be a Supported Media Segment Format, which includes TS,
  and EXT-X-PART supports BYTERANGE), but the decisive reason is **player
  reality**: Safari native LL-HLS plays fMP4 and *fails* on TS livestreams
  (Shaka #7263); AWS recommends CMAF. **Do not offer TS for the LL-HLS tier.**
- **CMAF-unified store:** one encode → one fMP4 segment/part store → HLS
  (plain), LL-HLS (byte-range parts), and (next, additive) DASH `.mpd`, all
  referencing the **same `.m4s` bytes**. Design the store CMAF-clean now; **defer
  the `.mpd` builder.**
- **Precision fixes folded from review:**
  - "Fewer, larger objects" is a property of **byte-range parts**, *not* of fMP4
    per se. fMP4 *enables* it but does not guarantee it — an fMP4 segmenter that
    writes one discrete `.m4s` per part yields *more* objects. **Make byte-range
    parts (EXT-X-PART …,BYTERANGE into one growing `.m4s`, contiguous offsets)
    the LL-HLS default, not discrete per-part files.**
  - fMP4 is **not** bookkeeping-free: it needs monotonic `moof` sequence
    numbers, `styp`/`moof`+`mdat` framing, and a once-served init (EXT-X-MAP). TS
    needs per-part continuity-counter + 188-byte alignment. Choose fMP4 because
    **Safari-native LL-HLS plays it and TS does not** — not because it has less
    bookkeeping.
  - **CMAF "only the manifest differs" is marketing.** Real CMAF→DASH has
    genuine divergences (`prft`/`emsg`, edit-list-vs-decode-time, SegmentTimeline
    alignment, A/V fragment alignment per CTA-5005-A). These are all
    DASH-manifest-side — exactly the part being deferred — so they don't
    undercut the store-now decision, but "design clean now eliminates *all*
    future store work" is overclaimed: see §6.

## 6. Rolling / DVR foundation (common prerequisite)

A **driver change, not a text-layer rewrite** — the window/MSN/DSN logic is
already §6.2.2-correct. Required:

- **Rolling driver:** convert `SegmentState`'s batch `done: Vec` +
  `finish_segments()` into push_segment + set_window + re-render + atomic rewrite
  on every closed segment/part. Bound the in-memory window with a `VecDeque`.
- **DVR depth = a TIME budget** (`dvr_seconds`); derive
  `window = max(floor, ceil(dvr_seconds / segment_seconds))`, floor =
  `max(3×TARGETDURATION, HOLD-BACK, PART-HOLD-BACK coverage)`. Three modes:
  `live` (sliding, omit ENDLIST), `event` (append-only), `vod_on_stop` (ENDLIST
  on clean stop).
- **Atomic publish (mandatory), split by artifact class:**
  - **Whole artifacts** (`.m3u8`, every *closed* segment, `init.mp4`): write to
    a **same-directory** `<name>.tmp`, **fsync**, `rename(2)` (== ffmpeg
    `+temp_file`; same-dir avoids `EXDEV`; fsync-before-rename closes the crash
    durability gap). Today `std::fs::write` (`pipeline.rs:1795`) and
    `Muxer::create_as(final_path)` write in-place — a fronting nginx/CDN can read
    a truncated object. Closed-segment atomicity requires muxing to a `.tmp` path
    and renaming after `Muxer::finish`.
  - **Growing byte-range parts** appended in-place to an open segment: **do NOT
    temp+rename per part** (incompatible with the single-growing-file model).
    Integrity comes from **durable-write-then-announce ordering**: append part
    bytes, fsync, *then* (1) advertise the new EXT-X-PART (offset,length) in the
    atomically-republished playlist and (2) release any held `_HLS_part`/preload
    GET. The §4 hold-until-whole-part rule is the part-integrity mechanism.
- **Bounded disk pruning with the spec grace period:** do **not** unlink on
  eviction. Enqueue an evicted file into a single background reaper (off the hot
  path, inv #10) with deadline `now + grace`, where
  `grace = longest_served_playlist_duration + segment_duration + 1×target_slack`.
  **Use the *longest distributed* playlist, not the LL-HLS live window** (§6.2.2:
  removed segment MUST stay available for segment + longest-playlist duration);
  DVR depth can exceed the live window. Disk bound is finite and computable at
  config time. With byte-range parts, part retention collapses into segment
  retention.
- **Two small text-layer ADDITIONS** (not a rewrite): a per-`Segment`
  EXT-X-PROGRAM-DATE-TIME field (derived from a single `utc0@tick0`
  monotonic→UTC anchor + `rescale(segment_start_tick)`, never summed EXTINF;
  emit on first listed segment, every post-discontinuity segment, ideally every
  segment) and a `set_part_window`/part-pruning method (drop stale EXT-X-PART
  beyond PART-HOLD-BACK / 3×PART-TARGET from the live edge). Also EXT-X-SKIP
  (SKIPPED-SEGMENTS) and EXT-X-PLAYLIST-TYPE (EVENT/VOD).
- **`program.ts`:** make opt-in finite-capture only; default **off** for live.
- **Gate advertised capabilities on behaviour:** do not emit CAN-SKIP-UNTIL /
  CAN-BLOCK-RELOAD until the origin actually honours them.

## 7. Headers contract

Used by Multiview's *own* origin (Tier 1) and emitted as reference nginx/traefik
config for the static-fronting case (Tier 0). Reuse the `spa.rs` two-tier
convention.

- **Content-Type:** `.m3u8` → `application/vnd.apple.mpegurl`; `.ts` →
  `video/mp2t`; `.m4s` / `.mp4`(init) → `video/mp4`. (Stock nginx lacks `.m4s`
  → the concrete post-CMAF bug.) In our axum origin, set Content-Type
  **explicitly per extension** — do not rely on `mime_guess` (also lacks `.m4s`).
- **Cache-Control** (AWS MediaPackage v2 table; align citation):
  - master playlist → `public, max-age=<segment_seconds/2>`
  - plain-HLS media playlist → `max-age=<segment_seconds/2>`
  - LL-HLS live media playlist / blocking response → `max-age=1, must-revalidate`
  - segments + init → `public, max-age=31536000, immutable` (1y is a deliberate
    Multiview choice — segments are write-once-immutable; AWS's own table says 14
    days. State it as our choice, don't attribute 1y to AWS.)
- **CORS** (server's job, every response): `Access-Control-Allow-Origin`
  reflecting the request Origin (fallback `*`, **never** with credentials),
  `Vary: Origin` (required when reflecting, or shared caches leak one origin's
  ACAO to another), `Access-Control-Allow-Methods: GET, HEAD, OPTIONS`,
  `Access-Control-Allow-Headers: Range, Origin`, `Access-Control-Expose-Headers:
  Content-Length, Content-Range`. Wire `tower-http` `CorsLayer` (already a dep).
- **Byte-range:** `Accept-Ranges: bytes` + `206`/`Content-Range` on the
  part-bearing segment (required for byte-range parts).
- **Compression:** gzip/br for `.m3u8` only; identity for media (preserve
  byte-range integrity).
- **Verify, don't assume:** `curl -I` the running stack and assert
  Content-Type / Cache-Control / ACAO / Accept-Ranges, rather than asserting
  nginx behaviour from memory.

## 8. Config surface (serde-default, backward-compatible)

`Output` is `#[serde(tag="kind")]` + `#[non_exhaustive]` with **no**
`deny_unknown_fields` on the variants, so new `#[serde(default, skip_serializing_if)]`
fields are non-breaking — existing path-only configs (`examples/*.toml`) keep
parsing. Type the fields (no stringly map):

| Field | Type / default | Notes |
|-------|----------------|-------|
| `container` | enum `{Fmp4, MpegTs}`, default `Fmp4` | Hls only; LlHls is implicitly fmp4-only. Thread `SegmentType` from config through `build_outputs` into the segmenter (today hardcoded `SegmentType::MpegTs` at `sink.rs:840`). |
| `segment_dir` | `Option<String>`, default = playlist dir | `hls_paths()` (`pipeline.rs:3072`) currently forces playlist dir == segment dir. |
| `base_url` | `Option<String>`, default unset (relative, same-dir; spec-correct) | Prefix prepended to every segment/part/init/variant URI; **MUST be a no-op when unset**. Threaded as a URI prefix into the pure-text Media/Master playlist layer. |
| `init_name` | `Option<String>`, default `"init.mp4"` | Drives EXT-X-MAP. |
| `segment_template` | `Option<String>` | `.m4s` naming. |
| `dvr_mode` | enum `{Live, Event, VodOnStop}`, default `Live` | |
| `dvr_seconds` | `Option<u32>` | DVR depth as TIME; derive window per §6. |
| `delete_grace_segments` | `Option<u32>` | Disk-prune cushion (RFC grace). |
| `program_date_time` | `bool`, default `true` (live) | |
| `serve` | `bool`, default `false` | Use Multiview's own origin (Tier 1) vs static-only (default, LGPL-clean). LL-HLS origin reuses the control listener under a distinct path prefix, or a dedicated lightweight origin. |
| `cache` / `cors_allowed_origins` | `Option<…>` sub-structs | Used when *we* serve; emitted as reference nginx/traefik for static fronting. Sane defaults so operators *honor*, not *invent*. |

**Validation (paired with the serde-default scaffolding):** `dvr_seconds >=`
the window floor (§6); `base_url` is a valid absolute URL or rooted path-prefix;
container/codec consistency.

**Text-layer additions for byte-range + delta + DVR:** add
`byterange: Option<(u64,u64)>` to `Part` (`media.rs:52-60`), render
`,BYTERANGE=<len>[@<off>]` on EXT-X-PART and `BYTERANGE-START` on the preload
hint (default `None` ⇒ existing whole-file TS playlists render unchanged); add
the per-`Segment` PDT field + EXT-X-SKIP render path + EXT-X-PLAYLIST-TYPE.

## 9. Reference fronting config

### 9.1 nginx (Tier 0 static; also Tier 2 origin pass-through)

```nginx
# Tier 0: static front for Multiview's on-disk HLS artifacts.
# Multiview writes files atomically (temp+rename); nginx only adds headers.
map $request_method $cors_max_age { OPTIONS 600; default ""; }

server {
  listen 8888;
  root   /usr/share/nginx/html;   # the HLS volume (read-only)

  # nginx default mime.types lacks .m4s — add the CMAF set explicitly.
  types {
    application/vnd.apple.mpegurl  m3u8;
    video/mp2t                     ts;
    video/mp4                      m4s mp4;
  }

  location ~* \.m3u8$ {
    add_header Cache-Control        "public, max-age=1, must-revalidate" always; # live playlist
    add_header Access-Control-Allow-Origin  $http_origin always;
    add_header Vary                 "Origin" always;
    add_header Access-Control-Allow-Methods "GET, HEAD, OPTIONS" always;
    add_header Access-Control-Allow-Headers "Range, Origin" always;
    gzip on; gzip_types application/vnd.apple.mpegurl;
  }

  location ~* \.(m4s|mp4|ts)$ {
    add_header Cache-Control        "public, max-age=31536000, immutable" always; # segments/init
    add_header Access-Control-Allow-Origin  $http_origin always;
    add_header Vary                 "Origin" always;
    add_header Access-Control-Expose-Headers "Content-Length, Content-Range" always;
    add_header Accept-Ranges        "bytes" always;     # byte-range parts
    # identity only — never gzip media (preserves byte-range integrity)
  }

  if ($request_method = OPTIONS) { return 204; }
}

### 9.2 nginx proxy in front of Multiview's LL-HLS origin (Tier 2)

```nginx
# Tier 2: LL-capable proxy/CDN edge in front of Multiview's async origin.
proxy_cache_path /var/cache/llhls keys_zone=llhls:16m levels=1:2 inactive=10s;

server {
  listen 443 ssl http2;           # HTTP/2 to players (preferred, not push)

  location / {
    proxy_pass         http://multiview_origin;
    # CACHE KEY MUST include the _HLS_* directives, or the live edge stalls:
    proxy_cache_key    "$scheme$host$uri$is_args$args";  # $args carries _HLS_*
    proxy_cache        llhls;
    proxy_cache_valid  200  1s;     # live playlist self-heals
    proxy_cache_lock   on;          # request coalescing / collapse-forwarding
    # Timeout > max blocking hold (~1 target duration + margin), NOT 3x part:
    proxy_read_timeout 30s;
    proxy_buffering    off;         # pass the hold-then-burst part through intact
    proxy_http_version 1.1;
  }
}

### 9.3 traefik (Tier 0, dynamic config sketch)

```yaml
http:
  middlewares:
    hls-playlist-headers:
      headers:
        accessControlAllowOriginListRegex: ["^.*$"]   # reflect (no credentials)
        accessControlAllowMethods: ["GET","HEAD","OPTIONS"]
        addVaryHeader: true
        customResponseHeaders:
          Cache-Control: "public, max-age=1, must-revalidate"
    hls-segment-headers:
      headers:
        accessControlAllowOriginListRegex: ["^.*$"]
        addVaryHeader: true
        customResponseHeaders:
          Cache-Control: "public, max-age=31536000, immutable"
          Accept-Ranges: "bytes"
# Route .m3u8 → hls-playlist-headers; .m4s/.mp4/.ts → hls-segment-headers.
# traefik file server has no .m4s MIME default — set Content-Type at the origin
# (Multiview) or via a fixed customResponseHeaders per route.

## 10. Sequencing

1. **Foundation** — CMAF/fMP4 segmenter + rolling live playlist + atomic publish
   + PDT + DVR window + bounded pruning. No engine risk; all off the hot path.
2. **Static headers + config knobs + reference nginx/traefik** — cheap, immediate
   operator value.
3. **Async blocking-reload / preload-hint axum origin** with bounded held-request
   isolation — the genuinely custom, inv-#10-sensitive piece.

Items 2 and 3 are independent given item 1.

## 11. Open questions / deferred

- DASH `.mpd` builder (CTA-5005-A interop, SegmentTimeline, `prft`) — deferred;
  store designed CMAF-clean so DASH needs no store change (write `prft` per
  fragment anchored to the same `utc0@tick0` clock; align A/V fragment durations;
  emit `styp`).
- Multi-rendition ABR fan-out beyond the single canvas (master playlist already
  models variants; encode-once means a true ABR ladder is a separate encode set).

---

---

# HLS / LL-HLS implementation backlog (PR-sized, dependency-ordered)

Reflects only what survived adversarial review. Each item is one PR. Crate(s)
named per item. TDD-first per repo guardrails (failing test committed
separately).

| ID | Title | Crates | Depends on | Scope (in) | Out of scope |
|----|-------|--------|-----------|-----------|--------------|
| **HLS-0** | Rolling-playlist driver + bounded in-memory window | `multiview-output`, `multiview-cli` | — | Convert `SegmentState.done: Vec` + `finish_segments()` to push_segment + `set_window` + re-render per closed segment using a `VecDeque`; drive the *existing* `MediaPlaylist` rolling methods; `live`/`event`/`vod_on_stop` ENDLIST behaviour. No text-layer changes. | Atomicity, pruning, fMP4, parts |
| **HLS-1** | Atomic publish for whole artifacts | `multiview-output`, `multiview-cli` | HLS-0 | Replace `std::fs::write` (`pipeline.rs:1795`) and in-place segment muxing with same-dir `.tmp` → fsync → `rename(2)`; closed segments muxed to `.tmp` then renamed after `Muxer::finish`. | Part files (byte-range), init |
| **HLS-2** | Deferred-unlink disk pruning (RFC grace) + `program.ts` opt-in | `multiview-output`, `multiview-cli`, `multiview-config` | HLS-0 | Background reaper task (off hot path) with deadline = longest-*distributed*-playlist + segment + 1×target slack; bound disk; make `prepend_file_sink` `program.ts` opt-in, default off. | LL-HLS part retention |
| **HLS-3** | PROGRAM-DATE-TIME (text field + anchor) | `multiview-output`, `multiview-engine`(anchor), `multiview-config` | HLS-0 | Per-`Segment` PDT field + render branch; single `utc0@tick0` monotonic→UTC anchor + `rescale(segment_start_tick)`; `program_date_time` config (default on, live); emit on first/post-discontinuity/every segment. | — |
| **HLS-4** | Configurable locations + `base_url` (no-op when unset) | `multiview-config`, `multiview-cli`, `multiview-output` | HLS-0 | `segment_dir` (split from `path`), `base_url` URI prefix threaded into the pure-text Media/Master layer, `init_name`, `segment_template`; serde-default; validation (valid URL / rooted prefix). MUST be a no-op when unset. | Headers, serving |
| **HLS-5** | Static-tier headers + reference nginx/traefik + CORS wiring | `multiview-control`, `deploy/`, `multiview-config` | HLS-4 | Wire `tower-http CorsLayer` (Origin-reflect + `Vary: Origin`); reference nginx/traefik with explicit `.m4s` MIME, Cache-Control tiers, `Accept-Ranges`; `cache`/`cors_allowed_origins` config; `curl -I` verification test. | Dynamic LL-HLS origin |
| **HLS-6** | FFI: muxer options + fragment flush | `multiview-ffmpeg` | — (parallel) | `Muxer::create_with_options(path, fmt, &[(k,v)])` stuffing an AVDictionary into `avformat_write_header`; `Muxer::flush_fragment()` wrapping `av_write_frame(ctx, NULL)`; stays in FFI-owned crate, `// SAFETY:`. | Segment-sink restructure |
| **HLS-7** | CMAF/fMP4 segmenter + `container` config + shared init | `multiview-output`, `multiview-config`, `multiview-cli` | HLS-1, HLS-6 | `container: {Fmp4,MpegTs}` (default Fmp4) threaded into the sink; open movenc with `cmaf+frag_custom+empty_moov+delay_moov`; write init **once** (EXT-X-MAP), append `.m4s` fragments referencing shared init (replace open-fresh-muxer-per-segment); `flush_fragment` at boundaries. TDD: two-GOP CMAF ⇒ one `ftyp`+`moov` + N `moof`+`mdat`, monotonic seq. | Byte-range parts |
| **HLS-8** | Byte-range parts: text + writer | `multiview-output` | HLS-7 | `Part.byterange: Option<(u64,u64)>` (`media.rs:52-60`) rendered `,BYTERANGE=<len>[@<off>]`; `BYTERANGE-START` on preload hint (default None ⇒ TS unchanged); live **part writer** appending parts into the growing `.m4s` with durable-write-then-announce ordering + per-(msn,part) publish signal. | Serving handler |
| **HLS-9** | Engine→delivery snapshot channel (drop-oldest watch + Notify) | `multiview-output`, `multiview-engine` | HLS-8 | `watch::Sender(latest_msn, latest_part, byte-offsets)` + per-(msn,part) `Notify`, published one-directionally from the segmenter; engine never awaits. Reuse `isolation.rs` pattern. | HTTP handler |
| **HLS-10** | Async LL-HLS origin: blocking reload + preload-hint + bounded isolation | `multiview-control` (or dedicated origin), `multiview-config` | HLS-5, HLS-9 | axum/hyper handler parsing `_HLS_msn`/`_HLS_part`; await `watch` (re-check after `Notify`); **hard count cap** → `503`; **HTTP-task timeout** (~HOLD-BACK); unsatisfiable → `400`; hold-then-burst part delivery (no CTE); 206/`Content-Range`; explicit per-ext Content-Type; `serve` toggle reuses control listener under a path prefix. HTTP/1.1 or HTTP/2 (HTTP/2 preferred, no push). | Delta playlists, CDN |
| **HLS-11** | CI chaos/soak isolation gate | `multiview-engine` tests, CI | HLS-10 | Held-request flood + abandoned + slow client; assert no output tick delayed; assert engine never back-pressured. | — |
| **HLS-12** | Delta playlists (EXT-X-SKIP) + part-window pruning + gate advertised caps | `multiview-output` | HLS-10 | EXT-X-SKIP/SKIPPED-SEGMENTS render path paired with `_HLS_skip`/CAN-SKIP-UNTIL; `set_part_window` dropping stale EXT-X-PART beyond PART-HOLD-BACK / 3×PART-TARGET; gate CAN-BLOCK-RELOAD/CAN-SKIP-UNTIL on behaviour being wired; EXT-X-PLAYLIST-TYPE. | — |
| **HLS-13** | Master / multi-variant polish + Tier-2 CDN reference config | `multiview-output`, `deploy/`, docs | HLS-10 | Ensure `base_url` + variant URIs compose for master; ship reference LL-capable CDN config (cache-key `_HLS_*`, coalescing, timeout > max hold, `max-age=1` live playlist) as a documented operator contract. | DASH |
| **HLS-14** *(deferred, additive)* | CMAF-unified DASH `.mpd` | `multiview-output`, `multiview-config` | HLS-7 | `Output::Dash` + `.mpd` builder over the same `.m4s` store; `prft` per fragment (same `utc0@tick0` anchor), `styp`, A/V fragment alignment (CTA-5005-A); DASH conformance gate. **Defer until requested.** | — |

**Parallelism:** HLS-6 has no deps (FFI, can start immediately). After HLS-0:
HLS-1/HLS-2/HLS-3/HLS-4 are independent. After HLS-4: HLS-5 (headers) and the
HLS-6→HLS-7→HLS-8→HLS-9→HLS-10 fMP4/serving spine are independent. HLS-11/12/13
follow HLS-10. HLS-14 follows HLS-7 and is deferred.

**Guardrail notes:** no hot-path `unwrap`/`expect`/`panic`; bounded drop-oldest
everywhere engine→delivery; re-stamp PTS from the tick counter (never raw input
PTS); keep the default build LGPL-clean (fMP4 via movenc, not GPL muxers; `ndi`
unaffected); every engine→outside channel must pass the chaos gate (HLS-11).
