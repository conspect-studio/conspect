# Runtime integration decisions — implementation work in progress

## Clock generations (required before pipeline wiring)

Do not advance the shared audio/caption source lifecycle for cue-only mapping changes. A source lifecycle and a raw-to-media mapping generation are distinct identities. `SourceFrameTime.generation` currently carries lifecycle, so it cannot alone establish that a cue mapping matches a selected frame after an implicit normalizer discontinuity.

Preferred minimal extension: carry a dedicated event clock context with each published video frame (source lifecycle + normalizer mapping generation + normalized source media position). This context must follow the actual latched frame, not the latest ingest anchor. Extend the frame-store metadata type additively and update constructors/tests explicitly; do not reinterpret its existing generation field. An old buffered frame must retain its old mapping generation. A new mapping generation invalidates pending cue projections from the old one.

Event service producer registration must be available on the ingest supervisor/control path without touching output-clock locks. Register using a monotonic service epoch distinct from lifecycle, and retain the pair mapping in the immutable event snapshot. If safe atomic epoch allocation/registration is not available, publish an explicit unavailable state until it is; never reuse old mapping state after reconnect. Source removal must retire producer and event context without changing other sources.

The exact API extension still requires code review and tests before implementation. Do not create a global unbounded registry or reset audio/captions to work around missing metadata.

## Service snapshot review

The first service draft deep-clones all event state in `registry.snapshot()` when loss atomics differ from the worker snapshot. This is unsuitable for per-frame access: an invalid/overflow rejection may not publish a new worker snapshot, causing repeated deep copies every tick. Return a small read result containing the immutable Arc plus current degradation/overflow/epoch metadata, or move loss publication onto the worker. No full event-map clone on the output clock.

Add deterministic replacement/drop publication race tests and a delayed-consumer epoch mismatch test. Arc acquisition overlapping retirement can return old state; use-time identity validation is mandatory.

## Status

This note records concrete integration blockers and preferred remedies, not completed implementation. Runtime ingest worker 0916f180 and service worker 2804d2 own their original scopes. Core/framestore metadata extension must be explicitly assigned before edits. Lead owns coordination and final integration.
