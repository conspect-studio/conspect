---
status: proposed
date: 2026-11-05
decision-makers: [openai/gpt-6-astra]
consulted: [OpenAI independent architecture review pending]
---

# Source-linked events, overlays and armed automation (#183)

## Context and drivers

Issue [#183](https://github.com/conspect-studio/conspect/issues/183) reports independent SCTE-35, timed ID3 and vendor SSAI signals on HLS sources. The operator requests automatic source-linked timers, multiview overlays that update/clear, external automation hooks, VT/switcher actions and scoped bars or bars+tone during breaks.

Requirements: R1 preserve distinct evidence origins; R2 align displays/actions with presented source media, not receipt time; R3 deduplicate and revise events; R4 support autonomous overlay lifecycle; R5 require explicit automation arming and authorization; R6 preserve manual intervention; R7 never block output cadence; R8 provide bounded history and diagnostics.

Existing [ADR-0072](ADR-0072.md) selects a normalized cue bus and existing command path; [ADR-0075](ADR-0075.md) describes manifest ingestion. This design extends those decisions rather than creating another command channel. Existing timers use wall-clock targets; they cannot represent source-relative events without a new timing adapter.

## Options and outcome

1. Create a persistent synthetic timer source per cue. Rejected: duplicate/configuration churn, stale targets and incorrect wall-clock semantics.
2. Run browser timers and client-triggered actions. Rejected: disconnected clients lose events and automation; preview latency differs from source time.
3. **Chosen: server-side source event tracker, observational overlay projections, and separately armed rule execution.** This meets R1–R8 and reuses the existing control command boundary.

## Contract

An observation identifies source, timeline epoch, evidence origin, event identity, semantic boundary, optional media PTS/duration and asset identity. Transport timestamps use exact integers. Segmentation descriptors supply time_signal identity and meaning; command type alone is insufficient. One section may contain several descriptors. Cross-origin evidence is correlated but never silently merged into proof of an aired advertisement.

The follow-up [mixed-content observation](https://github.com/conspect-studio/conspect/issues/183#issuecomment-6649491162) supersedes the earlier deterministic marker-onset hypothesis: an SSAI marker window can contain AD → live programme → AD. The same CAID also repeats across separate breaks, so it is asset identity, never a break-instance key. Vendor markers produce observational vicinity events, not inferred splice starts. Default production replacement rules require reliable boundary evidence and must not suppress programme on a vendor marker alone. Optional visual classification is separate evidence with confidence, not a universal channel-bug heuristic. The [later asset refinement](https://github.com/conspect-studio/conspect/issues/183#issuecomment-6650129288) confirms both rotating and recurring CAIDs, and that content can already be an ad at marker onset. Thus marker observation has no guaranteed preroll or precise splice timestamp. Evidence capture should retain pre-trigger and post-trigger segments under explicit byte/time limits. Optional content labels distinguish advertiser spot, network promo/bumper, programme and unknown; absence of a channel bug alone is not sufficient proof of an advertisement. Detection confidence from a few samples is not a general reliability estimate.

Each tracked event exposes announced/active/ended/cancelled/superseded/uncertain state, revision, timing basis and end confidence. Repeated input updates the same event. A matching end can arrive before the start takes effect; both boundaries remain scheduled. Marker presence/absence describes playlist observation, not an inferred break duration. Missing new cues at effective time do not invalidate advance announcements.

Source presentation position drives timers and action eligibility. Epoch changes invalidate stale mappings. Missing PDT does not preclude relative timers, but inferred UTC remains approximate. No stale event may replay an action after reconnect/restart. Retention, active events, pending actions and payload sizes have explicit bounds; overflow raises diagnostics and never blocks the engine.

Overlay bindings support source tile, dedicated event tile and layout-wide strip. Bindings are saved; generated events are not synthetic configuration sources. Policy selects warning lead, terminal hold and uncertain expiry. End/cancel/expiry clears presentation without deleting history or manufacturing end confirmation. Loss of source timing shows uncertainty, not a fabricated countdown.

Rules match typed lifecycle boundaries with source/origin/confidence conditions. Rules default disarmed. Dry run logs proposed actions without side effects. Inbound hooks require authentication, per-source authorization, bounded typed bodies and replay/idempotency protection; they cannot submit shell or arbitrary scripts. Outbound hooks use an approved destination policy and bounded asynchronous delivery. All control actions reuse authorized command/macro paths, with explicit late-event, readiness and failure policies.

Break replacement is an owned override lease: source/tile, selected output or program scope; bars/slate/VT replacement; explicit audio policy (original/silent/calibrated tone with channel mask). Monitor-only tone must never enter program audio. Keep original ingest/cues alive. Acquire video/audio/caption routing consistently at the existing apply boundary; restore only if the lease and routing generation still match. A later manual change revokes automatic restoration. Matching end is primary; duration fallback and maximum-hold policy are explicit. Emergency restore/disarm is always available. Tone uses the existing deterministic audio-store path, not an asynchronous producer.

## Consequences and confirmation

This touches control-plane and source/renderer boundaries: classify as R3 wherever output-clock/isolation invariants may be affected. Implementation must provide deterministic tests for duplicate/revised/cancelled cues, early end, timestamp wrap/discontinuity, unknown duration, stale state, replay and lost end. Include permission tests, manual override races, resource bounds, and a cadence/isolation soak with cue floods and unavailable webhook endpoints. VT readiness failure must not take an unready source. Verify bars+tone isolation and restoration of video/audio/captions. Verify actual server UI overlays and a controlled armed rule before reporting deployed behaviour.

Independent OpenAI architecture review (2026-11-05) confirmed the extension direction with required changes: future cue timestamps must use a non-mutating snapshot of the source normalizer; a video normalization call must never consume a future announcement. Actual source-frame timestamps must accompany the immutable event snapshot through delayed baking. Current command variants provide separate video/audio/subtitle routes and media transport, but no proven switcher/macro implementation; ADR-0072's existence claim is not implementation evidence. Atomic override support must coordinate audio sample boundaries and subtitle clear with video, not merely enqueue three commands in one drain tick. VT mailbox acceptance is not readiness. Conflicting override scopes are rejected rather than nested implicitly. Disarm prevents new actions; emergency restore separately performs compare-and-restore, reports generation conflicts and never overwrites a manual route. Full queues raise an alarm and fail closed for actions; recovery reconciles state without replaying historical transitions. A rule requires an explicit policy for events on sources not currently presented. Implementation review also requires: bounded semantic replay protection for A→B→A cue retransmissions (local arrival-order revisions never establish sender authority); an exposed normalizer mapping generation for implicit discontinuities; epoch invalidation before malformed-interval projection; marker withdrawal independent of clock freshness; a monotonic maximum-hold watchdog independent of stalled source media; source-scoped automation epoch/fence state; and release of revoked lease ownership while preserving replay tombstones. Execution must revalidate correlated event/epoch/authorization immediately before apply, including cancellation after reservation. These findings are required acceptance criteria, not deferred refinements.

Review tier: existing ADR answers the command architecture; independent same-vendor safety review completed, implementation review remains pending. User constraint permits OpenAI only, so cross-vendor review is unavailable and must not be represented as satisfied. Record findings and dissent here before acceptance. No production deployment precedes review and targeted verification; deployment is coordinated by the existing release owner.
