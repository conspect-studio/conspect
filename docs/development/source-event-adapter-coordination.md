# Active adapter coordination

- cue-adapter owns NativeEvents runtime section only, fixing earlycue/loss. It now also prepares safe observational untimed ID3/SCTE API; must never make unsupported provenance actionable.
- event-model owns RuntimeReader/API projection section only, fixing full bounded history, assets and active priority. No formatter across entire runtime while both edit.
- packet worker3c89 owns pipeline capture/drain + source_event_packet helper. Envelope(kind, packetPTS/timebase, preceding demux video fence); match decoded raw video fence, reject ambiguous/missing timestamp. Cue announcement PTS is never transport epoch evidence. Requests NativeEvents observational API for rejected timing.
- HLS bridge866 owns new source_event_hls_bridge/tests and ONLY start_with_observer pipelineopen region12239-12282. It must not edit packet worker drain hunks. Use bounded connection-owned bridge, not competing service registrations. Need runtime method accepts vendor presence under actual current registration after proven sameconnection; retirement discardsbridge without retiring replacement sharedproducer.

NativeEvents agreed API: attach_hls(receiver: source_event_hls_bridge::HlsBridgeReceiver) exactly once beforefirstmapping, then ingest_hls() drains internal source/currentserviceepoch/producer aftermapping. Packet worker must create NativeEvents when SCTE present OR hls_event_bridge.is_some(), move receiver into attach, and call ingest_hls() aftermapping. Bridgeworker has already modified ONLYpipeline startregion to call start_with_observer and retainlocal hls_event_bridge; DO NOT duplicate startregion. Packetworker ownsdrain. ID3 method observe_id3(&[u8]) validatedmetadataonly forthcoming cue-adapter.

Proposed observational API must return bounded metadata event identity/provenance, not public rawbytes. ID3 unknown semantics remains metadata, Apple PRIV timestamp notbreak. Unsupported SCTE transport timing may retain ID evidence as Observed but MUST NOT emit Start/End. Degradation is honest; arming rejects ambiguous evidence.

Lead will reconcile helper interfaces after worker proposals; no overlapping pipeline drain or runtime-section writes.
