# Immediate stress/runtime writer coordination

Stressworker8e690: event-model owns RuntimeReader hotpath HIGH repair now. Do not edit production runtime functions; if inserting test module share limited append only with reread, or use separate tests file/module. New source_event_stress.rs can depend public test harness if necessary; production helper must be lead-approved. Signal writequiescence promptly so event-model can finish.

R3 defect confirmed beforemeasurement: sample cloned VecDTO rows+rebuilt allDTOs eachtick. Fix Sample holds lightweight SourceEventRead (immutableArc evidence) plus exact frame/scalars. API constructsDTO rows offclock. Compactbadge selection scans scalar priority and builds onlyoneoverlay per source. Stress verify noAPIreader does not allocate perrow DTO. Bound64sources*256events, saturatedproducer queue/stalledconsumer, deterministic engine ticks/stamps, not just averageelapsed threshold.

Packet wrapfix: default decoded exact/no-wrap; only explicit decoded_with_wrap(...,WrapBits::Mpeg33) when source clock provenMPEG33 and rawranges valid. Existing pipeline normalizer unconditionallyMpeg33 is not transportproof, so leave gate default conservative until actual input.format/stream domainverified.
