# ADR-M015: IPTV-ORG catalogue — server-side sync, source provenance, deep stream probe

- **Status:** Proposed <!-- R3 probe slice awaits operator sign-off -->
- **Area:** Management / Web/API / Ingest
- **Date:** 2026-11-07
- **Source:** Operator request (IPTV-ORG source browser); design [`docs/io/iptv-org.md`](../io/iptv-org.md)

## Context

Operators want to find and add public live channels from the community
[iptv-org](https://github.com/iptv-org/iptv) database (Unlicense) without hand-copying URLs, find
local broadcasters by place during a breaking event, and keep sources linked to iptv-org so an
upstream URL change is noticed.

What is true on `main` today (2026-11-07):

- `SourceKind::Hls { url }` carries only a URL. 1,081 of the 18,602 iptv-org streams need a
  `User-Agent` and 341 need a `Referer`; Conspect cannot send either.
- `Source` has no provenance field; nothing records where a source came from.
- The control plane makes no outbound catalogue fetches. The only outbound dials are the managed-device
  poller and Cast sessions, both screened by the [ADR-M013](ADR-M013.md) `net_guard`.
- HLS source URLs are not screened at all (`Source::validate`, reachability is "a runtime concern").
- The rc3 line ships the managed IPTV playlist/EPG ([epg-playlist.md](../architecture/epg-playlist.md)):
  `Source.playlist_metadata`, `Source.epg`, M3U import, and the SSRF-screened `iptv::fetch` fetcher.
  This ADR builds on them and does not duplicate them.
- `cargo xtask soak-iptv` already models, joins and classifies the iptv-org JSON (dev-only, `ureq`).
- iptv-org streams have **no stable id**: 2,096 streams have no channel, 4,976 share a
  `channel@feed` with another stream; URLs are unique.

Constraints: invariant #1 (output clock) and #10 (engine never awaits a client) — any catalogue or
probe work must stay off the data plane; IPv6-first; per-object authorization; the default build
stays LGPL-clean and GPU-free; air-gapped installs must keep working.

## Decision

1. **Server-side catalogue.** A new `catalogue` module in `multiview-control` (feature `catalogue`,
   on in the shipped presets, off in the bare default build) syncs the iptv-org API JSON from the
   **fixed** host `iptv-org.github.io` with `If-None-Match` (ETag) on a configurable interval
   (default 6 h, minimum 10 min), joins it into one compact snapshot, and persists it under
   `<config-dir>/.multiview/catalogues/iptv-org/`. An operator can import a snapshot file for an
   offline site. The browser never contacts iptv-org, a map service or a stream directly.
   Endpoints: `GET /api/v1/catalogues/iptv-org/{status,index,places}`,
   `POST /api/v1/catalogues/iptv-org/refresh` (administer),
   `POST /api/v1/catalogues/iptv-org/import` (administer),
   `GET /api/v1/catalogues/iptv-org/logos/{key}` (bounded server-side logo cache).
2. **Provenance on `Source`.** An optional, kind-independent `origin` block:
   `{ provider = "iptv-org", channel?, feed?, stream_url, labels[], linked_at, follow }` with
   `follow = "manual" | "notify" | "auto"`, **default `notify`**. It round-trips through REST,
   TOML export/import and OpenAPI.
3. **HLS request headers.** `SourceKind::Hls` gains optional `user_agent` and `referrer`, validated
   (no CR/LF, ≤ 1 KiB) and passed to libav as the `user_agent` / `referer` AVOptions on the
   HLS demux open. No other header is settable.
4. **Link tracker.** After each sync the control plane recomputes a per-source link status
   (`in_sync`, `updated`, `stream_removed`, `channel_closed`, `blocklisted`, `unlinked`) and
   publishes it on the realtime bus and as a health warning. `auto` applies only URL/header
   changes for the same `channel@feed` through the normal If-Match write path, audited.
5. **Deep probe.** `POST /api/v1/catalogues/iptv-org/probes` (operate) runs a bounded,
   control-plane probe that proves real media, not just a reachable root: playlist → variant →
   two reloads (liveness, PROGRAM-DATE-TIME drift) → 2–3 segments demuxed **and decoded** with the
   existing `multiview-ffmpeg` wrappers → media facts, thumbnail and broadcast-event detection
   (`EXT-X-CUE-OUT/IN`, `EXT-X-DATERANGE` incl. SCTE35 attrs, in-band SCTE-35 via
   `multiview_input::scte`, ID3 timed metadata, discontinuities). Every dial goes through
   `net_guard` (resolved-IP screen, pinned IP, no unscreened redirects). Limits: 4 concurrent probes,
   20 s wall clock, 16 MiB per probe, 60 probes/min per principal. Results are cached and carry the
   probing host's identity ("from mda001").
6. **Network-path diagnostics.** The probe also returns a network report (host chain, DNS,
   per-family connection, TLS, per-hop HTTP/CDN headers, block-page excerpt) and the server's
   egress identity (public IPv4/IPv6 + country via the fixed host Cloudflare `cdn-cgi/trace`), with
   a plain-language geo-block assessment. Available for any source via
   `POST /api/v1/sources/{id}/network`.
7. **Map.** The web UI renders a MapLibre GL map from bundled Natural Earth admin-0/1 boundaries
   (public domain) and a committed city gazetteer generated from Wikidata P625 (CC0) by
   `cargo xtask iptv-gazetteer`. No tile server is needed; an optional operator basemap URL is off
   by default.

## Rationale

- Linking needs a job that runs with no browser open, so sync must be server-side; a single snapshot
  also serves air-gapped sites and keeps operator browsers off third-party hosts.
- A fixed upstream host adds no attacker-controlled egress. The probe does dial attacker-influenced
  URLs, so it must use the M013 guard — that is why the probe ships as its own R3 slice.
- `channel@feed` + `stream_url` is the most stable identity iptv-org offers; storing the linked
  URL lets the tracker tell "upstream changed" from "operator edited".
- `notify` default: a silent URL swap on an on-air source is surprising; operators opt in to `auto`.
- Decode-level probing: a 200 on the playlist proves little (in a 120-stream sample, 86 returned a
  playlist; a playlist alone does not prove segments decode or that the stream is live).
- Local coverage is limited by iptv-org itself (median 4 local streams per US state, Kansas 2); the
  UI states this rather than implying complete coverage.
- Wikidata covers 1,974 of the 1,983 cities iptv-org feeds reference; UN/LOCODE covers only 763.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Browser fetches iptv-org directly (CORS is open) | No linking without an open browser; leaks operator browsing; breaks air-gapped sites. |
| Store provenance in `form.extra` / unknown keys | The typed `Source` model drops unknown keys; provenance would not survive TOML round-trip. |
| Generic `http_headers` map on HLS | Header injection surface; iptv-org needs only UA and Referer. |
| Probe = HEAD/GET of the root playlist | Does not prove media decodes or is live; the operator explicitly required real-data proof. |
| Hosted map tiles (OSM, Mapbox) | Third-party egress from the browser, usage policies, offline breakage. |
| Leaflet + raster tiles | Needs tiles; MapLibre renders bundled vector boundaries offline. |
| Run the probe on the engine/data plane | Violates invariants #1/#10. |

## Consequences

- New control-plane egress to one fixed host, documented and kill-switchable
  (`[catalogues.iptv_org] enabled = false`).
- `multiview-ffmpeg` HLS open gains two AVOptions (R2 data-plane crate change; no clock impact).
- The probe extends `net_guard` usage to a new caller (R3: this ADR, a chaos test proving bounded
  concurrency/memory under hostile streams, and operator sign-off).
- Web bundle grows by MapLibre (1,104 KB min, 298 KB gz, lazy-loaded on the browser route only) and the
  boundary assets (~118 KB gz, lazy) and the gazetteer.
- Follow-up (tracked, not in scope): SSRF screening of ordinary HLS source URLs at ingest.
- iptv-org content terms: the catalogue is Unlicense; individual streams belong to their
  broadcasters. The UI states this and links each channel's website.
