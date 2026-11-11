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

### Builds on (do not duplicate)

The `release/mda001-rc3` line already ships the managed IPTV playlist and EPG
([epg-playlist.md](../architecture/epg-playlist.md)). This feature reuses it:

| Existing | Reuse |
|---|---|
| `crate::iptv::fetch` — SSRF-screened, bounded, conditional (`ETag`) `Fetcher` seam (`iptv-net` feature) | catalogue **sync and logo downloads** go through it (`catalogue` implies `iptv-net`; fixed host, 64 MiB / 60 s per file; logos 512 KiB, image types only). The **deep probe and network diagnostics keep `catalogue::guard::DialGuard`** (still `net_guard`-screened) because they need per-address pinned dials, request headers and per-hop detail that `iptv::fetch` does not offer |
| `Source.playlist_metadata` (`tvg_id`, `name`, `logo`, `group`) | filled on add: `tvg_id = channel@feed`, `name`, `logo`, `group = first category` |
| `Source.epg` (`mode`, `url`, `channel_id`) | **not** filled on add: `guides.json` gives `site`, `site_id`, `site_name` and `lang` but no feed URL (every `sources` array was empty on 2026-11-07). Index rows carry `guide {site, site_id, lang}` so the UI can show that a guide exists; the operator supplies the XMLTV URL |
| `Source.events` + HLS cue observer (ADR-0183) | the probe reuses `multiview_input::hls` cue parsing (`PlaylistCueMetadata`) for broadcast-event detection |
| M3U import dialog (`POST /api/v1/sources/import/m3u`) | unchanged; the browser is the rich alternative for iptv-org specifically |

### Integration rules (release/mda001-rc3)

1. **Durable writes.** Any new persisted resource or config mutation, including `follow = "auto"`
   applying an upstream change, commits through `commit_resource` under `lock_config_mutation`, with
   names and versions in the envelope. Otherwise the edit is lost on restart. The catalogue snapshot,
   logo cache and probe results are derived caches, not configuration.
2. **Media versus metadata.** HLS `user_agent` and `referrer` change media behaviour, so they stay out
   of the metadata-only path (`guide_only_change` in `routes/sources.rs`). An `origin`-only change may
   use that path.
3. **Activation is the operator's choice.** Add and bulk add from the browser require an explicit
   **Always on** or **On demand** choice, with no silent default. Both flows share one control
   (`web/src/iptv/ActivationChoice.tsx`): nothing is preselected, the add control stays disabled until
   one is chosen, and an always-on selection above `LARGE_ALWAYS_ON = 20` shows the same
   large-selection warning as the M3U import dialog (every always-on source opens a connection and a
   decoder at boot). The single "Add as source" step carries the choice into the Sources create dialog
   through the seed (`toSourceForm`); the bulk add writes it through the shared `sourceFormToBody`
   path (`catalogueSourceBody` in `web/src/iptv/toSource.ts`), so every entry states it explicitly.
   The `activation` field (`always_on` | `on_demand`, default `always_on`, from
   `feat/rc-demand-driven-sources`) is durable: the created document records what the operator chose.
   **The server enforces the bounds:** `POST /api/v1/catalogues/iptv-org/sources`
   (Write role, body `{"sources":[{id,name,body}]}`) authorises every id (per-object authorization,
   BOLA) and validates every entry before creating anything — an invalid, duplicate or out-of-scope
   entry is `422`/`403` and nothing is created — then creates the entries in order through the
   canonical `create_source` path (`lock_config_mutation`, `commit_resource`, audit, live apply). One
   request carries at most `BULK_ADD_MAX = 200` sources, which bounds both the request body and the
   single config-mutation lock hold (inv #10, bounded work). Above the old cap of 8 every entry must
   state its `activation` explicitly (`BULK_ADD_EXPLICIT_ACTIVATION_ABOVE = 8`); a batch that leaves
   any entry implicit is refused with `422` — silently starting more than 8 sources always on is the
   operation the old cap guarded against. At or below that threshold an absent `activation` still
   means `always_on`, so existing documents and callers keep the documented default.
4. **Build.** Production builds `--features nvidia,web`. The `nvidia` preset enables
   `catalogue-ffmpeg`, which enables `catalogue`; the PR shows the resolved feature check.
5. **Headers on every rail.** HLS `user_agent`/`referrer` go through the shared loopback fetcher and
   the variant pin; no source bypasses them. The video rail, the audio rail and caption fetches send
   the same headers. Logs record that headers are set, never their values.
   There is no per-origin policy: like libav, the headers go on every request of the source, including
   redirect hops and segment hosts. They are not secrets.

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
  In the US, 439 of ~3,000 streams are city/state-local, but coverage is thin and uneven: 46 of
  51 states/DC have any, the median is 4 streams, California has 107, and Kansas has 2 (KCKS-LD
  25.3 in Kansas City, WPS TV in Wichita, an education channel at 360p). The map finds the local
  broadcasters **iptv-org lists**; many real stations are absent. It must say so when a place has
  few or no local streams, and widen to the radius and state tiers rather than look empty.
- **Local stations are often feeds of a network channel:** KSHB is `NBC.us@KSHBTV`, so the channel
  name is "NBC". Search must index feed names and stream titles, and rows must lead with them.
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
- Bounded: 64 MiB per file, 60 s per request, via the existing `iptv::fetch` fetcher; the host is a
  compile-time constant. Kill switch: `[catalogues.iptv_org] enabled = false`.
- Snapshot written atomically (`tmp` + rename) with its source ETags and a schema version.

### 2.1a Logo cache

`GET …/logos/{k}` is a persistent, bounded disk cache under
`<config-dir>/.multiview/catalogues/iptv-org/logos/` (a derived cache, not configuration).

- **Store.** Key `sha256(url)`: `<key>.bin` (image bytes) + `<key>.json` sidecar (content type,
  `fetched_at`, upstream `ETag`/`Last-Modified`, status, consecutive failures, retry time). Writes are
  temp + rename. Bounded at **256 MiB and 20,000 files** (sidecars count); least-recently-used
  entries are evicted (recency in memory, recovered from sidecar mtimes at startup). A small
  in-memory hot set serves repeats. Image types only, ≤ 512 KiB.
- **TTL.** A cached copy is served for **30 days**; after that it is revalidated in the background
  (`If-None-Match` / `If-Modified-Since`) while the old copy keeps being served. A failed upstream
  never evicts a good copy.
- **Negative cache** (in the sidecar). 404/410/other 4xx, DNS failure, oversized, wrong type: **7
  days**. 429/5xx/timeout: **15 minutes, doubling per consecutive failure, up to 24 h**.
- **Politeness.** `User-Agent` names the software and links
  `https://github.com/conspect-studio/conspect` (Wikimedia's robot policy); at most **2 concurrent
  fetches per host, 8 globally**; every dial goes through the screened `iptv::fetch`.
- **Responses.** `200` with `Cache-Control: private, max-age=2592000, immutable` and a strong `ETag`;
  `304` on a matching `If-None-Match`; `204` with `Cache-Control: private, max-age=86400` for a
  known-missing logo (negative cached, oversized, wrong type, unknown key); `204` with
  `Cache-Control: no-store` when no fetch slot freed within 5 s (the browser retries later). Never an
  error status for a missing logo. The web UI shows a neutral initials placeholder for any of these.

### 2.2 Index

The joined index row is one **stream**, carrying everything the UI filters on:

```jsonc
{
  "k": "s:5f1c…",              // stable key: sha256(url)[..12]
  "url": "https://…/index.m3u8", "title": "KSHB 41 (Kansas City MO)",
  "ch": "NBC.us", "feed": "KSHBTV", "feed_name": "KSHB-TV", "name": "NBC", "alt": [], "net": null,
  "cats": ["general"], "langs": ["eng"], "country": "US",
  "area": ["ct/USMKC"], "geo": { "lvl": "city", "places": ["USMKC"] },
  "q": "720p", "proto": "hls",
  "flags": [],
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
| `already_added` | a source is linked to this stream — **client-computed**: the web derives it from the caller's own visible sources (`flagsOf`); the server never sets it | sources (web) |

NSFW channels are excluded (iptv-org stopped distributing them in January 2024; the flag is still
honoured defensively).

### 2.3 Provenance on `Source`

```toml
[[sources]]
id = "kshb-kansas-city"
display_name = "KSHB 41 (Kansas City MO)"
kind = "hls"
url = "https://…/index.m3u8"
user_agent = "Mozilla/5.0 …"     # new, optional, HLS only
referrer   = "https://…"         # new, optional, HLS only

[sources.origin]                 # new, optional, any kind
provider   = "iptv-org"
channel    = "NBC.us"            # absent for no-metadata streams
feed       = "KSHBTV"
stream_url = "https://…/index.m3u8"   # URL as linked
labels     = []
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
optional schedule for linked sources (`[catalogues.iptv_org] probe_linked_every = "60m"`, off by
default). A linked source that starts failing raises its link-health warning.

Sample (120 random HLS streams, probed from a Dallas host, 2026-11-07): 86 returned a playlist,
22 errored, 7 × 403, 5 × 404; tags seen: PROGRAM-DATE-TIME 49, DISCONTINUITY 40, CUE-OUT 3,
CUE-IN 1. Only the cue tags are broadcast events, so expect event badges on roughly 3 % of streams;
this one-shot sample read each playlist once and did not inspect in-band SCTE-35 or ID3.

## 3a. Network path diagnostics ("Network" panel)

When a stream is geo-blocked from this server, the operator needs to see **which hosts the server
really talks to, over which addresses, and how the internet sees the server**, so they can fix
routing (egress via a different site/VPN/address family) instead of guessing. The panel opens from
a probe result in the browser **and** from any existing source on the Sources page (it is not
limited to iptv-org sources).

`POST /api/v1/sources/{id}/network` (operate, per-object authorization) and the `network` block of
every probe result return the same report. It is collected on the control plane with the probe
bounds of §3 and every dial screened by `net_guard`.

| Section | Contents |
|---|---|
| **Server egress identity** | Public IPv4 and IPv6 as seen from the internet, egress country and CDN edge (Cloudflare `cdn-cgi/trace`: `ip`, `loc`, `colo`), queried per family. Cached 10 min. Fixed host; disable with `[network_diagnostics] egress_lookup = false`. |
| **Host chain** | Every distinct host involved: master playlist, media playlist, segment host(s), key URI host, every redirect hop. Segment CDNs often differ from the playlist host. |
| **DNS** (per host) | CNAME chain, A and AAAA records with TTL, the resolver used (hickory-resolver, system config, 3 s per lookup; a failure is shown as text; lookups only describe DNS, every dialled address is still screened), `net_guard` class of each address (public / private / blocked). |
| **Connection** (per host) | Address and family actually used, TCP connect time per address (v6 and v4 tried separately), TLS version and ALPN (from a small separate, bounded rustls handshake to the already-screened address; certificate not trusted-verified there, facts only),  certificate subject/SAN/issuer/expiry (parsed with `x509-parser`, already in `Cargo.lock`). |
| **HTTP** (per hop) | Status, redirect `Location` host, response time, `Server`, `Via`, `Age`, CDN identifiers (`cf-ray`, `x-cache`, `x-served-by`, `x-amz-cf-pop`, `x-akamai-*`, `x-cdn`), geo-related headers, and for 4xx a 512-byte text excerpt of the body (block pages often say "not available in your region"). Request headers sent (UA/Referer) are shown. |
| **Assessment** | Plain-language hints, for example: "Channel country AU; this server egresses from US (DFW) → geo-block likely. Route this host via an AU egress." · "IPv6 path returns 403, IPv4 path returns 200 → prefer IPv4 for this host." · "Playlist OK, segment host `cdn.example` 403 → the CDN, not the origin, blocks." |
| **Export** | Copy as text/JSON (credentials, userinfo and query strings redacted) and an equivalent `curl` command per hop (`--resolve`, `-4/-6`, headers) for reproducing from another host. |

Per-family comparison is the key tool: each host is fetched over IPv6 **and** IPv4 when both
exist, because many geo/CDN blocks differ by family. The probe never changes routing itself; the
assessment says what to change.

## 4. The browser (`/sources/iptv-org`)

Reached from **Browse IPTV-ORG** on the Sources page (a sub-page, not a separate nav item).

### 4.1 Map-first location search

The mockup uses real rows from the 2026-11-07 snapshot; only the layout is illustrative.

```
┌───────────────────────────────────────────────┬─────────────────────────────┐
│ 🔎 Where is it happening? [ Kansas City, MO ] │  Near Kansas City, Missouri │
│                                               │  ── Local (city) ────────── │
│      offline vector map — states shaded by    │  KSHB 41 · NBC · 720p       │
│      stream count, pins for cities with       │  KCKS-LD 25.3 · 720p (KS)   │
│      local streams, clustered when zoomed out │  ── Within 150 km ───────── │
│              ◉ Kansas City (2)                │  PBS Sedalia (KMOS) · 1080p │
│                   ◉ Sedalia (1)               │     🌐 Geo-blocked          │
│                                               │  ── National (US) ── 2,576 ▸│
│ [◎ near this server] [radius 150 km ▾]        │  [Probe all] [Add selected] │
└───────────────────────────────────────────────┴─────────────────────────────┘
```

- **Place search** over every country, subdivision, city and region name in the gazetteer,
  typo-tolerant, offline. iptv-org publishes one name per place, so `alt` is empty and alternative
  spellings match only through typo tolerance. Choosing a place flies the map there.
- **Locality-ranked results:** this city → cities within the radius (great-circle) → the
  subdivision → the country → regions that include it. Each tier shows its count; the national
  tier is collapsed by default so local results lead.
- Click a country or subdivision polygon to list everything that broadcasts there; click a pin
  for that city. Map filters combine with every other facet.
- The map is decorative-plus: the place search and result list are a complete keyboard and
  screen-reader path; shading is always paired with text counts.

**Map stack:** MapLibre GL JS (BSD-3) rendering bundled Natural Earth admin-0 and admin-1
boundaries (public domain, simplified, ~118 KB gz, lazy). City coordinates come from a committed
gazetteer generated from Wikidata P625 (CC0) by `cargo xtask iptv-gazetteer` — the server never
queries Wikidata at runtime. An optional operator basemap style URL is off by default.

### 4.2 Search, facets, results

- **Search** (minisearch, fuzzy + prefix) across channel name, alt names, feed name, network, owners,
  stream title.
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
**Unlink**, and **Probe** and **Network** actions.

### 4.4 Data-source attributions

Shown in the in-app “Data sources” footer of `/sources/iptv-org`:

| Data | Licence |
|---|---|
| [iptv-org](https://github.com/iptv-org/iptv) channel/stream database | Unlicense — the streams belong to their broadcasters |
| [Natural Earth](https://www.naturalearthdata.com/) admin-0 / admin-1 boundaries (bundled in `web/public/maps/`, simplified with mapshaper) | Public domain |
| [Wikidata](https://www.wikidata.org/) city coordinates (P625) | CC0 |
| [MapLibre GL JS](https://maplibre.org/) | BSD-3-Clause |

## 5. Security

- Sync egress: one compile-time host, TLS, no redirects.
- Probe egress: attacker-influenced URLs ⇒ always through `net_guard` (resolved-IP screen, pinned
  dial, re-screened redirects), bounded as in §3. Probe is `operate`; refresh/import are
  `administer`.
- **Catalogue egress is public-only.** Catalogue sync, logo fetches, the deep probe (the first URL
  and every playlist, segment, key and redirect hop) and `POST /sources/{id}/network` all dial under
  `AppState::catalogue_dial_policy` = `DialPolicy::deny_all_allowlistable()`, never the managed-device
  `dial_policy` (which permits RFC1918/ULA/CGNAT for LAN devices). Private, ULA, CGNAT, loopback,
  link-local/IMDS, unspecified, multicast and broadcast targets (IPv4-mapped IPv6 unwrapped) are refused
  before any connection, for IP literals, every resolved DNS answer and every redirect hop. The policy has
  no setter and no config key. **Consequence:** a source on a private network cannot be probed or
  diagnosed from the catalogue; there are no private diagnostics in this release. The device/Cast policy
  is unchanged.
- Per-object authorization: `already_added` and link status only reveal sources the caller may see.
- Logos are fetched and cached server-side (size-capped, image content types only) so the browser
  does not load third-party images.
- UA/Referer: no CR/LF, length-capped; only these two headers are settable.
- Follow-up (tracked separately): SSRF screening for ordinary HLS source URLs at ingest.

## 6. Delivery

| Slice | Scope | Class |
|---|---|---|
| A | `origin` + HLS `user_agent`/`referrer` (config, validation, OpenAPI mirror, web form fields), libav AVOptions | R2 |
| B | `catalogue` module (on the `iptv-net` fetcher): model (promoted from `xtask/src/iptv`), sync, snapshot, index/places/status/refresh/import/logos endpoints; `xtask iptv-gazetteer` + committed gazetteer | R2 |
| C | Deep probe + network-path diagnostics (§3a) + endpoints; chaos test with hostile fixtures (slowloris, huge segments, redirect to loopback, endless playlist) | R3 |
| D | Link tracker, events, health warnings, `follow` modes | R2 |
| E | Web: browser page, map, place search, facets, virtual list, probe panel, add/bulk add; Sources link column + review/switch dialogs | R2 |
| F | Docs, runbook, e2e | R2 |

## 7. Implementation notes (backend slices A–D)

Where the implementation made a concrete choice or deviates from §2–§3:

- **Feature layout.** The model/join/index/store/link-tracker/probe *analysis* and all
  routes are pure Rust and **always compiled** (so the OpenAPI document is identical in
  every build and `xtask` needs no TLS stack). Only network I/O sits behind the
  `multiview-control` feature `catalogue` (reqwest + rustls: the fixed-host sync client,
  the logo fetcher, the probe's guarded client). `catalogue-ffmpeg` adds the probe's
  decode stage (`multiview-ffmpeg::media_probe`). `multiview-cli` forwards both, and the
  shipped presets (`nvidia`, `apple`, `linux-vaapi`, `full`) enable `catalogue-ffmpeg`.
  Without `catalogue` the refresh/probe/logo endpoints answer an honest error; without
  `catalogue-ffmpeg` the probe reports stage 4 `skipped` and `verdict_basis` reads
  `stages 1-3 only …`, never claiming decoded media.
- **Dial guard.** Every probe/logo dial — the first URL and every redirect hop (followed
  manually, max 5, reqwest redirects off) — is screened with `net_guard`: IP literals in
  `DialGuard::screen_url`, DNS names by a `ScreeningResolver` over the *resolved*
  addresses. The only way to dial loopback is the test-only
  `DialGuard::for_tests_allowing_loopback_ports` (feature `_test-seams`, excluded from
  every release preset by `cargo xtask check-release-features`, never reachable from
  config or the API); it admits exactly the fixture's `[::1]:<port>`.
- **`places`** returns `streams` (tagged directly) and `streams_within` (including
  child places) for countries/subdivisions; cities carry `lat`/`lon` from the committed
  gazetteer (`crates/multiview-control/assets/iptv-org-gazetteer.tsv`, 1,973 cities, 42 KB;
  only cities referenced by a feed's `broadcast_area`). The embedded format is TSV, not
  gzip: it is already smaller than the header cost of compressing it.
- **`already_added`** is client-computed (web `flagsOf` over the sources the caller may see). The index is
  byte-identical for every caller and shares one cached gzip body and the base `ETag`.
- **Link status** is served at `GET /api/v1/catalogues/iptv-org/links` (per-object
  scoped; includes the change diff and ranked alternatives) and as a tally on `status`.
  `catalogue.link_status` is a lossless event on topic `inputs`, scoped by source id. The
  health warning is one aggregate code, `catalogue-link-attention` (a per-source code would
  defeat the coalescing key). `follow = auto` goes through `apply_source_update` — the
  same function behind `PUT /sources/{id}` — as actor `system:catalogue`.
- **Probe wire.** `POST probes` answers `202` with the job (`queued|running|done`); a
  15-minute cache hit answers `done` + `cached: true`. The 20 s wall clock cancels the
  probe future (dropping its sockets), the 16 MiB budget is shared by playlists and
  segments, and overflow of the 64-deep queue is shed instantly as `429`
  (`/problems/too-many-requests`). Results are not persisted across restarts.
- **HLS headers at ingest.** A source with `user_agent`/`referrer` is opened by libav
  directly (it carries the `user_agent`/`referer` AVOptions); the loopback segment
  fetcher and WebVTT variant pin do not carry them, so both are skipped for such a
  source. Sources without the fields are unchanged.
- **Config.** Durations use the existing `ms`/`s`/`m` units; there is no `h` unit (an existing test pins `30h` as rejected), so six hours is `360m`.

## 8. Measured cost (efficiency requirement)

Measured 2026-11-07 on the integration branch against the live iptv-org snapshot (18,599 index rows),
debug build, `multiview run --software` with a synthetic 2×2 layout, on the devcontainer host. A
release build is smaller and faster; host checks on mda001 are listed at the end.

| Item | Measured | Notes |
|---|---|---|
| Server memory, catalogue on vs off | RSS 339 MB vs 195 MB, **+144 MB** | Snapshot rows, places, logo map and the cached gzip index. |
| Server CPU at idle, catalogue on vs off | 12.08 vs 12.29 cores (the synthetic engine) | No measurable idle cost: no timer work between syncs. |
| Sync cadence | every 360 min (6 h), jittered; minimum 10 min | Configurable `[catalogues.iptv_org] interval`. |
| Sync bandwidth, first or changed | 6.5 MB gzip on the wire for all files | `guides.json` is the largest at 2.6 MB. |
| Sync bandwidth, unchanged | 0 B body per file (HTTP 304 via `If-None-Match`) | A forced refresh with nothing changed took 2.5 s wall. |
| Disk | 8.4 MB under `.multiview/catalogues/iptv-org/` | gzip files plus manifest. |
| Disk: logo cache | **≤ 256 MiB and ≤ 20,000 files** (bound, not a measurement) under `…/iptv-org/logos/` | Median logo 21 KB, p90 108 KB, cap 512 KiB; ~9,900 distinct URLs ≈ 200–300 MB if all were fetched, hence the LRU bound. |
| `GET …/index` | 2.2 MB gzip; served from a cached gzip body | ETag 304 when unchanged. |
| `GET …/places` | 203 KB gzip | |
| Probe limits | 4 concurrent, queue 64 (then 429), 20 s, 16 MiB, 60/min per principal, 15 min cache | Decode runs on the blocking pool, never the engine. |
| Web: lazy browser page chunk | 60.8 KB (18.2 KB gzip) | Loaded only on `/sources/iptv-org`. |
| Web: MapLibre | 1,078 KB (286 KB gzip) + worker 507 KB (143 KB gzip) + CSS 70 KB (10 KB gzip) | Lazy: the main chunk contains no MapLibre code. |
| Web: boundaries | 392 KB (115 KB gzip) | Lazy, fetched by the map only. |

**Fixed:** the server no longer marks `already_added` (the web computes it), so every caller gets the shared cached gzip body instead of a ~3 s per-request rebuild.

**Host checks still needed on mda001 (release build):** RSS delta, sync wall time and CPU with a
changed upstream, and probe CPU while the engine runs at full load.
