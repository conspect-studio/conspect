# Managed IPTV playlist and upstream EPG

Status: implementation design; release gates pending.

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

## Implementation status (this slice)

Shipped and tested: the `Source.epg` (`EpgSource`: mode automatic/manual/disabled, url, channel_id, secret_ref, allow_private_network) and `Source.playlist_metadata` (`PlaylistMetadata`: tvg_id, name, logo, group) config blocks, their structural validation, OpenAPI mirrors, and the web source editor's "Upstream EPG" / "IPTV playlist presentation" fields. `routes::sources::guide_only_change` (present before this slice) already exempts edits to these two keys from the canonical restart-apply path — this slice adds direct integration-test coverage (`epg_only_edit_applies_live_on_an_otherwise_restart_only_kind`, `epg_edit_combined_with_a_media_change_still_requires_restart`) proving a guide-only `PUT` reports `X-Multiview-Apply: live` with **no** engine command enqueued, even on an otherwise restart-only kind (rtsp), and that a concurrent media-identity change is never masked by an accompanying EPG edit.

Deferred to a follow-up slice (not started): the bounded M3U import API/UI (§ "Fetch and resource bounds"), the managed playlist-publication config section and `GET /iptv/playlist.m3u8`, the XMLTV passthrough `GET /iptv/guide.xml` with its background refresher, and the auth decision for those two public endpoints. Operators can author EPG/playlist metadata per source today; nothing yet serves a combined playlist or filtered guide.

## Review status

The installed model catalogue is stale: its designated Anthropic architecture-review route is unavailable in this session. Reserved design review remains pending; parser and configuration work can proceed independently. Do not represent this document as an approved independent review.
