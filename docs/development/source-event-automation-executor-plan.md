# Remaining actual automation executor work (not implemented)

Independent sizing confirms no existing atomic video/audio/caption command. Video applies clock-side, audio routing applies later bake-side via separate mailbox, subtitle route drains separately. Salvo only restores video. Do not report three queued commands as atomic success.

Required prepared transaction: resolve resources offclock, acquire global destination generation+lease across all manual video/audio/caption/layout edits, capture complete old routing bundle, publish immutable replacement bundle with effective output tick. Carry bundle/generation with queued bake item; audio applies at ProgramBus::position_at(effective_tick), split catch-up ranges spanning tick if frames shed. Captions clear/route from same bundle. Restore compares owned installed generation; manual edits revoke, no blind restore. Fail stale/full/unsupported before any mutation. Applied acknowledgement requires actual coherent output boundary, not mailbox acceptance.

Tone: existing AudioStore::line_up_tone supplies deterministic exact-cursor1kHz/-18dBFS allchannels. Extend validated level/channelmask as explicit config; no async tone producer. Monitor-only replacement cannot repoint shared program strip. Source-tile originalaudio policy can preserve program; tone requires dedicated scoped audio path.

Current output encodes once and fans identical media to sinks. Selected-output replacement needs separate composition/audio branch before support can be honest. Reject unsupported scope until implemented; it remains requested scope, not silently closed.

VT transport mailbox acceptance is not decoded readiness. Add actual asset/frame/PCM epoch readiness and recheck at take. External hooks typed authenticated, approved outbound targets, durable replay ledger. Operator arming/settings persistence and emergency restore mandatory.

Tests: siblingtile/program tone isolation; exactvideo/audio/captionboundary withshedticks; manualchange betweenreserve/apply/restore; failedprepare retains picture/caption; staleVT load/no firstframe; queueflood/watchdog/restart. R3 design/chaos gate before deployment.
