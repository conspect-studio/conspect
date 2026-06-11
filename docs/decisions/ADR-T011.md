# ADR-T011: HLS rendition isolation — pin the main demuxer to a video variant playlist; the isolated WebVTT reader is the sole WebVTT path

- **Status:** Accepted
- **Area:** Streaming/Timing (ingest)
- **Date:** 2026-06-09 (revised 2026-06-10 for FFmpeg-8.x robustness)
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md) §§3–4
- **Relates to:** [ADR-T004](ADR-T004.md) (HLS ingest pacing), [ADR-T002](ADR-T002.md)
  (last-good-frame + tile state machine), [ADR-E001](ADR-E001.md)
  (decode-at-display-resolution, invariant #6), invariants #1 (output-clock), #2
  (last-good-frame), #6 (decode-at-display-resolution), #10 (isolation)

> **Revision (2026-06-10):** the original fix (post-open `discard_unrouted_subtitles`
> + `strict=normal`) was box-validated as **INEFFECTIVE on FFmpeg 8.1.1** (the
> deploy target). The `strict` rendition gate this relied on lived only in FFmpeg
> **7.x**; **8.x removed it**, so 8.x always fetches the WebVTT rendition and
> `avformat_open_input` aborts loading the broken `.vtt` **before** the post-open
> discard can run. The fix is now to **pin the main demuxer to a video variant
> media playlist pre-open** (which carries no SUBTITLES rendition); the discard +
> `strict` remain as harmless defence-in-depth. See the *Empirical note* below.

> Numbered T011: T009 (per-tile ring publish) and T010 (Dante audio) were already
> assigned in this branch, so the next free Streaming/Timing slot is used.

## Context

An HLS source whose **master** playlist carries a WebVTT subtitle rendition
(`EXT-X-MEDIA:TYPE=SUBTITLES`, e.g. ABC News AU
`https://c.mjh.nz/abc-news.m3u8`) can kill the whole video input. libav's HLS
demuxer folds a selected rendition into the **one shared `AVFormatContext`** it
opens for the video. When that rendition is surfaced and its `.vtt` segment is
corrupt / 404 / token-expired, libav either aborts `avformat_open_input`
("Error when loading first segment", `hls.c`) or makes `av_read_frame` return
that rendition's error for the **entire context**. That error escaped the main
read loop in `multiview-cli`'s `open_and_stream`
(`Err(other) => return Err(...)`), killing the ingest thread and blacking out the
tile.

Whether libav even surfaces the WebVTT rendition is **version-gated**, and the
gate **moved between the major versions Multiview must support**. In FFmpeg 7.1
`hls.c` `new_rendition`:

```c
/* TODO: handle subtitles (each segment has to parsed separately) */
if (c->ctx->strict_std_compliance > FF_COMPLIANCE_EXPERIMENTAL)
    if (type == AVMEDIA_TYPE_SUBTITLE) {
        av_log(c->ctx, AV_LOG_WARNING, "Can't support the subtitle(uri: %s)\n", info->uri);
        return NULL;   // rendition dropped pre-probe
    }
```

So on **7.x**, a normal/strict open drops the rendition before probing it (safe).
**FFmpeg 8.x removed this gate** (the "avformat/hls: add WebVTT subtitle support"
patch added real WebVTT handling and deleted the `strict`-guarded early return):
on **8.x the HLS demuxer ALWAYS fetches the WebVTT rendition** and tries to load
its first `.vtt` segment as part of the open. When that segment is broken / 404 /
token-expired (ABC News AU), **`avformat_open_input` itself aborts** — *before*
any post-open `discard` could run. **`strict` is therefore not a usable guard on
the deploy target** (libavformat 62 / FFmpeg 8.1.1). The only version-robust
defence is to **never point the main demuxer at the master playlist** in the first
place: open a single **video variant media playlist**, which carries no SUBTITLES
rendition for libav to fold in.

Crucially, Multiview **already** ingests HLS WebVTT via a **separate isolated
reader** (`multiview-cli`'s `captions.rs::read_captions`): its own
`AVFormatContext` on its own thread (spawned by the `IngestSupervisor`), opened
with `allowed_extensions=ALL`. The main video demuxer therefore needs **nothing**
from the WebVTT rendition; stopping it from fetching the rendition loses no
stream.

## Decision

The **main video/audio demuxer is pinned to a single video variant media
playlist** (it never opens the master with its selectable SUBTITLES group); the
**isolated WebVTT reader is the sole WebVTT path**. Five parts:

1. **PRIMARY guard — variant-pin pre-open (`multiview-cli` + `multiview-input`).**
   Before opening an HLS URL for the main demuxer, fetch + parse the **master**
   playlist and resolve it to ONE **video variant** media-playlist URL, then open
   *that* (not the master). A variant media playlist lists only its own video/audio
   segments — there is no `#EXT-X-MEDIA:TYPE=SUBTITLES` rendition for libav to fold
   in — so the broken `.vtt` is **never fetched** and the open cannot abort, on
   **either 7.x or 8.x**.
   - `multiview-input::hls::MasterPlaylist` now captures each `#EXT-X-STREAM-INF`
     variant's `BANDWIDTH` and `RESOLUTION` height, and `pick_video_variant(target_height)`
     selects the variant nearest the **displayed tile height** — the smallest rung
     that meets/exceeds it, else the tallest declared, else (no target / no heights)
     the highest-`BANDWIDTH`, else the first. This aligns the pin with
     **decode-at-display-resolution (invariant #6, ADR-E001)**: a 360-px tile pins
     the 360p rung, not 1080p.
   - `multiview-cli`'s `resolve_hls_variant_url` reuses the caption planner's
     in-process libav `PlaylistFetcher` + `resolve_rendition_uri`. It runs on the
     **ingest thread** (control/IO plane) at open time — including on each
     reconnect, so a live master whose variant URLs rotate is re-resolved — and is
     **best-effort**: a fetch/parse miss, or a URL that is already a media playlist
     (no variants), returns `None` and the original URL is opened unchanged (the
     remaining guards below still apply). It never fails the build (invariants
     #1/#10).
2. **Defence-in-depth — discard unrouted subtitles (`multiview-ffmpeg`).** The safe
   free function `discard_unrouted_subtitles(input, keep)` iterates the opened
   input's streams and, for every stream whose `medium() == Subtitle` and index `!=
   keep`, sets `discard = AVDISCARD_ALL` (the one raw field write libav exposes —
   ffmpeg-next 8.1 `StreamMut` has no `set_discard`; bounded `// SAFETY:` in the
   crate's `unsafe = deny` island). `open_and_stream` calls it **before the first
   `read_packet`**, passing `keep = Some(dvbsub.stream_index)` (the routed MPEG-TS
   DVB-sub stream) or `None`. When the variant-pin succeeded there is no subtitle
   stream to discard (harmless); it remains a backstop on the fall-through path.
   **NOTE:** on 8.x this can no longer be the *primary* guard — the open aborts
   loading the `.vtt` before the first read — which is exactly why part 1 exists.
3. **Defence-in-depth — open-time hardening (`multiview-cli`
   `ingest_open_options`).** For an HLS location set `seg_max_retry=8` (a transient
   segment fetch retries), a sane `protocol_whitelist=file,http,https,tcp,tls,crypto,data`
   (HTTPS + AES `crypto` open, no `concat:` surprises), and `strict=normal`
   (defence-in-depth: it dropped the rendition pre-probe on 7.x, but is a no-op for
   this footgun on 8.x). **`allowed_extensions` is never widened to admit `.vtt`**
   in the main demuxer.
4. **Main-source recovery (unchanged).** `ingest_loop` already
   supervised-reconnects a live source on `Err` (capped-exponential + per-source
   jitter), so the main read loop keeps returning `Err` → reconnect (and the
   variant is re-resolved on the next open).
5. **Isolated WebVTT reader recovery.** `caption_loop` is a supervised reconnect
   loop mirroring `ingest_loop`: a **live** rendition (HLS) backs off and retries
   on EOF/error (a transient `.vtt` 404 / token-expiry never permanently kills
   captions); a finite VOD rendition plays out once. The video reconnect helpers
   (`reconnect_backoff` / `JitterRng` / `next_reconnect_attempt` /
   `sleep_interruptible`) are reused (`pub(crate)`).

## Policy invariants

- **The variant-pin selects a VIDEO variant by displayed height (invariant #6).**
  `pick_video_variant(target_height)` chooses the smallest rung that meets/exceeds
  the tile height — so the main demuxer also decodes near the displayed size, never
  the master's top rung for a small tile.
- **The pin re-resolves on each open.** A live master whose variant URLs rotate
  (or a YouTube `*.googlevideo.com` master) is re-fetched on every (re)connect, on
  the ingest thread — never on the output data plane.
- **Best-effort, never fail-the-build.** A master that cannot be fetched/parsed, or
  a URL that is already a media playlist, falls back to opening the original URL
  unchanged; the discard + reconnect bracket remain as guards (invariants #1/#10).
- **The discard keys strictly on `medium() == Subtitle`.** Audio renditions and
  video are **never** touched — an HLS audio rendition keeps flowing.
- **A routed in-container DVB-sub stream is kept** (`keep`): the MPEG-TS DVB-sub
  caption route decodes that stream as a sibling of the video packets, so it must
  not be discarded.
- **Discarding an unrouted stream loses nothing.** Every stream Multiview
  *consumes* has a route (video → decoder; DVB-sub → the kept subtitle route;
  CEA-608/708 → A53 side-data on the video frames) **or** its own isolated
  demuxer (HLS WebVTT → `read_captions`). An unrouted subtitle rendition is, by
  construction, served by the isolated reader or not consumed at all.
- **Isolation/output-clock preserved.** The variant-pin fetch + discard + recovery
  touch only the ingest threads, which write the lock-free last-good stores and
  never pace or stall the output clock (invariants #1/#10); a broken rendition
  rides WARN + RECOVER while the tile holds last-good (#2).

## Consequences

- An HLS source with a broken/expired WebVTT rendition keeps its **video tile
  alive on both FFmpeg 7.x and 8.x**, and the isolated reader keeps retrying the
  captions — the operator's hard "ingest all streams, a broken one recovers, never
  kills the others" requirement.
- Version-robust by construction: the main demuxer is never handed the master's
  SUBTITLES group, so the 8.x change (no `strict` gate) cannot reach the video
  path. The discard + `strict` remain as harmless backstops.
- One extra small master-playlist fetch per (re)open on the ingest thread (a few
  KB, bounded + retried by the shared fetcher), plus one stream-iteration; no
  per-packet cost. The fetch overlaps the open it precedes, off the output clock.

## Empirical note (libav versions)

- **FFmpeg 8.1.1 / libavformat 62 (the deploy target) — box-validated 2026-06-10:**
  the **original** fix (post-open discard + `strict=normal`) was **INEFFECTIVE**.
  8.x removed the `strict` rendition gate, so the HLS demuxer always fetched the
  ABC-News-AU WebVTT rendition; `avformat_open_input` aborted loading
  `index_7_0_*.vtt` (`[hls] Error when loading first segment …` → `Invalid data
  found`), `in_abc` never delivered a frame, and the original signature fired
  repeatedly — *before* the post-open discard could run. This revision (variant-pin
  pre-open) must be re-validated on 8.1.1 on hardware: the variant-pin logic is
  unit-proven offline, but the actual libav-8.x open of a pinned variant playlist
  is box-only.
- **FFmpeg 7.1.4 / libavformat 61 (the dev box):** with the default `strict`
  (normal) the 7.x HLS demuxer drops the WebVTT rendition at parse ("Can't support
  the subtitle"), so the original fix *appeared* to work here — which is exactly
  why the regression escaped to the 8.x deploy. The pure-Rust variant selection +
  master parsing is exercised offline on this box
  (`crates/multiview-input/src/hls.rs` tests, `multiview-cli` `variant_pin_tests`);
  `discard_unrouted_subtitles` removing a surfaced subtitle stream while leaving
  audio/video untouched is proven by
  `crates/multiview-ffmpeg/tests/discard_subtitles.rs`.

## Alternatives rejected / considered

- **Keep relying on `strict=normal` + post-open discard (the original fix).**
  Box-proven dead on 8.x: the gate it depends on was removed, and the open aborts
  before the discard runs. Retained only as defence-in-depth.
- **Pre-filter the master playlist** (fetch it, strip the SUBTITLES `#EXT-X-MEDIA`
  group, feed the modified master to libav via a `data:` URL or temp file). This
  also works on 8.x and keeps libav's own ABR variant selection, but it is a more
  invasive plumbing change (rewriting + re-hosting the master) for no benefit over
  pinning one variant, and it loses the decode-at-display-resolution alignment the
  variant-pin gives for free. Kept as the documented fallback if variant-pinning
  ever proves infeasible.
- **Widen `allowed_extensions` to accept `.vtt` in the main demuxer.** Re-introduces
  the shared-context fragility — the corrupt segment is fetched by the one video
  context, fatal to the video. The isolated reader (own context,
  `allowed_extensions=ALL`) is the only place that path belongs.
- **Swallow the read error in the main loop** (`Err(other) => continue`). Blinds
  the supervisor to a genuinely dead source and breaks the live-source
  reconnect/last-good contract (invariant #2). The error must keep propagating to
  the reconnect bracket; the fix is to stop the broken rendition from being read
  at all.
