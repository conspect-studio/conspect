# IPTV-ORG Source Browser — design

Find a public live channel in the community [iptv-org](https://github.com/iptv-org/iptv) database —
by name, category, language, quality, **or by place on a map** — check that it really plays from
this server, and add it as a Conspect source in one step. The source stays **linked** to its
iptv-org entry, so when upstream changes the stream URL, closes the channel, or offers a better
alternative, the operator is told.

| | |
|---|---|
| **Key ADR** | [ADR-M015](../decisions/ADR-M015.md) — server-side sync, provenance, deep probe |
| **Related** | [ADR-M013](../decisions/ADR-M013.md) (outbound-dial guard), [ADR-W015](../decisions/ADR-W015.md) (typed validation), [ADR-W018](../decisions/ADR-W018.md) (live source apply), [ADR-M006](../decisions/ADR-M006.md) (config as code), [ADR-0015](../decisions/ADR-0015.md) (resolver-bound sources) |
| **Crates** | `multiview-config` (provenance + HLS headers), `multiview-ffmpeg` (HLS AVOptions), `multiview-control` (`catalogue` module: sync, index, link tracker, probe), `xtask` (gazetteer generator) |
| **Web** | `/sources/iptv-org` (lazy route), Sources page link-status column |
| **Upstream data** | `https://iptv-org.github.io/api/*.json` — Unlicense. Streams belong to their broadcasters. |

> **Governing principle (unchanged):** catalogue sync, link tracking and probing run on the
> **control plane** with bounded concurrency, bounded memory and timeouts. None of it touches the
> output clock (invariant #1) or makes the engine await a client (invariant #10). A linked source
> is an ordinary `hls` (or `rtmp`/`srt`/`rtsp`) source to the engine.

---

## 1. Upstream data (measured 2026-11-07)

| File | Rows | Size (gz) | Used for |
|---|---|---|---|
| `streams.json` | 18,602 | 0.6 MB | URL, title, quality, `labels`, `user_agent`, `referrer` |
| `channels.json` | 31,747 | 1.0 MB | name, alt names, network, owners, country, categories, closed / `replaced_by`, website |
| `feeds.json` | 45,392 | 0.6 MB | `broadcast_area` (`r/`, `c/`, `s/`, `ct/`), languages, timezones, format |
| `logos.json` | 35,598 | 0.8 MB | logo URL per channel/feed |
| `categories`, `languages`, `countries`, `subdivisions`, `cities`, `regions`, `blocklist`, `timezones` | small | < 1.1 MB | facets, place names, blocklist |

Facts that shape the design:

- **Streams have no id.** Identity is `channel@feed` + URL. 2,096 streams have no channel (linked
  by URL only); 4,976 share a `channel@feed` with another stream (those are *alternatives*).
- **Labels:** `Geo-blocked` 1,225 · `Not 24/7` 1,794. **Headers:** `user_agent` 1,081 · `referrer` 341.
- **Protocols:** 96 % HLS; a few `rtmp` (11), `srt` (3), `rtsp` (2), `mmsh` (2, unsupported).
- **Geography (via feed `broadcast_area`):** country 14,362 · region 781 · city 754 · subdivision
  501 · none 108. 1,983 distinct cities are referenced; Wikidata has coordinates for 1,974.
  In the US, 439 of ~3,000 streams are city/state-local, and call-sign stations are almost always
  city-tagged — the "local broadcaster at a breaking event" case works.
- **Freshness:** GitHub Pages serves `ETag`, `Last-Modified`, `Cache-Control: max-age=600`.

## 2. Architecture

```
iptv-org.github.io/api ──(fixed host, If-None-Match)──► catalogue::sync  (multiview-control)
                                                          │ join + classify → Snapshot
                                                          │ persist <config-dir>/.multiview/catalogues/iptv-org/
                                                          │ (or POST …/import for air-gapped sites)
                                                          ▼
        GET  /api/v1/catalogues/iptv-org/status     sync state, counts, etag, errors
        GET  /api/v1/catalogues/iptv-org/index      compact joined index (ETag'd, gzip)
        GET  /api/v1/catalogues/iptv-org/places     place gazetteer slice (ETag'd)
        GET  /api/v1/catalogues/iptv-org/logos/{k}  bounded logo cache
        POST /api/v1/catalogues/iptv-org/refresh    (administer)
        POST /api/v1/catalogues/iptv-org/import     (administer)
        POST /api/v1/catalogues/iptv-org/probes     (operate)  → probe job
        GET  /api/v1/catalogues/iptv-org/probes/{id}
                                                          │
                          catalogue::links ◄──────────────┘ after every sync
                          per-source LinkStatus → realtime event + health warning
```

### 2.1 Sync

- Interval default **6 h** (minimum 10 min), jittered; manual refresh for administrators.
- Conditional GET per file; a 304 for every file is a no-op. A partial failure keeps the previous
  snapshot (never a half-joined one) and records the error in `status`.
- Bounded: 64 MiB per file, 60 s per request, `reqwest` (rustls), redirects off, the host is a
  compile-time constant. Kill switch: `[catalogues.iptv_org] enabled = false`.
- Snapshot written atomically (`tmp` + rename) with its source ETags and a schema version.

### 2.2 Index

The joined index row is one **stream**, carrying everything the UI filters on:

```jsonc
{
  "k": "s:5f1c…",              // stable key: sha256(url)[..12]
  "url": "https://…/index.m3u8", "title": "KAKE 10 News",
  "ch": "KAKE.us", "feed": "SD", "name": "KAKE", "alt": ["ABC 10"], "net": "ABC",
  "cats": ["news"], "langs": ["eng"], "country": "US",
  "area": ["ct/USICT"], "geo": { "lvl": "city", "places": ["USICT"] },
  "q": "720p", "proto": "hls",
  "flags": ["geo_blocked", "needs_headers", "http_only"],
  "ua": "…", "ref": null, "logo": "l:9a2e…",
  "closed": null, "replaced_by": null, "website": "https://…"
}
```

`flags` are computed server-side (one vocabulary for UI, link tracker and docs):

| Flag | Meaning | Source |
|---|---|---|
| `geo_blocked` | iptv-org says it may be geo-blocked | `labels` |
| `not_24_7` | not on air all day | `labels` |
| `needs_headers` | needs a `User-Agent` and/or `Referer` | `user_agent` / `referrer` |
| `http_only` | plain HTTP, no TLS | URL scheme |
| `closed` | channel closed (may carry `replaced_by`) | `channels.closed` |
| `blocklisted` | channel on the DMCA/NSFW blocklist — hidden by default | `blocklist` |
| `unsupported` | protocol Conspect cannot ingest (`mmsh`) | URL scheme |
| `no_metadata` | stream has no channel | `channel == null` |
| `already_added` | a source is linked to this stream (computed per request, scoped to the caller's visible sources) | sources |

NSFW channels are excluded (iptv-org stopped distributing them in January 2024; the flag is still
honoured defensively).

### 2.3 Provenance on `Source`

```toml
[[sources]]
id = "kake-wichita"
display_name = "KAKE 10 (Wichita)"
kind = "hls"
url = "https://…/index.m3u8"
user_agent = "Mozilla/5.0 …"     # new, optional, HLS only
referrer   = "https://…"         # new, optional, HLS only

[sources.origin]                 # new, optional, any kind
provider   = "iptv-org"
channel    = "KAKE.us"           # absent for no-metadata streams
feed       = "SD"
stream_url = "https://…/index.m3u8"   # URL as linked
labels     = ["Geo-blocked"]
linked_at  = "2026-11-07T05:00:00Z"
follow     = "notify"            # manual | notify (default) | auto
```

`user_agent` / `referrer` are validated (printable, no CR/LF, ≤ 1 KiB) and passed to libav as the
`user_agent` / `referer` AVOptions on the HLS open. They are **not secrets** and are exported.

### 2.4 Link tracker

After each sync, for every source with `origin.provider = "iptv-org"`:

| Status | Rule | Operator action |
|---|---|---|
| `in_sync` | `stream_url` still present, headers unchanged | none |
| `updated` | same `channel@feed`, stream URL/headers/labels changed | **Review changes** (diff), Apply |
| `stream_removed` | URL gone; other streams exist for `channel@feed` | **Switch to alternative** (ranked by probe verdict, quality, flags) |
| `channel_closed` | channel closed; `replaced_by` offered when present | Switch / Unlink |
| `blocklisted` | channel added to the blocklist | Unlink / remove |
| `unlinked` | no match (or operator detached) | — |
| `edited` | operator changed `url` away from `origin.stream_url` | Re-link or Unlink |

`follow = "auto"` applies only `updated` (URL/header change for the same `channel@feed`) through the
normal If-Match write path, audited as the system principal. It never switches channels and never
applies a change that would add `geo_blocked` or `unsupported`. Status changes publish a realtime
`catalogue.link_status` event and a per-source health warning.

## 3. Deep probe

The probe proves that a stream **actually delivers live media from this server**, not that a
root URL answers.

| Stage | Check | Failure verdict |
|---|---|---|
| 1 Guard | `net_guard` screens the resolved IP (ADR-M013); dial the pinned IP; redirects re-screened (max 5) | `blocked` |
| 2 Playlist | GET with the stream's UA/Referer; parse master; pick the variant closest to the advertised quality | `403`/`451` → `geo_blocked_here`; `404`/`410`/DNS/timeout → `dead` |
| 3 Liveness | reload the media playlist twice (target-duration spaced, capped); media sequence must advance; PROGRAM-DATE-TIME (if present) within 5 min of wall clock; `EXT-X-ENDLIST` ⇒ not live | `not_live` / `stalled` |
| 4 Media | fetch 2–3 newest segments; demux **and decode** ≥ 1 video frame and ≥ 100 ms audio with `multiview-ffmpeg` | `no_media` |
| 5 Facts | resolution, fps, codecs, bitrate (measured vs `BANDWIDTH`), audio tracks + languages, captions (608/708, WebVTT renditions), a 320 px JPEG thumbnail | — |
| 6 Events | playlist `EXT-X-CUE-OUT`/`CUE-OUT-CONT`/`CUE-IN`, `EXT-X-DATERANGE` (incl. `SCTE35-OUT/IN/CMD`), `#EXT-OATCLS-SCTE35`; in-band SCTE-35 PIDs parsed with `multiview_input::scte`; ID3 timed metadata; discontinuities | reported, never a failure |

Verdicts: `live` ✅ · `degraded` 🟡 (plays but not live, stalled, or quality far below advertised) ·
`geo_blocked_here` 🌐 · `dead` 🔴 · `no_media` 🔴 · `blocked` 🚫.

Every result records `probed_from` (host name) and `probed_at`. Geo-blocking depends on where
the server is.

**Bounds:** 4 concurrent probes per process (semaphore), queue 64 (overflow ⇒ 429), 20 s
wall clock, 16 MiB transfer, 60 probes/min per principal, results cached 15 min (keyed by URL +
headers). Probe jobs run on the control-plane runtime and a dedicated blocking pool for decode;
they never share a queue with the engine.

Entry points: per-row **Probe**, **Probe all** for the visible results (capped at 50), and an
optional schedule for linked sources (`[catalogues.iptv_org] probe_linked_every = "1h"`, off by
default). A linked source that starts failing raises its link-health warning.

Sample (120 random HLS streams, probed from a Dallas host, 2026-11-07): 86 returned a playlist,
22 errored, 7 × 403, 5 × 404; tags seen: PROGRAM-DATE-TIME 49, DISCONTINUITY 40, CUE-OUT 3,
CUE-IN 1.

## 4. The browser (`/sources/iptv-org`)

Reached from **Browse IPTV-ORG** on the Sources page (a sub-page, not a separate nav item).

### 4.1 Map-first location search

```
┌───────────────────────────────────────────────┬─────────────────────────────┐
│ 🔎 Where is it happening?  [ Wichita, KS    ] │  Near Wichita, Kansas       │
│                                               │  ── Local (city) ────────── │
│      offline vector map — states shaded by    │  KAKE  ABC · 720p ✅ live 2m │
│      stream count, pins for cities with       │  KSN   NBC · 1080p 🌐 Geo   │
│      local streams, clustered when zoomed out │  ── State: Kansas ───────── │
│              ◉ Wichita (3)                    │  KCKS-LD 25.3 · ⏱ Not 24/7  │
│      ◉ Kansas City (5)                        │  ── National (US) ── 2,576 ▸│
│ [◎ near this server] [radius 150 km ▾]        │  [Probe all] [Add selected] │
└───────────────────────────────────────────────┴─────────────────────────────┘
```

- **Place search** over every country, subdivision, city and region name in the gazetteer
  (incl. alternative names), typo-tolerant, offline. Choosing a place flies the map there.
- **Locality-ranked results:** this city → cities within the radius (great-circle) → the
  subdivision → the country → regions that include it. Each tier shows its count; the national
  tier is collapsed by default so local results lead.
- Click a country or subdivision polygon to list everything that broadcasts there; click a pin
  for that city. Map filters combine with every other facet.
- The map is decorative-plus: the place search and result list are a complete keyboard and
  screen-reader path; shading is always paired with text counts.

**Map stack:** MapLibre GL JS (BSD-3) rendering bundled Natural Earth admin-0 and admin-1
boundaries (public domain, simplified, ~1 MB gz, lazy). City coordinates come from a committed
gazetteer generated from Wikidata P625 (CC0) by `cargo xtask iptv-gazetteer` — the server never
queries Wikidata at runtime. An optional operator basemap style URL is off by default.

### 4.2 Search, facets, results

- **Search** (minisearch, fuzzy + prefix) across channel name, alt names, network, owners, stream
  title.
- **Facets** (multi-select, live counts): category · language · quality · protocol · flags ·
  location tree (region → country → subdivision → city).
- **Results** grouped by channel (logo, flag emoji, categories, stream count); expand for feeds
  and streams. Virtualised list (`@tanstack/react-virtual`).
- **Flags** render as icon **+** text badges, never colour alone.
- **Detail panel:** channel metadata, website, each feed's broadcast area/timezones/languages,
  every stream with flags and its latest probe.
- **Add as source:** seeds the existing source dialog (id, name, kind, URL, headers, `origin`).
  **Bulk add** creates several sources in one confirmed step, reporting per-item results.
- i18n (Lingui, `en`/`ar`/pseudo), RTL-safe, WCAG 2.1 AA.

### 4.3 Sources page

A **Link** column (`in sync`, `updated`, …) with **Review changes**, **Switch to alternative**,
**Unlink**, and **Probe** actions.

## 5. Security

- Sync egress: one compile-time host, TLS, no redirects.
- Probe egress: attacker-influenced URLs ⇒ always through `net_guard` (resolved-IP screen, pinned
  dial, re-screened redirects), bounded as in §3. Probe is `operate`; refresh/import are
  `administer`.
- Per-object authorization: `already_added` and link status only reveal sources the caller may see.
- Logos are fetched and cached server-side (size-capped, image content types only) so the browser
  does not load third-party images.
- UA/Referer: no CR/LF, length-capped; only these two headers are settable.
- Follow-up (tracked separately): SSRF screening for ordinary HLS source URLs at ingest.

## 6. Delivery

| Slice | Scope | Class |
|---|---|---|
| A | `origin` + HLS `user_agent`/`referrer` (config, validation, OpenAPI mirror, web form fields), libav AVOptions | R2 |
| B | `catalogue` module: model (promoted from `xtask/src/iptv`), sync, snapshot, index/places/status/refresh/import/logos endpoints; `xtask iptv-gazetteer` + committed gazetteer | R2 |
| C | Deep probe + endpoints; chaos test with hostile fixtures (slowloris, huge segments, redirect to loopback, endless playlist) | R3 |
| D | Link tracker, events, health warnings, `follow` modes | R2 |
| E | Web: browser page, map, place search, facets, virtual list, probe panel, add/bulk add; Sources link column + review/switch dialogs | R2 |
| F | Docs, runbook, e2e | R2 |
