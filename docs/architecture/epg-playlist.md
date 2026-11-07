# Managed IPTV playlist and upstream EPG

Status: implemented (see Implementation status); release gates pending.

## Scope

Add optional source EPG and IPTV metadata, bounded M3U catalogue import, an explicitly enabled managed IPTV publication, and filtered XMLTV delivery. This is not an HLS master playlist, a media relay, a new encoder, or automatic source synchronization. Import requires an operator selection. Refresh never changes source URLs, IDs, operator overrides, layout bindings or deletion decisions.

The integration branch starts at production checkpoint `fdb84ae7`; `origin/main` is older. Production-preserving integration and the prerequisite hotfix chain must be reconciled before the omnibus can ship. No deployment may replace those fixes with the old main baseline.

## Invariants

All fetch, parse, refresh and publication work belongs to the management plane. No frame/audio callback performs network or filesystem I/O for this feature. There is no engine command for an EPG-only change. Downloading a catalogue never allocates an encoder or decoder. Slow clients consume only capped delivery resources, not an engine queue.

Use existing output playback endpoints. Direct input URLs require explicit publication permission and cannot disclose stored credentials. Non-playable targets remain unavailable with a reason. Program and preview require a compatible existing output; a composite does not inherit a selected input's guide.

## Data and durability

Source EPG configuration records mode (automatic/manual/disabled), provider XMLTV URL, exact channel ID, optional secret reference, and explicit private-network permission. IPTV presentation metadata remains separate from media identity. Manual settings override imported declarations. Arbitrary comments produce suggestions, not automatic EPG assignments.

Publication configuration contains enabled state, a stable ordered set of resource references, presentation overrides and delivery policy. Default is disabled. Source/output references are authorized and validated at mutation time and again at publication time. Source removal invalidates the publication immediately; it must not leak through a stale snapshot.

Existing ETag/If-Match and audit conventions apply. Source changes use the canonical source mutation path; they must not bypass the desired-state transaction under development. Publication state must have independent durable storage until integrated with that transaction. Cache files are derived state, never authoritative configuration. Restart must restore configuration without silently replacing current source definitions.

## Fetch and resource bounds

Deduplicate feeds by URL, credential identity and network policy. Reuse HTTP clients within the same policy. Use conditional requests and refresh backoff with jitter. No upstream fetch occurs during a download request. No periodic refresh runs without an enabled consumer.

Reuse the repository's resolved-address screening policy. Screen IP literals and actual DNS results, disable ambient proxies, and validate every redirect separately. Do not forward credentials across origins. Private-network permission does not authorize cloud metadata, unspecified or multicast destinations. Logs and public errors must not contain credentials or sensitive query strings.

Enforce independent caps on feed count, concurrent fetches, compressed bytes, expanded bytes, parser tokens/depth, channels, programmes, output bytes, schedule window, disk usage and client concurrency. Implement XML filtering as a bounded streaming transform, not a document tree. DTD and entity declarations are rejected. Filter selected channels only and rewrite IDs consistently to stable publication IDs.

An update becomes visible only after complete validation. Generate plain and gzip snapshots once, with deterministic ETags. Atomically publish a consistent playlist/guide generation. Retain the last valid guide on transient failure, expose its age and error state, and stop serving it after the configured maximum stale age. Never serve stale authorization or deleted entries as last-good data.

## Verification and release gates

TDD covers source round trips and validation, M3U/HLS distinction, quoted attributes and relative URLs, comment suggestions, XML escaping and timezone handling, mapping collisions, limits, malformed feeds, entity attacks, DNS rebinding, redirects, private-network policy, credential redaction, authorization, conditional requests, refresh cancellation, disable/delete races and restart recovery.

Measure idle overhead with the feature disabled, repeated catalogue reads with no parse/fetch work, shared-feed refresh count, peak parser memory, cache disk bounds, and slow upstream/client isolation. Include chaos/soak evidence for output-clock non-interference. Report measured results rather than estimated percentage savings.

Before deployment: independent cross-vendor review, targeted tests, full CI, production-preserving omnibus merge, state capture and rollback artifact. After deployment: verify the public URL, authenticated playlist and XMLTV, an actual player, metadata correspondence, and unchanged active layout/source/audio state.

## Implementation status (shipped)

**Per-source configuration.** `Source.epg` (mode automatic/manual/disabled, url, channel_id, secret_ref, allow_private_network) and `Source.playlist_metadata` (tvg_id, name, logo, group), structural validation, OpenAPI mirrors and the web source editor sections. Guide/presentation-only `PUT`s apply live with no engine command (`guide_only_change`; integration-tested on a restart-only kind).

**M3U import.** `crates/multiview-control/src/iptv/m3u.rs` is a pure, bounded extended-M3U parser: `#EXTM3U url-tvg` / `x-tvg-url` (comma lists split, deduplicated), `#EXTINF` `tvg-id`/`tvg-name`/`tvg-logo`/`group-title` and the display name after the first unquoted comma, quoted/unquoted attributes, relative URLs resolved against the fetch URL, at most 2 MiB and 5000 entries, and an HLS media/master playlist (`#EXT-X-…`) rejected as "not a catalogue". `POST /api/v1/sources/import/m3u` (`Write` role) takes `{url}` or `{text}`; a URL is fetched through the SSRF-screened fetcher (below) with a 20 s timeout and the 2 MiB cap, public addresses only unless `allow_private_network` (operator dial policy). It returns candidates and creates nothing. The Sources page "Import from M3U" dialog lists candidates with checkboxes and creates the selection through the ordinary `POST /api/v1/sources/{id}` path as HLS sources with `epg {mode: manual, url: <first url-tvg>, channel_id: tvg-id}` (when both exist) and `playlist_metadata`; ids are slugs with `-2`, `-3`… on collision.

**Managed publication.** Optional `[iptv]` config section (`enabled` default false, optional `public_base_url`, ordered `entries[{kind: output|source, ref, tvg_id?, name?, logo?, group?, allow_direct_source_url?}]`, max 512) persisted through the canonical desired-state transaction (`prospective_config` + `commit_desired`), so it survives restart and is visible to the config-file watcher as an `iptv` section. Document validation is structural only (deleting a published source/output is never blocked; the entry resolves as unavailable). `GET`/`PUT /api/v1/iptv` uses a content `ETag` (`If-Match` required: `428`/`412`), validates every reference on `PUT` (unknown source/output → `422`; an output must be HLS/LL-HLS; a source entry requires `allow_direct_source_url`), and authorizes **every referenced id** per object both in the new and in the current document (a scoped key can neither publish nor unpublish objects outside its scope; `GET` filters entries to the caller's scope). Output entries link to the same `/hls/{mount}/{playlist}` path the listener mounts (config order, sanitised `Output::id()`, `-2` collision suffix — mirrored from `multiview-cli` `hls_mounts`); source entries link to the upstream URL with userinfo removed. The web "IPTV playlist" page (`/iptv`) enables publication, adds/removes/reorders outputs and sources, edits overrides, copies the URLs and shows guide feed status.

**Public files and the auth choice.** `GET /iptv/playlist.m3u8`, `GET /iptv/guide.xml` (gzip when `Accept-Encoding: gzip`) and `GET /iptv/guide.xml.gz` are mounted outside `/api/v1` and are **unauthenticated, exactly like the `/hls/{output}` delivery mounts**: IPTV players and set-top boxes cannot send bearer tokens. They expose only what an operator explicitly published (output playback paths that are already public under `/hls`; source URLs only with `allow_direct_source_url`, never credentials) and answer `404` while disabled. Both carry strong `ETag`s with `If-None-Match` → `304`, `Cache-Control: public, max-age=60` and `Access-Control-Allow-Origin: *`. The playlist header is `#EXTM3U url-tvg="<base>/iptv/guide.xml"`; `<base>` is `public_base_url`, else the request `Host` (validated) with `X-Forwarded-Proto` or the listener's TLS state. A request never triggers an upstream fetch.

**XMLTV passthrough.** One management-plane tokio task (`iptv::spawn_refresher`, started by `multiview run`) re-reads accepted intent every 60 s or on a publication change; while disabled it fetches nothing and drops cached guide data. For each distinct `epg.url` of published, playable sources (EPG mode not disabled, max 32 feeds) it fetches sequentially at most every 30 min + up to 5 min jitter (a changed wanted-set refetches, but never more than once a minute), with `If-None-Match`/`If-Modified-Since`. The fetcher (`iptv/fetch.rs`, cargo feature `iptv-net`, enabled by the cli `web` feature) uses `net_guard::resolve_and_screen` inside the client resolver (vetted addresses are dialled), screens IP literals and every redirect hop (max 5, http(s), no userinfo), disables ambient proxies, sends no credentials, caps the body at 50 MiB on the wire with a 120 s timeout and spools it to an anonymous temp file. Private-network feeds are refused unless every source using that feed sets `allow_private_network`. Decompression (gzip by magic, `MultiGzDecoder`) is capped at 200 MiB expanded; the `quick-xml` 0.41 streaming filter (`iptv/xmltv.rs`, on a blocking thread) rejects DOCTYPE and non-predefined entity references, enforces depth/element/channel/programme/output caps, keeps only `<channel>`/`<programme>` of the wanted provider ids, rewrites ids to the published `tvg-id` and keeps programmes overlapping now…now+7 days. Fragments are cached per feed and assembled into one guide (published-entry order; a channel element is synthesized for entries without upstream data) generated as plain + gzip with a content `ETag` and swapped atomically. A failed feed keeps its last good fragment (stale data dropped after 48 h) and reports the error; `GET /api/v1/iptv` returns per-feed last attempt/success, next attempt, error, channel and programme counts. A publication change re-assembles from cached fragments without refetching, so a removed or deleted entry is never served from a stale snapshot.

**Dependencies.** `quick-xml` raised to 0.41 workspace-wide (RUSTSEC-2026-0194/0195; `multiview-weather` adapted to the 0.41 API) and the yanked `crypto-bigint` 0.7.6 updated; `flate2` was already in the lock.

**Not done.** `epg.secret_ref` is not resolved (feeds requiring credentials fail honestly); `automatic` EPG mode has no feed discovery and behaves like manual only when url + channel_id are set; no guide disk cache (in-memory only; refetched after restart); no per-client delivery cap beyond the listener limits; no comment-derived EPG suggestions on import; the soak/idle-overhead measurements and cross-vendor review listed above are outstanding.

## Review status

The installed model catalogue is stale: its designated Anthropic architecture-review route is unavailable in this session. Reserved design review remains pending; parser and configuration work can proceed independently. Do not represent this document as an approved independent review.
