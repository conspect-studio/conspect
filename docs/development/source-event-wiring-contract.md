# Source event wiring contract (active implementation)

## Per-frame identity

`SourceFrameTime` retains existing `pts` and `generation` (shared source lifecycle for captions/audio). Add optional `event_clock: Option<EventClockContext>` where context carries `connection_epoch` and `mapping_generation`. Context is copied with pixels through the ring/latch; never read a latest global mapping to annotate an old frame. None means unsupported/unavailable, not generation zero. Normalizer mapping tokens restart per connection, so qualify with a connection epoch. Service registration uses a separate strictly increasing service epoch; snapshot must retain the frame identity tuple for exact selection matching.

## Active worker scopes

- Frame metadata worker deea26ec: framestore tile, engine narrow literals/tests, pipeline narrow struct literal compatibility only. No ingestion wiring yet.
- Caption compatibility worker f253035e: captions.rs and caption_ingest.rs literal None additions only.
- Service repair worker a163a6fe: service module/tests only; lightweight read wrapper and cloneable off-clock registration controller. No frame context shape invention necessary yet.
- Renderer worker f5a25747: additive overlays.rs event chip helper + tests; draw_list_with_events takes same-frame source projection map, no registry reads.
- Lead: HLS original metadata retention and subsequent integration; final reconciliation of identity snapshot registration is not yet implemented.

## Required service APIs

Service repair proposes `SourceEventRead { evidence: Arc<SourceEventSnapshot>, epoch, degraded, queue_overflow }`, registry.is_current(&read), and registration_controller().register(&self, source, service_epoch) serialized off-clock. No deep event clone on read, no register call from output clock. Extend registration later with exact frame clock context after metadata API is finalized; until then do not claim selected-frame matching implemented.

## Review outcomes

Foundation ea91809c and automation model0f201098 independently reviewed; not runtime deployment approval. Restore executor must distinguish definitive failed compare from unknown/transient dispatch outcome. An emergency/expiry Restore must not require the original Start still be current. Keep a global destination ownership check across independent per-source automation models.
